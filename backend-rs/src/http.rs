// HTTP 라우터. 응답 형식(JSON, content-type, 오류 문구)은 Node의 server.mjs와 같게 맞춘다.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::header::{CONTENT_TYPE, HOST, ORIGIN};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post, put};
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::agent_runtime::lifecycle::StartRequest;
use crate::agent_runtime::service::{AgentError, RUNTIME_UNAVAILABLE};
use crate::character_profile::{
    ProfileError, update_character_context_settings, update_character_identity_prompt, update_character_name,
};
use crate::character_settings::{
    CharacterSettingsError, ModelCatalog, list_characters, update_character_settings_atomically,
    update_single_character_settings,
};
use crate::health::{Runtime, health};
use crate::cli_updates::{
    CliUpdateChecker, CliUpdateError, apply_cli_updates, backends_for_identifier, has_running_work,
    packages_for_identifier, shared_install_prefix,
};
use crate::db::with_transaction;
use crate::js::{
    SearchParams, date_parse, encode_uri_component, iso_string, js_property, js_string, js_string_or_empty, to_js_json,
};
use crate::local_embedding::LocalEmbeddingService;
use crate::local_profile_selection::{
    LOCAL_REASONING_OPTIONS, LocalModelControl, VerifyHost, control_local_model, list_local_profiles, save_local_profile,
    select_local_profile, set_local_host_address, set_local_reasoning, verify_host_by_ssh,
};
use crate::local_provider::service::LocalProviderService;
use crate::model_catalog::{ModelCatalogError, ModelCatalogService};
use crate::rag::{
    RagDatabase, RagFailure, SearchRequest, add_rag_document, database_message, rag_vector_minimum_score,
    search_rag_documents,
};
use crate::realtime::Realtime;
use crate::roleplay::{RoleplayError, RoleplayService};
use crate::runtime_cli_paths::{RuntimeCLIPathsError, synchronize_runtime_cli_paths};
use crate::reply_delivery::{RouteSaveError, cancel_pending_reply_delivery, read_reply_routes, save_reply_route};
use crate::turn_deletion::delete_archived_turn;
use crate::turn_feed::{
    FeedError, archive_feed, character_history, global_history, list_active_sessions, list_turn_sources,
    list_work_records, live_feed, live_feed_turn, replace_turn_sources,
};
use crate::turn_feedback::{FeedbackOutcome, normalize_turn_feedback, replace_turn_feedback};
use crate::work_record_provenance::is_uuid;
use crate::usage_report::{TurnCostSummaryCache, read_usage_report};
use crate::usage_summary::UsageSummaryReader;
use crate::voice::{VoiceFollowControl, read_voice_turn, valid_character_id};
use crate::wiki_knowledge::{
    NewProposal, WikiError, approve_wiki_proposal, archive_wiki_page, create_wiki_proposal, get_wiki_page,
    list_wiki_pages, list_wiki_proposals, reject_wiki_proposal, resolve_wiki_page_key_race, verify_wiki_proposal,
};
use crate::work_board::{
    WorkBoardError, add_ticket_activity, create_goal, create_project, create_ticket, list_board, list_ticket_activity,
    read_board_work_record, update_goal, update_project, update_ticket,
};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub runtime: Arc<Runtime>,
    pub release_id: Option<String>,
    /// 직원별 비용·평가 요약 캐시. 비용·평가 변경 방송이 realtime을 거치며 무효화한다.
    pub cost_summary: Arc<TurnCostSummaryCache>,
    /// 웹소켓 /ws 연결과 방송(Node sockets·broadcast). 운영에서는 cost_summary와 같은 캐시를 받는다.
    pub realtime: Arc<Realtime>,
    /// Node의 localEmbeddingService. 복제본은 같은 워커를 공유하고, 워커는 첫 검색 때 뜬다.
    pub embedding: LocalEmbeddingService,
    /// Node의 modelCatalogService. 없으면 모델 목록 경로가 503이다.
    pub model_catalog: Option<Arc<ModelCatalogService>>,
    /// Node의 cliUpdateChecker. 갱신 설치도 같은 실행기를 쓴다.
    pub cli_updates: Arc<CliUpdateChecker>,
    /// Node의 readUsageSummary. 없으면 사용량 요약 경로가 503이다.
    pub usage_summary: Option<Arc<UsageSummaryReader>>,
    /// Node의 roleplayService. 없으면 롤플레이 경로가 503이다.
    pub roleplay: Option<RoleplayService>,
    /// Node의 voiceFollow. 음성 따라 읽기 자식 프로세스를 켜고 끈다.
    pub voice_follow: Arc<VoiceFollowControl>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/api/maintenance/drain", any(drain_handler))
        .route("/api/characters", get(list_characters_handler))
        .route("/api/characters/settings/bulk", put(bulk_settings_handler))
        .route("/api/characters/{id}/settings", put(single_settings_handler))
        .route("/api/characters/{id}/name", put(character_name_handler))
        .route("/api/characters/{id}/identity-prompt", put(identity_prompt_handler))
        .route("/api/characters/{id}/context-settings", put(context_settings_handler))
        .route("/api/characters/{id}/context/compact", post(compact_context_handler))
        .route("/api/characters/{id}/history", get(character_history_handler))
        .route("/api/local-profiles", get(local_profiles_handler).put(save_local_profile_handler))
        .route("/api/characters/{id}/local-model", post(local_model_handler))
        .route("/api/characters/{id}/local-address", put(local_address_handler))
        .route("/api/characters/{id}/local-reasoning", put(local_reasoning_handler))
        .route("/api/characters/{id}/local-profile", put(local_profile_handler))
        .route("/api/runtime/cli-paths", put(cli_paths_handler))
        .route("/api/model-catalog", get(model_catalog_handler))
        .route("/api/model-catalog/exclusions", put(model_catalog_exclusions_handler))
        .route("/api/active-sessions", get(active_sessions_handler))
        .route("/api/usage-summary", get(usage_summary_handler))
        .route("/api/history", get(history_handler))
        .route("/api/work-records", get(work_records_handler))
        .route("/api/turns/{id}", delete(delete_turn_handler))
        .route("/api/turns/{id}/sources", get(turn_sources_handler).put(replace_turn_sources_handler))
        .route("/api/turns/{id}/feedback", put(turn_feedback_handler))
        .route("/api/reply-routes", get(reply_routes_handler))
        .route("/api/reply-routes/{id}", put(save_reply_route_handler))
        .route("/api/reply-deliveries/{id}/cancel", post(cancel_reply_delivery_handler))
        .route("/api/live-feed", get(live_feed_handler))
        .route("/api/archive-feed", get(archive_feed_handler))
        .route("/api/live-feed/{id}", get(live_feed_turn_handler))
        .route("/api/work-board", any(work_board_handler))
        .route("/api/work-board/", any(work_board_handler))
        .route("/api/work-board/{*rest}", any(work_board_handler))
        // Node readJSON은 크기 제한이 없다. 프리셋 가져오기는 20MiB 파일의 base64라 기본 2MB 제한을 끈다.
        .route("/api/roleplay/", any(roleplay_handler).layer(DefaultBodyLimit::disable()))
        .route("/api/roleplay/{*rest}", any(roleplay_handler).layer(DefaultBodyLimit::disable()))
        .route("/api/rag/documents", post(rag_documents_handler))
        .route("/api/rag/search", post(rag_search_handler))
        .route("/api/wiki/pages", get(wiki_pages_handler))
        .route("/api/wiki/pages/{*rest}", any(wiki_page_handler))
        .route("/api/wiki/proposals", get(wiki_proposals_handler).post(create_wiki_proposal_handler))
        .route("/api/wiki/proposals/{*rest}", post(wiki_proposal_action_handler))
        .route("/api/usage-report", get(usage_report_handler))
        .route("/api/voice-feed", get(voice_feed_handler))
        .route("/api/voice-follow", get(voice_follow_status_handler).post(voice_follow_set_handler))
        .route("/api/cli-updates", get(cli_updates_handler))
        .route("/api/cli-updates/apply", post(apply_cli_updates_handler))
        .route("/api/agent-jobs", get(agent_jobs_handler).post(start_agent_job_handler))
        .route("/api/agent-jobs/{id}", delete(cancel_agent_job_handler))
        .route("/api/agent-jobs/{id}/steer", post(steer_agent_job_handler))
        .route("/api/workspace-reviews/{id}", any(workspace_review_handler))
        .route("/api/workspace-reviews/{id}/approve", any(workspace_approve_handler))
        .route("/api/workspace-reviews/{id}/reject", any(workspace_reject_handler))
        .route("/api/developer-tool-events", post(developer_tool_event_handler))
        .route("/api/terminal-sessions", get(terminal_sessions_handler).post(open_terminal_session_handler))
        .route("/api/terminal-sessions/{id}", delete(close_terminal_session_handler))
        .route("/api/terminal-sessions/{id}/events", post(terminal_event_handler))
        .route("/api/terminal-sessions/{id}/interrupt", post(interrupt_terminal_session_handler))
        .route("/api/terminal-sessions/{id}/dispatch", post(terminal_dispatch_handler))
        .route("/ws", any(websocket_handler))
        .fallback(not_found)
        // Node는 메서드가 달라도 404를 내므로 405 대신 같은 404를 낸다.
        .method_not_allowed_fallback(not_found)
        // 경로·대체 처리기를 모두 등록한 뒤에 감싸야 404 경로에도 적용된다.
        .layer(middleware::from_fn(reject_malformed_route_parameters))
        .with_state(state)
}

