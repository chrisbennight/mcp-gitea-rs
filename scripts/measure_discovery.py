"""Measure cold and warm discovery using a fresh server and loopback fake.

Reports server resident memory and RPC JSON bytes, not allocator or token counts.
Run both binary candidates with this same harness and repeat for variance.
"""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import secrets
import socket
import statistics
import subprocess
import threading
import time
import urllib.error
import urllib.request

from measure_results import file_hash, memory


def source_hash(root):
    digest = hashlib.sha256()
    for path in sorted((root / "crates").rglob("*.rs")) + [root / "Cargo.toml", root / "Cargo.lock"]:
        digest.update(str(path.relative_to(root)).encode() + b"\0" + path.read_bytes())
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--source-root", type=Path, required=True)
    parser.add_argument("--iterations", type=int, default=30, choices=range(1, 101))
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = args.source_root.resolve(strict=True)
    source = source_hash(root)

    class Upstream(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            if self.path != "/api/v1/repos/fixture/fixture":
                self.send_error(404)
                return
            payload = b'{"id":1,"name":"fixture","full_name":"fixture/fixture"}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    bearer = secrets.token_hex(32)
    environment = {key: value for key, value in os.environ.items() if not key.startswith("GITEA_MCP_")}
    environment.update({"GITEA_MCP_UPSTREAM_URL": f"http://127.0.0.1:{upstream.server_port}",
                        "GITEA_MCP_SERVICE_TOKEN": secrets.token_hex(20),
                        "GITEA_MCP_GATEWAY_BEARER_CURRENT": bearer,
                        "GITEA_MCP_HOST": "127.0.0.1", "GITEA_MCP_PORT": str(port),
                        "GITEA_MCP_ALLOWED_HOSTS": f"127.0.0.1:{port}"})
    started = time.perf_counter()
    process = subprocess.Popen([str(binary)], env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    session = None
    identifier = 0

    def rpc(method, params=None, notification=False):
        nonlocal identifier
        identifier += 1
        message = {"jsonrpc": "2.0", "method": method}
        if not notification:
            message["id"] = identifier
        if params is not None:
            message["params"] = params
        headers = {"Authorization": "Bearer " + bearer, "Content-Type": "application/json",
                   "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-11-25"}
        if session:
            headers["Mcp-Session-Id"] = session
        request = urllib.request.Request(origin + "/mcp", data=json.dumps(message).encode(), headers=headers)
        begin = time.perf_counter()
        with urllib.request.urlopen(request, timeout=30) as response:
            if notification:
                response.read()
                return None
            if "text/event-stream" in response.headers.get("Content-Type", ""):
                for line in response:
                    if line.startswith(b"data:") and line[5:].strip():
                        data = line[5:].strip()
                        decoded = json.loads(data)
                        if decoded.get("id") == identifier:
                            break
                else:
                    raise RuntimeError("missing RPC response")
            else:
                data = response.read()
                decoded = json.loads(data)
            assert "error" not in decoded and not decoded["result"].get("isError"), "benchmark RPC failed"
            return decoded["result"], len(data), time.perf_counter() - begin, response.headers

    try:
        for _ in range(200):
            if process.poll() is not None:
                raise RuntimeError("server exited before readiness")
            try:
                with urllib.request.urlopen(origin + "/healthz", timeout=0.2):
                    break
            except (urllib.error.URLError, TimeoutError):
                time.sleep(0.025)
        else:
            raise RuntimeError("server not ready")
        report = {"health_ready_seconds": time.perf_counter() - started,
                  "memory_at_readiness": memory(process.pid), "iterations": args.iterations,
                  "binary_sha256": file_hash(binary), "source_sha256": source,
                  "harness_sha256": file_hash(Path(__file__)),
                  "scope": "fresh process; loopback fake; debug build unless supplied otherwise"}
        initialized = rpc("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                         "clientInfo": {"name": "discovery-measurement", "version": "1"}})
        session = initialized[3]["Mcp-Session-Id"]
        report["initialize_seconds"] = initialized[2]
        rpc("notifications/initialized", notification=True)
        requests = {
            "tools_list": ("tools/list", {}),
            "search": ("tools/call", {"name":"catalog.search", "arguments":{"query":"repoGet", "limit":1}}),
            "describe": ("tools/call", {"name":"catalog.describe", "arguments":{"name":"repoGet"}}),
            "validated_read": ("tools/call", {"name":"api.read", "arguments":{
                "operation_id":"repoGet", "arguments":{"owner":"fixture","repo":"fixture"}}}),
        }
        cases = {}
        for name, request in requests.items():
            result, size, duration, _ = rpc(*request)
            cases[name] = {"first_seconds":duration, "first_rpc_json_bytes":size}
            if name == "tools_list":
                definitions = [{key: tool[key] for key in ["name","description","inputSchema"]} for tool in result["tools"]]
                report["published_definition_bytes"] = sum(len(json.dumps(tool,separators=(",",":"),ensure_ascii=False).encode()) for tool in definitions)
                bootstrap = next(tool for tool in definitions if tool["name"] == "repository.bootstrap")
                report["bootstrap_definition_bytes"] = len(json.dumps(bootstrap,separators=(",",":"),ensure_ascii=False).encode())
        report["memory_after_first_calls"] = memory(process.pid)
        for name, request in requests.items():
            rows = [rpc(*request)[1:3] for _ in range(args.iterations)]
            cases[name].update({"warm_median_seconds":statistics.median(row[1] for row in rows),
                                "warm_maximum_seconds":max(row[1] for row in rows),
                                "warm_minimum_seconds":min(row[1] for row in rows),
                                "warm_rpc_json_bytes_min":min(row[0] for row in rows),
                                "warm_rpc_json_bytes_max":max(row[0] for row in rows)})
        report["memory_after_warm_calls"] = memory(process.pid)
        report["cases"] = cases
        assert source_hash(root) == source, "source changed during measurement"
        print(json.dumps(report, indent=2, sort_keys=True))
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        upstream.shutdown()
        upstream.server_close()


if __name__ == "__main__":
    main()
