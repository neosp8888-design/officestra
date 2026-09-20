-- Add separately selectable 26B-A4B 64K/KV8 GPU-vision profiles for both runners.
-- Preserve existing assignments and conversation/model snapshots.
INSERT INTO local_agent_profiles (id, definition, enabled)
SELECT 'local-4090-meromero-26b-' || runner.backend || '-llamacpp',
  jsonb_set(jsonb_set(source.definition, '{host,modelKey}',
    '"g4-meromero-26b-a4b-it-uncensored-heretic"'::jsonb), '{profile}',
    (source.definition->'profile') || jsonb_build_object(
      'id','local-4090-meromero-26b-' || runner.backend || '-llamacpp',
      'backend',runner.backend,
      'model','officestra-meromero-26b-a4b-uncensored-heretic-q4km',
      'contextWindow',65536,'maxOutputTokens',8192,'reasoning','default',
      'credentialVersion','managed-meromero-26b-a4b-q4km-v1',
      'usageProtocol',CASE WHEN runner.backend='codex' THEN 'openai-responses-v1' ELSE 'anthropic-normalized-v1' END)),
  true
FROM local_agent_profiles source
CROSS JOIN (VALUES ('claude'),('codex')) runner(backend)
WHERE source.id='local-4090-meromero-codex-llamacpp'
ON CONFLICT (id) DO NOTHING;
