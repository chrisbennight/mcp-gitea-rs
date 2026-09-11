"""Run MCP client checks against a disposable Docker server and loopback Gitea fake.

Requires Docker on the local Linux host and the official Python SDK environment.
The optional TypeScript client must live beside its installed SDK package.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request


class Upstream(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.headers.get("Authorization") != "token fixture-service-token":
            self.send_error(401)
            return
        if self.path == "/api/v1/version":
            body = json.dumps({"version": "1.26.4"}).encode()
            media_type = "application/json"
        elif self.path == "/api/v1/repos/fixture/fixture/actions/jobs/1/logs":
            body = b"fixture log line\n" * 8192
            media_type = "text/plain"
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", media_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", help="Locally built image to test")
    parser.add_argument("--typescript-client", type=Path)
    args = parser.parse_args()
    if args.typescript_client and not args.typescript_client.is_file():
        parser.error("TypeScript client path must be an existing file")
    with socket.socket() as available:
        available.bind(("127.0.0.1", 0))
        port = available.getsockname()[1]
    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    worker = threading.Thread(target=upstream.serve_forever, daemon=True)
    worker.start()
    name = "mcp-compatibility-" + secrets.token_hex(6)
    bearer = secrets.token_hex(32)
    env = os.environ.copy()
    env.update({
        "GITEA_MCP_UPSTREAM_URL": f"http://127.0.0.1:{upstream.server_port}",
        "GITEA_MCP_SERVICE_TOKEN": "fixture-service-token",
        "GITEA_MCP_GATEWAY_BEARER_CURRENT": bearer,
        "GITEA_MCP_HOST": "127.0.0.1",
        "GITEA_MCP_PORT": str(port),
        "GITEA_MCP_ALLOWED_HOSTS": f"127.0.0.1:{port}",
        "MCP_TEST_URL": f"http://127.0.0.1:{port}/mcp",
        "MCP_TEST_BEARER": bearer,
    })
    command = ["docker", "run", "--detach", "--pull=never", "--name", name,
               "--network", "host", "--read-only", "--cap-drop", "ALL",
               "--security-opt", "no-new-privileges"]
    for key in ("GITEA_MCP_UPSTREAM_URL", "GITEA_MCP_SERVICE_TOKEN",
                "GITEA_MCP_GATEWAY_BEARER_CURRENT", "GITEA_MCP_HOST",
                "GITEA_MCP_PORT", "GITEA_MCP_ALLOWED_HOSTS"):
        command.extend(["--env", key])
    command.append(args.image)
    created = False
    try:
        # Cleanup also runs if Docker reports an ambiguous creation failure.
        created = True
        subprocess.run(command, env=env, check=True, stdout=subprocess.DEVNULL, timeout=30)
        for _ in range(30):
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=1) as response:
                    assert response.status == 200
                break
            except (urllib.error.URLError, TimeoutError):
                time.sleep(1)
        else:
            raise RuntimeError("Docker server did not become ready")
        request = urllib.request.Request(env["MCP_TEST_URL"], data=b"{}",
                                         headers={"Content-Type": "application/json"})
        try:
            urllib.request.urlopen(request, timeout=5)
        except urllib.error.HTTPError as error:
            assert error.code == 401
        else:
            raise AssertionError("MCP accepted an unauthenticated request")
        print("Docker PAT-only startup and ingress rejection PASS", flush=True)
        subprocess.run([sys.executable, str(Path(__file__).with_name("python_client.py"))],
                       env=env, check=True, timeout=60)
        if args.typescript_client:
            subprocess.run(["node", str(args.typescript_client.resolve())],
                           env=env, check=True, timeout=60)
    finally:
        try:
            if created:
                subprocess.run(["docker", "rm", "--force", name], check=True,
                               stdout=subprocess.DEVNULL, timeout=30)
        finally:
            upstream.shutdown()
            upstream.server_close()
            worker.join(timeout=5)


if __name__ == "__main__":
    main()
