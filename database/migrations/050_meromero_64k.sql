-- Verified 64K/KV8 full-GPU configuration. Preserve runner, model, reasoning,
-- assignments and sessions; the existing resume policy permits 32K -> 64K.
UPDATE local_agent_profiles
SET definition=jsonb_set(definition,'{profile,contextWindow}','65536'::jsonb)
WHERE id IN ('local-4090-meromero-claude-llamacpp','local-4090-meromero-codex-llamacpp')
  AND definition->'profile'->>'model'='officestra-meromero-31b-uncensored-heretic-q4km'
  AND definition->'profile'->>'runtime'='llama-cpp-b10982'
  AND definition->'profile'->>'kvCacheQuantization'='q8_0'
  AND definition->'profile'->>'contextWindow'='32768';
