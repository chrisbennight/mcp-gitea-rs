import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("codex_agent", Path(__file__).resolve().parents[1] / "eval/codex_agent.py")
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


class CodexMetricsTests(unittest.TestCase):
    def test_started_and_completed_call_count_once_and_cost_is_unknown(self):
        events = [{"type":"item.started", "item":{"type":"mcp_tool_call", "id":"a", "server":"gitea", "tool":"api.read"}},
                  {"type":"item.completed", "item":{"type":"mcp_tool_call", "id":"a", "server":"gitea", "tool":"api.read", "result":{"isError":True}}},
                  {"type":"turn.completed", "usage":{"input_tokens":100, "cached_input_tokens":80, "output_tokens":10}}]
        result = runner.metrics(events, "fixed-model")
        self.assertEqual(result["tool_calls"], 1)
        self.assertEqual(result["tool_errors"], 1)
        self.assertEqual(result["usage"], events[-1]["usage"])
        self.assertIsNone(result["cost_usd"])
        self.assertNotIn("agent_error", result)

    def test_partial_accounting_and_out_of_scope_tools_fail(self):
        for event in [{"type":"turn.failed"},
                      {"type":"item.completed", "item":{"type":"command_execution"}},
                      {"type":"item.completed", "item":{"type":"mcp_tool_call", "id":"a", "server":"other", "tool":"mutate"}}]:
            self.assertIn("agent_error", runner.metrics([event], "fixed-model"))

    def test_metrics_do_not_retain_arguments_or_tool_payloads(self):
        marker = "synthetic-credential-marker"
        result = runner.metrics([{"type":"item.completed", "item":{"type":"mcp_tool_call", "id":"a", "server":"gitea", "tool":"api.read", "arguments":{"secret":marker}, "result":{"payload":marker}}}], "fixed-model")
        self.assertNotIn(marker, str(result))