/// Node 요청 처리기는 분기 전에 아래 경로의 `[^/]+` 조각을 모두 decodeURIComponent로 푼다.
/// 그래서 메서드·경로 구현 여부와 무관하게 잘못된 % 순서는 500 "URI malformed"다.
fn node_route_parameter(path: &str) -> Option<&str> {
    let segments: Vec<&str> = path.strip_prefix("/api/")?.split('/').collect();
    let id = match segments.as_slice() {
        ["characters", id, "name" | "settings" | "identity-prompt" | "context-settings" | "history"]
        | ["characters", id, "context", "compact"]
        | ["agent-jobs", id]
        | ["agent-jobs", id, "steer"]
        | ["terminal-sessions", id]
        | ["terminal-sessions", id, "events" | "interrupt" | "dispatch"]
        | ["live-feed", id]
        | ["turns", id]
        | ["turns", id, "sources" | "feedback"]
        | ["workspace-reviews", id]
        | ["workspace-reviews", id, "approve" | "reject"] => *id,
        _ => return None,
    };
    (!id.is_empty()).then_some(id)
}

async fn reject_malformed_route_parameters(request: Request, next: Next) -> Response {
    if let Some(parameter) = node_route_parameter(request.uri().path())
        && let Err(response) = decode_uri_component(parameter)
    {
        return response;
    }
    next.run(request).await
}

/// Node의 send(): JSON.stringify 본문과 `application/json; charset=utf-8`.
pub fn send(status: StatusCode, body: &impl Serialize) -> Response {
    let bytes = to_js_json(body).unwrap_or_else(|_| b"{}".to_vec());
    (status, [(CONTENT_TYPE, "application/json; charset=utf-8")], bytes).into_response()
}

pub fn error(status: StatusCode, message: &str) -> Response {
    send(status, &json!({ "error": message }))
}

async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "경로를 찾을 수 없습니다.")
}

/// Node server.on("upgrade"). /ws의 Upgrade 요청만 웹소켓으로 넘기고, 일반 요청은 다른 경로처럼 404다.
async fn websocket_handler(State(state): State<AppState>, request: Request) -> Response {
    let runtime = Arc::clone(&state.runtime);
    match state.realtime.upgrade(request, move || runtime.compacting_character_ids()).await {
        Some(response) => response,
        None => not_found().await,
    }
}

async fn health_handler(State(state): State<AppState>) -> Response {
    let database_ok = sqlx::query("SELECT 1").execute(&state.pool).await.is_ok();
    let status = if database_ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    send(
        status,
        &health(&state.runtime, std::process::id(), state.release_id.as_deref(), database_ok),
    )
}

async fn drain_handler(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method == Method::GET {
        return send(StatusCode::OK, &with_ok(state.runtime.maintenance_status()));
    }
    if method != Method::POST && method != Method::DELETE {
        return not_found().await;
    }
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    if !body.is_empty() && serde_json::from_slice::<Value>(&body).is_err() {
        return error(StatusCode::BAD_REQUEST, "JSON 본문을 해석할 수 없습니다.");
    }
    let status = if method == Method::POST {
        state.runtime.begin_drain()
    } else {
        state.runtime.cancel_drain()
    };
    send(StatusCode::OK, &with_ok(status))
}

async fn list_characters_handler(State(state): State<AppState>) -> Response {
    match list_characters(&state.pool).await {
        Ok(characters) => send(StatusCode::OK, &json!({ "characters": characters })),
        Err(database_error) => error(StatusCode::INTERNAL_SERVER_ERROR, &database_error.to_string()),
    }
}

async fn bulk_settings_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    match update_character_settings_atomically(&state.pool, &state.runtime, &body, settings_catalog(&state)).await {
        Ok(result) => send(StatusCode::OK, &result),
        Err(CharacterSettingsError::TargetsNotFound(ids)) => send(
            StatusCode::NOT_FOUND,
            &json!({ "error": CharacterSettingsError::TargetsNotFound(ids.clone()).to_string(), "characterIds": ids }),
        ),
        Err(settings_error) => character_settings_error(settings_error),
    }
}

async fn single_settings_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    body: Bytes,
) -> Response {
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    match update_single_character_settings(&state.pool, &state.runtime, &character_id, &body, settings_catalog(&state)).await {
        Ok(character) => send(StatusCode::OK, &character),
        Err(settings_error) => character_settings_error(settings_error),
    }
}

/// 설정 검사에 실행 중인 모델 카탈로그를 쓴다. 없으면 내장 목록만 쓴다.
fn settings_catalog(state: &AppState) -> Option<&dyn ModelCatalog> {
    state.model_catalog.as_deref().map(|service| service as &dyn ModelCatalog)
}

fn profile_response(result: Result<Value, ProfileError>) -> Response {
    match result {
        Ok(body) => send(StatusCode::OK, &body),
        Err(profile_error) => error(
            StatusCode::from_u16(profile_error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            &profile_error.message(),
        ),
    }
}

/// PUT /api/characters/:id/name. Node처럼 출처 검사 없이 본문만 읽는다.
async fn character_name_handler(State(state): State<AppState>, Path(character_id): Path<String>, body: Bytes) -> Response {
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    profile_response(update_character_name(&state.pool, &character_id, &body).await)
}

/// PUT /api/characters/:id/identity-prompt. Node처럼 출처 검사 없이 본문만 읽는다.
async fn identity_prompt_handler(State(state): State<AppState>, Path(character_id): Path<String>, body: Bytes) -> Response {
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    profile_response(update_character_identity_prompt(&state.pool, &character_id, &body).await)
}

/// PUT /api/characters/:id/context-settings.
async fn context_settings_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    profile_response(update_character_context_settings(&state.pool, &character_id, &body).await)
}

/// POST /api/characters/:id/context/compact. 본문은 Node처럼 읽기만 하고 쓰지 않는다.
async fn compact_context_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    if let Err(response) = read_json(&body) {
        return response;
    }
    if state.runtime.services().is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, RUNTIME_UNAVAILABLE);
    }
    match state.runtime.compact_context(&character_id, false, None).await {
        Ok(payload) => send(StatusCode::OK, &payload),
        Err(AgentError::CharacterNotFound(message) | AgentError::SessionNotFound(message)) => {
            error(StatusCode::NOT_FOUND, &message)
        }
        Err(AgentError::Draining(message)) => error(StatusCode::SERVICE_UNAVAILABLE, &message),
        Err(AgentError::Unavailable) => error(StatusCode::SERVICE_UNAVAILABLE, RUNTIME_UNAVAILABLE),
        Err(AgentError::Busy(message)) => error(StatusCode::CONFLICT, &message),
        Err(other) => error(StatusCode::INTERNAL_SERVER_ERROR, &other.to_string()),
    }
}

