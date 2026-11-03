// 터미널 업무 전달: PTY에 쓰기 전 접수 잠금, 소유 앱의 확인(claim·reject), 훅이 확인한 턴 시작을 맡는다.
//
// 큐가 아니다. 바쁜 세션은 바로 거절하고, PTY 쓰기 성공은 접수가 아니며 CLI 훅·감시기만 턴을 확정한다.

use std::sync::{Arc, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::{SessionState, Shared, TerminalRuntime, TerminalSession, TerminalTurnStart};
use crate::agent_runtime::lifecycle::StartRequest;
use crate::agent_runtime::service::AgentError;
use crate::js::{js_string_or_empty, js_trim, js_truthy, now_milliseconds, random_uuid};
use crate::reply_delivery::reply_routing_prompt;

const MAX_PROMPT_BYTES: usize = 256_000;
const TIMEOUT_MESSAGE: &str = "터미널의 접수를 확인하지 못했습니다. 자동 재전송하지 않았습니다. 터미널을 확인하세요.";
const PREFLIGHT_BUSY: &str = "터미널의 현재 업무가 끝난 뒤 시작하세요.";
const CANCELLED: &str = "터미널 전달이 취소되었습니다.";
const INVALID_CONTROL: &str = "잘못된 터미널 전달 확인입니다.";

type Settlement = oneshot::Sender<Result<Value, AgentError>>;

/// state.dispatch. 첫 await 전에 걸어 DB 사전 확인 중에도 다른 전달을 막는다.
pub(crate) struct PendingDispatch {
    pub(crate) id: String,
    text: String,
    sender_character_id: Option<Value>,
    request_source: Option<String>,
    delivery_id: Option<String>,
    pub(super) wire: Option<String>,
    claimed: bool,
    pub(super) expired: bool,
    expires_at: i64,
    resolve: Option<Settlement>,
    timer: Option<JoinHandle<()>>,
}

impl PendingDispatch {
    /// clearTimeout 후 resolve·reject. 이미 정해졌으면 아무것도 하지 않는다.
    fn settle(&mut self, result: Result<Value, AgentError>) {
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
        if let Some(resolve) = self.resolve.take() {
            let _ = resolve.send(result);
        }
    }
}

/// cancelDispatch.
pub(crate) fn cancel(state: &mut SessionState) {
    if let Some(mut pending) = state.dispatch.take() {
        pending.settle(Err(AgentError::Busy(CANCELLED.into())));
    }
}

/// `^<pasted_content id="([A-Za-z0-9_-]{1,64})">\r?\n([\s\S]*)\r?\n</pasted_content id="\1">$`의 둘째 묶음.
/// Claude가 긴 붙여 넣기를 감싸는 봉투 하나만 벗긴다. 탐욕 일치라 닫는 줄 앞의 \r은 본문에 남는다.
pub(crate) fn unwrap_pasted_content(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("<pasted_content id=\"")?;
    let end = rest.find('"')?;
    let id = &rest[..end];
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
        return None;
    }
    let rest = rest[end..].strip_prefix("\">")?;
    let body = rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix('\n'))?;
    body.strip_suffix(format!("\n</pasted_content id=\"{id}\">").as_str())
}

/// 전달 본문. 첨부 경로는 절대경로 한 줄씩이고, 본문은 비지 않고 256,000바이트 이하이며 탭·줄바꿈 밖의 제어 문자가 없다.
fn dispatch_text(request: &StartRequest) -> Result<String, AgentError> {
    let invalid_attachments = || AgentError::Failed("Invalid terminal attachments".into());
    let paths: Vec<String> = match &request.attachment_paths {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(path) if path.starts_with('/') && !path.contains(['\r', '\n']) => Ok(path.clone()),
                _ => Err(invalid_attachments()),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(invalid_attachments()),
    };
    let prompt = js_string_or_empty(request.prompt.as_ref());
    let text = std::iter::once(js_trim(&prompt).to_string())
        .chain(paths)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let control = |ch: char| matches!(ch, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}');
    if text.is_empty() || text.len() > MAX_PROMPT_BYTES || text.chars().any(control) {
        return Err(AgentError::Failed("Invalid terminal prompt".into()));
    }
    Ok(text)
}

fn same_token(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |difference, (a, b)| difference | (a ^ b)) == 0
}

