// 직원별 전체 비용·평가 요약 캐시와 백엔드별 사용량 보고. Node usage-report.mjs를 옮겼다.

use std::future::Future;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::js::{iso_string, js_trim, now_milliseconds};

/// Node와 같은 집계 SQL. 바깥 SELECT가 row_to_json으로 열 순서대로 키를 만든다.
pub const COST_SUMMARY_SQL: &str = r#"
          SELECT character.id AS "characterId",
            COUNT(DISTINCT turn.id)::integer AS "finishedTurnCount",
            COUNT(DISTINCT turn.id) FILTER (
              WHERE turn.status = 'failed'
            )::integer AS "failedTurnCount",
            COUNT(DISTINCT turn.id) FILTER (
              WHERE turn.status = 'interrupted'
            )::integer AS "interruptedTurnCount",
            COUNT(usage.cost_usd)::integer AS "pricedTurnCount",
            COALESCE(SUM(usage.cost_usd), 0)::double precision AS "totalCostUsd",
            COALESCE(SUM(usage.cost_usd) FILTER (
              WHERE turn.ended_at > turn.started_at
            ), 0)::double precision AS "timedCostUsd",
            COALESCE(SUM(EXTRACT(EPOCH FROM (
              turn.ended_at - turn.started_at
            ))) FILTER (
              WHERE usage.cost_usd IS NOT NULL
                AND turn.ended_at > turn.started_at
            ), 0)::double precision AS "totalDurationSeconds",
            COUNT(*) FILTER (
              WHERE feedback.feedback = 'liked'
            )::integer AS "likedCount",
            COUNT(*) FILTER (
              WHERE feedback.feedback = 'disliked'
            )::integer AS "dislikedCount"
          FROM characters AS character
          LEFT JOIN cli_sessions AS session ON session.character_id = character.id
          LEFT JOIN turns AS turn ON turn.cli_session_id = session.id
            AND turn.status IN ('completed', 'failed', 'interrupted')
          LEFT JOIN usage_records AS usage ON usage.turn_id = turn.id
            AND usage.cost_usd >= 0
          LEFT JOIN turn_response_feedback AS feedback
            ON feedback.turn_id = turn.id
          GROUP BY character.id
          ORDER BY character.id
        "#;

#[derive(Default)]
struct CacheState {
    generation: u64,
    version: f64,
    cached: Option<Arc<Value>>,
}

/// 대화 페이지·스트리밍과 무관한 요약이라 최초 조회와 비용·평가 변경 뒤에만 다시 집계한다.
#[derive(Default)]
pub struct TurnCostSummaryCache {
    state: Mutex<CacheState>,
    refresh: tokio::sync::Mutex<()>,
}

