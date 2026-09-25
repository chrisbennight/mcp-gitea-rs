"""Measure retained-result access with a disposable loopback fake, never live Gitea.

Prints aggregate bytes, latency and Linux server memory measurements only.
Credentials and payloads stay in memory. Run each mode in a fresh process:
python3 scripts/measure_results.py target/debug/mcp-gitea-rs --mode selection
python3 scripts/measure_results.py target/debug/mcp-gitea-rs --mode compatibility
"""

import argparse
import concurrent.futures
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


def file_hash(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def source_hash():
    root = Path(__file__).resolve().parents[1]
    source = hashlib.sha256()
    paths = sorted((root / "crates").rglob("*.rs")) + [root / "Cargo.toml", root / "Cargo.lock"]
    for path in paths:
        source.update(str(path.relative_to(root)).encode() + b"\0" + path.read_bytes())
    return source.hexdigest()


def memory(pid):
    values = {}
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith(("VmRSS:", "VmHWM:")):
            key, value, _ = line.split()
            values[key.removesuffix(":") + "_kib"] = int(value)
    return values


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--mode", choices=["selection", "compatibility"], required=True)
    parser.add_argument("--readers", type=int, choices=range(1, 9), default=8)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    source = source_hash()
    marker = b"FAILURE: synthetic compilation error\n"
    payload = (b"successful build line\n" * 500_000)[:10 * 1024 * 1024 - len(marker)] + marker

    class Upstream(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            if self.path != "/api/v1/repos/fixture/fixture/actions/jobs/1/logs":
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    worker = threading.Thread(target=upstream.serve_forever, daemon=True)
    worker.start()
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    bearer = secrets.token_hex(32)
    env = {key: value for key, value in os.environ.items() if not key.startswith("GITEA_MCP_")}
    env.update({"GITEA_MCP_UPSTREAM_URL": f"http://127.0.0.1:{upstream.server_port}",
                "GITEA_MCP_SERVICE_TOKEN": secrets.token_hex(20),
                "GITEA_MCP_GATEWAY_BEARER_CURRENT": bearer,
                "GITEA_MCP_HOST": "127.0.0.1", "GITEA_MCP_PORT": str(port),
                "GITEA_MCP_ALLOWED_HOSTS": f"127.0.0.1:{port}",
                "GITEA_MCP_FILE_PUBLIC_ORIGIN": origin,
                "GITEA_MCP_MAX_CONCURRENT_REQUESTS": "8", "GITEA_MCP_LOG_LEVEL": "info"})
    process = subprocess.Popen([str(binary)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    lock = threading.Lock()
    identifier = 0
    session = None

    def rpc(method, params=None, notification=False):
        nonlocal identifier
        with lock:
            identifier += 1
            request_id = identifier
        message = {"jsonrpc": "2.0", "method": method}
        if not notification:
            message["id"] = request_id
        if params is not None:
            message["params"] = params
        headers = {"Authorization": "Bearer " + bearer, "Content-Type": "application/json",
                   "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-11-25"}
        if session:
            headers["Mcp-Session-Id"] = session
        request = urllib.request.Request(origin + "/mcp", data=json.dumps(message).encode(), headers=headers)
        started = time.perf_counter()
        with urllib.request.urlopen(request, timeout=30) as response:
            if notification:
                response.read()
                return None
            if "text/event-stream" in response.headers.get("Content-Type", ""):
                for line in response:
                    if not line.startswith(b"data:") or not line[5:].strip():
                        continue
                    data = line[5:].strip()
                    decoded = json.loads(data)
                    if decoded.get("id") == request_id:
                        break
                else:
                    raise RuntimeError("no matching RPC response")
            else:
                data = response.read()
                decoded = json.loads(data)
            if "error" in decoded:
                raise RuntimeError("benchmark RPC failed")
            return decoded["result"], len(data), time.perf_counter() - started, response.headers

    try:
        for _ in range(200):
            if process.poll() is not None:
                raise RuntimeError("benchmark server exited before readiness")
            try:
                with urllib.request.urlopen(origin + "/healthz", timeout=0.2):
                    break
            except (urllib.error.URLError, TimeoutError):
                time.sleep(0.025)
        else:
            raise RuntimeError("benchmark server did not become ready")
        initialized = rpc("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                         "clientInfo": {"name": "result-measurement", "version": "1"}})
        session = initialized[3]["Mcp-Session-Id"]
        rpc("notifications/initialized", notification=True)
        retained = rpc("tools/call", {"name": "api.read", "arguments": {
            "operation_id": "downloadActionsRunJobLogs", "arguments": {"owner": "fixture", "repo": "fixture", "job_id": 1}}})
        uri = retained[0]["structuredContent"]["payload"]["resource_uri"]
        before = memory(process.pid)

        def read(_):
            if args.mode == "selection":
                result, size, latency, _ = rpc("tools/call", {"name": "result.select", "arguments": {
                    "uri": uri, "mode": "search", "text": "FAILURE:"}})
                found = marker.decode().strip() in result["structuredContent"]["selection"]["data"]
                assert size <= 8192
            else:
                result, size, latency, _ = rpc("resources/read", {"uri": uri})
                text = result["contents"][0]["text"]
                found = text.encode() == payload
            assert found
            return {"rpc_json_bytes": size, "latency_seconds": latency}

        with concurrent.futures.ThreadPoolExecutor(max_workers=args.readers) as pool:
            measurements = list(pool.map(read, range(args.readers)))
        after = memory(process.pid)
        report = {"mode": args.mode, "scope": "loopback fake; fresh process; debug build unless caller supplies otherwise",
                  "binary_sha256": file_hash(binary), "source_sha256": source,
                  "harness_sha256": file_hash(Path(__file__)), "fixture_bytes": len(payload),
                  "concurrent_readers": args.readers, "initial_rpc_json_bytes": retained[1],
                  "before_access": before, "after_access": after,
                  "rpc_json_bytes_per_reader": [row["rpc_json_bytes"] for row in measurements],
                  "median_latency_seconds": statistics.median(row["latency_seconds"] for row in measurements),
                  "maximum_latency_seconds": max(row["latency_seconds"] for row in measurements)}
        if args.mode == "selection":
            grant = rpc("files/authorizeDownload", {"uri": uri})[0]
            digest = hashlib.sha256()
            count = 0
            request = urllib.request.Request(grant["download"]["url"], headers=grant["download"]["headers"])
            with urllib.request.urlopen(request, timeout=30) as response:
                while chunk := response.read(64 * 1024):
                    count += len(chunk)
                    digest.update(chunk)
            assert count == len(payload) and digest.digest() == hashlib.sha256(payload).digest()
            report["download_bytes"] = count
            report["download_integrity"] = "passed"
        assert source_hash() == source, "source changed during measurement"
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
