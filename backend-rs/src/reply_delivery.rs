// 직원 답변 자동 전달. 전달 안내·예약 기록, 전달 태그 조회·저장·예약 취소와 주기적 전달 서비스를 맡는다. Node reply-delivery.mjs를 옮겼다.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row};
use tokio::task::JoinHandle;

use crate::agent_runtime::lifecycle::StartRequest;
use crate::agent_runtime::service::{AgentError, Broadcast};
use crate::health::Runtime;
use crate::js::{js_trim, js_truthy};
use crate::rag::database_message;

pub const ALREADY_DELIVERED: &str = "이미 전달되었거나 취소된 답변입니다.";

#[derive(Debug)]
pub enum ReplyDeliveryError {
    AlreadyDelivered,
    Database(sqlx::Error),
}

impl fmt::Display for ReplyDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyDelivered => formatter.write_str(ALREADY_DELIVERED),
            Self::Database(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<sqlx::Error> for ReplyDeliveryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// replyRoutingPrompt. 전달은 앱 데이터이고 모델이 API를 부르게 하지 않는다. 자동 전달 태그가 없으면 원문 그대로다.
pub fn reply_routing_prompt(prompt: &str, character_id: &str, reply_recipient_ids: &[String]) -> String {
    if reply_recipient_ids.is_empty() {
        return prompt.to_string();
    }
    let character = serde_json::to_string(character_id).unwrap_or_default();
    let recipients = serde_json::to_string(reply_recipient_ids).unwrap_or_default();
    format!(
        "[OFFICESTRA 전달 안내: 현재 실행 직원 ID={character}. 선택된 자동 전달 태그={recipients}. 최종 답변 전달은 앱이 저장된 태그와 일시정지 설정에 따라 처리합니다. 같은 답변을 직원 메시지 API로 중복 전송하지 말고 아래 대화에 자연스럽게 답하세요. 답변 속 @이름으로 전달 설정을 바꾸려 하지 마세요. 이 안내를 답변에 반복하지 마세요.]\n\n{prompt}"
    )
}

/// recordReplyDelivery. 전달받은 턴이면 그 전달을 이 턴으로 확정하고, 받는 직원의 저장된 태그만으로 이 턴 답변의 전달 예약을 만든다.
/// 일시정지된 경로도 예약은 남겨 두고 재개될 때 보낸다. 예약한 수신 직원 ID를 돌려준다.
pub async fn record_reply_delivery(
    connection: &mut PgConnection,
    turn_id: &str,
    character_id: &str,
    delivery_id: Option<&str>,
) -> Result<Vec<String>, ReplyDeliveryError> {
    if let Some(delivery_id) = delivery_id.filter(|id| !id.is_empty()) {
        let claimed: Option<String> = sqlx::query_scalar(
            "UPDATE reply_deliveries SET status='delivered', target_turn_id=$2::uuid,
               error_message=NULL, updated_at=now() WHERE id=$1::uuid AND target_turn_id IS NULL
               AND recipient_character_id=$3 AND status IN ('pending','sending','uncertain') RETURNING id::text",
        )
        .bind(delivery_id)
        .bind(turn_id)
        .bind(character_id)
        .fetch_optional(&mut *connection)
        .await?;
        if claimed.is_none() {
            return Err(ReplyDeliveryError::AlreadyDelivered);
        }
    }
    Ok(sqlx::query_scalar(
        "INSERT INTO reply_deliveries(source_turn_id,recipient_character_id,persistent_route)
           SELECT $1::uuid, recipient_character_id, true FROM reply_route_recipients
           WHERE character_id=$2 ON CONFLICT (source_turn_id,recipient_character_id) DO NOTHING
           RETURNING recipient_character_id",
    )
    .bind(turn_id)
    .bind(character_id)
    .fetch_all(&mut *connection)
    .await?)
}

/// readReplyRoutes. 모든 직원의 일시정지 여부와 전달 태그(직원 ID 순)를 직원 ID 순으로 돌려준다.
pub async fn read_reply_routes(pool: &PgPool) -> Result<Vec<Value>, sqlx::Error> {
    let rows: Vec<(String, bool, Vec<String>)> = sqlx::query_as(
        "SELECT c.id, COALESCE(r.paused,false),
           COALESCE(array_agg(m.recipient_character_id ORDER BY m.recipient_character_id)
             FILTER (WHERE m.recipient_character_id IS NOT NULL), ARRAY[]::text[])
         FROM characters c LEFT JOIN reply_routes r ON r.character_id=c.id
         LEFT JOIN reply_route_recipients m ON m.character_id=c.id GROUP BY c.id,r.paused ORDER BY c.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(character_id, paused, recipient_ids)| {
            json!({ "characterId": character_id, "paused": paused, "recipientIds": recipient_ids })
        })
        .collect())
}

