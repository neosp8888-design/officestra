-- Routing belongs to the employee, independent of GUI/terminal or input origin.
CREATE TABLE IF NOT EXISTS reply_routes (
  character_id text PRIMARY KEY REFERENCES characters(id) ON DELETE CASCADE,
  paused boolean NOT NULL DEFAULT false,
  updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS reply_route_recipients (
  character_id text NOT NULL REFERENCES reply_routes(character_id) ON DELETE CASCADE,
  recipient_character_id text NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
  PRIMARY KEY (character_id, recipient_character_id),
  CHECK (character_id <> recipient_character_id)
);
ALTER TABLE reply_deliveries DROP CONSTRAINT IF EXISTS reply_deliveries_source_turn_id_key;
CREATE UNIQUE INDEX IF NOT EXISTS reply_deliveries_source_recipient_idx
  ON reply_deliveries(source_turn_id, recipient_character_id);
ALTER TABLE reply_deliveries ADD COLUMN IF NOT EXISTS persistent_route boolean NOT NULL DEFAULT false;
