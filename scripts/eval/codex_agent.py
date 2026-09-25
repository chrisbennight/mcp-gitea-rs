"""Token-accounted Codex runner for disposable MCP surface evaluations.

Uses the installed client's saved authentication without reading or copying it.
Only aggregate event metadata is retained; tool arguments and results stay in
memory. User configuration, project instructions, shell, web, apps, and agent
spawning are disabled for the measured run.
"""

import collections
import json
import os
from pathlib import Path
import selectors
import subprocess
import tempfile
import time


def metrics(events, model):
    calls = {}
    usage = {}
    complete = False
    forbidden = False
    failed = False
    for event in events:
        kind = event.get("type")
        if kind in ("turn.failed", "error"):
            failed = True
        if kind == "turn.completed":
            usage = event.get("usage", {})
            complete = bool(usage) and "input_tokens" in usage and "output_tokens" in usage
        item = event.get("item", {})
        if item.get("type") in ("command_execution", "file_change", "web_search"):
            forbidden = True
        if item.get("type") == "mcp_tool_call":
            identifier = item.get("id")
            if identifier is None:
                failed = True
                continue
            result = item.get("result") or {}
            calls[identifier] = {
                "tool": item.get("tool", "unknown"),
                "server": item.get("server"),
                "error": item.get("status") == "failed" or bool(item.get("error")) or bool(result.get("isError")),
            }
            if item.get("server") != "gitea":
                forbidden = True
    report = {"model": model, "usage": usage, "cost_usd": None,
              "cost_basis": "ChatGPT-authenticated CLI does not report billed dollar cost",
              "tool_calls": len(calls), "tool_errors": sum(call["error"] for call in calls.values()),
              "tool_names": dict(collections.Counter(call["tool"] for call in calls.values())),
              "result_seen": complete, "forbidden_tool_used": forbidden}
    if failed or forbidden or not complete:
        report["agent_error"] = ("out-of-scope tool used" if forbidden else
                                 "client error or incomplete token accounting")
    return report


def run(prompt, mcp_config, transcript_path, model, timeout, max_calls):
    if not model:
        raise ValueError("Codex comparisons require an explicit model")
    connection = json.loads(Path(mcp_config).read_text())["mcpServers"]["gitea"]
    bearer = connection["headers"]["Authorization"].removeprefix("Bearer ")
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith(("GITEA_EVAL_", "GITEA_MCP_"))}
    environment["GITEA_SURFACE_EVAL_BEARER"] = bearer
    environment["RUST_LOG"] = "off"
    events = []
    stop_reason = None
    with tempfile.TemporaryDirectory(prefix="gitea-codex-eval-") as workdir:
        command = ["codex", "exec", "--ignore-user-config", "--ephemeral",
                   "--skip-git-repo-check", "--sandbox", "read-only", "--json", "-m", model]
        settings = {
            "features.shell_tool": "false", "features.apps": "false",
            "features.remote_plugin": "false", "features.multi_agent": "false",
            "web_search": '"disabled"', "project_doc_max_bytes": "0",
            "mcp_servers.gitea.url": json.dumps(connection["url"]),
            "mcp_servers.gitea.bearer_token_env_var": '"GITEA_SURFACE_EVAL_BEARER"',
            "mcp_servers.gitea.required": "true",
            "mcp_servers.gitea.default_tools_approval_mode": '"approve"',
            "model_reasoning_effort": '"medium"',
        }
        for key, value in settings.items():
            command.extend(["-c", f"{key}={value}"])
        command.append("Use only the gitea MCP tools to complete this disposable test task. "
                       "Do not use shell, files, web, other services, or other agents.\n\n" + prompt)
        process = subprocess.Popen(command, cwd=workdir, env=environment,
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + timeout
            pending = b""
            calls = set()
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                while selector.get_map():
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        stop_reason = f"timed out after {timeout}s"
                        break
                    for key, _ in selector.select(min(remaining, 1)):
                        chunk = os.read(key.fd, 65536)
                        if not chunk:
                            selector.unregister(key.fileobj)
                            continue
                        pending += chunk
                        while b"\n" in pending:
                            line, pending = pending.split(b"\n", 1)
                            try:
                                event = json.loads(line)
                            except (ValueError, UnicodeError):
                                continue
                            if not isinstance(event, dict):
                                continue
                            events.append(event)
                            item = event.get("item", {})
                            if item.get("type") == "mcp_tool_call":
                                calls.add(item.get("id"))
                            if len(calls) > max_calls:
                                stop_reason = f"exceeded {max_calls} tool calls"
                                break
                        if stop_reason:
                            break
                    if stop_reason:
                        break
            if stop_reason:
                process.terminate()
            process.wait(timeout=10)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=10)
            process.stdout.close()
    report = metrics(events, model)
    if stop_reason:
        report["agent_error"] = stop_reason
    elif process.returncode:
        report["agent_error"] = f"codex exited {process.returncode}"
    report.pop("result_seen", None)
    transcript_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    return report
