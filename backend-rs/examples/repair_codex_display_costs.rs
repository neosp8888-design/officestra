//! One-off display-cost repair. Dry run by default; no token, prompt, or non-Codex writes.
//! cargo run --offline --example repair_codex_display_costs -- pricing.md backup.json [--apply] [--previous-backup path] [--since ISO]
use std::{collections::HashMap, fs::{self, OpenOptions}, io::{BufRead, BufReader, Write}, os::unix::fs::OpenOptionsExt};
use officestra_backend::{config::Config, db, pricing_catalog::{parse_openai_pricing_markdown, PricingCatalogs}, terminal_usage::{codex_rollout_turn_usage, CodexScope, TurnWindow}, token_cost_estimator::{estimate_turn_token_cost, CostQuery}};
use serde_json::{json, Value};
use sqlx::Row;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await { eprintln!("{error}"); std::process::exit(1); }
}

async fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--fetch-check"] {
        use officestra_backend::pricing_catalog::{HttpPricingFetcher, PricingFetcher};
        let page = HttpPricingFetcher::new()?.fetch("https://developers.openai.com/api/docs/pricing.md", std::time::Duration::from_secs(15)).await?;
        let parsed = parse_openai_pricing_markdown(&page.body)?;
        println!("{}", json!({"status":page.status,"rates":parsed.to_json()["models"]["gpt-6.1-sol"]}));
        return Ok(());
    }
    if args.len() < 2 {
        return Err("usage: repair_codex_display_costs pricing.md backup.json [--apply] [--previous-backup path] [--since ISO]".into());
    }
    let mut apply = false;
    let mut previous_path = None;
    let mut since = None;
    let mut rest = args[2..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--apply" => apply = true,
            "--previous-backup" => previous_path = Some(rest.next().ok_or("missing previous backup path")?),
            "--since" => since = Some(rest.next().ok_or("missing since timestamp")?),
            _ => return Err("unknown argument".into()),
        }
    }
    let parsed = parse_openai_pricing_markdown(&fs::read_to_string(&args[0]).map_err(|_| "cannot read pricing source")?)?;
    let corrected = PricingCatalogs::new();
    corrected.install("codex", &parsed.to_json())?;
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await.map_err(|_| "database connection failed")?;
    let mut tx = pool.begin().await.map_err(|_| "cannot begin transaction")?;
    // Same snapshot for preview, old-price comparison, and optimistic guarded updates.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").execute(&mut *tx).await.map_err(|_| "cannot set isolation")?;
    if !apply { sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *tx).await.map_err(|_| "cannot set read only")?; }
    let old: Value = sqlx::query_scalar("SELECT catalog FROM pricing_catalogs WHERE provider='codex'")
        .fetch_one(&mut *tx).await.map_err(|_| "cannot read Codex pricing")?;
    // A prior backup can be supplied again after restart to catch a late old-runtime completion.
    let prior_backup: Option<Value> = previous_path.map(|path| {
        let text = fs::read_to_string(path).map_err(|_| "cannot read previous backup")?;
        serde_json::from_str(&text).map_err(|_| "invalid previous backup")
    }).transpose()?;
    if prior_backup.as_ref().is_some_and(|v| !v.get("previousCatalog").is_some_and(Value::is_object)) {
        return Err("previous backup has no catalog".into());
    }
    let old_prices = prior_backup.as_ref().and_then(|v| v.get("previousCatalog")).unwrap_or(&old);
    let previous = PricingCatalogs::new(); previous.install("codex", old_prices)?;
    let rows = sqlx::query(concat!(
        "SELECT t.id::text AS id, t.model, t.resolved_model, t.fast_mode, (extract(epoch FROM t.started_at)*1000)::double precision AS started, (extract(epoch FROM t.ended_at)*1000)::double precision AS ended, ",
        "s.external_id, u.input_tokens, u.cached_input_tokens, u.output_tokens, u.cache_write_input_tokens, u.cost_usd::double precision AS cost ",
        "FROM usage_records u JOIN turns t ON t.id=u.turn_id JOIN cli_sessions s ON s.id=t.cli_session_id ",
        "WHERE t.backend='codex' AND t.ended_at IS NOT NULL AND u.cost_usd IS NOT NULL ",
        "AND ($1::text IS NULL OR t.started_at >= ($1::text)::timestamptz) ORDER BY t.started_at"
    )).bind(since.map(String::as_str)).fetch_all(&mut *tx).await.map_err(|_| "cannot read Codex usage")?;
    let root = config.codex_home.unwrap_or_else(|| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex")).join("sessions");
    let mut compact = HashMap::new();
    let mut repairs = Vec::new();
    let mut skipped = 0;
    for row in &rows {
        let model: Option<String> = row.get::<Option<String>, _>("resolved_model").or_else(|| row.get("model"));
        // Only models whose fetched prices changed are eligible; historical unknown models are untouched.
        let Some(model) = model else { skipped += 1; continue };
        if !model.starts_with("gpt-") || parsed.get(&model).is_none() || previous.current("codex").get(&model) == corrected.current("codex").get(&model) {
            continue;
        }
        let Some(external): Option<String> = row.get("external_id") else { skipped += 1; continue };
        let path = if let Some(path) = compact.get(&external) { path } else {
            let Some(path) = find_rollout(&root, &external) else { skipped += 1; continue };
            let destination = std::env::temp_dir().join(format!("office-cost-repair-{}-{}.jsonl", std::process::id(), compact.len()));
            let mut out = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&destination).map_err(|_| "cannot create compact audit")?;
            for line in BufReader::new(fs::File::open(path).map_err(|_| "cannot read rollout")?).lines() {
                let line = line.map_err(|_| "cannot read rollout line")?;
                let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
                if event["type"] == "turn_context" {
                    let context = json!({ "type": "turn_context", "timestamp": event["timestamp"], "payload": {
                        "model": event["payload"]["model"], "turn_id": event["payload"]["turn_id"], "service_tier": event["payload"]["service_tier"]
                    }});
                    writeln!(out, "{context}").map_err(|_| "cannot write compact audit")?;
                } else if event["type"] == "event_msg" && event["payload"]["type"] == "token_count" {
                    writeln!(out, "{line}").map_err(|_| "cannot write compact audit")?;
                }
            }
            compact.insert(external.clone(), destination.to_string_lossy().into_owned());
            compact.get(&external).unwrap()
        };
        let started = Some(row.get::<f64, _>("started"));
        let ended = Some(row.get::<f64, _>("ended"));
        let Some(usage) = codex_rollout_turn_usage(Some(path), TurnWindow { started_at: started, ended_at: ended }, CodexScope::Window, 0) else { skipped += 1; continue };
        let fields = [("input_tokens", "inputTokens"), ("cached_input_tokens", "cachedInputTokens"), ("output_tokens", "outputTokens"), ("cache_write_input_tokens", "cacheWriteInputTokens")];
        if fields.iter().any(|(db, key)| {
            let stored = row.get::<Option<i64>, _>(*db).unwrap_or(0);
            stored.unsigned_abs() > (1_u64 << 53) || stored as f64 != usage[*key].as_f64().unwrap_or(0.0)
        }) {
            skipped += 1; continue;
        }
        let query = CostQuery { backend: Some("codex"), model: Some(&model), fast_mode: row.get("fast_mode"), usage: Some(&usage), priced_at: started };
        let (Some(wrong), Some(right)) = (estimate_turn_token_cost(&previous, &query), estimate_turn_token_cost(&corrected, &query)) else { skipped += 1; continue };
        let stored: f64 = row.get("cost");
        if (stored - wrong).abs() > 0.00000001 || (stored - right).abs() <= 0.00000001 { continue; }
        repairs.push(json!({ "turnId": row.get::<String, _>("id"), "previousCostUsd": stored, "correctedCostUsd": right }));
    }
    for path in compact.values() { let _ = fs::remove_file(path); }
    let backup = json!({ "previousCatalog": old_prices, "replacementCatalog": parsed.to_json(), "repairs": repairs });
    // Never overwrite an audit backup. A rerun must use a new backup filename.
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&args[1]).map_err(|_| "backup already exists or cannot be created; use a new path")?;
    serde_json::to_writer_pretty(&mut file, &backup).map_err(|_| "cannot save backup")?;
    file.sync_all().map_err(|_| "cannot sync backup")?;
    if apply {
        for repair in &repairs {
            let changed = sqlx::query("UPDATE usage_records u SET cost_usd=$1::double precision::numeric FROM turns t WHERE u.turn_id=t.id AND t.backend='codex' AND t.id::text=$2 AND abs(u.cost_usd-$3::double precision::numeric)<0.00000001")
                .bind(repair["correctedCostUsd"].as_f64().unwrap()).bind(repair["turnId"].as_str().unwrap()).bind(repair["previousCostUsd"].as_f64().unwrap())
                .execute(&mut *tx).await.map_err(|_| "cost update failed; rolled back")?;
            if changed.rows_affected() != 1 { return Err("cost changed during repair; rolled back".into()); }
        }
        let mut replacement = officestra_backend::pricing_catalog::merge_pricing_catalog(&old, &parsed.to_json()).to_json();
        replacement["openaiParserVersion"] = Value::from(officestra_backend::pricing_catalog::OPENAI_PRICING_PARSER_VERSION);
        sqlx::query("UPDATE pricing_catalogs SET catalog=$1, updated_at=now() WHERE provider='codex'")
            .bind(replacement).execute(&mut *tx).await.map_err(|_| "catalog update failed; rolled back")?;
        tx.commit().await.map_err(|_| "commit failed")?;
    } else { tx.rollback().await.map_err(|_| "rollback failed")?; }
    let sum = |key| repairs.iter().map(|v| v[key].as_f64().unwrap_or(0.0)).sum::<f64>();
    println!("{}: {} Codex costs; {:.8} -> {:.8} USD; {} unverifiable records skipped; other providers/tokens unchanged", if apply {"APPLIED"} else {"PREVIEW"}, repairs.len(), sum("previousCostUsd"), sum("correctedCostUsd"), skipped);
    pool.close().await;
    Ok(())
}

fn find_rollout(root: &std::path::Path, external: &str) -> Option<std::path::PathBuf> {
    for entry in fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() { if let Some(found) = find_rollout(&path, external) { return Some(found); } }
        else if path.extension().is_some_and(|extension| extension == "jsonl") && entry.file_name().to_string_lossy().contains(external) { return Some(path); }
    }
    None
}
