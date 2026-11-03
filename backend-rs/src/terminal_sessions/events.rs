// 터미널 CLI 훅 처리: Claude UserPromptSubmit·Stop·상태줄 비용과 Codex notify 완료를 턴 기록으로 옮긴다.

use std::sync::Arc;

use serde_json::{Value, json};

use super::launch::{date_from_epoch, last_claude_assistant_message, nonnegative_cost};
use super::progress::ProgressWatcher;
use super::{ClaudeTranscript, Shared, TerminalRuntime, TerminalSession, TerminalTurnCompletion, TurnContext};
use crate::agent_runtime::claude_session::find_claude_session_path;
use crate::agent_runtime::service::AgentError;
use crate::codex_rollout_turns::GateDecision;
use crate::js::{iso_string, js_string_or_empty, js_trim, now_milliseconds};
use crate::structured_turn_result::consume_structured_turn_result;
use crate::terminal_turn_activities::{
    ClaudeTerminalTurn, ClaudeTurnRead, WAIT_POLL_MS, WAIT_TIMEOUT_MS, wait_for_claude_terminal_turn,
};

fn text_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str).filter(|text| !text.is_empty())
}

/// UserPromptSubmit 때의 원본 크기·inode. 파일이 아직 없으면 0부터, 다른 오류면 모름(None)이다.
fn transcript_start(path: Option<&str>) -> (Option<u64>, Option<u64>) {
    let Some(path) = path else {
        return (None, None);
    };
    match std::fs::metadata(path) {
        Ok(metadata) => (Some(metadata.len()), Some(std::os::unix::fs::MetadataExt::ino(&metadata))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Some(0), None),
        Err(_) => (None, None),
    }
}

impl<R: TerminalRuntime> Shared<R> {
    /// handleClaudeEvent. 상태줄 payload에는 hook_event_name이 없고 cost 객체가 있다.
    pub(super) async fn claude_event(self: &Arc<Self>, session: &Arc<TerminalSession>, payload: &Value) -> Result<Value, AgentError> {
        let hook = js_string_or_empty(payload.get("hook_event_name"));
        let event = if !hook.is_empty() {
            hook
        } else if payload.get("cost").is_some_and(|cost| cost.is_object() || cost.is_array()) {
            "StatusLine".into()
        } else {
            String::new()
        };
        self.bind_if_needed(session, &js_string_or_empty(payload.get("session_id"))).await?;
        match event.as_str() {
            "StatusLine" => self.record_claude_cost(session, payload.get("cost")).await,
            "UserPromptSubmit" => self.claude_prompt_submitted(session, payload).await,
            "Stop" if session.lock_state().running_turn_id.is_some() => self.claude_stopped(session, payload).await,
            _ => Ok(json!({ "accepted": false, "reason": "ignored-hook" })),
        }
    }

    async fn claude_prompt_submitted(self: &Arc<Self>, session: &Arc<TerminalSession>, payload: &Value) -> Result<Value, AgentError> {
        session.stop_progress().await;
        // Esc로 중단된 턴은 Stop 훅이 오지 않아 running으로 남는다. 새 질문은 이전 턴이 끝났다는 뜻이다.
        let running = session.lock_state().running_turn_id.clone();
        if let Some(running) = running {
            self.runtime.interrupt_terminal_turn(&session.character_id, &running).await?;
            session.lock_state().running_turn_id = None;
            self.reset(session)?;
        }
        let external = session.lock_state().external_session_id.clone();
        let path = text_field(payload, "transcript_path").map(str::to_string).or_else(|| {
            external.as_deref().and_then(|external| find_claude_session_path(&self.options.roots.claude_projects(), external))
        });
        let (offset, inode) = transcript_start(path.as_deref());
        session.lock_state().claude_transcript =
            Some(ClaudeTranscript { path, offset, inode, started_at: iso_string(now_milliseconds()) });
        let turn_id = self.begin_dispatched_turn(session, js_string_or_empty(payload.get("prompt")), None).await?;
        session.set_progress(ProgressWatcher::start(self, session));
        self.changed(session);
        Ok(json!({ "accepted": true, "turnId": turn_id }))
    }

