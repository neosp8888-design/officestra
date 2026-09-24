#!/usr/bin/env python3
"""Start Claude's next five-hour window with one isolated short CLI turn.

Run periodically from launchd. The saved due time prevents duplicate turns.
"""

import datetime as dt
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib import error, request


BASE_URL = "http://127.0.0.1:4317"
CHARACTER_ID = "left-woman"
PROMPT = "Reply OK."
CLAUDE = "/Users/neo/.nvm/versions/node/v24.14.0/bin/claude"
CODEX = "/Users/neo/.nvm/versions/node/v24.14.0/bin/codex"
RECOVERY_SCHEMA = Path(__file__).with_name("claude-window-recovery.schema.json")
INTERVAL = dt.timedelta(hours=5, minutes=1)
RESET_MARGIN = dt.timedelta(minutes=1)
STATE_PATH = Path.home() / "Library/Application Support/OFFICESTRA/claude-window-nudge.json"


def parse_time(value):
    return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))


def iso(value):
    return value.astimezone(dt.timezone.utc).isoformat().replace("+00:00", "Z")


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Content-Type": "application/json"} if data is not None else {}
    req = request.Request(BASE_URL + path, data=data, headers=headers, method=method)
    try:
        with request.urlopen(req, timeout=10) as response:
            return response.status, json.load(response)
    except error.HTTPError as exc:
        return exc.code, json.load(exc)


def usage_line(payload):
    """Summarize the model, CLI-reported cost and tokens from `--output-format json`.

    Only values the CLI reported are written; missing ones stay "unreported"
    instead of being estimated from token counts.
    """
    def value(source, key):
        found = source.get(key) if isinstance(source, dict) else None
        return "unreported" if found is None else found

    parts = [f"cost_usd={value(payload, 'total_cost_usd')}"]
    usage = payload.get("usage") if isinstance(payload.get("usage"), dict) else {}
    parts += [f"{label}={value(usage, key)}" for label, key in (
        ("input", "input_tokens"), ("output", "output_tokens"),
        ("cache_read", "cache_read_input_tokens"),
        ("cache_creation", "cache_creation_input_tokens"))]
    models = payload.get("modelUsage") if isinstance(payload.get("modelUsage"), dict) else {}
    per_model = [
        f"{name}(in={value(item, 'inputTokens')},out={value(item, 'outputTokens')},"
        f"cache_read={value(item, 'cacheReadInputTokens')},"
        f"cache_creation={value(item, 'cacheCreationInputTokens')},cost_usd={value(item, 'costUSD')})"
        for name, item in models.items()]
    return "claude usage models=" + (",".join(per_model) or "unreported") + " " + " ".join(parts)


def run_probe(model="haiku"):
    # A fresh, disposable session avoids resending the employee's old transcript
    # after Claude's prompt cache expires during a long idle period.
    try:
        with tempfile.TemporaryDirectory(prefix="office-claude-window-") as workdir:
            environment = os.environ.copy()
            environment["DISABLE_AUTOUPDATER"] = "1"
            # A user-wide request override can force adaptive thinking onto
            # Haiku, which rejects it before consuming any tokens.
            environment.pop("CLAUDE_CODE_EXTRA_BODY", None)
            result = subprocess.run(
                [CLAUDE, "-p", PROMPT, "--model", model,
                 "--restricted", "--strict-mcp-config", "--tools", "",
                 "--no-session-persistence", "--system-prompt", "Reply OK.",
                 "--output-format", "json"],
                cwd=workdir, env=environment, capture_output=True,
                text=True, timeout=90,
            )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return False, type(exc).__name__
    try:
        payload = json.loads(result.stdout)
        # Keep what each isolated turn actually used; the CLI output is otherwise discarded.
        print(f"{iso(dt.datetime.now(dt.timezone.utc))} requested={model} {usage_line(payload)}", flush=True)
        detail = str(payload.get("result", ""))[:400]
        ok = result.returncode == 0 and payload.get("is_error", False) is False and "result" in payload
        return ok, detail if not ok else "OK"
    except json.JSONDecodeError:
        return False, (result.stderr or "invalid CLI JSON")[-400:]


def decide_recovery(stage, detail):
    """Ask a read-only Codex CLI run for one allowlisted recovery action."""
    prompt = (
        "Diagnose an isolated Claude Code window-start probe. "
        "You have no authority to edit files, send messages, or run tools. "
        "Choose exactly one action from retry_haiku, retry_sonnet, wait, stop. "
        "A retry uses a fresh disposable session with no tools, no resumed history, "
        "and no forced adaptive-thinking override. Choose wait for a usage-limit "
        "or temporary availability error; stop if unsafe or uncertain. "
        "Use retry_sonnet only if Haiku is incompatible or a successful Haiku "
        "probe did not advance the five-hour reset. "
        f"Stage: {stage}. Observed result: {detail[:400]}"
    )
    try:
        with tempfile.TemporaryDirectory(prefix="office-window-recovery-") as workdir:
            result = subprocess.run(
                [CODEX, "exec", "--ephemeral", "--ignore-user-config",
                 "--ignore-rules", "--sandbox", "read-only",
                 "--skip-git-repo-check", "-C", workdir, "-m", "gpt-6-luna",
                 "--output-schema", str(RECOVERY_SCHEMA), "-"],
                input=prompt, capture_output=True, text=True, timeout=120,
            )
        decision = json.loads(result.stdout) if result.returncode == 0 else {}
        action = decision.get("action")
        if action in {"retry_haiku", "retry_sonnet", "wait", "stop"}:
            return action, str(decision.get("reason", ""))[:160]
    except (OSError, ValueError, subprocess.TimeoutExpired):
        pass
    return "stop", "Codex CLI did not return a valid recovery decision"


