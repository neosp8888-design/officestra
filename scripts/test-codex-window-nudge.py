#!/usr/bin/env python3
import datetime as dt
import importlib.util
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("codex_nudge", Path(__file__).with_name("codex-window-nudge.py"))
nudge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nudge)


class CodexWindowNudgeTests(unittest.TestCase):
    def setUp(self):
        self.now = dt.datetime(2026, 10, 6, 15, 0, tzinfo=dt.timezone.utc)
        self.snapshot = {"codexLimitStale": False, "codexLimitError": None,
                         "codexFiveHourResetAt": "2026-10-06T14:59:00Z",
                         "codexFiveHour": 100, "codexWeekly": 80}
        self.saved = []

    def tick(self, state=None, probe=None):
        return nudge.tick(self.now, state or {}, lambda: self.snapshot,
                          probe or (lambda: self.fail("unexpected model request")), self.saved.append)

    def test_initial_schedule_uses_observed_reset(self):
        self.snapshot["codexFiveHourResetAt"] = "2026-10-06T16:42:04Z"
        state, _ = self.tick()
        self.assertEqual(state["dueAt"], "2026-10-06T16:43:04Z")

    def test_not_due_does_not_poll_or_send(self):
        state = {"dueAt": "2026-10-06T16:00:00Z"}
        actual, outcome = nudge.tick(self.now, state, lambda: self.fail("poll"))
        self.assertEqual(actual, state)
        self.assertEqual(outcome, "not due")

    def test_regular_task_advances_schedule_without_probe(self):
        self.snapshot["codexFiveHourResetAt"] = "2026-10-06T19:50:00Z"
        state, _ = self.tick({"dueAt": "2026-10-06T15:00:00Z"})
        self.assertEqual(state["dueAt"], "2026-10-06T19:51:00Z")

    def test_idle_rolling_reset_starts_window_and_verifies_when_reset_stops_rolling(self):
        # No open window: the provider reports now + 5h, which used to postpone forever.
        self.snapshot["codexFiveHourResetAt"] = "2026-10-06T20:00:20Z"
        state, outcome = self.tick({"dueAt": "2026-10-06T15:00:00Z"}, probe=lambda: (True, {}))
        self.assertEqual(outcome, "sent one isolated short turn")
        self.now += dt.timedelta(minutes=3)
        self.snapshot["codexFiveHourResetAt"] = "2026-10-06T20:00:21Z"
        result, outcome = self.tick(state)
        self.assertEqual(outcome, "reset advance verified")
        self.assertEqual(result["dueAt"], "2026-10-06T20:01:21Z")

    def test_idle_reset_that_keeps_rolling_is_never_verified_and_disables(self):
        self.snapshot["codexFiveHourResetAt"] = "2026-10-06T20:00:00Z"
        state, _ = self.tick(probe=lambda: (True, {}))
        for minutes in (3, 30, 44):
            self.now = dt.datetime(2026, 10, 6, 15, minutes, tzinfo=dt.timezone.utc)
            self.snapshot["codexFiveHourResetAt"] = nudge.iso(self.now + nudge.WINDOW)
            self.assertEqual(self.tick(state)[1], "waiting for reset verification")
        self.now = dt.datetime(2026, 10, 6, 15, 45, tzinfo=dt.timezone.utc)
        self.snapshot["codexFiveHourResetAt"] = nudge.iso(self.now + nudge.WINDOW)
        self.assertTrue(self.tick(state)[0]["disabled"])

    def test_persists_before_one_probe_and_never_resends(self):
        def probe():
            self.assertEqual(len(self.saved), 1)
            self.assertEqual(self.saved[0]["pendingAt"], "2026-10-06T15:00:00Z")
            return True, {"model": "gpt-6-luna", "reply": "OK"}
        state, outcome = self.tick(probe=probe)
        self.assertEqual(outcome, "sent one isolated short turn")
        result, _ = self.tick(state)
        self.assertEqual(result, state)

    def test_crash_pending_record_verifies_instead_of_resending(self):
        state, _ = self.tick(probe=lambda: (True, {}))
        self.snapshot["codexFiveHourResetAt"] = "2026-10-06T20:00:04Z"
        result, outcome = self.tick(self.saved[0])
        self.assertEqual(outcome, "reset advance verified")
        self.assertIsNone(result["pendingAt"])
        self.assertEqual(result["dueAt"], "2026-10-06T20:01:04Z")

    def test_timeout_does_not_retry(self):
        def timeout():
            raise subprocess.TimeoutExpired("codex", 90)
        state, _ = self.tick(probe=timeout)
        self.assertIsNotNone(state["pendingAt"])
        result, _ = self.tick(state)
        self.assertEqual(result, state)

    def test_unverified_probe_disables_after_45_minutes(self):
        state, _ = self.tick(probe=lambda: (False, {}))
        self.now += dt.timedelta(minutes=45)
        result, _ = self.tick(state)
        self.assertTrue(result["disabled"])
        self.assertEqual(self.tick(result)[0], result)

    def test_stale_or_missing_reset_never_sends(self):
        for field, value in [("codexLimitStale", True), ("codexLimitStale", None),
                             ("codexLimitError", "unavailable"), ("codexFiveHourResetAt", None)]:
            with self.subTest(field=field, value=value):
                with patch.dict(self.snapshot, {field: value}):
                    self.assertEqual(self.tick()[0], {})

    def test_weekly_exhaustion_waits_until_weekly_reset(self):
        self.snapshot.update(codexWeekly=0, codexWeeklyResetAt="2026-10-13T15:00:00Z")
        state, outcome = self.tick()
        self.assertEqual(state["dueAt"], "2026-10-13T15:01:00Z")
        self.assertIn("exhausted", outcome)

    def test_zero_allowance_defers(self):
        self.snapshot["codexFiveHour"] = 0
        state, _ = self.tick()
        self.assertEqual(state["dueAt"], "2026-10-06T15:02:00Z")

    def test_probe_isolated_low_standard_and_reports_actual_usage(self):
        events = [{"type": "item.completed", "item": {"type": "agent_message", "text": "OK"}},
                  {"type": "turn.completed", "usage": {"input_tokens": 20, "output_tokens": 1}}]
        result = subprocess.CompletedProcess([], 0, "\n".join(map(json.dumps, events)), "")
        with patch.object(nudge.subprocess, "run", return_value=result) as run:
            ok, detail = nudge.run_probe()
        self.assertTrue(ok)
        self.assertEqual(detail["usage"], {"input_tokens": 20, "output_tokens": 1})
        args = run.call_args.args[0]
        for flag in ("--ephemeral", "--ignore-user-config", "--ignore-rules", "--json"):
            self.assertIn(flag, args)
        self.assertIn("gpt-6-luna", args)
        self.assertIn('model_reasoning_effort="low"', args)
        self.assertIn('service_tier="default"', args)
        self.assertIn("project_doc_max_bytes=0", args)
        self.assertTrue(any(value.startswith("model_instructions_file=") for value in args))
        self.assertIn("skip_host_skill_discovery", args)
        self.assertIn("memories", args)
        self.assertIn("shell_tool", args)
        self.assertNotIn("--resume", args)
        self.assertNotIn("OPENAI_API_KEY", run.call_args.kwargs["env"])

    def test_tool_use_or_no_completed_turn_is_not_success(self):
        for extra in ([{"type": "item.started", "item": {"type": "command_execution"}}], []):
            events = [{"type": "item.completed", "item": {"type": "agent_message", "text": "OK"}}] + extra
            if extra:
                events.append({"type": "turn.completed", "usage": {}})
            result = subprocess.CompletedProcess([], 0, "\n".join(map(json.dumps, events)), "")
            with patch.object(nudge.subprocess, "run", return_value=result):
                self.assertFalse(nudge.run_probe()[0])


if __name__ == "__main__":
    unittest.main()
