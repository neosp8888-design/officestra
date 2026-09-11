-- Includes late terminal status-line writes and backfills.
CREATE OR REPLACE FUNCTION preserve_local_usage_cost() RETURNS trigger AS $$
BEGIN
  IF EXISTS (SELECT 1 FROM turns WHERE id=NEW.turn_id AND provider_kind='local') THEN
    IF NEW.cost_usd IS NOT NULL THEN
      NEW.reported_cost_audit = COALESCE(NEW.reported_cost_audit,'{}'::jsonb)
        || jsonb_build_object('lastReportedCostUsd',NEW.cost_usd);
    END IF;
    NEW.cost_usd = NULL;
    NEW.cost_basis = 'local-not-applicable';
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS local_usage_cost_guard ON usage_records;
CREATE TRIGGER local_usage_cost_guard BEFORE INSERT OR UPDATE ON usage_records
FOR EACH ROW EXECUTE FUNCTION preserve_local_usage_cost();
