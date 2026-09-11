"""Exercise the disposable Docker fixture using the official Python MCP SDK."""
import asyncio
import os

import httpx2
from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client
from mcp.shared.exceptions import MCPError


async def main():
    session_ids = []

    async def remember_session(response):
        session_id = response.headers.get("mcp-session-id")
        if session_id and session_id not in session_ids:
            session_ids.append(session_id)

    async with httpx2.AsyncClient(
        headers={"Authorization": "Bearer " + os.environ["MCP_TEST_BEARER"]},
        timeout=10,
        event_hooks={"response": [remember_session]},
    ) as http:
        async with streamable_http_client(os.environ["MCP_TEST_URL"], http_client=http) as streams:
            async with ClientSession(*streams, read_timeout_seconds=10) as session:
                initialized = await session.initialize()
                tools = await session.list_tools()
                names = {tool.name for tool in tools.tools}
                assert {"api.read", "catalog.search", "access_token.create"} <= names
                for tool in tools.tools:
                    schema = tool.input_schema
                    assert schema["type"] == "object"
                    assert not ({"oneOf", "anyOf", "allOf"} & schema.keys())
                version = await session.call_tool("server.version", {})
                assert version.structured_content["version"] == "1.26.4"
                found = await session.call_tool("catalog.search", {"query": "repository"})
                assert not found.is_error
                resources = await session.list_resources()
                assert any(str(r.uri) == "gitea-catalog:/index" for r in resources.resources)
                catalog = await session.read_resource("gitea-catalog:/index")
                assert catalog.contents
                logs = await session.call_tool("api.read", {
                    "operation_id": "downloadActionsRunJobLogs",
                    "arguments": {"owner": "fixture", "repo": "fixture", "job_id": 1},
                })
                payload = logs.structured_content["payload"]
                assert payload["retained"] is True
                uri = payload["resource_uri"]
                retained = await session.read_resource(uri)
                assert retained.contents[0].text == "fixture log line\n" * 8192
                async with streamable_http_client(os.environ["MCP_TEST_URL"], http_client=http) as other_streams:
                    async with ClientSession(*other_streams, read_timeout_seconds=10) as other:
                        await other.initialize()
                        try:
                            await other.read_resource(uri)
                        except MCPError as error:
                            assert "no stored payload" in error.message
                        else:
                            raise AssertionError("another session read the retained result")
                print("Python SDK: initialize, tool schemas, discovery, upstream read, retained resources, session isolation PASS; protocol=" + initialized.protocol_version)
        assert session_ids
        ended = await http.post(os.environ["MCP_TEST_URL"],
            headers={"Mcp-Session-Id": session_ids[0],
                     "Mcp-Protocol-Version": initialized.protocol_version,
                     "Accept": "application/json, text/event-stream"},
            json={"jsonrpc": "2.0", "id": 99, "method": "ping"})
        assert ended.status_code == 404
        print("Terminated session rejected with HTTP 404 PASS")


if __name__ == "__main__":
    asyncio.run(main())