async fn workspace_review_handler(
    State(state): State<AppState>,
    method: Method,
    Path(turn_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    workspace_review(state, method, &turn_id, None, &headers, &body).await
}

async fn workspace_approve_handler(
    State(state): State<AppState>,
    method: Method,
    Path(turn_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    workspace_review(state, method, &turn_id, Some("approve"), &headers, &body).await
}

async fn workspace_reject_handler(
    State(state): State<AppState>,
    method: Method,
    Path(turn_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    workspace_review(state, method, &turn_id, Some("reject"), &headers, &body).await
}

/// Node workspaceReview. 실행기가 없으면 메서드와 무관하게 503이고, 메서드·동작이 맞지 않으면 404다.
async fn workspace_review(
    state: AppState,
    method: Method,
    turn_id: &str,
    action: Option<&str>,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    if state.runtime.services().is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, RUNTIME_UNAVAILABLE);
    }
    let result = match (method, action) {
        (Method::GET, None) => state.runtime.fetch_workspace_review(turn_id).await,
        (Method::POST, Some("approve")) => {
            if let Err(response) = trusted_json_mutation(headers) {
                return response;
            }
            let body = match read_json(body) {
                Ok(body) => body,
                Err(response) => return response,
            };
            let review_tree = match js_property(&body, "reviewTree") {
                Ok(review_tree) => review_tree.cloned(),
                Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, &message),
            };
            state.runtime.approve_workspace(turn_id, review_tree, "user").await
        }
        (Method::POST, Some("reject")) => {
            if let Err(response) = trusted_json_mutation(headers) {
                return response;
            }
            if let Err(response) = read_json(body) {
                return response;
            }
            state.runtime.reject_workspace(turn_id).await
        }
        _ => return not_found().await,
    };
    match result {
        Ok(payload) => send(StatusCode::OK, &payload),
        Err(AgentError::JobNotFound(message)) => error(StatusCode::NOT_FOUND, &message),
        Err(AgentError::Busy(message) | AgentError::Git(_, message)) => error(StatusCode::CONFLICT, &message),
        Err(AgentError::Draining(message)) => error(StatusCode::SERVICE_UNAVAILABLE, &message),
        Err(AgentError::Unavailable) => error(StatusCode::SERVICE_UNAVAILABLE, RUNTIME_UNAVAILABLE),
        Err(other) => error(StatusCode::INTERNAL_SERVER_ERROR, &other.to_string()),
    }
}

async fn character_history_handler(State(state): State<AppState>, Path(character_id): Path<String>) -> Response {
    feed_response(character_history(&state.pool, &character_id).await)
}

/// Node의 상태 매핑. 400 검증, 404 대상 없음, 409 드레인 충돌·업무 중·작업 공간, 그 밖은 500.
fn character_settings_error(settings_error: CharacterSettingsError) -> Response {
    let status = match &settings_error {
        CharacterSettingsError::Validation(_) => StatusCode::BAD_REQUEST,
        CharacterSettingsError::TargetsNotFound(_) | CharacterSettingsError::CharacterNotFound => StatusCode::NOT_FOUND,
        CharacterSettingsError::DrainConflict(_) | CharacterSettingsError::Busy(_) | CharacterSettingsError::Workspace(_) => {
            StatusCode::CONFLICT
        }
        CharacterSettingsError::Failed(_) | CharacterSettingsError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error(status, &settings_error.to_string())
}

/// 실행기 서비스에 붙은 로컬 제공자. 서비스가 없으면 None이다.
fn local_providers(state: &AppState) -> Option<LocalProviderService> {
    state.runtime.services().ok().and_then(|services| services.local_providers.clone())
}

fn local_control(providers: &Option<LocalProviderService>) -> Option<&dyn LocalModelControl> {
    providers.as_ref().map(|providers| providers as &dyn LocalModelControl)
}

fn status_of(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// GET /api/local-profiles.
async fn local_profiles_handler(State(state): State<AppState>) -> Response {
    match list_local_profiles(&state.pool, local_providers(&state).as_ref()).await {
        Ok(body) => send(StatusCode::OK, &body),
        Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

/// PUT /api/local-profiles.
async fn save_local_profile_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    match save_local_profile(&state.pool, local_providers(&state).as_ref(), &body).await {
        Ok(result) => send(StatusCode::OK, &result),
        Err((code, message)) => error(status_of(code), &message),
    }
}

/// POST /api/characters/:id/local-model. 바쁨은 409, 그 밖은 400이고 그래픽 메모리 점유면 앱이 고를 정보를 붙인다.
async fn local_model_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let providers = local_providers(&state);
    let action = body.get("action").and_then(Value::as_str);
    match control_local_model(&state.pool, &state.runtime, local_control(&providers), &character_id, action, body.get("comfyRelease")).await {
        Ok(result) => send(StatusCode::OK, &result),
        Err(control_error) => {
            let mut payload = json!({ "error": control_error.to_string() });
            if let Some(gpu) = control_error.gpu() {
                payload["code"] = json!("local-gpu-occupied");
                payload["gpu"] = gpu.clone();
            }
            send(status_of(control_error.route_status()), &payload)
        }
    }
}

/// PUT /api/characters/:id/local-address.
async fn local_address_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let providers = local_providers(&state);
    let realtime = Arc::clone(&state.realtime);
    let broadcast = move |event: Value| realtime.broadcast(&event);
    let address = body.get("address").unwrap_or(&Value::Null);
    let verify: &VerifyHost = &verify_host_by_ssh;
    match set_local_host_address(&state.pool, &state.runtime, local_control(&providers), &character_id, address, verify, &broadcast).await {
        Ok(result) => send(StatusCode::OK, &result),
        Err(address_error) => error(status_of(address_error.route_status()), &address_error.to_string()),
    }
}

/// PUT /api/characters/:id/local-reasoning. 선택지 밖이면 먼저 400이고, 그 뒤 오류는 서버 공통 처리다.
async fn local_reasoning_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let reasoning = body.get("reasoning").unwrap_or(&Value::Null);
    if !reasoning.as_str().is_some_and(|value| LOCAL_REASONING_OPTIONS.contains(&value)) {
        return error(StatusCode::BAD_REQUEST, "Unsupported local reasoning option");
    }
    let realtime = Arc::clone(&state.realtime);
    let broadcast = move |event: Value| realtime.broadcast(&event);
    match set_local_reasoning(&state.pool, &state.runtime, &character_id, reasoning, &broadcast).await {
        Ok(result) => send(StatusCode::OK, &result),
        Err(reasoning_error) => error(status_of(reasoning_error.generic_status()), &reasoning_error.to_string()),
    }
}

/// PUT /api/characters/:id/local-profile. profileId는 문자열이거나 null(클라우드 복귀)이어야 한다.
async fn local_profile_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let profile_id = match body.get("profileId") {
        Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id.as_str()),
        _ => return error(StatusCode::BAD_REQUEST, "profileId is required (null restores cloud settings)"),
    };
    let providers = local_providers(&state);
    let realtime = Arc::clone(&state.realtime);
    let broadcast = move |event: Value| realtime.broadcast(&event);
    match select_local_profile(&state.pool, &state.runtime, local_control(&providers), &character_id, profile_id, &broadcast).await {
        Ok(result) => send(StatusCode::OK, &result),
        Err(selection_error) => error(status_of(selection_error.generic_status()), &selection_error.to_string()),
    }
}

async fn cli_paths_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    match synchronize_runtime_cli_paths(&state.pool, &body).await {
        Ok(result) => send(StatusCode::OK, &result),
        Err(RuntimeCLIPathsError::Validation(message)) => error(StatusCode::BAD_REQUEST, &message),
        Err(database_error) => error(StatusCode::INTERNAL_SERVER_ERROR, &database_error.to_string()),
    }
}

const MODEL_CATALOG_UNAVAILABLE: &str = "모델 목록 수집기가 준비되지 않았습니다.";

/// Node의 modelCatalog(). force=1이면 기한과 무관하게 세 제공자를 다시 받은 뒤 응답한다.
async fn model_catalog_handler(State(state): State<AppState>, uri: Uri) -> Response {
    let Some(service) = state.model_catalog else {
        return error(StatusCode::SERVICE_UNAVAILABLE, MODEL_CATALOG_UNAVAILABLE);
    };
    if SearchParams::parse(uri.query()).get("force") == Some("1")
        && let Err(message) = service.refresh_due(true).await
    {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &message);
    }
    send(StatusCode::OK, &service.snapshot())
}

