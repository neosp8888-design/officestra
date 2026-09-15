-- Add a separate opt-in runner; preserve LM Studio profiles and employee sessions.
INSERT INTO local_agent_profiles (id, definition, enabled)
SELECT 'local-4090-qwen38-llamacpp',
       jsonb_set(definition, '{profile}', (definition->'profile') ||
         '{"id":"local-4090-qwen38-llamacpp","backend":"codex","runtime":"llama-cpp-b10982","contextWindow":65536,"kvCacheQuantization":"q8_0","usageProtocol":"openai-responses-v1","reasoning":"default"}'::jsonb),
       true
FROM local_agent_profiles WHERE id='local-4090-qwen38'
ON CONFLICT (id) DO NOTHING;