    async fn claude_stopped(self: &Arc<Self>, session: &Arc<TerminalSession>, payload: &Value) -> Result<Value, AgentError> {
        let Some(turn_id) = session.lock_state().running_turn_id.clone() else {
            return Ok(json!({ "accepted": false, "reason": "ignored-hook" }));
        };
        session.stop_progress().await;
        let reported = js_trim(&js_string_or_empty(payload.get("last_assistant_message"))).to_string();
        let response = if reported.is_empty() {
            last_claude_assistant_message(text_field(payload, "transcript_path"))
        } else {
            reported
        };
        let ended_at = now_milliseconds();
        let (source, external) = {
            let state = session.lock_state();
            (state.claude_transcript.clone(), state.external_session_id.clone())
        };
        // 시작 훅이 지목한 같은 원본만 읽는다. 경로가 다르면 과거 세션을 섞지 않는다.
        let transcript_path =
            text_field(payload, "transcript_path").map(str::to_string).or_else(|| source.as_ref().and_then(|source| source.path.clone()));
        let transcript = match (&source, &transcript_path) {
            (Some(source), Some(path)) if source.path.as_ref().is_none_or(|start| start == path) => {
                let options = ClaudeTurnRead {
                    offset: source.offset,
                    session_id: external,
                    workdir: Some(session.workdir.clone()),
                    final_response: Some(response.clone()),
                    started_at: Some(source.started_at.clone()),
                    ended_at: Some(iso_string(ended_at)),
                    require_timestamps: false,
                    inode: source.inode,
                };
                wait_for_claude_terminal_turn(Some(path.clone()), options, WAIT_TIMEOUT_MS, WAIT_POLL_MS).await
            }
            _ => ClaudeTerminalTurn::default(),
        };
        if transcript_path.is_some() && !transcript.final_found {
            eprintln!("터미널 원본 최종 기록 확인 지연: 사용량/활동이 일부 누락될 수 있습니다.");
        }
        let path = session.lock_state().structured_result_path.take();
        let structured = consume_structured_turn_result(path.as_deref());
        let (images, reported_cost) = {
            let mut state = session.lock_state();
            let images = state.initial_generated_images.clone();
            (images, settle_claude_turn_cost(&mut state.claude_cost))
        };
        let completion = TerminalTurnCompletion {
            character_id: session.character_id.clone(),
            turn_id: turn_id.clone(),
            response,
            ended_at: Some(ended_at),
            usage: transcript.usage,
            resolved_model: transcript.model,
            initial_generated_images: images,
            structured: Some(structured),
            reported_cost_usd: reported_cost,
            activities: transcript.activities,
            ..TerminalTurnCompletion::default()
        };
        // 완료 저장이 실패해도 턴을 running으로 두면 세션이 그 턴에 묶여 다음 질문이 모두 거절된다.
        let result = async {
            self.complete_turn(session, completion).await?;
            {
                let mut state = session.lock_state();
                state.running_turn_id = None;
                state.last_completed_turn_id = Some(turn_id.clone());
            }
            self.reset(session)
        }
        .await;
        let outcome = match result {
            Ok(()) => Ok(json!({ "accepted": true, "turnId": turn_id })),
            Err(error) => self.interrupt_after_failure(session, &turn_id, error).await,
        };
        self.changed(session);
        outcome
    }

    /// 완료 실패 뒤 턴을 중단으로 닫고 원래 오류를 돌려준다. 중단도 실패하면 그 오류다.
    async fn interrupt_after_failure(&self, session: &TerminalSession, turn_id: &str, error: AgentError) -> Result<Value, AgentError> {
        self.runtime.interrupt_terminal_turn(&session.character_id, turn_id).await?;
        session.lock_state().running_turn_id = None;
        Err(error)
    }