/// Node의 updateModelCatalogExclusions(). 출처 검사와 본문 해석이 수집기 준비 여부보다 먼저다.
async fn model_catalog_exclusions_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(service) = state.model_catalog else {
        return error(StatusCode::SERVICE_UNAVAILABLE, MODEL_CATALOG_UNAVAILABLE);
    };
    let provider = match js_property(&body, "provider") {
        Ok(provider) => js_string_or_empty(provider),
        Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    };
    let excluded = js_property(&body, "excludedModels").ok().flatten();
    match service.set_exclusions(&provider, excluded).await {
        Ok(snapshot) => send(StatusCode::OK, &snapshot),
        Err(ModelCatalogError::Validation(message)) => error(StatusCode::BAD_REQUEST, &message),
        Err(ModelCatalogError::Failure(message)) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

/// Node의 usageSummary(). force는 소문자로 바꿔 1·true일 때만 켠다. 실패는 공통 처리기처럼 500 { error }다.
async fn usage_summary_handler(State(state): State<AppState>, uri: Uri) -> Response {
    let force = SearchParams::parse(uri.query()).get("force").map(str::to_lowercase);
    let force = matches!(force.as_deref(), Some("1" | "true"));
    let Some(reader) = state.usage_summary else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "사용량 요약기가 준비되지 않았습니다.");
    };
    match reader.read(force).await {
        Ok(summary) => send(StatusCode::OK, &summary),
        Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

/// Node GET /api/voice-feed. 직원 ID 모양과 Date.parse 가능한 since가 있어야 하고, since는 toISOString으로 바꿔 조회한다.
async fn voice_feed_handler(State(state): State<AppState>, uri: Uri) -> Response {
    let params = SearchParams::parse(uri.query());
    let character_id = params.get("characterId").filter(|id| valid_character_id(id));
    let since = params.get("since").and_then(date_parse);
    let (Some(character_id), Some(since)) = (character_id, since) else {
        return error(StatusCode::BAD_REQUEST, "characterId와 유효한 since가 필요합니다.");
    };
    match read_voice_turn(&state.pool, character_id, &iso_string(since)).await {
        Ok(turn) => send(StatusCode::OK, &json!({ "turn": turn })),
        Err(database_error) => error(StatusCode::INTERNAL_SERVER_ERROR, &database_error.to_string()),
    }
}

/// Node GET /api/voice-follow. 따라 읽기 상태 `{characterId, state, error}`.
async fn voice_follow_status_handler(State(state): State<AppState>) -> Response {
    send(StatusCode::OK, &state.voice_follow.status())
}

/// Node POST /api/voice-follow. 본문 읽기(null 본문의 TypeError 포함)와 켜고 끄기 실패는 모두 400 { error }다.
async fn voice_follow_set_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let result = js_property(&body, "enabled")
        .and_then(|enabled| Ok((enabled, js_property(&body, "characterId")?)))
        .and_then(|(enabled, character_id)| state.voice_follow.set(enabled, character_id));
    match result {
        Ok(status) => send(StatusCode::OK, &status),
        Err(message) => error(StatusCode::BAD_REQUEST, &message),
    }
}

/// Node의 cliUpdates(). CLI가 새로 설치됐는지도 함께 보되 모델 목록 수집은 응답을 붙잡지 않는다.
async fn cli_updates_handler(State(state): State<AppState>, uri: Uri) -> Response {
    let force = SearchParams::parse(uri.query()).get("force").map(str::to_lowercase);
    let force = matches!(force.as_deref(), Some("1" | "true"));
    if let Some(service) = state.model_catalog.clone() {
        tokio::spawn(async move {
            if let Err(error) = service.refresh_updated_executables().await {
                eprintln!("CLI 갱신 뒤 모델 목록을 다시 받지 못했습니다. {error}");
            }
        });
    }
    send(StatusCode::OK, &state.cli_updates.read(force).await)
}

/// Node의 applyCLIUpdatesEndpoint(). 출처 검사·본문 해석 뒤 현재 상태를 먼저 읽고 대상을 고른다.
async fn apply_cli_updates_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    match apply_cli_updates_route(&state, &body).await {
        Ok(result) => send(StatusCode::OK, &result),
        Err(CliUpdateError::Busy(message)) => error(StatusCode::CONFLICT, &message),
        Err(CliUpdateError::UnknownPackage(message)) => error(StatusCode::BAD_REQUEST, &message),
        Err(CliUpdateError::Failure(message)) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

async fn apply_cli_updates_route(state: &AppState, body: &Value) -> Result<Value, CliUpdateError> {
    let checker = &state.cli_updates;
    let current = checker.read(false).await;
    let id = js_property(body, "id").map_err(CliUpdateError::Failure)?;
    let backends = backends_for_identifier(id)?;
    let packages = packages_for_identifier(id)?;
    let prefix = shared_install_prefix(&current, id)?;
    let runner = checker.runner();
    let pool = &state.pool;
    let running = |backends: Vec<&'static str>| async move { has_running_work(pool, &backends).await };
    let closes_claude = backends.contains(&"claude");
    let mut result = apply_cli_updates(runner.as_ref(), &packages, backends, prefix.as_deref(), running).await?;
    if closes_claude {
        state.runtime.close_claude_workers("Claude Code CLI가 갱신되어 지속 세션을 다시 시작합니다.");
    }
    checker.invalidate();
    let status = checker.read(true).await;
    if let Value::Object(fields) = &mut result {
        fields.insert("status".into(), status);
    }
    Ok(result)
}

const AGENT_JOBS_NOTE: &str =
    "현재 시점의 접수 가능 여부입니다. 실제 접수는 POST 응답으로 확인하세요. 기록·세션의 active는 실행 상태가 아닙니다.";
const AGENT_JOB_BUSY_HINT: &str = "수신 직원의 실행 상태를 확인하세요. work-records의 active는 원인이 아닙니다. DELETE /api/agent-jobs는 기록 정리가 아니라 업무 중단이므로 접수 오류 해결에 사용하지 마세요.";

/// GET /api/agent-jobs. 직원별 현재 접수 가능 여부다.
async fn agent_jobs_handler(State(state): State<AppState>, uri: Uri) -> Response {
    if state.runtime.services().is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, RUNTIME_UNAVAILABLE);
    }
    let params = SearchParams::parse(uri.query());
    let character_id = params.get("characterId");
    match state.runtime.message_availability(character_id).await {
        Ok(employees) if character_id.is_some_and(|id| !id.is_empty()) && employees.is_empty() => {
            error(StatusCode::NOT_FOUND, "수신 직원을 찾을 수 없습니다.")
        }
        Ok(employees) => send(StatusCode::OK, &json!({ "employees": employees, "note": AGENT_JOBS_NOTE })),
        Err(agent_error) => error(StatusCode::INTERNAL_SERVER_ERROR, &agent_error.to_string()),
    }
}

/// POST /api/agent-jobs. Node startAgentJob처럼 출처 검사 없이 받고, 본문을 먼저 읽는다.
async fn start_agent_job_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if state.runtime.services().is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, RUNTIME_UNAVAILABLE);
    }
    start_agent_job(&state, &body).await.unwrap_or_else(|response| response)
}

async fn character_exists(pool: &PgPool, character_id: &str) -> Result<bool, Response> {
    sqlx::query_scalar::<_, String>("SELECT id FROM characters WHERE id = $1")
        .bind(character_id)
        .fetch_optional(pool)
        .await
        .map(|row| row.is_some())
        .map_err(|database_error| error(StatusCode::INTERNAL_SERVER_ERROR, &database_message(&database_error)))
}