/// 전달 태그 저장 실패. Invalid는 Node의 TypeError라 400이다.
#[derive(Debug)]
pub enum RouteSaveError {
    Invalid(String),
    Database(sqlx::Error),
}

impl fmt::Display for RouteSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Database(error) => formatter.write_str(&database_message(error)),
        }
    }
}

impl From<sqlx::Error> for RouteSaveError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

const INVALID_ROUTE: &str = "다른 직원을 선택하고 일시정지 상태를 지정하세요.";

/// saveReplyRoute. 호출한 쪽이 트랜잭션을 잡아 태그 교체와 해제된 예약 취소가 전달 서비스에 한 번에 보이게 한다.
/// `body`는 요청 본문 그대로다. null이면 Node의 구조 분해 TypeError와 같은 문구다.
pub async fn save_reply_route(
    connection: &mut PgConnection,
    character_id: &str,
    body: &Value,
) -> Result<Value, RouteSaveError> {
    if body.is_null() {
        return Err(RouteSaveError::Invalid(
            "Cannot destructure property 'recipientIds' of 'object null' as it is null.".into(),
        ));
    }
    let field = |key: &str| body.as_object().and_then(|fields| fields.get(key));
    let (Some(Value::Array(requested)), Some(Value::Bool(paused))) = (field("recipientIds"), field("paused")) else {
        return Err(RouteSaveError::Invalid(INVALID_ROUTE.into()));
    };
    let mut ids: Vec<String> = Vec::with_capacity(requested.len());
    let mut seen = HashSet::new();
    for id in requested {
        let Value::String(id) = id else {
            return Err(RouteSaveError::Invalid(INVALID_ROUTE.into()));
        };
        if id == character_id {
            return Err(RouteSaveError::Invalid(INVALID_ROUTE.into()));
        }
        if seen.insert(id.as_str()) {
            ids.push(id.clone());
        }
    }
    let lookup: Vec<String> = std::iter::once(character_id.to_string()).chain(ids.iter().cloned()).collect();
    let found = sqlx::query("SELECT id FROM characters WHERE id=ANY($1::text[])")
        .bind(&lookup)
        .fetch_all(&mut *connection)
        .await?
        .len();
    if found != ids.len() + 1 {
        return Err(RouteSaveError::Invalid("전달 설정의 직원을 찾을 수 없습니다.".into()));
    }
    sqlx::query(
        "INSERT INTO reply_routes(character_id,paused) VALUES ($1,$2)
         ON CONFLICT(character_id) DO UPDATE SET paused=$2,updated_at=now()",
    )
    .bind(character_id)
    .bind(paused)
    .execute(&mut *connection)
    .await?;
    sqlx::query("DELETE FROM reply_route_recipients WHERE character_id=$1")
        .bind(character_id)
        .execute(&mut *connection)
        .await?;
    sqlx::query("INSERT INTO reply_route_recipients(character_id,recipient_character_id) SELECT $1,unnest($2::text[])")
        .bind(character_id)
        .bind(&ids)
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "UPDATE reply_deliveries d SET status='cancelled',updated_at=now(),
           error_message='전달 대상에서 해제되었습니다.' FROM turns t JOIN cli_sessions s ON s.id=t.cli_session_id
         WHERE d.source_turn_id=t.id AND s.character_id=$1 AND d.status='pending'
           AND NOT(d.recipient_character_id=ANY($2::text[]))",
    )
    .bind(character_id)
    .bind(&ids)
    .execute(&mut *connection)
    .await?;
    Ok(json!({ "characterId": character_id, "recipientIds": ids, "paused": paused }))
}