    /// recordClaudeSessionCost. 값이 줄면 CLI가 새로 시작한 것이라 기준을 0으로 되돌리고,
    /// 진행 중인 턴이 없으면 방금 끝난 턴에 차액을 더한다(마지막 갱신은 Stop 훅보다 늦게 온다).
    async fn record_claude_cost(&self, session: &TerminalSession, cost: Option<&Value>) -> Result<Value, AgentError> {
        let Some(total) = nonnegative_cost(cost.and_then(|cost| cost.get("total_cost_usd"))) else {
            return Ok(json!({ "accepted": false, "reason": "no-cost" }));
        };
        let (delta, last) = {
            let mut state = session.lock_state();
            let tracker = &mut state.claude_cost;
            if total < tracker.settled {
                tracker.settled = 0.0;
            }
            tracker.reported = Some(total);
            if let Some(running) = state.running_turn_id.clone() {
                return Ok(json!({ "accepted": true, "turnId": running }));
            }
            let tracker = &mut state.claude_cost;
            let delta = total - tracker.settled;
            tracker.settled = total;
            (delta, state.last_completed_turn_id.clone())
        };
        let Some(last) = last.filter(|_| delta > 0.0) else {
            return Ok(json!({ "accepted": true, "turnId": null }));
        };
        self.runtime.add_terminal_turn_reported_cost(&session.character_id, &last, delta).await?;
        Ok(json!({ "accepted": true, "turnId": last }))
    }

    /// handleCodexEvent. 관문을 통과한 notify만 받는다. 감시기가 먼저 만든 같은 턴이면 그 턴을 완료하고,
    /// 다른 턴이 남아 있으면(그 notify가 버려진 경우) 중단한 뒤 이 notify 기준으로 처음부터 기록한다.
    pub(super) async fn codex_event(
        self: &Arc<Self>,
        session: &Arc<TerminalSession>,
        context: &mut TurnContext,
        payload: &Value,
    ) -> Result<Value, AgentError> {
        let Some(gate) = context.codex_gate.as_mut() else {
            return Err(AgentError::Failed("Codex 터미널 관문이 없습니다.".into()));
        };
        let decision = gate.accept(payload).await?;
        let (bound, turn) = match decision {
            GateDecision::Rejected(reason) => {
                println!("Codex 터미널 notify 폐기({}): {reason}", session.character_id);
                return Ok(json!({ "accepted": false, "reason": reason }));
            }
            GateDecision::Accepted { bound_external_session_id, turn } => (bound_external_session_id, turn),
        };
        if let Some(bound) = bound.filter(|bound| !bound.is_empty()) {
            self.bind_if_needed(session, &bound).await?;
        }
        if let Some(cursor) = context.codex.as_mut() {
            cursor.mark_completed(&turn.turn_id);
        }
        let (running, codex_turn) = {
            let state = session.lock_state();
            (state.running_turn_id.clone(), state.codex_turn_id.clone())
        };
        let turn_id = match running {
            Some(running) if codex_turn.as_deref() == Some(turn.turn_id.as_str()) => running,
            running => {
                if let Some(running) = running {
                    self.runtime.interrupt_terminal_turn(&session.character_id, &running).await?;
                    session.lock_state().running_turn_id = None;
                }
                let started_at = date_from_epoch(turn.started_at.unwrap_or(f64::NAN));
                self.begin_dispatched_turn(session, turn.prompt.clone(), Some(started_at)).await?
            }
        };
        session.lock_state().codex_turn_id = None;
        let result = async {
            let path = session.lock_state().structured_result_path.take();
            let structured = consume_structured_turn_result(path.as_deref());
            let images = session.lock_state().initial_generated_images.clone();
            self.complete_turn(
                session,
                TerminalTurnCompletion {
                    character_id: session.character_id.clone(),
                    turn_id: turn_id.clone(),
                    response: turn.response.clone(),
                    ended_at: Some(date_from_epoch(turn.completed_at.unwrap_or(f64::NAN))),
                    initial_generated_images: images,
                    structured: Some(structured),
                    activities: turn.activities.clone(),
                    ..TerminalTurnCompletion::default()
                },
            )
            .await?;
            session.lock_state().running_turn_id = None;
            self.reset(session)
        }
        .await;
        let outcome = match result {
            Ok(()) => Ok(json!({ "accepted": true, "turnId": turn_id })),
            Err(error) => self.interrupt_after_failure(session, &turn_id, error).await,
        };
        self.changed(session);
        outcome
    }
}

/// settleClaudeTurnCost. 상태줄이 한 번도 오지 않았으면 None이라 비용을 꾸며내지 않는다.
fn settle_claude_turn_cost(tracker: &mut super::ClaudeCost) -> Option<f64> {
    let reported = tracker.reported?;
    let delta = (reported - tracker.settled).max(0.0);
    tracker.settled = reported;
    Some(delta)
}