async fn start_agent_job(state: &AppState, body: &Value) -> Result<Response, Response> {
    // 본문이 null이면 Node처럼 첫 속성 읽기에서 500이다.
    let reply_recipient =
        js_property(body, "replyRecipientId").map_err(|message| error(StatusCode::INTERNAL_SERVER_ERROR, &message))?;
    let reply_recipient_id = match reply_recipient {
        None | Some(Value::Null) => None,
        Some(value) => {
            let character_id = body.get("characterId").and_then(Value::as_str);
            let Some(recipient) = value.as_str().filter(|recipient| Some(*recipient) != character_id) else {
                return Ok(error(StatusCode::BAD_REQUEST, "답변을 받을 다른 직원을 선택하세요."));
            };
            if !character_exists(&state.pool, recipient).await? {
                return Ok(error(StatusCode::BAD_REQUEST, "답변을 받을 직원을 찾을 수 없습니다."));
            }
            Some(recipient.to_string())
        }
    };
    let sender = body.get("senderCharacterId").filter(|sender| !sender.is_null());
    if let Some(sender) = sender
        && !character_exists(&state.pool, &js_string(sender)).await?
    {
        return Ok(error(StatusCode::BAD_REQUEST, "발신 직원을 찾을 수 없습니다."));
    }
    // 표시용 출처 표시일 뿐 권한 증명이 아니다.
    let request_source = match body.get("requestSource") {
        None | Some(Value::Null) => None,
        Some(Value::String(source)) if source == "remote" => Some(source.clone()),
        Some(_) => return Ok(error(StatusCode::BAD_REQUEST, "requestSource는 remote만 허용합니다.")),
    };
    let character_id = js_string_or_empty(body.get("characterId"));
    let request = StartRequest {
        character_id: character_id.clone(),
        prompt: body.get("prompt").cloned(),
        conversation_id: body.get("conversationId").cloned(),
        attachment_paths: body.get("attachmentPaths").cloned(),
        sender_character_id: sender.cloned(),
        request_source,
        reply_recipient_id,
        delivery_id: None,
    };
    Ok(match state.runtime.start(request).await {
        Ok(job) => send(StatusCode::ACCEPTED, &job),
        Err(AgentError::Draining(message)) => error(StatusCode::SERVICE_UNAVAILABLE, &message),
        Err(AgentError::Unavailable) => error(StatusCode::SERVICE_UNAVAILABLE, RUNTIME_UNAVAILABLE),
        Err(AgentError::CharacterNotFound(message)) => error(StatusCode::NOT_FOUND, &message),
        Err(AgentError::Busy(message) | AgentError::DeliveryUncertain(message)) => send(
            StatusCode::CONFLICT,
            &json!({
                "error": message,
                "characterId": character_id,
                "statusURL": format!("/api/agent-jobs?characterId={}", encode_uri_component(&character_id)),
                "hint": AGENT_JOB_BUSY_HINT,
            }),
        ),
        Err(other) => error(StatusCode::INTERNAL_SERVER_ERROR, &other.to_string()),
    })
}

/// DELETE /api/agent-jobs/:characterId. 기록 정리가 아니라 실행 중인 업무 중단이다.
async fn cancel_agent_job_handler(State(state): State<AppState>, Path(character_id): Path<String>) -> Response {
    if state.runtime.services().is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, RUNTIME_UNAVAILABLE);
    }
    match state.runtime.cancel(&character_id).await {
        Ok(result) => send(StatusCode::OK, &result),
        Err(AgentError::JobNotFound(message)) => error(StatusCode::NOT_FOUND, &message),
        Err(other) => error(StatusCode::INTERNAL_SERVER_ERROR, &other.to_string()),
    }
}

/// POST /api/agent-jobs/:characterId/steer. 실행 중인 GUI 업무에 끼어든다. 받아들이면 202,
/// 이 업무가 끼어들기를 받을 수 없으면 409(앱은 다음 턴 예약으로 돌린다), 실행 중인 업무가 없으면 404다.
async fn steer_agent_job_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if state.runtime.services().is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, RUNTIME_UNAVAILABLE);
    }
    match state.runtime.steer(&character_id, body.get("prompt"), body.get("attachmentPaths")).await {
        Ok(result) => send(StatusCode::ACCEPTED, &result),
        Err(AgentError::JobNotFound(message)) => error(StatusCode::NOT_FOUND, &message),
        Err(AgentError::Busy(message)) => error(StatusCode::CONFLICT, &message),
        Err(AgentError::Failed(message)) => error(StatusCode::BAD_REQUEST, &message),
        Err(AgentError::Unavailable) => error(StatusCode::SERVICE_UNAVAILABLE, RUNTIME_UNAVAILABLE),
        Err(other) => error(StatusCode::INTERNAL_SERVER_ERROR, &other.to_string()),
    }
}

/// POST /api/developer-tool-events. 받으면 202, 받지 않은 이유가 있으면 409다.
async fn developer_tool_event_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    match state.runtime.record_developer_tool_event(body).await {
        Ok(result) => {
            let accepted = result.get("accepted").and_then(Value::as_bool) == Some(true);
            send(if accepted { StatusCode::ACCEPTED } else { StatusCode::CONFLICT }, &result)
        }
        Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

const TERMINAL_UNAVAILABLE: &str = "터미널 실행기가 준비되지 않았습니다.";

/// GET /api/terminal-sessions. 열린 터미널 목록이다.
async fn terminal_sessions_handler(State(state): State<AppState>) -> Response {
    let sessions = state.runtime.terminal_sessions().map_or_else(|| json!([]), |manager| manager.list());
    send(StatusCode::OK, &json!({ "sessions": sessions }))
}

/// POST /api/terminal-sessions. 앱이 PTY로 띄울 실행 명세(소유 토큰 포함)를 201로 돌려준다.
async fn open_terminal_session_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(manager) = state.runtime.terminal_sessions() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, TERMINAL_UNAVAILABLE);
    };
    match manager.open(&js_string_or_empty(body.get("characterId"))).await {
        Ok(spec) => send(StatusCode::CREATED, &spec),
        Err(AgentError::Draining(message)) => error(StatusCode::SERVICE_UNAVAILABLE, &message),
        Err(AgentError::CharacterNotFound(message)) => error(StatusCode::NOT_FOUND, &message),
        Err(AgentError::Busy(message)) => error(StatusCode::CONFLICT, &message),
        Err(other) => error(StatusCode::INTERNAL_SERVER_ERROR, &other.to_string()),
    }
}

/// DELETE /api/terminal-sessions/:characterId. 닫으면 진행 중인 터미널 턴은 중단으로 남는다.
async fn close_terminal_session_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    if let Err(response) = read_json(&body) {
        return response;
    }
    let Some(manager) = state.runtime.terminal_sessions() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, TERMINAL_UNAVAILABLE);
    };
    match manager.close(&character_id).await {
        Ok(true) => send(StatusCode::OK, &json!({ "ok": true })),
        Ok(false) => error(StatusCode::NOT_FOUND, "열린 터미널 세션을 찾을 수 없습니다."),
        Err(other) => error(StatusCode::INTERNAL_SERVER_ERROR, &other.to_string()),
    }
}

/// POST /api/terminal-sessions/:characterId/events. 훅 실행 파일이 보낸 CLI 이벤트다. 바쁨은 409, 그 밖의 실패는 404다.
async fn terminal_event_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(manager) = state.runtime.terminal_sessions() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, TERMINAL_UNAVAILABLE);
    };
    match manager.handle_event(&character_id, body).await {
        Ok(result) => send(StatusCode::ACCEPTED, &result),
        Err(AgentError::Busy(message) | AgentError::DeliveryUncertain(message)) => error(StatusCode::CONFLICT, &message),
        Err(other) => error(StatusCode::NOT_FOUND, &other.to_string()),
    }
}

/// POST /api/terminal-sessions/:characterId/interrupt. 앱이 Esc를 보낸 뒤 부른다. 실패는 모두 404다.
async fn interrupt_terminal_session_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    if let Err(response) = read_json(&body) {
        return response;
    }
    let Some(manager) = state.runtime.terminal_sessions() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, TERMINAL_UNAVAILABLE);
    };
    match manager.interrupt(&character_id).await {
        Ok(result) => send(StatusCode::OK, &result),
        Err(other) => error(StatusCode::NOT_FOUND, &other.to_string()),
    }
}

/// POST /api/terminal-sessions/:characterId/dispatch. 앱이 전달 본문을 가져가거나(claim) 입력 실패를 알린다(reject).
/// Node는 이 경로에서 관리자 유무를 따로 보지 않아 없으면 TypeError 500이다.
async fn terminal_dispatch_handler(
    State(state): State<AppState>,
    Path(character_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(manager) = state.runtime.terminal_sessions() else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot read properties of undefined (reading 'dispatchControl')",
        );
    };
    match manager.dispatch_control(&character_id, &body) {
        Ok(result) => send(StatusCode::OK, &result),
        Err(AgentError::Busy(message) | AgentError::DeliveryUncertain(message)) => error(StatusCode::CONFLICT, &message),
        Err(other) => error(StatusCode::INTERNAL_SERVER_ERROR, &other.to_string()),
    }
}

async fn work_board_handler(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    work_board_route(&state.pool, &method, uri.path(), &headers, &body)
        .await
        .unwrap_or_else(|response| response)
}

