-- Display attribution only: 'remote' marks a prompt sent by the mobile remote session.
-- NULL means the app user or unknown; employee senders stay in sender_character_id.
ALTER TABLE turns
  ADD COLUMN IF NOT EXISTS request_source text;

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1
    FROM pg_constraint
    WHERE conname = 'turns_request_source_check'
      AND conrelid = 'turns'::regclass
  ) THEN
    ALTER TABLE turns
      ADD CONSTRAINT turns_request_source_check
      CHECK (request_source IS NULL OR request_source = 'remote');
  END IF;
END
$$;