/// 응답 기한. 확인 전이면 잠금을 풀고, 앱이 이미 가져갔으면(PTY에 썼을 수 있음) 맞는 훅·중단·닫기까지 잠금을 남긴다.
fn spawn_timer(session: Weak<TerminalSession>, id: String, timeout: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        let Some(session) = session.upgrade() else {
            return;
        };
        let (resolve, claimed) = {
            let mut state = session.lock_state();
            let Some(pending) = state.dispatch.as_mut().filter(|pending| pending.id == id) else {
                return;
            };
            pending.expired = true;
            pending.timer = None;
            let claimed = pending.claimed;
            let resolve = pending.resolve.take();
            if !claimed {
                state.dispatch = None;
            }
            (resolve, claimed)
        };
        if let Some(resolve) = resolve {
            let error = if claimed {
                AgentError::DeliveryUncertain(TIMEOUT_MESSAGE.into())
            } else {
                AgentError::Busy(TIMEOUT_MESSAGE.into())
            };
            let _ = resolve.send(Err(error));
        }
    })
}

/// DB 사전 확인 뒤 앱에 전달 알림을 보낸다. 실패하면 이 전달만 거절한다.
async fn preflight<R: TerminalRuntime>(shared: Arc<Shared<R>>, session: Arc<TerminalSession>, id: String, text: String) {
    let result = async {
        if shared.runtime.terminal_dispatch_busy(&session.character_id).await? {
            return Err(AgentError::Busy(PREFLIGHT_BUSY.into()));
        }
        let tags = shared.runtime.reply_route_tags(&session.character_id).await?;
        {
            let mut guard = session.lock_state();
            let state = &mut *guard;
            let ready = !state.closed && !state.closing && state.running_turn_id.is_none();
            let Some(pending) = state.dispatch.as_mut().filter(|pending| ready && pending.id == id && !pending.expired) else {
                return Err(AgentError::Busy(PREFLIGHT_BUSY.into()));
            };
            pending.wire = Some(format!(
                "[OFFICESTRA_REQUEST:{id}]\n{}",
                reply_routing_prompt(&text, &session.character_id, &tags)
            ));
        }
        shared.runtime.broadcast(json!({
            "type": "terminal.dispatch",
            "characterId": session.character_id,
            "dispatchId": id,
            "terminalSessionId": session.terminal_session_id,
        }));
        Ok(())
    }
    .await;
    if let Err(error) = result {
        let mut state = session.lock_state();
        if state.dispatch.as_ref().is_some_and(|pending| pending.id == id)
            && let Some(mut pending) = state.dispatch.take()
        {
            pending.settle(Err(error));
        }
    }
}

impl<R: TerminalRuntime> Shared<R> {
    /// dispatch. 앱이 PTY에 쓸 전달을 걸고, 훅이 같은 본문으로 턴을 시작할 때까지 기다린다.
    pub(super) async fn dispatch(self: Arc<Self>, request: StartRequest) -> Result<Value, AgentError> {
        let id = request.character_id.clone();
        let session = self.session(&id);
        if self.runtime.is_draining() {
            return Err(AgentError::Draining("백엔드가 재시작 준비 중입니다.".into()));
        }
        let opening = self.registry().opening.contains(&id);
        let busy = self.runtime.character_busy(&id);
        let Some(session) = session else {
            return Err(AgentError::Busy("터미널이 아직 준비되지 않았습니다.".into()));
        };
        let (receiver, pending_id, text) = {
            let mut state = session.lock_state();
            if state.closed || state.closing || opening {
                return Err(AgentError::Busy("터미널이 아직 준비되지 않았습니다.".into()));
            }
            if state.running_turn_id.is_some() || state.dispatch.is_some() || busy {
                return Err(AgentError::Busy("이 직원의 현재 업무가 끝난 뒤 새 업무를 시작하세요.".into()));
            }
            if let Some(conversation) = request.conversation_id.as_ref().filter(|value| js_truthy(Some(value)))
                && conversation.as_str() != Some(session.conversation_id.as_str())
            {
                return Err(AgentError::Busy("터미널의 현재 대화와 일치하지 않습니다.".into()));
            }
            let text = dispatch_text(&request)?;
            let (resolve, receiver) = oneshot::channel();
            let pending_id = random_uuid();
            let timeout = self.options.dispatch_timeout;
            let timer = spawn_timer(Arc::downgrade(&session), pending_id.clone(), timeout);
            state.dispatch = Some(PendingDispatch {
                id: pending_id.clone(),
                text: text.clone(),
                sender_character_id: request.sender_character_id.clone(),
                request_source: request.request_source.clone(),
                delivery_id: request.delivery_id.clone(),
                wire: None,
                claimed: false,
                expired: false,
                expires_at: now_milliseconds() as i64 + timeout.as_millis() as i64,
                resolve: Some(resolve),
                timer: Some(timer),
            });
            (receiver, pending_id, text)
        };
        tokio::spawn(preflight(Arc::clone(&self), session, pending_id, text));
        receiver.await.unwrap_or_else(|_| Err(AgentError::Busy(CANCELLED.into())))
    }

