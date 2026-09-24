#!/usr/bin/env python3
"""Focused tests for Claude window nudge scheduling and duplicate prevention."""

import datetime as dt
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("nudge", Path(__file__).with_name("claude-window-nudge.py"))
nudge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nudge)


class WindowNudgeTests(unittest.TestCase):
    def setUp(self):
        self.now = dt.datetime(2026, 9, 23, 15, 0, tzinfo=dt.timezone.utc)

    def test_initial_turn_is_scheduled_just_after_reset(self):
        calls = []
        def call(method, path, body=None):
            calls.append((method, path))
            return 200, {"claudeFiveHourResetAt": "2026-09-23T15:49:59Z"}
        state, outcome = nudge.tick(self.now, {}, call, lambda *_: self.fail("must not send"))
        self.assertEqual(state["dueAt"], "2026-09-23T15:50:59Z")
        self.assertEqual(outcome, "scheduled first turn")
        self.assertEqual(len(calls), 1)

    def test_probe_uses_fresh_session_without_adaptive_override(self):
        class ProcessResult:
            returncode = 0
            stdout = json.dumps({"is_error": False, "result": "OK"})
        with patch.object(nudge.subprocess, "run", return_value=ProcessResult()) as run:
            self.assertEqual(nudge.run_probe(), (True, "OK"))
        args = run.call_args.args[0]
        environment = run.call_args.kwargs["env"]
        self.assertIn("--no-session-persistence", args)
        self.assertIn("--restricted", args)
        self.assertNotIn("--resume", args)
        self.assertNotIn("--effort", args)
        self.assertNotIn("CLAUDE_CODE_EXTRA_BODY", environment)
        self.assertEqual(environment["DISABLE_AUTOUPDATER"], "1")

    def test_existing_turn_starts_next_window_without_auto_message(self):
        def call(method, path, body=None):
            if path.startswith("/api/agent-jobs?"):
                return 200, {"employees": [{"canReceive": True}]}
            if path.startswith("/api/live-feed?"):
                return 200, {"turns": [{"characterId": "left-woman", "status": "completed",
                                        "startedAt": "2026-09-23T15:00:05Z"}]}
            self.fail("must not send")
        state, outcome = nudge.tick(self.now, {"dueAt": "2026-09-23T15:00:00Z"}, call,
                                    lambda *_: self.fail("must not send"))
        self.assertEqual(outcome, "already started by another turn")
        self.assertEqual(state["dueAt"], "2026-09-23T20:01:05Z")

    def test_sends_once_and_waits_five_hours(self):
        calls = []
        probes = []
        def call(method, path, body=None):
            calls.append((method, path, body))
            if path.startswith("/api/agent-jobs?"):
                return 200, {"employees": [{"canReceive": True}]}
            if path.startswith("/api/live-feed?"):
                return 200, {"turns": []}
            self.fail("must not use employee message API")
        def probe(model):
            probes.append(model)
            return True, "OK"
        state, outcome = nudge.tick(self.now, {"dueAt": "2026-09-23T15:00:00Z"}, call, probe)
        self.assertEqual(outcome, "sent one isolated short turn")
        self.assertEqual(state["dueAt"], "2026-09-23T20:01:00Z")
        self.assertEqual(state["verifyAfter"], "2026-09-23T15:10:00Z")
        nudge.tick(self.now + dt.timedelta(minutes=1), state, call, probe)
        self.assertEqual(probes, ["haiku"])
        self.assertFalse(any(item[0] == "POST" for item in calls))

    def test_busy_employee_is_not_interrupted(self):
        def call(method, path, body=None):
            return 200, {"employees": [{"canReceive": False}]}
        state = {"dueAt": "2026-09-23T15:00:00Z"}
        result, outcome = nudge.tick(self.now, state, call,
                                     lambda *_: self.fail("must not send"))
        self.assertEqual(result, state)
        self.assertEqual(outcome, "recipient busy or unavailable")

    def test_failed_probe_is_delayed(self):
        def call(method, path, body=None):
            if path.startswith("/api/agent-jobs?"):
                return 200, {"employees": [{"canReceive": True}]}
            return 200, {"turns": []}
        result, outcome = nudge.tick(self.now, {"dueAt": "2026-09-23T15:00:00Z"},
                                     call, lambda _: (False, "network timeout"),
                                     lambda *_: ("wait", "temporary error"))
        self.assertIn("recovery chose wait", outcome)
        self.assertEqual(result["dueAt"], "2026-09-23T15:15:00Z")
        self.assertEqual(result["failedAttempts"], 1)

    def test_three_failed_probes_disable_future_messages(self):
        def call(method, path, body=None):
            if path.startswith("/api/agent-jobs?"):
                return 200, {"employees": [{"canReceive": True}]}
            return 200, {"turns": []}
        state = {"dueAt": "2026-09-23T15:00:00Z", "failedAttempts": 2}
        result, outcome = nudge.tick(self.now, state, call,
                                     lambda _: (False, "network timeout"),
                                     lambda *_: ("wait", "temporary error"))
        self.assertTrue(result["disabled"])
        self.assertEqual(outcome, "isolated probe failed three times; disabled")
        again, _ = nudge.tick(self.now, result, call, lambda *_: self.fail("must not send"))
        self.assertEqual(again, result)

    def test_verifies_reset_advanced_before_next_probe(self):
        state = {"dueAt": "2026-09-23T20:01:00Z",
                 "verifyAfter": "2026-09-23T15:10:00Z",
                 "verifyUntil": "2026-09-23T15:45:00Z",
                 "verifyPreviousResetAt": "2026-09-23T14:59:00Z"}
        def call(method, path, body=None):
            return 200, {"claudeLimitStale": False,
                         "claudeFiveHourResetAt": "2026-09-23T20:00:00Z"}
        result, outcome = nudge.tick(self.now + dt.timedelta(minutes=11), state,
                                     call, lambda *_: self.fail("must not send"))
        self.assertEqual(outcome, "reset advance verified")
        self.assertEqual(result["dueAt"], "2026-09-23T20:01:00Z")
        self.assertNotIn("verifyAfter", result)

    def test_stale_but_advanced_reset_is_valid_evidence(self):
        state = {"dueAt": "2026-09-23T20:02:00Z",
                 "verifyAfter": "2026-09-23T15:10:00Z",
                 "verifyUntil": "2026-09-23T15:45:00Z",
                 "verifyPreviousResetAt": "2026-09-23T14:59:00Z"}
        def call(method, path, body=None):
            return 200, {"claudeLimitStale": True,
                         "claudeFiveHourResetAt": "2026-09-23T20:00:00Z"}
        result, outcome = nudge.tick(self.now + dt.timedelta(minutes=11), state,
                                     call, lambda *_: self.fail("must not send"))
        self.assertEqual(outcome, "reset advance verified")
        self.assertEqual(result, {"dueAt": "2026-09-23T20:01:00Z"})

    def test_disables_when_fresh_limit_did_not_advance(self):
        state = {"dueAt": "2026-09-23T20:01:00Z",
                 "verifyAfter": "2026-09-23T15:10:00Z",
                 "verifyUntil": "2026-09-23T15:45:00Z",
                 "verifyPreviousResetAt": "2026-09-23T14:59:00Z"}
        def call(method, path, body=None):
            return 200, {"claudeLimitStale": False,
                         "claudeFiveHourResetAt": "2026-09-23T14:59:00Z"}
        result, outcome = nudge.tick(self.now + dt.timedelta(minutes=11), state,
                                     call, lambda *_: self.fail("must not send"),
                                     lambda *_: ("stop", "no safe retry"))
        self.assertTrue(result["disabled"])
        self.assertEqual(outcome, "reset did not advance: no safe retry")

    def test_unchanged_reset_can_try_sonnet_once(self):
        state = {"dueAt": "2026-09-23T20:01:00Z",
                 "verifyAfter": "2026-09-23T15:10:00Z",
                 "verifyUntil": "2026-09-23T15:45:00Z",
                 "verifyPreviousResetAt": "2026-09-23T14:59:00Z"}
        models = []
        def call(method, path, body=None):
            return 200, {"claudeLimitStale": False,
                         "claudeFiveHourResetAt": "2026-09-23T14:59:00Z"}
        def probe(model):
            models.append(model)
            return True, "OK"
        result, outcome = nudge.tick(self.now + dt.timedelta(minutes=11), state,
                                     call, probe,
                                     lambda *_: ("retry_sonnet", "Haiku did not advance reset"))
        self.assertEqual(models, ["sonnet"])
        self.assertTrue(result["verificationRetried"])
        self.assertIn("sonnet recovery probe sent", outcome)
        result, outcome = nudge.tick(self.now + dt.timedelta(minutes=22), result,
                                     call, lambda *_: self.fail("must not send"),
                                     lambda *_: self.fail("must not decide twice"))
        self.assertTrue(result["disabled"])
        self.assertEqual(outcome, "reset still unchanged after recovery; disabled")

    def test_codex_decision_can_retry_sonnet_once_after_haiku_error(self):
        models = []
        def call(method, path, body=None):
            if path.startswith("/api/agent-jobs?"):
                return 200, {"employees": [{"canReceive": True}]}
            return 200, {"turns": []}
        def probe(model):
            models.append(model)
            return (False, "Haiku unavailable") if model == "haiku" else (True, "OK")
        state, outcome = nudge.tick(self.now, {"dueAt": "2026-09-23T15:00:00Z"},
                                    call, probe, lambda *_: ("retry_sonnet", "model unavailable"))
        self.assertEqual(models, ["haiku", "sonnet"])
        self.assertEqual(outcome, "sent one isolated short turn")
        self.assertIn("verifyAfter", state)

    def test_codex_recovery_is_read_only_and_structured(self):
        class ProcessResult:
            returncode = 0
            stdout = json.dumps({"action": "wait", "reason": "quota exhausted"})
        with patch.object(nudge.subprocess, "run", return_value=ProcessResult()) as run:
            self.assertEqual(nudge.decide_recovery("probe_failed", "session limit"),
                             ("wait", "quota exhausted"))
        args = run.call_args.args[0]
        self.assertIn("--sandbox", args)
        self.assertEqual(args[args.index("--sandbox") + 1], "read-only")
        self.assertIn("--output-schema", args)

    def test_usage_line_reports_model_cost_and_tokens_from_cli_json(self):
        payload = {
            "total_cost_usd": 0.00123,
            "usage": {"input_tokens": 10, "output_tokens": 4,
                      "cache_read_input_tokens": 0, "cache_creation_input_tokens": 850},
            "modelUsage": {"claude-haiku-4-5-20251001": {
                "inputTokens": 10, "outputTokens": 4, "cacheReadInputTokens": 0,
                "cacheCreationInputTokens": 850, "costUSD": 0.00123}},
        }
        line = nudge.usage_line(payload)
        self.assertIn("models=claude-haiku-4-5-20251001(in=10,out=4,cache_read=0,cache_creation=850,cost_usd=0.00123)", line)
        self.assertIn("cost_usd=0.00123 input=10 output=4 cache_read=0 cache_creation=850", line)

    def test_usage_line_never_invents_missing_cost(self):
        line = nudge.usage_line({"is_error": True, "result": "limit reached"})
        self.assertEqual(line, "claude usage models=unreported cost_usd=unreported input=unreported "
                               "output=unreported cache_read=unreported cache_creation=unreported")


if __name__ == "__main__":
    unittest.main()