async fn roleplay_handler(State(state): State<AppState>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    // Node처럼 요청이 끊겨도 저장·답 생성 접수가 끝까지 돌도록 따로 돌린다.
    let task = tokio::spawn(async move { roleplay_route(&state, &method, uri.path(), &headers, &body).await.unwrap_or_else(|response| response) });
    task.await.unwrap_or_else(|join_error| error(StatusCode::INTERNAL_SERVER_ERROR, &join_error.to_string()))
}

/// Node의 roleplayEndpoint. 직원 ID는 분기 전에 풀어서 잘못된 % 순서는 메서드와 무관하게 500이다.
/// 조회 셋이 먼저이고, 그 밖은 형식·출처 검사와 본문 해석 뒤에 고르며 맞는 것이 없으면 405다.
async fn roleplay_route(state: &AppState, method: &Method, path: &str, headers: &HeaderMap, body: &Bytes) -> Result<Response, Response> {
    static PROFILE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^/api/roleplay/profiles/([^/]+)(?:/(chats|messages))?$").expect("롤플레이 경로 정규식"));
    let captures = PROFILE.captures(path);
    let character_id = match captures.as_ref().and_then(|captures| captures.get(1)) {
        Some(id) => Some(decode_uri_component(id.as_str())?),
        None => None,
    };
    let action = captures.as_ref().and_then(|captures| captures.get(2)).map(|action| action.as_str());
    let Some(service) = state.roleplay.as_ref() else {
        return Err(error(StatusCode::SERVICE_UNAVAILABLE, "롤플레이 서비스가 준비되지 않았습니다."));
    };
    let failed = |failure: RoleplayError| {
        error(StatusCode::from_u16(failure.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), failure.message())
    };
    let get = *method == Method::GET;
    if get && path == "/api/roleplay/assets" {
        return Ok(send(StatusCode::OK, &json!({ "assets": service.list_assets().await.map_err(failed)? })));
    }
    if get && path == "/api/roleplay/models" {
        return Ok(send(StatusCode::OK, &service.list_proxy_models().await.map_err(failed)?));
    }
    if get && let Some(id) = &character_id && action.is_none() {
        return Ok(send(StatusCode::OK, &service.read_profile(id).await.map_err(failed)?));
    }
    trusted_json_mutation(headers)?;
    let body = read_json(body)?;
    let post = *method == Method::POST;
    match (character_id.as_deref(), action) {
        _ if post && path == "/api/roleplay/assets" => {
            let result = service.import_asset(&body).await.map_err(failed)?;
            let status = if result["created"] == json!(true) { StatusCode::CREATED } else { StatusCode::OK };
            Ok(send(status, &result))
        }
        (Some(id), None) if *method == Method::PUT => Ok(send(StatusCode::OK, &service.update_profile(id, &body).await.map_err(failed)?)),
        (Some(id), Some("chats")) if post => Ok(send(StatusCode::CREATED, &service.start_new_chat(id).await.map_err(failed)?)),
        (Some(id), Some("messages")) if post => Ok(send(StatusCode::ACCEPTED, &service.send_message(id, &body).await.map_err(failed)?)),
        _ => Ok(error(StatusCode::METHOD_NOT_ALLOWED, "지원하지 않는 롤플레이 요청입니다.")),
    }
}

/// Node의 workBoardEndpoint. 조회 경로가 먼저이고, 그 밖은 형식·출처 검사와 본문 해석 뒤에 고른다.
async fn work_board_route(
    pool: &PgPool,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, Response> {
    let segments: Vec<&str> = path.split('/').collect();
    let route = segments.get(3..).unwrap_or_default();
    let id = || decode_uri_component(segments.get(4).copied().unwrap_or_default());
    let get = *method == Method::GET;
    match route {
        [] if get => return Ok(send(StatusCode::OK, &list_board(pool).await.map_err(work_board_error)?)),
        ["records", record] if get && !record.is_empty() => {
            let record = read_board_work_record(pool, &id()?).await.map_err(work_board_error)?;
            return Ok(send(StatusCode::OK, &json!({ "record": record })));
        }
        ["tickets", ticket, "activity"] if get && !ticket.is_empty() => {
            let activity = list_ticket_activity(pool, &id()?).await.map_err(work_board_error)?;
            return Ok(send(StatusCode::OK, &json!({ "activity": activity })));
        }
        _ => {}
    }
    trusted_json_mutation(headers)?;
    let body = read_json(body)?;
    let post = *method == Method::POST;
    let patch = *method == Method::PATCH;
    let (status, key, result) = match route {
        ["projects"] if post => (StatusCode::CREATED, "project", create_project(pool, &body).await),
        ["tickets"] if post => {
            (StatusCode::CREATED, "ticket", with_transaction(pool, async |connection| create_ticket(connection, &body).await).await)
        }
        ["tickets", ticket, "activity"] if post && !ticket.is_empty() => {
            let ticket = id()?;
            let result = with_transaction(pool, async |connection| add_ticket_activity(connection, &ticket, &body).await).await;
            (StatusCode::CREATED, "activity", result)
        }
        ["goals"] if post => {
            (StatusCode::CREATED, "goal", with_transaction(pool, async |connection| create_goal(connection, &body).await).await)
        }
        ["projects", project] if patch && !project.is_empty() => {
            (StatusCode::OK, "project", update_project(pool, &id()?, &body).await)
        }
        ["tickets", ticket] if patch && !ticket.is_empty() => {
            let ticket = id()?;
            let result = with_transaction(pool, async |connection| update_ticket(connection, &ticket, &body).await).await;
            (StatusCode::OK, "ticket", result)
        }
        ["goals", goal] if patch && !goal.is_empty() => {
            let goal = id()?;
            (StatusCode::OK, "goal", with_transaction(pool, async |connection| update_goal(connection, &goal, &body).await).await)
        }
        _ => return Ok(error(StatusCode::METHOD_NOT_ALLOWED, "지원하지 않는 업무 보드 요청입니다.")),
    };
    let value = result.map_err(work_board_error)?;
    Ok(send(status, &json!({ key: value })))
}

fn rag_failure(failure: RagFailure) -> Response {
    error(StatusCode::from_u16(failure.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), &failure.message)
}

/// Node의 addRAGDocument. 출처 검사 없이 본문만 읽는다.
async fn rag_documents_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    match add_rag_document(&mut RagDatabase::Pool(&state.pool), &body).await {
        Ok(row) => send(StatusCode::CREATED, &row),
        Err(failure) => rag_failure(failure),
    }
}

/// Node의 searchRAG. 임베딩이 실패해 전문검색으로 대체하면 사유를 경고로 남긴다.
async fn rag_search_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let fields = (|| Ok::<_, String>((js_property(&body, "query")?, js_property(&body, "embedding")?, js_property(&body, "limit")?)))();
    let (query, embedding, limit) = match fields {
        Ok(fields) => fields,
        Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    };
    let request = SearchRequest {
        query,
        embedding,
        limit,
        service: Some(&state.embedding),
        minimum_score: rag_vector_minimum_score(std::env::var("OFFICE_RAG_VECTOR_MIN_SCORE").ok().as_deref()),
    };
    match search_rag_documents(&mut RagDatabase::Pool(&state.pool), request).await {
        Ok(result) => {
            if let Some(fallback) = result.fallback_error.filter(|fallback| !fallback.is_empty()) {
                eprintln!("임베딩 비활성(사유): {fallback}. PostgreSQL 전문검색으로 대체했습니다.");
            }
            send(StatusCode::OK, &json!({ "documents": result.documents }))
        }
        Err(database_error) => error(StatusCode::INTERNAL_SERVER_ERROR, &database_message(&database_error)),
    }
}

/// Node routeWikiPage·routeWikiProposalAction. 퍼센트 인코딩을 풀지 않은 경로에 대소문자 무시로 맞춘다.
static WIKI_PAGE_ROUTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i-u)^/api/wiki/pages/([0-9a-f-]{36})$").expect("위키 페이지 경로 정규식"));
static WIKI_PROPOSAL_ACTION_ROUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i-u)^/api/wiki/proposals/([0-9a-f-]{36})/(verify|approve|reject)$").expect("위키 제안 경로 정규식")
});
const USER_DECISION_HEADER: &str = "x-officestra-user-decision";
const USER_DECISION_REQUIRED: &str = "사내 위키 화면에서 사용자가 직접 결정해야 합니다.";

