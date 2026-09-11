-- Display attribution only, not an authorization identity. NULL means unknown/user.
ALTER TABLE turns ADD COLUMN IF NOT EXISTS sender_character_id text
    REFERENCES characters(id) ON DELETE SET NULL;