/// cancelPendingReplyDelivery. 아직 보내지 않은 이 턴의 예약만 취소하고, 취소한 예약이 있었는지 돌려준다.
pub async fn cancel_pending_reply_delivery(pool: &PgPool, source_turn_id: &str) -> Result<bool, sqlx::Error> {
    let cancelled = sqlx::query(
        "UPDATE reply_deliveries SET status='cancelled',error_message='사용자가 전달 예약을 취소했습니다.',updated_at=now()
         WHERE source_turn_id=$1::uuid AND status='pending' AND target_turn_id IS NULL RETURNING id",
    )
    .bind(source_turn_id)
    .fetch_all(pool)
    .await?;
    Ok(!cancelled.is_empty())
}

/// 전달 서비스가 받는 직원에게 넘기는 시작 요청.
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryStart {
    pub character_id: String,
    pub prompt: String,
    pub conversation_id: Option<String>,
    pub sender_character_id: String,
    pub delivery_id: String,
}

/// 전달 서비스가 쓰는 실행기 기능. 운영은 Runtime이고, 시험은 가짜 실행기를 넣는다.
pub trait DeliveryRuntime: Send + Sync + 'static {
    fn draining(&self) -> bool;
    /// messageAvailability(). 직원마다 characterId·canReceive가 있는 목록이다.
    fn message_availability(&self) -> impl Future<Output = Result<Vec<Value>, AgentError>> + Send;
    fn start_delivery(&self, start: DeliveryStart) -> impl Future<Output = Result<(), AgentError>> + Send;
}

impl DeliveryRuntime for Arc<Runtime> {
    fn draining(&self) -> bool {
        self.is_draining()
    }

    async fn message_availability(&self) -> Result<Vec<Value>, AgentError> {
        Runtime::message_availability(self, None).await
    }

    async fn start_delivery(&self, start: DeliveryStart) -> Result<(), AgentError> {
        self.start(StartRequest {
            character_id: start.character_id,
            prompt: Some(Value::String(start.prompt)),
            conversation_id: start.conversation_id.map(Value::String),
            sender_character_id: Some(Value::String(start.sender_character_id)),
            delivery_id: Some(start.delivery_id),
            ..Default::default()
        })
        .await
        .map(|_| ())
    }
}

const DEFAULT_INTERVAL: Duration = Duration::from_millis(1500);

type Inflight = Shared<BoxFuture<'static, Result<(), String>>>;

struct DeliveryState<R> {
    pool: PgPool,
    runtime: R,
    broadcast: Broadcast,
    closed: AtomicBool,
    inflight: Mutex<Option<Inflight>>,
}

/// 전달 대기 행. DISTINCT ON으로 받는 직원마다 가장 오래된 예약 하나다.
struct PendingDelivery {
    id: String,
    source_turn_id: String,
    recipient_character_id: String,
    source_status: String,
    needs_input: bool,
    sender_id: String,
    response: Option<String>,
    conversation_id: Option<String>,
}

/// ReplyDeliveryService. 1.5초마다 전달 대기 예약을 받는 직원에게 시작시키고, 여러 백엔드가 떠도 권고 잠금으로 한 곳만 보낸다.
pub struct ReplyDeliveryService<R: DeliveryRuntime> {
    state: Arc<DeliveryState<R>>,
    interval: Duration,
    timer: Mutex<Option<JoinHandle<()>>>,
}

