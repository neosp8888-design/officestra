-- Retain model files, history and definitions; hide the superseded LM Studio
-- options. Do not change employee assignments or active sessions.
UPDATE local_agent_profiles
SET enabled=false, updated_at=now()
WHERE id IN ('local-4090','local-4090-qwen38','local-4090-qwen38-codex')
  AND enabled=true
  AND definition->'profile'->>'runtime' IS DISTINCT FROM 'llama-cpp-b10982';
