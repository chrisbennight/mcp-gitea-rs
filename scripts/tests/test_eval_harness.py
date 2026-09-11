import importlib.util
import json
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "eval_harness", ROOT / "scripts" / "eval" / "harness.py"
)
harness = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
# Registered before execution: the module's dataclasses resolve their string
# annotations through sys.modules at class-creation time.
sys.modules["eval_harness"] = harness
SPEC.loader.exec_module(harness)


def _event(payload):
    return json.dumps(payload)


class TranscriptMetricsTests(unittest.TestCase):
    def test_counts_tool_calls_errors_and_final_usage(self):
        lines = [
            _event(
                {
                    "type": "assistant",
                    "message": {
                        "model": "claude-test",
                        "content": [
                            {"type": "text", "text": "let me look"},
                            {"type": "tool_use", "name": "mcp__gitea__repository.get"},
                            {"type": "tool_use", "name": "mcp__gitea__repository.get"},
                        ],
                    },
                }
            ),
            _event(
                {
                    "type": "user",
                    "message": {
                        "content": [
                            {"type": "tool_result", "is_error": True},
                            {"type": "tool_result"},
                        ]
                    },
                }
            ),
            _event(
                {
                    "type": "result",
                    "num_turns": 3,
                    "total_cost_usd": 0.05,
                    "duration_ms": 1200,
                    "usage": {"input_tokens": 10, "output_tokens": 20},
                }
            ),
        ]
        metrics = harness.transcript_metrics(lines)
        self.assertEqual(metrics["tool_calls"], 2)
        self.assertEqual(metrics["tool_errors"], 1)
        self.assertEqual(metrics["tool_names"], {"mcp__gitea__repository.get": 2})
        self.assertEqual(metrics["num_turns"], 3)
        self.assertEqual(metrics["usage"], {"input_tokens": 10, "output_tokens": 20})
        self.assertEqual(metrics["model"], "claude-test")

    def test_partial_or_malformed_transcript_still_reports(self):
        lines = ["not json", "", _event({"type": "system"}), _event(["list"])]
        metrics = harness.transcript_metrics(lines)
        self.assertEqual(metrics["tool_calls"], 0)
        self.assertEqual(metrics["tool_errors"], 0)
        self.assertIsNone(metrics["num_turns"])
        # A stream that never carried its result event is visibly incomplete,
        # so the runner can fail the task instead of passing zeroed metrics.
        self.assertFalse(metrics["result_seen"])

    def test_a_complete_transcript_marks_its_result_event(self):
        metrics = harness.transcript_metrics(
            [
                _event(
                    {
                        "type": "result",
                        "num_turns": 2,
                        "total_cost_usd": 0.01,
                        "usage": {"input_tokens": 1},
                    }
                )
            ]
        )
        self.assertTrue(metrics["result_seen"])

    def test_a_result_event_without_cost_is_incomplete(self):
        metrics = harness.transcript_metrics(
            [_event({"type": "result", "num_turns": 2, "usage": {"input_tokens": 1}})]
        )
        self.assertFalse(metrics["result_seen"])

    def test_an_interrupted_stream_still_accounts_its_spent_tokens(self):
        lines = [
            _event(
                {
                    "type": "assistant",
                    "message": {"model": "m", "usage": {"input_tokens": 7}, "content": []},
                }
            ),
            _event(
                {
                    "type": "assistant",
                    "message": {"model": "m", "usage": {"input_tokens": 5}, "content": []},
                }
            ),
        ]
        metrics = harness.transcript_metrics(lines)
        self.assertFalse(metrics["result_seen"])
        self.assertEqual(metrics["usage"], {"input_tokens": 12})

    def test_a_hollow_result_event_is_not_complete_accounting(self):
        metrics = harness.transcript_metrics([_event({"type": "result", "usage": {}})])
        self.assertFalse(metrics["result_seen"])


class RedactionTests(unittest.TestCase):
    def test_token_shaped_values_are_masked(self):
        secret = "a" * 39 + "b"
        line = json.dumps({"token": secret, "sha": "0" * 40, "name": "eval"})
        redacted = harness.redact(line)
        self.assertNotIn(secret, redacted)
        self.assertNotIn("0" * 40, redacted)
        self.assertIn("eval", redacted)

    def test_shorter_hex_survives(self):
        self.assertEqual(harness.redact("deadbeef"), "deadbeef")


class AggregateReportTests(unittest.TestCase):
    def test_skips_are_counted_apart_and_totals_fold(self):
        results = [
            {
                "task": "a",
                "outcome": "passed",
                "success": True,
                "tool_calls": 3,
                "tool_errors": 1,
                "usage": {"input_tokens": 10, "output_tokens": 5},
                "cost_usd": 0.01,
            },
            {
                "task": "b",
                "outcome": "skipped",
                "success": False,
                "detail": "precondition unmet",
            },
            {
                "task": "c",
                "outcome": "failed",
                "success": False,
                "tool_calls": 2,
                "tool_errors": 0,
                "usage": {"input_tokens": 4},
                "cost_usd": 0.02,
            },
        ]
        interrupted = results + [
            {
                "task": "d",
                "outcome": "failed",
                "success": False,
                "cost_usd": None,
                "usage": {"input_tokens": 3},
            },
            # A seed crash spends nothing; it must not count as unpriced.
            {"task": "e", "outcome": "failed", "success": False, "detail": "seed crashed"},
        ]
        self.assertEqual(
            harness.aggregate_report("flat", interrupted)["cost_unaccounted_tasks"], 1
        )
        report = harness.aggregate_report("flat", results)
        self.assertEqual(report["cost_unaccounted_tasks"], 0)
        self.assertEqual(report["tasks_total"], 3)
        self.assertEqual(report["tasks_passed"], 1)
        self.assertEqual(report["tasks_skipped"], 1)
        self.assertEqual(report["tool_calls_total"], 5)
        self.assertEqual(report["tool_errors_total"], 1)
        self.assertEqual(report["usage_total"], {"input_tokens": 14, "output_tokens": 5})
        self.assertEqual(report["cost_usd_total"], 0.03)


class TaskTableTests(unittest.TestCase):
    def test_task_ids_are_unique_and_fully_formed(self):
        tasks = harness.tasks()
        identifiers = [task.id for task in tasks]
        self.assertEqual(len(identifiers), len(set(identifiers)))
        for task in tasks:
            self.assertTrue(task.prompt.strip(), task.id)
            self.assertTrue(task.workload.strip(), task.id)
            self.assertTrue(callable(task.seed), task.id)
            self.assertTrue(callable(task.verify), task.id)

    def test_tasks_cover_the_planned_workload_shapes(self):
        # The task set is drawn from PLAN.md's agent workloads; a shape
        # silently dropping out would hollow the baseline without failing it.
        covered = {task.workload for task in harness.tasks()}
        for shape in [
            "repository lifecycle",
            "change review",
            "CI operation",
            "publication",
            "access and identity administration",
            "fleet queries",
            "triage",
            "content read",
        ]:
            self.assertIn(shape, covered)


if __name__ == "__main__":
    unittest.main()