impl TurnCostSummaryCache {
    fn state(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn observe(&self, event: &Value) {
        if event.get("type").and_then(Value::as_str) != Some("feed.changed")
            || (event.get("costChanged") != Some(&Value::Bool(true)) && event.get("feedbackChanged") != Some(&Value::Bool(true)))
        {
            return;
        }
        let mut state = self.state();
        state.generation += 1;
        state.cached = None;
    }

    pub async fn read(&self, pool: &PgPool) -> Result<Arc<Value>, sqlx::Error> {
        self.read_with(|| async {
            sqlx::query_scalar::<_, Value>(sqlx::AssertSqlSafe(format!(
                r#"SELECT row_to_json(summary) FROM ({COST_SUMMARY_SQL}) AS summary ORDER BY summary."characterId""#
            )))
            .fetch_all(pool)
            .await
        })
        .await
    }

    /// 동시에 들어온 조회는 한 번의 집계를 기다린다. 집계 도중 비용이 바뀌면 결과를 버리고 다시 집계한다.
    pub async fn read_with<F, Fut, E>(&self, mut query: F) -> Result<Arc<Value>, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Vec<Value>, E>>,
    {
        loop {
            if let Some(cached) = self.state().cached.clone() {
                return Ok(cached);
            }
            let _refreshing = self.refresh.lock().await;
            let generation = {
                let state = self.state();
                if let Some(cached) = state.cached.clone() {
                    return Ok(cached);
                }
                state.generation
            };
            let rows = query().await?;
            let mut state = self.state();
            if generation != state.generation {
                continue;
            }
            state.version = now_milliseconds().max(state.version + 1.0);
            let cached = Arc::new(json!({ "version": state.version, "characters": rows }));
            state.cached = Some(cached.clone());
            return Ok(cached);
        }
    }
}

pub const USAGE_REPORT_BACKENDS: [&str; 3] = ["codex", "claude", "antigravity"];
pub const USAGE_REPORT_GRANULARITIES: [&str; 2] = ["day", "month"];
/// 일별은 최근 30일, 월별은 최근 12개월을 본다.
pub const USAGE_REPORT_DAY_SPAN: u32 = 30;
pub const USAGE_REPORT_MONTH_SPAN: u32 = 12;

/// Node UsageReportError. 입력 오류는 400, DB 오류는 서버 공통 처리처럼 500이다.
#[derive(Debug, PartialEq)]
pub struct UsageReportError {
    pub status: u16,
    pub message: String,
}

#[derive(Debug, PartialEq)]
pub struct UsageReportOptions {
    pub backend: String,
    pub granularity: String,
    pub time_zone: String,
}

/// normalizeUsageReportOptions. 인자는 searchParams.get 값이고 None은 null이다. 서버처럼 집계 단위는 day, 시간대는 UTC가 기본이다.
pub fn normalize_usage_report_options(
    backend: Option<&str>,
    granularity: Option<&str>,
    time_zone: Option<&str>,
) -> Result<UsageReportOptions, UsageReportError> {
    let invalid = |message: String| UsageReportError { status: 400, message };
    let backend = match backend {
        Some(backend) if USAGE_REPORT_BACKENDS.contains(&backend) => backend,
        other => return Err(invalid(format!("알 수 없는 백엔드입니다. {}", other.unwrap_or("null")))),
    };
    let granularity = granularity.unwrap_or("day");
    if !USAGE_REPORT_GRANULARITIES.contains(&granularity) {
        return Err(invalid(format!("알 수 없는 집계 단위입니다. {granularity}")));
    }
    let original = time_zone.unwrap_or("UTC");
    let zone = js_trim(original);
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '/');
    if zone.is_empty() || zone.len() > 64 || !zone.chars().all(allowed) {
        return Err(invalid(format!("올바르지 않은 시간대입니다. {original}")));
    }
    Ok(UsageReportOptions { backend: backend.into(), granularity: granularity.into(), time_zone: zone.into() })
}

/// usageReportQuery. 기간 라벨은 시간대를 적용한 뒤 자른다. 일별은 YYYY-MM-DD, 월별은 YYYY-MM.
pub fn usage_report_query(granularity: &str) -> String {
    let (period_format, span) = if granularity == "day" {
        ("YYYY-MM-DD", format!("{} days", USAGE_REPORT_DAY_SPAN - 1))
    } else {
        ("YYYY-MM", format!("{} months", USAGE_REPORT_MONTH_SPAN - 1))
    };
    format!(
        r#"
    WITH local_turn AS (
      SELECT
        turn.id,
        session.character_id,
        COALESCE(turn.resolved_model, turn.model) AS model,
        turn.effort,
        date_trunc('{granularity}', turn.started_at AT TIME ZONE $2) AS bucket
      FROM turns AS turn
      LEFT JOIN cli_sessions AS session ON session.id = turn.cli_session_id
      WHERE turn.backend = $1
        AND turn.provider_kind = 'cloud'
        AND turn.started_at IS NOT NULL
        AND turn.started_at AT TIME ZONE $2
          >= date_trunc('{granularity}', now() AT TIME ZONE $2)
            - interval '{span}'
    )
    SELECT
      to_char(local_turn.bucket, '{period_format}') AS period,
      COALESCE(local_turn.character_id, '') AS "characterId",
      COALESCE(local_turn.model, '') AS model,
      COALESCE(local_turn.effort, '') AS effort,
      COUNT(*)::int AS turns,
      COALESCE(SUM(usage.cost_usd), 0)::float8 AS "costUSD",
      COALESCE(SUM(usage.input_tokens), 0)::bigint AS "inputTokens",
      COALESCE(SUM(usage.cached_input_tokens), 0)::bigint AS "cachedInputTokens",
      COALESCE(SUM(usage.output_tokens), 0)::bigint AS "outputTokens",
      COUNT(*) FILTER (WHERE feedback.feedback = 'liked')::int AS liked,
      COUNT(*) FILTER (WHERE feedback.feedback = 'disliked')::int AS disliked
    FROM local_turn
    LEFT JOIN usage_records AS usage ON usage.turn_id = local_turn.id
    LEFT JOIN turn_response_feedback AS feedback
      ON feedback.turn_id = local_turn.id
    GROUP BY 1, 2, 3, 4
    ORDER BY 1, 2, 3, 4
  "#
    )
}

