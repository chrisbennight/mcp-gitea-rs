// Run from a directory where the official SDK is installed (see README.md).
import assert from 'node:assert/strict';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';

const transport = () => new StreamableHTTPClientTransport(new URL(process.env.MCP_TEST_URL), {
  requestInit: { headers: { Authorization: `Bearer ${process.env.MCP_TEST_BEARER}` } },
});
const client = new Client({ name: 'gitea-compatibility', version: '1.0.0' });
try {
  await client.connect(transport());
  const tools = await client.listTools();
  assert(tools.tools.some(tool => tool.name === 'api.read'));
  for (const tool of tools.tools) {
    assert.equal(tool.inputSchema.type, 'object');
    for (const keyword of ['oneOf', 'anyOf', 'allOf']) assert(!(keyword in tool.inputSchema));
  }
  const version = await client.callTool({ name: 'server.version', arguments: {} });
  assert.equal(version.structuredContent.version, '1.26.4');
  const catalog = await client.readResource({ uri: 'gitea-catalog:/index' });
  assert(catalog.contents.length);
  const logs = await client.callTool({ name: 'api.read', arguments: {
    operation_id: 'downloadActionsRunJobLogs', arguments: { owner: 'fixture', repo: 'fixture', job_id: 1 },
  }});
  assert.equal(logs.structuredContent.payload.retained, true);
  const uri = logs.structuredContent.payload.resource_uri;
  assert.equal((await client.readResource({ uri })).contents[0].text, "fixture log line\n".repeat(8192));
  const other = new Client({ name: 'other-session', version: '1.0.0' });
  try {
    await other.connect(transport());
    await assert.rejects(other.readResource({ uri }), /no stored payload/);
  } finally {
    await other.close();
  }
  console.log('TypeScript SDK: initialize, tool schemas, upstream read, catalog, retained resources, session isolation PASS');
} finally {
  await client.close();
}
