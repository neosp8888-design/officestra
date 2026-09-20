-- Add opt-in text-only 32K/KV8 profiles. Preserve assignments and sessions.
INSERT INTO local_agent_profiles (id, definition, enabled)
SELECT 'local-4090-meromero-' || runner.backend || '-llamacpp',
  jsonb_set(jsonb_set(jsonb_set(source.definition, '{host,modelKey}', '"g4-meromero-31b-uncensored-heretic"'::jsonb), '{host,address}', to_jsonb(COALESCE((
    SELECT c.config->>'localHostAddress'
    FROM characters c JOIN local_agent_profiles p ON p.id=c.config->>'localProfileId'
    WHERE p.definition->'host'->>'hostKeyAlias'=source.definition->'host'->>'hostKeyAlias'
      AND NULLIF(c.config->>'localHostAddress','') IS NOT NULL
    ORDER BY c.updated_at DESC,c.id LIMIT 1
  ),source.definition->'host'->>'address'))), '{profile}',
    (source.definition->'profile') || jsonb_build_object(
      'id','local-4090-meromero-' || runner.backend || '-llamacpp',
      'backend',runner.backend,'model','officestra-meromero-31b-uncensored-heretic-q4km',
      'contextWindow',32768,'maxOutputTokens',8192,'reasoning','default',
      'credentialVersion','managed-meromero-31b-q4km-v1',
      'usageProtocol',CASE WHEN runner.backend='codex' THEN 'openai-responses-v1' ELSE 'anthropic-normalized-v1' END)),
  true
FROM local_agent_profiles source CROSS JOIN (VALUES ('claude'),('codex')) runner(backend)
WHERE source.id='local-4090-qwen38-llamacpp'
ON CONFLICT (id) DO NOTHING;