fn repository_root(state: &AppState) -> Option<&str> {
    state.runtime.repository_root.as_deref().and_then(std::path::Path::to_str)
}

fn wiki_response(status: StatusCode, key: &str, result: Result<Value, WikiError>) -> Response {
    match result {
        Ok(value) => send(status, &json!({ key: value })),
        Err(wiki_error) => wiki_error_response(&wiki_error),
    }
}

fn wiki_error_response(wiki_error: &WikiError) -> Response {
    error(StatusCode::from_u16(wiki_error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), &wiki_error.message())
}

fn user_decision(headers: &HeaderMap) -> Option<&str> {
    headers.get(USER_DECISION_HEADER).and_then(|value| value.to_str().ok())
}

async fn wiki_pages_handler(State(state): State<AppState>, uri: Uri) -> Response {
    let parameters = SearchParams::parse(uri.query());
    let pages = list_wiki_pages(&state.pool, parameters.get("query"), parameters.get("limit"), repository_root(&state)).await;
    match pages {
        Ok(pages) => send(StatusCode::OK, &json!({ "pages": pages })),
        Err(wiki_error) => wiki_error_response(&wiki_error),
    }
}

/// Node의 wikiPageEndpoint·deleteWikiPageEndpoint. 삭제는 화면 버튼이 보내는 `delete:{id}` 헤더가 있어야 보관 처리한다.
async fn wiki_page_handler(State(state): State<AppState>, method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let Some(page_id) = WIKI_PAGE_ROUTE.captures(uri.path()).map(|captures| captures[1].to_string()) else {
        return not_found().await;
    };
    let root = repository_root(&state);
    if method == Method::GET {
        return match get_wiki_page(&state.pool, &page_id, root).await {
            Ok(Some(page)) => send(StatusCode::OK, &json!({ "page": page })),
            Ok(None) => error(StatusCode::NOT_FOUND, "위키 페이지를 찾을 수 없습니다."),
            Err(wiki_error) => wiki_error_response(&wiki_error),
        };
    }
    if method != Method::DELETE {
        return not_found().await;
    }
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    if user_decision(&headers) != Some(format!("delete:{page_id}").as_str()) {
        return error(StatusCode::FORBIDDEN, USER_DECISION_REQUIRED);
    }
    let page = with_transaction(&state.pool, async |connection| archive_wiki_page(connection, &page_id, root).await).await;
    wiki_response(StatusCode::OK, "page", page)
}

async fn wiki_proposals_handler(State(state): State<AppState>, uri: Uri) -> Response {
    let parameters = SearchParams::parse(uri.query());
    match list_wiki_proposals(&state.pool, parameters.get("state"), repository_root(&state)).await {
        Ok(proposals) => send(StatusCode::OK, &json!({ "proposals": proposals })),
        Err(wiki_error) => wiki_error_response(&wiki_error),
    }
}

/// Node의 createWikiProposalEndpoint. 공개 API는 사용자 승인 등급의 pending_user 제안만 만든다.
async fn create_wiki_proposal_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    match js_property(&body, "approvalTier") {
        Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, &message),
        Ok(Some(tier)) if !tier.is_null() && tier.as_str() != Some("user") => {
            return error(StatusCode::BAD_REQUEST, "공개 제안 API는 사용자 승인 등급만 만들 수 있습니다.");
        }
        Ok(_) => {}
    }
    let input = NewProposal {
        project_id: None,
        repository_root: repository_root(&state),
        page_key: body.get("pageKey"),
        approval_grade: "user",
        state: "pending_user",
        kind: body.get("kind"),
        ordinal: None,
        draft_title: body.get("title"),
        draft_body: body.get("body"),
        source_work_record_ids: body.get("sourceRecordIds"),
        author_character_id: None,
        author_turn_id: None,
        base_version: body.get("baseVersion"),
    };
    let proposal = with_transaction(&state.pool, async |connection| create_wiki_proposal(connection, input).await).await;
    wiki_response(StatusCode::CREATED, "proposal", proposal)
}

/// Node의 wikiProposalActionEndpoint. 경로는 대소문자를 무시하지만 헤더 검사와 분기는 소문자 동작 이름만 본다.
/// 그래서 `/APPROVE`·`/Verify`처럼 대소문자가 다른 경로는 헤더 없이 승인 분기로 간다. Node의 결함을 그대로 옮겼다(보고 대상).
async fn wiki_proposal_action_handler(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(captures) = WIKI_PROPOSAL_ACTION_ROUTE.captures(uri.path()) else {
        return not_found().await;
    };
    let (proposal_id, action) = (captures[1].to_string(), captures[2].to_string());
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    if ["approve", "reject"].contains(&action.as_str())
        && user_decision(&headers) != Some(format!("{action}:{proposal_id}").as_str())
    {
        return error(StatusCode::FORBIDDEN, USER_DECISION_REQUIRED);
    }
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let root = repository_root(&state);
    if action == "verify" || action == "reject" {
        let field = if action == "verify" { "characterId" } else { "reason" };
        let value = match js_property(&body, field) {
            Ok(value) => value,
            Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, &message),
        };
        let proposal = with_transaction(&state.pool, async |connection| {
            if action == "verify" {
                verify_wiki_proposal(connection, &proposal_id, value, root).await
            } else {
                reject_wiki_proposal(connection, &proposal_id, value, root).await
            }
        })
        .await;
        return wiki_response(StatusCode::OK, "proposal", proposal);
    }
    let outcome =
        with_transaction(&state.pool, async |connection| approve_wiki_proposal(connection, &proposal_id, "user", None, root).await)
            .await;
    match outcome {
        Ok(outcome) => {
            let status = if outcome.conflicted { StatusCode::CONFLICT } else { StatusCode::OK };
            send(status, &json!({ "proposal": outcome.proposal }))
        }
        // 같은 pageKey 동시 게시는 유니크 제약으로 트랜잭션이 끝나므로 새 트랜잭션에서 conflict로 종결한다.
        Err(wiki_error) if wiki_error.is_page_key_race() => {
            let proposal = with_transaction(&state.pool, async |connection| {
                resolve_wiki_page_key_race(connection, &proposal_id, root).await
            })
            .await;
            wiki_response(StatusCode::CONFLICT, "proposal", proposal)
        }
        Err(wiki_error) => wiki_error_response(&wiki_error),
    }
}