impl<R: DeliveryRuntime> ReplyDeliveryService<R> {
    pub fn new(pool: PgPool, runtime: R, broadcast: Broadcast) -> Self {
        Self::with_interval(pool, runtime, broadcast, DEFAULT_INTERVAL)
    }

    pub fn with_interval(pool: PgPool, runtime: R, broadcast: Broadcast, interval: Duration) -> Self {
        Self {
            state: Arc::new(DeliveryState {
                pool,
                runtime,
                broadcast,
                closed: AtomicBool::new(false),
                inflight: Mutex::new(None),
            }),
            interval,
            timer: Mutex::new(None),
        }
    }

    /// start(). setInterval처럼 확인 결과를 기다리지 않고 주기마다 tick을 띄우며, 실패는 경고만 남긴다.
    pub fn start(&self) {
        let state = Arc::clone(&self.state);
        let interval = self.interval;
        let timer = tokio::spawn(async move {
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticks.tick().await;
                let tick = tick(&state);
                tokio::spawn(async move {
                    if let Err(message) = tick.await {
                        eprintln!("답변 전달 확인 실패: {message}");
                    }
                });
            }
        });
        if let Some(previous) = self.timer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).replace(timer) {
            previous.abort();
        }
    }

    /// stop(). 새 확인을 막고 진행 중인 확인이 끝날 때까지 기다린다.
    pub async fn stop(&self) -> Result<(), String> {
        self.state.closed.store(true, Ordering::SeqCst);
        if let Some(timer) = self.timer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
            timer.abort();
        }
        let inflight = self.state.inflight.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        match inflight {
            Some(inflight) => inflight.await,
            None => Ok(()),
        }
    }

    /// tick(). 닫혔거나 드레인 중이면 건너뛰고, 확인이 진행 중이면 그 결과를 함께 기다린다.
    pub async fn tick(&self) -> Result<(), String> {
        tick(&self.state).await
    }
}

fn tick<R: DeliveryRuntime>(state: &Arc<DeliveryState<R>>) -> BoxFuture<'static, Result<(), String>> {
    if state.closed.load(Ordering::SeqCst) || state.runtime.draining() {
        return async { Ok(()) }.boxed();
    }
    let mut inflight = state.inflight.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = inflight.as_ref() {
        return existing.clone().boxed();
    }
    let running = Arc::clone(state);
    // 진행 표시는 확인을 띄운 쪽이 기다리지 않아도 끝나는 즉시 지운다(Node의 finally). 잠금을 쥔 채 띄워 지우기가 먼저 일어날 수 없다.
    let task = tokio::spawn(async move {
        let result = running.drain().await;
        *running.inflight.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        result
    });
    let shared = async move { task.await.unwrap_or_else(|join_error| Err(join_error.to_string())) }.boxed().shared();
    *inflight = Some(shared.clone());
    shared.boxed()
}

impl<R: DeliveryRuntime> DeliveryState<R> {
    fn closing(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.runtime.draining()
    }

    fn changed(&self, turn_id: &str, character_id: &str) {
        (self.broadcast)(json!({ "type": "feed.changed", "turnId": turn_id, "characterId": character_id }));
    }

