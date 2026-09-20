-- Opt-in Claude runner using the same pinned model/server/MTP as Codex.
-- Preserve all existing profiles, assignments and sessions.
INSERT INTO local_agent_profiles (id, definition, enabled)
SELECT 'local-4090-qwen38-claude-llamacpp',
       jsonb_set(definition, '{profile}', (definition->'profile') ||
         '{"id":"local-4090-qwen38-claude-llamacpp","backend":"claude","usageProtocol":"anthropic-normalized-v1","reasoning":"default"}'::jsonb),
       true
FROM local_agent_profiles WHERE id='local-4090-qwen38-llamacpp'
ON CONFLICT (id) DO NOTHING;
