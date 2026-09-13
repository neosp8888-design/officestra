# Qwen3.8 operating review — API exchange with local employee

Scope: analysis only. No model/employee setting changes, restart, build, commit or push.

## Current verified configuration
- Live 4317 PID53434, release93afccd5a0fbdbcda72ce091a5f26187c5bca564b918324d8a959da647ccb75b; health and DB healthy.
- right-woman assigned local-4090-qwen38; context65536; reasoning on.
- Review-time resource snapshots: VRAM95.42%, RAM44.34%; later VRAM95.41%, RAM55.59%.
  These are point samples, not a sustained maximum or proof of safety under all workloads.
- VRAM stop threshold98%, RAM77%; production bridge poll3000ms, host-load guard1500ms.
- max_tokens greater than4096 is rejected. thinking budget_tokens1024 is sent, but
  actual Qwen hard thinking cap and final-answer reservation have not been validated.

## API discussion
- First request: ac946dfd-82bc-4b97-977f-d8c32af61a07 (GUI, completed, 154.975 seconds).
- Second request: 8d49dfb1-f15c-4591-a30f-a8203ae132c2.
- Both POST /api/agent-jobs requests used senderCharacterId=boss and characterId=right-woman.
  Replies were read from GET /api/live-feed/<turnId>. No recursive callback jobs.
- Local employee accepted corrections and agreed: retain current model/settings and98%
  decision, prioritize the actual terminal logging gap; do not blame quantization or
  claim a verified thinking cap. No remaining disagreement in the second reply.

## Evidence and findings
1. Four Qwen3.8 turns before this review: three completed and one interrupted.
   8b7c7850-3134-459c-8d53-ee4c16930c9c records user cancellation explicitly.
   No failed status in this small set; not a general reliability estimate.
2. Session375a870d-cf52-4fe7-bc7b-69ee19ff5247 JSONL records one auto-compaction:
   48603 ->2777 tokens, duration115721ms. No repeated compaction observed in that file.
   This explains a substantial wait, but does not establish all latency causes.
3. Confirmed terminal persistence gap, turn9a40a347-5adc-4344-90f4-289b5a04ecea:
   transcript contains thinking and text; usage input15731/output175.
   The same usage is repeated across message blocks; must not sum duplicates.
   Correction from the subsequent repair: DB had one usage_records row with null
   token fields (and an existing cost audit), plus zero activities, not zero usage rows.
   Stop did not explicitly pass usage, but completeTerminalTurn already had a
   transcript/time-window fallback. That fallback now reads input15731/output175
   correctly. Therefore absence of an explicit usage argument is not a proven root
   cause; historical flush timing/path/start-offset conditions remain unconfirmed.
   See terminal-record-repair-2026-09-14.md for the bounded retry and scoped repair.
4. Ordinary GUI turns have thinking/tool activities and token usage. This review's
   API request also exercises the actual configured local CLI, not an external model.
5. Potential risks, not demonstrated failures: other GPU apps can exceed98% between
   samples; long responses share the4096 generation allowance with reasoning.
   Do not reduce the current threshold to97% (prior measured peak97.32%) without
   a user-approved resource/model tradeoff. No recommendation to enlarge budgets blindly.

Conclusion: retain Q4_K_M/64K/thinking-on/VRAM98 configuration. Not "no issues": fix
terminal usage/activity persistence separately if requested. Longer real-world workload
quality, exact thinking-token accounting, and all concurrent-GPU conditions remain unproven.

Sources inspected: backend/src/local-agent-profile.mjs, local-inference-bridge.mjs,
local-provider-service.mjs, terminal-sessions.mjs, terminal-turn-activities.mjs,
agent-runtime.mjs; read-only turns/messages/usage_records/turn_activities queries;
/Users/neo/.claude/projects/-Users-neo-office/375a870d-cf52-4fe7-bc7b-69ee19ff5247.jsonl.

Only this report file was added during the review.
