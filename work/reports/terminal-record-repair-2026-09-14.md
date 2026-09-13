# Terminal record repair — 2026-09-14

## Scope and plan
Exchange API review turns with the actual right-woman employee; inspect the
missing terminal record; preserve existing final response and cost handling;
add bounded collection/retry and regression tests; repair only the verified
historical gap; build the app. No backend restart or git commit/push by this agent.

## Evidence and implementation
- Missing turn: 9a40a347-5adc-4344-90f4-289b5a04ecea, terminal/local/completed.
- Transcript session: 375a870d-cf52-4fe7-bc7b-69ee19ff5247. Input15731/output175
  appeared twice under one message.id across thinking/text blocks; counted once.
- Existing usage row had null token fields and a cost audit; no activity rows.
  The existing completion fallback currently reads the transcript correctly.
  Historical timing/path/offset cause is not conclusively established.
- Stop now collects usage and activities together, with a 1200ms retry budget
  at 100ms intervals to tolerate a delayed final transcript write.
- Missing start path, replaced inode or invalid offset uses a session/time-bounded
  tail recovery (at most4MiB). A truncated tail must include an earlier boundary;
  otherwise it is rejected rather than reported as complete usage.
- Repeated assistant message.id snapshots retain latest non-null usage fields.
  Final response is not duplicated as activity. Existing completion/work-record
  pipeline and Claude reported-cost handling remain in use.
- Original missing record restored: input15731/output175, one thinking activity,
  cost null/local-not-applicable. Original cost audit preserved. Final response,
  model configuration and session were not changed. Repeat repair is idempotent.
- GET /api/live-feed/<turnId> confirms restored thinking and context usage.

## Verification
- Final full backend suite including tail-boundary regression: tests617, pass610,
  fail0, skipped7 (skips are not passes).
- Final focused suite including tail-boundary regression: tests44, pass44,
  fail0, skipped0.
- npm backend check and git diff --check passed.
- scripts/build-app.sh completed; packaged embedding smoke passed; app signature
  valid. Three changed backend source modules match the packaged copies bytewise.
- No Swift source changes; no claim of manual UI verification.
- Running backend remained PID53434/release93afccd5a0fbdbcda72ce091a5f26187c5bca564b918324d8a959da647ccb75b.
  Historical DB repair is active; prevention code requires user backend restart.
- Follow-up after user app-button restart: new terminal turn
  4242a0f7-bdc6-4ce2-9e0d-8d1d873033d6 auto-saved without manual repair —
  DB input38309/output110, one thinking activity, final response intact.
- Bounded retry is not a guarantee against arbitrary write delays. Oversized or
  unidentified recovery spans fail safely; existing fallback and warning remain.

## Changed files
- backend/src/terminal-usage.mjs
- backend/src/terminal-turn-activities.mjs
- backend/src/terminal-sessions.mjs
- backend/test/terminal-turn-activities.test.mjs
- backend/test/terminal-sessions.test.mjs
- work/local-llm/repair-qwen-terminal-record.mjs
- work/reports/qwen38-operating-review-2026-09-14.md (evidence correction)
- work/reports/terminal-record-repair-2026-09-14.md

## API review
- First review: 73efda02-2c36-4150-978f-14a09ce2b8e6; scope grew to about19min.
  This agent cancelled that review only, without restarting services. Actual
  runtime API marked it interrupted (its generic error says user cancellation).
- Completed review appeared in d8629a17-f8ef-4bd8-b226-12788179cb59, a separate
  user '?' follow-up: reviewer independently confirmed repaired DB values,
  activities, packaged-source equality and44 related tests. Two attempted
  follow-up POSTs received409 while that separate turn ran; neither created a job.
- Final API exchange845b133b-63a7-4994-b48e-bccec98bb992 completed with agreement:
  fix/repair/build by the primary agent, restart by user, historical exact cause
  unproven, and a real new terminal turn still needs verification after restart.
- Review requests use characterId=right-woman, senderCharacterId=boss.

## Shared repository observation
During review, HEAD changed externally to f98a418, containing the five backend
source/test files above. This agent did not run commit/push or undo that commit.
