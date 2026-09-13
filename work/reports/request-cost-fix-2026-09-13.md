# Request-level context-tier pricing repair

Claude (Anthropic) uses CLI-reported cost only; no token recomputation is introduced.
Codex and Antigravity costs now select a price per model request, then sum.
Missing or inconsistent request records must not generate an ambiguous long-context price.
GUI reads only the Codex rollout bytes appended since job startup when resuming;
terminal reads the existing task boundaries. Repeated cumulative usage events are deduplicated.

Official rule: https://developers.openai.com/api/docs/models/gpt-6-astra
Long-context multipliers apply when an individual request input exceeds 272,000 tokens.
These are API-equivalent estimates, not subscription bills or verified invoices.

## Scoped historical repair and rollback values

The repair script below only allows these five completed turns and guards the old cost and tokens.
No other past records are changed. Restore these prior costs by the exact turn UUID only if rollback is required.

| Turn UUID | Previous cost USD | Corrected cost USD | Unique requests | Maximum request input |
|---|---:|---:|---:|---:|
| eb4962ec-3bb8-45b0-8c64-73ca3a9d9157 | 7.17211400 | 3.60603200 | 5 | 235940 |
| 5b3699e8-d70e-4447-ba68-a3365bd1d088 | 4.00945000 | 2.05445000 | 11 | 147407 |
| 0df53f10-145d-401f-a0a0-5080973d5dfa | 3.98956000 | 2.00803000 | 4 | 156509 |
| b08a85c7-1269-4004-9167-456a1a66d533 | 4.39966300 | 2.21719400 | 4 | 165928 |
| ba00e328-8d43-4f74-ba4b-c9a7a85a2191 | 7.11639400 | 3.59867200 | 11 | 177515 |

Totals: 26.68718100 → 13.48437800; difference 13.20280300 USD.
Input, cached input, cache write and output sums match the DB exactly for all 35 requests.
Dry run: `node work/reports/repair-request-costs-2026-09-13.mjs`.
Apply: same command with `--apply`; atomic transaction, no backend restart.

## Verification completed

Applied the scoped five-record repair atomically. All five live-feed detail APIs returned HTTP 200
and the corrected amounts above. No token fields or other historical rows were changed.

Full backend test summary (local):
```
ℹ tests 607
ℹ suites 0
ℹ pass 600
ℹ fail 0
ℹ cancelled 0
ℹ skipped 7
ℹ todo 0
ℹ duration_ms 26555.938208
```
The seven skipped tests are isolated-PostgreSQL integration tests; they were not counted as passed.
`npm --prefix backend run check` and `git diff --check`: exit 0.
`scripts/build-app.sh`: exit 0; signed bundle valid on disk.
All six modified production modules match the packaged backend byte-for-byte.
Backend listener remained PID 93689 throughout; no restart, commit or push performed.
New calculation requires the user to restart the backend from the app button.
No app UI verification or new real-provider paid request was performed.

Limits: exact request source totals must reconcile before using request-level pricing.
If missing/inconsistent, an ambiguous tiered price is left unknown rather than fabricated.
No claim is made that every past turn has been repaired, or all provider billing is error-free.

## Changed files

- backend/src/token-cost-estimator.mjs
- backend/src/terminal-usage.mjs
- backend/src/antigravity-local-state.mjs
- backend/src/agent-runtime.mjs
- backend/src/terminal-sessions.mjs
- backend/src/backfill-terminal-turn-usage.mjs
- backend/test/token-cost-estimator.test.mjs
- backend/test/terminal-usage.test.mjs
- backend/test/antigravity-local-state.test.mjs
- backend/test/agent-runtime.test.mjs
- work/reports/repair-request-costs-2026-09-13.mjs
- work/reports/request-cost-fix-2026-09-13.md