/// readUsageReport. 행의 키 순서와 숫자 변환(Number)은 SELECT 열 순서와 JS 숫자 표기로 같아진다.
pub async fn read_usage_report(
    executor: impl sqlx::PgExecutor<'_>,
    backend: Option<&str>,
    granularity: Option<&str>,
    time_zone: Option<&str>,
) -> Result<Value, UsageReportError> {
    let options = normalize_usage_report_options(backend, granularity, time_zone)?;
    let sql = format!(
        r#"SELECT row_to_json(report) FROM ({}) AS report ORDER BY report.period, report."characterId", report.model, report.effort"#,
        usage_report_query(&options.granularity)
    );
    let rows: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(&options.backend)
        .bind(&options.time_zone)
        .fetch_all(executor)
        .await
        .map_err(|error| UsageReportError { status: 500, message: crate::rag::database_message(&error) })?;
    Ok(json!({
        "backend": options.backend,
        "granularity": options.granularity,
        "timeZone": options.time_zone,
        "generatedAt": iso_string(now_milliseconds()),
        "rows": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn 사용량_보고_옵션은_node와_같은_문장으로_거부한다() {
        let error = |message: &str| Err(UsageReportError { status: 400, message: message.into() });
        assert_eq!(normalize_usage_report_options(None, None, None), error("알 수 없는 백엔드입니다. null"));
        assert_eq!(normalize_usage_report_options(Some("gemini"), None, None), error("알 수 없는 백엔드입니다. gemini"));
        assert_eq!(normalize_usage_report_options(Some("codex"), Some("week"), None), error("알 수 없는 집계 단위입니다. week"));
        assert_eq!(normalize_usage_report_options(Some("codex"), Some(""), None), error("알 수 없는 집계 단위입니다. "));
        assert_eq!(normalize_usage_report_options(Some("codex"), None, Some("  ")), error("올바르지 않은 시간대입니다.   "));
        assert_eq!(normalize_usage_report_options(Some("codex"), None, Some("Asia/Seoul;")), error("올바르지 않은 시간대입니다. Asia/Seoul;"));
        assert_eq!(
            normalize_usage_report_options(Some("codex"), None, Some(&"a".repeat(65))),
            error(&format!("올바르지 않은 시간대입니다. {}", "a".repeat(65)))
        );
        assert_eq!(
            normalize_usage_report_options(Some("claude"), Some("month"), Some(" Asia/Seoul ")),
            Ok(UsageReportOptions { backend: "claude".into(), granularity: "month".into(), time_zone: "Asia/Seoul".into() })
        );
        assert_eq!(normalize_usage_report_options(Some("antigravity"), None, None).unwrap().time_zone, "UTC");
    }

    #[test]
    fn 사용량_보고_질의는_단위별_기간과_라벨을_쓴다() {
        let day = usage_report_query("day");
        assert!(day.contains("date_trunc('day', turn.started_at AT TIME ZONE $2)"));
        assert!(day.contains("interval '29 days'"));
        assert!(day.contains("'YYYY-MM-DD'"));
        let month = usage_report_query("month");
        assert!(month.contains("interval '11 months'"));
        assert!(month.contains("to_char(local_turn.bucket, 'YYYY-MM')"));
        assert!(month.contains("turn.provider_kind = 'cloud'"));
    }

    #[test]
    fn 직원_전체_비용은_페이지_모델과_무관하며_미확인_비용과_진행_중_턴을_제외한다() {
        for expected in [
            "GROUP BY character.id",
            "COUNT(usage.cost_usd)",
            "SUM(usage.cost_usd)",
            "EXTRACT(EPOCH FROM",
            "turn.ended_at > turn.started_at",
            "feedback.feedback = 'liked'",
            "feedback.feedback = 'disliked'",
            "LEFT JOIN turn_response_feedback AS feedback",
            "'completed', 'failed', 'interrupted'",
            r#"COUNT(DISTINCT turn.id)::integer AS "finishedTurnCount""#,
            "WHERE turn.status = 'failed'",
            "WHERE turn.status = 'interrupted'",
        ] {
            assert!(COST_SUMMARY_SQL.contains(expected), "{expected}");
        }
        // 장애·한도 소진·사용자 취소·재시작을 몰래 빼지 않는다.
        for forbidden in ["error_message", "NOT IN", "LIMIT", "external_id", "active_cli_sessions", "turn.model"] {
            assert!(!COST_SUMMARY_SQL.contains(forbidden), "{forbidden}");
        }
    }

    fn row(count: usize) -> Vec<Value> {
        vec![json!({ "characterId": "boss", "pricedTurnCount": count, "totalCostUsd": count * 2 })]
    }

    #[tokio::test]
    async fn 동시_조회와_스트리밍_이벤트는_재집계하지_않고_관련_변경만_반영한다() {
        let calls = AtomicUsize::new(0);
        let counter = &calls;
        let cache = TurnCostSummaryCache::default();
        let query = move || async move { Ok::<_, ()>(row(counter.fetch_add(1, Ordering::SeqCst) + 1)) };
        let snapshots = futures_join_all((0..50).map(|_| cache.read_with(query))).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for snapshot in &snapshots {
            assert!(Arc::ptr_eq(snapshot.as_ref().unwrap(), snapshots[0].as_ref().unwrap()));
        }
        for _ in 0..100 {
            cache.observe(&json!({ "type": "feed.changed", "turnId": "streaming" }));
            cache.read_with(query).await.unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cache.observe(&json!({ "type": "feed.changed", "costChanged": true }));
        let updated = cache.read_with(query).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let first = snapshots[0].as_ref().unwrap();
        assert!(updated["version"].as_f64() > first["version"].as_f64());
        assert_eq!(updated["characters"][0]["pricedTurnCount"], 2);
        cache.observe(&json!({ "type": "feed.changed", "feedbackChanged": true }));
        let feedback_updated = cache.read_with(query).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(feedback_updated["version"].as_f64() > updated["version"].as_f64());
    }

    #[tokio::test]
    async fn 집계_도중_비용이_변경되면_오래된_결과를_보내지_않고_한번_더_조회한다() {
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let cache = Arc::new(TurnCostSummaryCache::default());
        let pending = tokio::spawn({
            let (calls, release, cache) = (calls.clone(), release.clone(), cache.clone());
            async move {
                cache
                    .read_with(|| {
                        let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                        let release = release.clone();
                        async move {
                            if call == 1 {
                                release.notified().await;
                            }
                            Ok::<_, ()>(vec![json!({ "characterId": "boss", "pricedTurnCount": call, "totalCostUsd": call })])
                        }
                    })
                    .await
            }
        });
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        cache.observe(&json!({ "type": "feed.changed", "costChanged": true }));
        release.notify_one();
        let updated = pending.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(updated["characters"][0]["totalCostUsd"], 2);
    }

    /// futures 크레이트 없이 같은 작업 안에서 여러 조회를 동시에 돌린다.
    async fn futures_join_all<F: Future>(futures: impl Iterator<Item = F>) -> Vec<F::Output> {
        let mut set = Vec::new();
        for future in futures {
            set.push(Box::pin(future));
        }
        let mut outputs: Vec<Option<F::Output>> = (0..set.len()).map(|_| None).collect();
        std::future::poll_fn(|context| {
            let mut done = true;
            for (index, future) in set.iter_mut().enumerate() {
                if outputs[index].is_none() {
                    match future.as_mut().poll(context) {
                        std::task::Poll::Ready(output) => outputs[index] = Some(output),
                        std::task::Poll::Pending => done = false,
                    }
                }
            }
            if done { std::task::Poll::Ready(()) } else { std::task::Poll::Pending }
        })
        .await;
        outputs.into_iter().map(Option::unwrap).collect()
    }
}
