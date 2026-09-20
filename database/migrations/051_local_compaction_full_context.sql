ALTER TABLE characters DROP CONSTRAINT IF EXISTS characters_auto_compact_percent_check;
ALTER TABLE characters ADD CONSTRAINT characters_auto_compact_percent_check
CHECK (auto_compact_percent BETWEEN 20 AND 100);

-- Preserve the prior cloud threshold when returning from a local profile.
UPDATE characters
SET config = jsonb_set(config, '{localPreviousSettings}',
    (config->'localPreviousSettings') || jsonb_build_object('autoCompactPercent', auto_compact_percent))
WHERE NULLIF(config->>'localProfileId', '') IS NOT NULL
  AND jsonb_typeof(config->'localPreviousSettings') = 'object'
  AND NOT (config->'localPreviousSettings' ? 'autoCompactPercent');

UPDATE characters SET auto_compact_percent = 100, updated_at = now()
WHERE NULLIF(config->>'localProfileId', '') IS NOT NULL
  AND auto_compact_percent <> 100;
