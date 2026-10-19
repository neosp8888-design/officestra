#!/usr/bin/env python3
"""One isolated Luna 'OK' turn after Codex's observed five-hour reset.

launchd checks every two minutes. Persist before sending; never resume employee
history or retry an uncertain request. Provider reset advancement is verified.
"""

import datetime as dt
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib import request


CODEX = "/Users/neo/.nvm/versions/node/v24.14.0/bin/codex"
MODEL = "gpt-6-luna"
PROMPT = "Reply exactly OK. Do not use tools."
STATE_PATH = Path.home() / "Library/Application Support/OFFICESTRA/codex-window-nudge.json"
MARGIN = dt.timedelta(minutes=1)
VERIFY_DELAY = dt.timedelta(minutes=2)
VERIFY_TIMEOUT = dt.timedelta(minutes=45)
WINDOW = dt.timedelta(hours=5)
# With no open window the provider reports a reset that rolls with now (now + 5h).
IDLE_TOLERANCE = dt.timedelta(minutes=2)


def parse_time(value):
    parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError("reset time must include timezone")
    return parsed


def iso(value):
    return value.astimezone(dt.timezone.utc).isoformat().replace("+00:00", "Z")


def summary():
    with request.urlopen("http://127.0.0.1:4317/api/usage-summary", timeout=25) as response:
        return json.load(response)


def save_state(state):
    STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix="codex-window-", dir=STATE_PATH.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(state, stream)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, STATE_PATH)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def run_probe():
    # No user config/MCP/hooks, no employee transcript, no premium fallback.
    with tempfile.TemporaryDirectory(prefix="office-codex-window-") as workdir:
        instructions = Path(workdir) / "instructions.txt"
        instructions.write_text(PROMPT + "\n")
        environment = {k: os.environ[k] for k in
                       ("HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "LANG")
                       if k in os.environ}
        environment["PATH"] = str(Path(CODEX).parent) + ":/usr/bin:/bin:/opt/homebrew/bin"
        result = subprocess.run(
            [CODEX, "exec", "--ephemeral", "--ignore-user-config", "--ignore-rules",
             "--skip-git-repo-check", "--sandbox", "read-only", "-C", workdir,
             "-m", MODEL, "-c", 'model_reasoning_effort="low"',
             "-c", 'service_tier="default"', "-c", "features.fast_mode=false",
             "-c", "model_instructions_file=" + json.dumps(str(instructions)),
             "-c", "project_doc_max_bytes=0",
             "--disable", "plugins", "--disable", "apps", "--disable", "memories",
             "--disable", "shell_tool", "--disable", "multi_agent",
             "--disable", "browser_use", "--disable", "computer_use",
             "--disable", "image_generation", "--disable", "view_image",
             "--disable", "skill_search", "--enable", "skip_host_skill_discovery",
             "--json", "-"], input=PROMPT, env=environment,
            capture_output=True, text=True, timeout=90)
    events = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
    replies = [e.get("item", {}).get("text", "") for e in events
               if e.get("type") == "item.completed" and e.get("item", {}).get("type") == "agent_message"]
    completed = [e for e in events if e.get("type") == "turn.completed"]
    tools = [e for e in events if e.get("type") == "item.started"
             and e.get("item", {}).get("type") not in ("agent_message", "reasoning")]
    ok = result.returncode == 0 and replies == ["OK"] and bool(completed) and not tools
    usage = completed[-1].get("usage", {}) if completed else {}
    # Raw stderr/JSON may include paths/session information; keep only counts.
    detail = {"model": MODEL, "reply": "OK" if ok else "unexpected response",
              "exitCode": result.returncode, "toolCalls": len(tools), "usage": usage}
    return ok, detail


def tick(now, state, read_summary=summary, probe=run_probe, persist=save_state):
    if state.get("disabled"):
        return state, "disabled; inspect saved state before re-enabling"
    due = parse_time(state["dueAt"]) if state.get("dueAt") else None
    if due and now < due and not state.get("pendingAt"):
        return state, "not due"
    snapshot = read_summary()
    if snapshot.get("codexLimitStale") is not False or snapshot.get("codexLimitError"):
        return state, "waiting for fresh Codex limits"
    if not snapshot.get("codexFiveHourResetAt"):
        return state, "waiting for a known reset time"
    reset = parse_time(snapshot["codexFiveHourResetAt"])
    if state.get("pendingAt"):
        previous = parse_time(state["previousResetAt"])
        # Expired window: the reset jumps forward. Idle start: it stops rolling with now.
        if reset > now and (reset > previous + dt.timedelta(hours=1) or reset <= now + WINDOW - MARGIN):
            return {**state, "dueAt": iso(reset + MARGIN), "pendingAt": None,
                    "verifiedAt": iso(now), "verifyAfter": None}, "reset advance verified"
        if now >= parse_time(state["pendingAt"]) + VERIFY_TIMEOUT:
            return {**state, "disabled": True}, "reset advance unverified; disabled without retry"
        return state, "waiting for reset verification"
    idle = snapshot.get("codexFiveHour") == 100 and abs(reset - (now + WINDOW)) <= IDLE_TOLERANCE
    # Also detects windows started by a regular Codex task outside OFFICESTRA.
    if reset + MARGIN > now and not idle:
        return {**state, "dueAt": iso(reset + MARGIN)}, "scheduled from provider reset"
    weekly = snapshot.get("codexWeekly")
    if weekly is not None and weekly <= 0:
        weekly_reset = snapshot.get("codexWeeklyResetAt")
        next_due = parse_time(weekly_reset) + MARGIN if weekly_reset else now + dt.timedelta(hours=1)
        return {**state, "dueAt": iso(max(next_due, now + dt.timedelta(minutes=15)))}, "weekly allowance exhausted"
    if snapshot.get("codexFiveHour") == 0:
        return {**state, "dueAt": iso(now + dt.timedelta(minutes=2))}, "waiting for allowance refresh"
    pending = {**state, "pendingAt": iso(now), "previousResetAt": iso(reset),
               "dueAt": iso(now + dt.timedelta(hours=5, minutes=1)),
               "verifyAfter": iso(now + VERIFY_DELAY)}
    # Crash/timeout after this point cannot cause a second request for this window.
    persist(pending)
    try:
        ok, detail = probe()
    except (OSError, ValueError, subprocess.TimeoutExpired):
        return {**pending, "lastProbe": {"reply": "unknown; no retry"}}, "probe uncertain; verifying without retry"
    return {**pending, "lastProbe": detail, "probeSucceeded": ok}, "sent one isolated short turn" if ok else "probe failed; verifying without retry"


def main():
    STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(str(STATE_PATH) + ".lock", os.O_CREAT | os.O_RDWR, 0o600)
    with os.fdopen(descriptor, "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 0
        now = dt.datetime.now(dt.timezone.utc)
        try:
            previous = json.loads(STATE_PATH.read_text()) if STATE_PATH.exists() else {}
            if previous.get("pendingAt") and previous.get("verifyAfter") and now < parse_time(previous["verifyAfter"]):
                return 0
            state, outcome = tick(now, previous)
            if state != previous:
                save_state(state)
            if outcome != "not due":
                print(f"{iso(now)} {outcome}" + (" " + json.dumps(state.get("lastProbe")) if outcome.startswith("sent") else ""), flush=True)
        except (OSError, ValueError, KeyError) as exc:
            print(f"{iso(now)} unable to check: {type(exc).__name__}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
