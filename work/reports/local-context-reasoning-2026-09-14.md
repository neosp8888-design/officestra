# Local context label and verified thinking controls

## Changes
- Replaced the fixed 32K label with selected LocalModelOption.contextWindow.
  Missing catalog data shows a dash instead of an invented window.
- Added a capability-gated Default / Thinking on / Thinking off picker, Korean
  and English. Older backends and models without measured support show no picker.
- Added idle-only, session-locked local-reasoning endpoint; preserves session/history,
  permissions, cloud restore point, model and context. Running/compacting/terminal
  use blocks changes. No active employee settings were changed during development.
- Selected reasoning is carried in each launch profile and turn provider snapshot.
  GUI and terminal share the bridge transformation. Default preserves prior requests.
  Explicit on/off overrides conflicting CLI thinking flags at the bridge only.
- On uses Anthropic thinking enabled with budget_tokens 1024; off uses disabled.
  This is a compatibility parameter, not a verified hard Qwen thinking-token cap.
  Output maximum remains 4096; no context/GPU guard changes in this follow-up.
- Separate runtime entries/signatures pin different reasoning choices; idle old entries
  are cleaned up before a changed choice loads. Reasoning alone does not invalidate resume.

## Actual engine verification
- LM Studio 0.4.24 build1; Qwen3.8-27B UD Q4_K_M; 65536 context; VRAM98/RAM77.
- Official template supports low/medium/xhigh, default xhigh, only when thinking enabled:
  https://huggingface.co/Qwen/Qwen3.8-27B/raw/main/chat_template.jinja
- Installed Anthropic adapter /v1/messages/count_tokens returns an unsupported-endpoint
  error inside HTTP200. Did not count these calls as passing validation.
- Actual generation probes: output_config.effort low/medium/xhigh all had input_tokens60
  for the same prompt. INVALID returned400, so acceptance alone is not delivery proof.
  Top-level reasoning_effort and chat_template_kwargs low/medium/xhigh/INVALID all had
  input_tokens20 with no reasoning. Do not expose these ignored intensity choices.
- Through the new authenticated bridge, same short question:
  off => input23/output2, text answer, no thinking block;
  on => input63/output37, thinking block followed by text answer.
- Real Claude CLI through the thinking-on bridge executed Bash printf THINKING_TOOL_OK
  and completed successfully (exit0, result.is_error=false). Custom-model recognition
  warning was emitted; execution succeeded. No user files modified by this test.
- Cleanup confirmed inference server PID0 and loaded models[].
- Raw test results: work/reports/qwen38-reasoning-probe.json;
  harness: work/local-llm/qwen38-reasoning-probe.mjs.

## Verification status
- Full backend: tests614, pass607, fail0, skipped7, cancelled0, todo0.
- Final service/selection regression after status identity adjustment: pass28, fail0, skipped1.
- Swift selected presentation tests: 6 passed. Full Swift: Executed 602 tests,
  with 0 failures (0 unexpected) in 138.253 (138.297) seconds.
- App build exited0, packaged embedding smoke passed, signature valid. Five changed
  backend modules are byte-identical to source. Bundle backend release:
  93afccd5a0fbdbcda72ce091a5f26187c5bca564b918324d8a959da647ccb75b.
- Live backend is still PID35181, release9c08c30a2e40bdfbc69ff8312c66a60b0425243ef4c7c9dedb9c2c13855bda71,
  /health and DB healthy. User must reopen the app and restart backend via its button.
- App screen verification after restart was not performed; presentation decode/format
  tests passed. No claim that the running app already displays the new controls.
- No 4317 restart, production reasoning selection, commit or push performed.

## Files changed in this follow-up
Sources/OfficeGame/OfficeGameApp.swift
Sources/OfficeGame/OfficeDatabaseClient.swift
Sources/OfficeGame/AgentDirector.swift
Sources/OfficeGame/Resources/en.lproj/Localizable.strings
Tests/OfficeCoreTests/LocalProviderPresentationTests.swift
backend/src/local-agent-profile.mjs
backend/src/local-inference-bridge.mjs
backend/src/local-provider-service.mjs
backend/src/local-profile-selection.mjs
backend/src/server.mjs
backend/test/local-agent-profile.test.mjs
backend/test/local-inference-bridge.test.mjs
backend/test/local-provider-service.test.mjs
backend/test/local-profile-selection.test.mjs
work/local-llm/qwen38-reasoning-probe.mjs
work/reports/qwen38-reasoning-probe.json
work/reports/local-context-reasoning-2026-09-14.md

Earlier uncommitted VRAM98 changes are preserved, not reverted or claimed as new here.