fn work_board_error(board_error: WorkBoardError) -> Response {
    let status = StatusCode::from_u16(board_error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    error(status, &board_error.to_string())
}

/// 조회 결과 응답. 검증 400, 대상 없음 404, 그 밖은 500과 오류 문구다.
fn feed_response(result: Result<Value, FeedError>) -> Response {
    match result {
        Ok(body) => send(StatusCode::OK, &body),
        Err(feed_error) => error(
            StatusCode::from_u16(feed_error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            &feed_error.message(),
        ),
    }
}

async fn active_sessions_handler(State(state): State<AppState>) -> Response {
    feed_response(list_active_sessions(&state.pool).await)
}

async fn history_handler(State(state): State<AppState>, uri: Uri) -> Response {
    feed_response(global_history(&state.pool, &SearchParams::parse(uri.query())).await)
}

async fn work_records_handler(State(state): State<AppState>, uri: Uri) -> Response {
    feed_response(list_work_records(&state.pool, &SearchParams::parse(uri.query())).await)
}

async fn turn_sources_handler(State(state): State<AppState>, Path(turn_id): Path<String>) -> Response {
    feed_response(list_turn_sources(&state.pool, &turn_id).await)
}

/// PUT /api/turns/:id/sources. 바꾼 뒤 그 턴의 피드 변경을 방송한다.
async fn replace_turn_sources_handler(
    State(state): State<AppState>,
    Path(turn_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = match trusted_json_mutation(&headers).and_then(|()| read_json(&body)) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let stored = replace_turn_sources(&state.pool, &turn_id, &body).await;
    if stored.is_ok() {
        state.realtime.broadcast(&json!({ "type": "feed.changed", "turnId": turn_id }));
    }
    feed_response(stored)
}

/// PUT /api/turns/:id/feedback(Node updateTurnFeedback). 완료된 턴만 평가하고, 비용·평가 요약이 다시 계산되게 방송한다.
async fn turn_feedback_handler(
    State(state): State<AppState>,
    Path(turn_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = match trusted_json_mutation(&headers).and_then(|()| read_json(&body)) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if !is_uuid(&turn_id) {
        return error(StatusCode::BAD_REQUEST, "turnId 값은 UUID여야 합니다.");
    }
    let feedback = match normalize_turn_feedback(body.as_object().and_then(|fields| fields.get("feedback"))) {
        Ok(feedback) => feedback,
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    let stored = with_transaction(&state.pool, async |connection| {
        replace_turn_feedback(connection, &turn_id, feedback.as_deref()).await
    })
    .await;
    match stored {
        Ok(FeedbackOutcome::Missing) => error(StatusCode::NOT_FOUND, "대화를 찾을 수 없습니다."),
        Ok(FeedbackOutcome::Unavailable) => error(StatusCode::CONFLICT, "완료된 대화 응답만 평가할 수 있습니다."),
        Ok(FeedbackOutcome::Stored(feedback)) => {
            state.realtime.broadcast(&json!({ "type": "feed.changed", "turnId": turn_id, "feedbackChanged": true }));
            send(StatusCode::OK, &json!({ "feedback": feedback }))
        }
        Err(database_error) => error(StatusCode::INTERNAL_SERVER_ERROR, &database_message(&database_error)),
    }
}

/// DELETE /api/turns/:id(Node deleteTurn). 지운 턴의 직원 피드와 비용 요약이 바뀌었다고 방송한다.
async fn delete_turn_handler(
    State(state): State<AppState>,
    Path(turn_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = trusted_json_mutation(&headers).and_then(|()| read_json(&body)) {
        return response;
    }
    match delete_archived_turn(&state.pool, &turn_id).await {
        Ok(None) => error(StatusCode::NOT_FOUND, "대화를 찾을 수 없습니다."),
        Ok(Some(deleted)) => {
            state.realtime.broadcast(&json!({
                "type": "feed.changed",
                "turnId": deleted.id,
                "characterId": deleted.character_id,
                "costChanged": true,
            }));
            send(StatusCode::OK, &deleted.response())
        }
        Err(deletion_error) => error(
            StatusCode::from_u16(deletion_error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            &deletion_error.to_string(),
        ),
    }
}

async fn reply_routes_handler(State(state): State<AppState>) -> Response {
    match read_reply_routes(&state.pool).await {
        Ok(routes) => send(StatusCode::OK, &json!({ "routes": routes })),
        Err(database_error) => error(StatusCode::INTERNAL_SERVER_ERROR, &database_message(&database_error)),
    }
}

/// PUT /api/reply-routes/:id. Node처럼 출처 검사 뒤에 경로 조각을 풀고, 저장 뒤 전달 설정과 피드 변경을 차례로 방송한다.
async fn save_reply_route_handler(State(state): State<AppState>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers) {
        return response;
    }
    let character_id = match decode_uri_component(uri.path().split('/').nth(3).unwrap_or("")) {
        Ok(character_id) => character_id,
        Err(response) => return response,
    };
    let body = match read_json(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let saved =
        with_transaction(&state.pool, async |connection| save_reply_route(connection, &character_id, &body).await).await;
    match saved {
        Ok(route) => {
            state.realtime.broadcast(&json!({ "type": "reply-routes.changed", "characterId": character_id }));
            state.realtime.broadcast(&json!({ "type": "feed.changed", "characterId": character_id }));
            send(StatusCode::OK, &route)
        }
        Err(RouteSaveError::Invalid(message)) => error(StatusCode::BAD_REQUEST, &message),
        Err(database_error) => error(StatusCode::INTERNAL_SERVER_ERROR, &database_error.to_string()),
    }
}

/// POST /api/reply-deliveries/:id/cancel. 경로 조각은 Node처럼 풀지 않고 UUID인지만 본다.
async fn cancel_reply_delivery_handler(State(state): State<AppState>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = trusted_json_mutation(&headers).and_then(|()| read_json(&body)) {
        return response;
    }
    let source_turn_id = uri.path().split('/').nth(3).unwrap_or("");
    if !is_uuid(source_turn_id) {
        return error(StatusCode::BAD_REQUEST, "올바른 대화 ID가 필요합니다.");
    }
    match cancel_pending_reply_delivery(&state.pool, source_turn_id).await {
        Ok(true) => {
            state.realtime.broadcast(&json!({ "type": "feed.changed", "turnId": source_turn_id }));
            send(StatusCode::OK, &json!({ "cancelled": true }))
        }
        Ok(false) => error(StatusCode::CONFLICT, "이미 전달을 시작했거나 종료된 예약입니다."),
        Err(database_error) => error(StatusCode::INTERNAL_SERVER_ERROR, &database_message(&database_error)),
    }
}

async fn live_feed_handler(State(state): State<AppState>, uri: Uri) -> Response {
    feed_response(live_feed(&state.pool, &state.cost_summary, &SearchParams::parse(uri.query())).await)
}

async fn archive_feed_handler(State(state): State<AppState>, uri: Uri) -> Response {
    feed_response(archive_feed(&state.pool, &SearchParams::parse(uri.query())).await)
}

async fn live_feed_turn_handler(State(state): State<AppState>, Path(turn_id): Path<String>) -> Response {
    feed_response(live_feed_turn(&state.pool, &state.cost_summary, &turn_id).await)
}

/// Node의 usageReport. 집계 단위는 day, 시간대(tz)는 UTC가 기본이다.
async fn usage_report_handler(State(state): State<AppState>, uri: Uri) -> Response {
    let parameters = SearchParams::parse(uri.query());
    let report =
        read_usage_report(&state.pool, parameters.get("backend"), parameters.get("granularity"), parameters.get("tz")).await;
    match report {
        Ok(report) => send(StatusCode::OK, &report),
        Err(failure) => {
            error(StatusCode::from_u16(failure.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), &failure.message)
        }
    }
}

/// JS decodeURIComponent. 잘못된 % 순서나 UTF-8이 아니면 Node처럼 500 "URI malformed"다.
pub fn decode_uri_component(text: &str) -> Result<String, Response> {
    crate::js::decode_uri_component(text).ok_or_else(|| error(StatusCode::INTERNAL_SERVER_ERROR, "URI malformed"))
}

/// Node의 readJSON(): 빈 본문은 빈 객체, 해석 실패는 요청 처리기의 500 경로로 간다.
fn read_json(body: &Bytes) -> Result<Value, Response> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|parse_error| error(StatusCode::INTERNAL_SERVER_ERROR, &parse_error.to_string()))
}

fn with_ok(status: impl Serialize) -> Value {
    let mut value = json!({ "ok": true });
    if let (Some(target), Ok(Value::Object(fields))) = (value.as_object_mut(), serde_json::to_value(status)) {
        target.extend(fields);
    }
    value
}

/// 상태 변경 요청의 형식·출처 검사. Node의 trustedJSONMutation과 같은 규칙이다.
pub fn trusted_json_mutation(headers: &HeaderMap) -> Result<(), Response> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.starts_with("application/json") {
        return Err(error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "상태 변경 요청은 application/json 형식이어야 합니다.",
        ));
    }
    let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) else {
        return Ok(());
    };
    let host = headers.get(HOST).and_then(|value| value.to_str().ok()).unwrap_or("");
    if origin_is_trusted(origin, host) {
        return Ok(());
    }
    Err(error(StatusCode::FORBIDDEN, "신뢰할 수 없는 요청 출처입니다."))
}

/// Origin의 호스트 이름이 루프백이고 host(포트 포함)가 요청의 Host 헤더와 같을 때만 믿는다.
pub fn origin_is_trusted(origin: &str, request_host: &str) -> bool {
    let Some(rest) = origin.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostname = if let Some(stripped) = host.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    ["127.0.0.1", "localhost", "::1"].contains(&hostname) && host == request_host
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 루프백_출처만_host가_같을_때_믿는다() {
        assert!(origin_is_trusted("http://127.0.0.1:4318", "127.0.0.1:4318"));
        assert!(origin_is_trusted("http://localhost:4318/", "localhost:4318"));
        assert!(origin_is_trusted("http://[::1]:4318", "[::1]:4318"));
        assert!(!origin_is_trusted("http://127.0.0.1:4317", "127.0.0.1:4318"));
        assert!(!origin_is_trusted("http://example.com", "example.com"));
        assert!(!origin_is_trusted("not a url", "127.0.0.1:4318"));
    }
}