    /// dispatchControl. 소유 앱만 전달을 가져가거나(claim) PTY에 쓰지 않았다고(reject) 알릴 수 있다.
    pub(super) fn dispatch_control(&self, character_id: &str, body: &Value) -> Result<Value, AgentError> {
        if !body.is_object() {
            return Err(AgentError::Busy(INVALID_CONTROL.into()));
        }
        let compacting = self.runtime.is_compacting(character_id);
        let mismatch = || AgentError::Busy("터미널 소유 세션이 일치하지 않습니다.".into());
        let Some(session) = self.session(character_id) else {
            return Err(mismatch());
        };
        let token = js_string_or_empty(body.get("ownerToken"));
        let mut guard = session.lock_state();
        let state = &mut *guard;
        if state.closed
            || state.closing
            || body.get("terminalSessionId").and_then(Value::as_str) != Some(session.terminal_session_id.as_str())
            || token.is_empty()
            || !same_token(token.as_bytes(), session.owner_token.as_bytes())
        {
            return Err(mismatch());
        }
        let dispatch_id = body.get("dispatchId").and_then(Value::as_str);
        let Some(pending) = state.dispatch.as_mut().filter(|pending| dispatch_id == Some(pending.id.as_str())) else {
            return Err(AgentError::Busy("만료되었거나 이미 처리한 터미널 요청입니다.".into()));
        };
        let action = body.get("action").and_then(Value::as_str);
        if action == Some("claim") {
            if pending.wire.is_none() || pending.expired || pending.claimed || state.running_turn_id.is_some() || compacting {
                return Err(AgentError::Busy("터미널이 사용 중입니다.".into()));
            }
            pending.claimed = true;
            return Ok(json!({ "prompt": pending.wire, "expiresAt": pending.expires_at }));
        }
        if action == Some("reject") && pending.claimed {
            // 소유 앱만 PTY에 쓰지 않았다고 확인할 수 있다. 기한이 지난 뒤에도 받는다.
            if let Some(mut pending) = state.dispatch.take() {
                pending.settle(Err(AgentError::Busy("터미널 입력 중이거나 입력창이 준비되지 않았습니다.".into())));
            }
            return Ok(json!({ "accepted": false }));
        }
        Err(AgentError::Busy(INVALID_CONTROL.into()))
    }

    /// beginDispatchedTurn. 훅이 본 질문이 가져간 전달 본문과 같을 때만 그 전달의 발신자·출처·전달 ID로 턴을 연다.
    /// Node와 달리 기다리는 사이 전달이 바뀌었으면 새 전달을 지우지 않는다.
    pub(crate) async fn begin_dispatched_turn(
        &self,
        session: &TerminalSession,
        prompt: String,
        started_at: Option<f64>,
    ) -> Result<String, AgentError> {
        let trimmed = js_trim(&prompt);
        let dispatched = if session.backend() == "claude" { unwrap_pasted_content(trimmed).unwrap_or(trimmed) } else { trimmed };
        let matched = session
            .lock_state()
            .dispatch
            .as_ref()
            .filter(|pending| pending.claimed && pending.wire.as_deref() == Some(dispatched))
            .map(|pending| {
                (
                    pending.id.clone(),
                    pending.text.clone(),
                    pending.sender_character_id.clone(),
                    pending.request_source.clone(),
                    pending.delivery_id.clone(),
                )
            });
        let mut start = TerminalTurnStart {
            character_id: session.character_id.clone(),
            session_id: session.session_id.clone(),
            prompt: prompt.clone(),
            started_at,
            execution: session.character.clone(),
            sender_character_id: None,
            request_source: None,
            delivery_id: None,
        };
        if let Some((_, text, sender, source, delivery)) = &matched {
            start.prompt = text.clone();
            start.sender_character_id = sender.clone();
            start.request_source = source.clone();
            start.delivery_id = delivery.clone();
        }
        let turn_id = self.runtime.begin_terminal_turn(start).await?;
        let mut state = session.lock_state();
        state.running_turn_id = Some(turn_id.clone());
        if let Some((id, ..)) = matched
            && state.dispatch.as_ref().is_some_and(|pending| pending.id == id)
            && let Some(mut pending) = state.dispatch.take()
        {
            pending.settle(Ok(json!({
                "turnId": turn_id,
                "conversationId": session.conversation_id,
                "status": "running",
            })));
        }
        Ok(turn_id)
    }
}