def read_state():
    try:
        return json.loads(STATE_PATH.read_text())
    except FileNotFoundError:
        return {}


def save_state(state):
    STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
    temporary = STATE_PATH.with_suffix(".tmp")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w") as stream:
        json.dump(state, stream)
        stream.write("\n")
    os.replace(temporary, STATE_PATH)


def tick(now, state, call=api, probe=run_probe, recovery=decide_recovery):
    if state.get("disabled"):
        return state, "disabled after repeated probe failures"
    if state.get("verifyAfter") and now >= parse_time(state["verifyAfter"]):
        if now >= parse_time(state["verifyUntil"]):
            return {**state, "disabled": True}, "could not verify reset; disabled"
        status, summary = call("GET", "/api/usage-summary")
        if status != 200 or not summary.get("claudeFiveHourResetAt"):
            return state, "waiting for reset verification"
        observed = parse_time(summary["claudeFiveHourResetAt"])
        previous = parse_time(state["verifyPreviousResetAt"])
        # A reused last-good value still proves that a later reset was observed.
        # Staleness only prevents treating an unchanged value as a failure.
        if observed > previous + dt.timedelta(hours=1):
            return {"dueAt": iso(observed + RESET_MARGIN)}, "reset advance verified"
        if summary.get("claudeLimitStale"):
            return state, "waiting for fresh reset verification"
        if observed <= previous + dt.timedelta(hours=1):
            if state.get("verificationRetried"):
                return {**state, "disabled": True}, "reset still unchanged after recovery; disabled"
            action, reason = recovery("reset_unchanged", "Successful isolated probe; five-hour reset remained at " + iso(observed))
            if action in {"retry_haiku", "retry_sonnet"}:
                model = "sonnet" if action == "retry_sonnet" else "haiku"
                ok, detail = probe(model)
                if ok:
                    return {"dueAt": iso(now + INTERVAL),
                            "verifyAfter": iso(now + dt.timedelta(minutes=10)),
                            "verifyUntil": iso(now + dt.timedelta(minutes=45)),
                            "verifyPreviousResetAt": iso(previous),
                            "verificationRetried": True}, f"{model} recovery probe sent: {reason}"
                return {**state, "disabled": True}, f"recovery probe failed: {detail}"
            if action == "wait":
                return {**state, "verifyAfter": iso(now + dt.timedelta(minutes=5))}, f"verification deferred: {reason}"
            return {**state, "disabled": True}, f"reset did not advance: {reason}"
    due = parse_time(state["dueAt"]) if state.get("dueAt") else None
    if due is None:
        status, summary = call("GET", "/api/usage-summary")
        if status != 200 or not summary.get("claudeFiveHourResetAt"):
            return state, "waiting for a known reset time"
        reset = parse_time(summary["claudeFiveHourResetAt"])
        if reset <= now:
            return state, "waiting for a future reset time"
        return {"dueAt": iso(reset + RESET_MARGIN)}, "scheduled first turn"
    if now < due:
        return state, "not due"

    status, availability = call("GET", "/api/agent-jobs?characterId=" + CHARACTER_ID)
    employees = availability.get("employees", []) if status == 200 else []
    if not any(item.get("canReceive") for item in employees):
        return state, "recipient busy or unavailable"

    status, feed = call("GET", "/api/live-feed?limit=300")
    if status != 200:
        return state, "feed unavailable"
    recent = [parse_time(turn["startedAt"]) for turn in feed.get("turns", [])
              if turn.get("characterId") == CHARACTER_ID
              and turn.get("status") in ("running", "completed")
              and turn.get("startedAt")
              and parse_time(turn["startedAt"]) >= due - RESET_MARGIN]
    if recent:
        return {"dueAt": iso(max(recent) + INTERVAL)}, "already started by another turn"

    ok, detail = probe("haiku")
    if not ok:
        action, reason = recovery("probe_failed", detail)
        if action in {"retry_haiku", "retry_sonnet"}:
            model = "sonnet" if action == "retry_sonnet" else "haiku"
            ok, detail = probe(model)
        elif action == "stop":
            return {"dueAt": iso(now), "disabled": True}, f"Codex recovery stopped: {reason}"
        if not ok:
            failures = state.get("failedAttempts", 0) + 1
            if failures >= 3:
                return {"dueAt": iso(now), "failedAttempts": failures,
                        "disabled": True}, "isolated probe failed three times; disabled"
            if action == "wait":
                status, summary = call("GET", "/api/usage-summary")
                reset = parse_time(summary["claudeFiveHourResetAt"]) if status == 200 and summary.get("claudeFiveHourResetAt") else None
                next_due = reset + RESET_MARGIN if reset and reset > now else now + dt.timedelta(minutes=15)
            else:
                next_due = now + dt.timedelta(minutes=15)
            return {"dueAt": iso(next_due), "failedAttempts": failures}, f"probe failed; recovery chose {action}: {detail}"
    return {"dueAt": iso(now + INTERVAL),
            "verifyAfter": iso(now + dt.timedelta(minutes=10)),
            "verifyUntil": iso(now + dt.timedelta(minutes=45)),
            "verifyPreviousResetAt": iso(due - RESET_MARGIN)}, "sent one isolated short turn"


def main():
    now = dt.datetime.now(dt.timezone.utc)
    try:
        previous = read_state()
        state, outcome = tick(now, previous)
        if state != previous:
            save_state(state)
        if outcome != "not due":
            print(f"{iso(now)} {outcome}")
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as exc:
        print(f"{iso(now)} unable to check: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
