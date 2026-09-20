-- User-selected, single-hop reply delivery. Models do not decide routing.
CREATE TABLE IF NOT EXISTS reply_deliveries (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  source_turn_id uuid NOT NULL UNIQUE REFERENCES turns(id) ON DELETE CASCADE,
  recipient_character_id text NOT NULL REFERENCES characters(id),
  status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','sending','uncertain','delivered','failed','cancelled')),
  target_turn_id uuid REFERENCES turns(id) ON DELETE SET NULL,
  error_message text,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS reply_deliveries_pending_idx ON reply_deliveries(created_at) WHERE status IN ('pending','sending');