    /// drain(). 전용 연결에서 권고 잠금을 잡은 경우에만 보내고, 끝나면 잠금을 푼다.
    async fn drain(&self) -> Result<(), String> {
        let mut connection = self.pool.acquire().await.map_err(|error| database_message(&error))?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext('officestra:reply-delivery')) AS locked")
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| database_message(&error))?;
        if !locked {
            return Ok(());
        }
        let delivered = self.deliver(&mut connection).await.map_err(|error| match error {
            DeliverError::Database(error) => database_message(&error),
            DeliverError::Runtime(error) => error.to_string(),
        });
        let unlocked =
            sqlx::query("SELECT pg_advisory_unlock(hashtext('officestra:reply-delivery'))").execute(&mut *connection).await;
        match unlocked {
            Ok(_) => delivered,
            Err(error) => {
                // 잠금을 못 풀었으면 세션 잠금이 남은 연결을 풀에 돌려주지 않는다.
                connection.close_on_drop();
                Err(database_message(&error))
            }
        }
    }

    async fn deliver(&self, connection: &mut PgConnection) -> Result<(), DeliverError> {
        // PTY 결과를 모르는 끊긴 전송은 다시 보내지 않는다. 늦은 종료 훅이 uncertain 전송을 확정할 수 있다.
        sqlx::query(
            "UPDATE reply_deliveries SET status='uncertain',error_message='전달 도중 연결이 종료되어 접수 여부를 확인해야 합니다. 중복 방지를 위해 자동 재전송하지 않았습니다.',updated_at=now() WHERE status='sending' AND target_turn_id IS NULL",
        )
        .execute(&mut *connection)
        .await?;
        // 태그가 유일한 권한이 되기 전의 일회성 예약도 포함한다. 나중에 태그를 켜도 되살리지 않는다.
        let cancelled = sqlx::query(
            "UPDATE reply_deliveries d SET status='cancelled',
               error_message='선택된 자동 전달 태그가 없어 취소되었습니다.',updated_at=now()
             FROM turns t JOIN cli_sessions s ON s.id=t.cli_session_id
             WHERE d.source_turn_id=t.id AND d.status='pending' AND d.target_turn_id IS NULL
             AND NOT EXISTS (SELECT 1 FROM reply_route_recipients m
               WHERE m.character_id=s.character_id AND m.recipient_character_id=d.recipient_character_id)
             RETURNING d.source_turn_id::text AS source_turn_id,s.character_id AS sender_id",
        )
        .fetch_all(&mut *connection)
        .await?;
        for row in &cancelled {
            self.changed(&row.try_get::<String, _>("source_turn_id")?, &row.try_get::<String, _>("sender_id")?);
        }
        let rows = sqlx::query(
            "SELECT DISTINCT ON (delivery.recipient_character_id) delivery.id::text AS id,
               delivery.source_turn_id::text AS source_turn_id, delivery.recipient_character_id,
               source.status AS source_status, source.needs_input,
               session.character_id AS sender_id,
               (SELECT text FROM messages WHERE turn_id=source.id AND role='assistant' ORDER BY received_at DESC LIMIT 1) AS response,
               (SELECT current_session.conversation_id::text FROM active_cli_sessions active
                 JOIN cli_sessions current_session ON current_session.id=active.cli_session_id
                 WHERE active.character_id=delivery.recipient_character_id AND current_session.ended_at IS NULL) AS conversation_id
             FROM reply_deliveries delivery JOIN turns source ON source.id=delivery.source_turn_id
             JOIN cli_sessions session ON session.id=source.cli_session_id
             WHERE delivery.status='pending' AND source.status NOT IN ('pending','running')
             AND EXISTS (
               SELECT 1 FROM reply_routes r JOIN reply_route_recipients m USING(character_id)
               WHERE r.character_id=session.character_id AND NOT r.paused
                 AND m.recipient_character_id=delivery.recipient_character_id)
             ORDER BY delivery.recipient_character_id,delivery.created_at,delivery.id",
        )
        .fetch_all(&mut *connection)
        .await?;
        for row in rows {
            let row = PendingDelivery {
                id: row.try_get("id")?,
                source_turn_id: row.try_get("source_turn_id")?,
                recipient_character_id: row.try_get("recipient_character_id")?,
                source_status: row.try_get("source_status")?,
                needs_input: row.try_get("needs_input")?,
                sender_id: row.try_get("sender_id")?,
                response: row.try_get("response")?,
                conversation_id: row.try_get("conversation_id")?,
            };
            if self.closing() {
                break;
            }
            let response = row.response.as_deref().map(js_trim).unwrap_or("");
            if row.source_status != "completed" || row.needs_input || response.is_empty() {
                let message = if row.needs_input {
                    "사용자 확인이 필요한 답변은 자동 전달하지 않습니다."
                } else {
                    "완료된 답변이 없어 전달하지 않았습니다."
                };
                self.update(connection, &row, "cancelled", Some(message)).await?;
                continue;
            }
            // DB 완료가 프로세스·자원 정리보다 먼저라, GPU를 함께 쓰는 보낸 직원이 자리를 비운 뒤에 받는 직원을 시작한다.
            let statuses = self.runtime.message_availability().await.map_err(DeliverError::Runtime)?;
            let can_receive = |character_id: &str| {
                statuses
                    .iter()
                    .find(|status| status.get("characterId").and_then(Value::as_str) == Some(character_id))
                    .is_some_and(|status| js_truthy(status.get("canReceive")))
            };
            if !can_receive(&row.sender_id) || !can_receive(&row.recipient_character_id) {
                continue;
            }
            if !self.update(connection, &row, "sending", None).await? {
                continue;
            }
            let started = self
                .runtime
                .start_delivery(DeliveryStart {
                    character_id: row.recipient_character_id.clone(),
                    prompt: row.response.clone().unwrap_or_default(),
                    conversation_id: row.conversation_id.clone(),
                    sender_character_id: row.sender_id.clone(),
                    delivery_id: row.id.clone(),
                })
                .await;
            if let Err(start_error) = started {
                // 터미널에 이미 쓰였을 수 있으면 중복을 막으려고 다시 보내지 않는다(uncertain).
                // 바쁘거나 드레인 중이면 예약을 그대로 두고 다음 주기에 다시 보낸다.
                if let AgentError::DeliveryUncertain(message) = &start_error {
                    self.update(connection, &row, "uncertain", Some(message)).await?;
                } else if matches!(start_error, AgentError::Busy(_) | AgentError::Draining(_)) {
                    self.update(connection, &row, "pending", None).await?;
                } else {
                    self.update(connection, &row, "failed", Some(&start_error.to_string())).await?;
                }
            }
            self.changed(&row.source_turn_id, &row.sender_id);
        }
        Ok(())
    }

    /// update(). 아직 보내지 않은 예약만 바꾸고, sending은 경로가 살아 있고 일시정지가 아닐 때만 허용한다.
    async fn update(
        &self,
        connection: &mut PgConnection,
        row: &PendingDelivery,
        status: &str,
        message: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let updated = sqlx::query(
            "UPDATE reply_deliveries SET status=$2,error_message=$3,updated_at=now()
             WHERE id=$1::uuid AND status IN ('pending','sending') AND target_turn_id IS NULL
             AND ($2<>'sending' OR EXISTS (
               SELECT 1 FROM reply_routes r JOIN reply_route_recipients m USING(character_id)
               JOIN cli_sessions s ON s.character_id=r.character_id JOIN turns t ON t.cli_session_id=s.id
               WHERE t.id=reply_deliveries.source_turn_id AND NOT r.paused
                 AND m.recipient_character_id=reply_deliveries.recipient_character_id)) RETURNING id",
        )
        .bind(&row.id)
        .bind(status)
        .bind(message)
        .fetch_all(&mut *connection)
        .await?;
        if !updated.is_empty() {
            self.changed(&row.source_turn_id, &row.sender_id);
        }
        Ok(!updated.is_empty())
    }
}

enum DeliverError {
    Database(sqlx::Error),
    Runtime(AgentError),
}

impl From<sqlx::Error> for DeliverError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 전달_안내는_실행용이고_모델이_전달하지_않게_한다() {
        assert_eq!(reply_routing_prompt("안녕", "left-woman", &[]), "안녕");
        let routed = reply_routing_prompt("안녕", "left-woman", &["right-woman".into(), "boss".into()]);
        assert!(routed.starts_with("[OFFICESTRA 전달 안내: 현재 실행 직원 ID=\"left-woman\". "));
        assert!(routed.contains("선택된 자동 전달 태그=[\"right-woman\",\"boss\"]"));
        assert!(routed.contains("중복 전송하지 말고"));
        assert!(routed.ends_with("]\n\n안녕"));
    }
}
