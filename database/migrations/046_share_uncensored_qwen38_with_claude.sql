-- Keep the Claude Code runner but load the exact uncensored Q4_K_M weights
-- already pinned for the Codex runner. The model identity change intentionally
-- starts a fresh employee session; history and active workspaces are preserved.
UPDATE local_agent_profiles
SET definition = jsonb_set(
        jsonb_set(
          jsonb_set(
            definition,
            '{host,modelKey}',
            to_jsonb('qwen3.8-27b-uncensored'::text)
          ),
          '{profile,model}',
          to_jsonb('officestra-qwen38-27b-uncensored-q4km'::text)
        ),
        '{profile,credentialVersion}',
        to_jsonb('managed-qwen38-uncensored-q4km-v1'::text)
      ),
    updated_at = now()
WHERE id = 'local-4090-qwen38'
  AND definition->'profile'->>'backend' = 'claude';

UPDATE characters
SET model = 'officestra-qwen38-27b-uncensored-q4km',
    updated_at = now()
WHERE config->>'localProfileId' = 'local-4090-qwen38'
  AND EXISTS (
    SELECT 1
    FROM local_agent_profiles AS profile
    WHERE profile.id = 'local-4090-qwen38'
      AND profile.definition->'host'->>'modelKey' =
        'qwen3.8-27b-uncensored'
      AND profile.definition->'profile'->>'model' =
        'officestra-qwen38-27b-uncensored-q4km'
  );

WITH safe_sessions AS (
  SELECT active.character_id, active.cli_session_id
  FROM active_cli_sessions AS active
  JOIN characters AS character ON character.id = active.character_id
  WHERE character.config->>'localProfileId' = 'local-4090-qwen38'
    AND character.model = 'officestra-qwen38-27b-uncensored-q4km'
    AND NOT EXISTS (
      SELECT 1
      FROM task_workspaces AS workspace
      WHERE workspace.cli_session_id = active.cli_session_id
        AND workspace.status IN (
          'provisioning', 'active', 'awaiting_approval',
          'merging', 'conflict'
        )
    )
), ended_sessions AS (
  UPDATE cli_sessions AS session
  SET ended_at = now()
  FROM safe_sessions
  WHERE session.id = safe_sessions.cli_session_id
    AND session.ended_at IS NULL
  RETURNING session.id
)
DELETE FROM active_cli_sessions AS active
USING safe_sessions
WHERE active.character_id = safe_sessions.character_id
  AND active.cli_session_id = safe_sessions.cli_session_id;
