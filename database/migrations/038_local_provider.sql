CREATE TABLE IF NOT EXISTS local_agent_profiles (
  id text PRIMARY KEY,
  definition jsonb NOT NULL,
  enabled boolean NOT NULL DEFAULT false,
  updated_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE turns ADD COLUMN IF NOT EXISTS provider_kind text NOT NULL DEFAULT 'cloud'
  CHECK (provider_kind IN ('cloud','local'));
ALTER TABLE turns ADD COLUMN IF NOT EXISTS provider_snapshot jsonb;
ALTER TABLE usage_records ADD COLUMN IF NOT EXISTS cost_basis text;
ALTER TABLE usage_records ADD COLUMN IF NOT EXISTS reported_cost_audit jsonb;
