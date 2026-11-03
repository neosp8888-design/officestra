// 직원의 자연어 응답과 근거·위키 제안 메타데이터를 분리하는 별도 결과 파일. Node structured-turn-result.mjs를 옮겼다.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::time::Duration;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::agent_event_parser::normalized_wiki_proposals;
use crate::js::{is_js_whitespace, join_path, node_fs_message as node_message, node_tmpdir, parse_js_json, random_uuid, uv_error};
use crate::work_record_provenance::normalize_response_sources;

const RESULT_VERSION: f64 = 1.0;
const MAX_RESULT_BYTES: u64 = 256 * 1024;
const LEGACY_PROTOCOL_PREFIX: &str = "\n근거를 쓴 응답";
const LEGACY_SOURCES_MARKER: &str = "\n[OFFICE_SOURCES]";

pub const STRUCTURED_RESULT_ENV: &str = "OFFICESTRA_RESULT_PATH";
pub const STRUCTURED_RESULT_GUIDANCE: &str = "답변 본문에 근거·위키 JSON을 쓰지 않는다. 실제 사용한 근거나 지속 지식 제안이 있을 때만 완료 전에 `officestra-result` 명령으로 별도 제출한다(`officestra-result --help`).";
const EMPLOYEE_MESSAGE_STATUS_GUIDANCE: &str = "직접 API 전송을 명시적으로 요청받은 경우에만, 직원에게 보내기 전 GET /api/agent-jobs?characterId=수신직원ID의 canReceive로 실행 상태를 확인한다. work-records의 lifecycleState: active는 보존 중인 기록이며 실행 상태가 아니다. active 세션도 열린 대화일 뿐이다. 이 두 active를 이유로 기다리거나 삭제하지 않는다. POST 응답 202는 접수, 409는 해당 수신 직원이 현재 접수 불가라는 뜻이다. 409를 풀려고 DELETE /api/agent-jobs를 호출하지 않는다. DELETE는 기록 정리가 아니라 실제 업무 중단이다. 수신자가 바쁘면 현재 턴을 마무리하고, 발신자 자신의 상태가 끝나길 기다리지 않는다.";

/// identityPromptWithStructuredResult. 예전 근거 JSON 계약을 잘라 내고 별도 제출 안내를 붙인다.
/// 직원 ID가 있으면 발신자 표시와 접수 상태 확인 안내를 더한다. 이미 붙은 안내는 다시 붙지 않는다.
pub fn identity_prompt_with_structured_result(value: &str, character_id: Option<&str>) -> String {
    let sources_marker_index = value.find(LEGACY_SOURCES_MARKER);
    let legacy_prefix_index = value.find(LEGACY_PROTOCOL_PREFIX);
    let legacy_index = match (sources_marker_index, legacy_prefix_index) {
        (Some(marker), Some(prefix)) if prefix < marker => Some(prefix),
        _ => sources_marker_index,
    };
    let guidance_index = value.find(&format!("\n{STRUCTURED_RESULT_GUIDANCE}"));
    let protocol_index = [legacy_index, guidance_index].into_iter().flatten().min();
    let identity = protocol_index.map_or(value, |index| &value[..index]).trim_end_matches(is_js_whitespace);
    let mut parts = vec![identity.to_string(), String::new(), STRUCTURED_RESULT_GUIDANCE.to_string()];
    if let Some(id) = character_id.filter(|id| !id.is_empty()) {
        let quoted = serde_json::to_string(id).unwrap_or_default();
        parts.push(format!(
            "현재 실행 직원의 ID는 {quoted}이다. 직원에게 POST /api/agent-jobs로 메시지를 보낼 때 JSON에 senderCharacterId: {quoted}를 포함한다. 수신 직원은 characterId에 지정하며, 다른 직원에게 답할 때 자신의 ID를 수신자로 넣거나 발신자와 수신자를 뒤집지 않는다. 대화 속 배역과 실제 실행 직원 ID를 혼동하지 않는다. 이는 말풍선 발신자 표시용이며 권한 증명이 아니다."
        ));
        parts.push(EMPLOYEE_MESSAGE_STATUS_GUIDANCE.to_string());
    }
    if parts[0].is_empty() {
        parts.remove(0);
    }
    parts.join("\n")
}

/// structuredTurnResultPath. `<tmpdir>/officestra-turn-results/<실행 ID>/<workdir sha256 앞 20자>/<직원 ID>.json`.
/// 직원 ID의 허용되지 않은 글자는 UTF-16 단위마다 `_`로 바꾸고 80자로 자른다.
pub fn structured_turn_result_path(workdir: &str, character_id: Option<&str>, runtime_id: &str) -> String {
    let digest = Sha256::digest(workdir.as_bytes());
    let workdir_key: String = digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()[..20].to_string();
    let mut safe = String::new();
    for character in character_id.unwrap_or("unknown").chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            safe.push(character);
        } else {
            safe.extend(std::iter::repeat_n('_', character.len_utf16()));
        }
    }
    safe.truncate(80);
    if safe.is_empty() {
        safe = "unknown".into();
    }
    join_path(&[&node_tmpdir(), "officestra-turn-results", runtime_id, &workdir_key, &format!("{safe}.json")])
}

/// prepareStructuredTurnResult. 남은 잠금을 지우고 빈 결과 파일을 만든 뒤 경로를 돌려준다.
pub fn prepare_structured_turn_result(workdir: &str, character_id: Option<&str>, runtime_id: &str) -> Result<String, String> {
    let path = structured_turn_result_path(workdir, character_id, runtime_id);
    remove_force(&format!("{path}.lock"));
    write_stored_state(&path, &empty_stored_state())?;
    Ok(path)
}

/// discardStructuredTurnResult.
pub fn discard_structured_turn_result(path: Option<&str>) {
    let Some(path) = path.filter(|path| !path.is_empty()) else {
        return;
    };
    remove_force(path);
    remove_force(&format!("{path}.lock"));
}

/// submitStructuredResponseSource. 잠금 안에서 기존 근거 뒤에 하나를 더해 다시 검증하고 저장한다.
pub fn submit_structured_response_source(path: &str, value: Value) -> Result<usize, String> {
    with_stored_state_lock(path, || {
        let mut state = read_stored_state(path)?;
        let mut sources = state.sources.clone();
        sources.push(value);
        let normalized = normalize_response_sources(&Value::Array(sources), 20).map_err(|error| error.0)?;
        state.sources = public_response_sources(&normalized);
        state.sources_submitted = true;
        write_stored_state(path, &state)?;
        Ok(state.sources.len())
    })
}

/// submitStructuredWikiProposal.
pub fn submit_structured_wiki_proposal(path: &str, value: Value) -> Result<usize, String> {
    with_stored_state_lock(path, || {
        let mut state = read_stored_state(path)?;
        let mut proposals = state.wiki_proposals.clone();
        proposals.push(value);
        state.wiki_proposals = normalized_wiki_proposals(&Value::Array(proposals))?;
        state.wiki_proposals_submitted = true;
        write_stored_state(path, &state)?;
        Ok(state.wiki_proposals.len())
    })
}

/// 결과 파일에 저장된 형태(공개 근거 형식).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredState {
    pub sources_submitted: bool,
    pub wiki_proposals_submitted: bool,
    pub sources: Vec<Value>,
    pub wiki_proposals: Vec<Value>,
}

impl StoredState {
    fn to_json(&self) -> Value {
        json!({
            "version": 1,
            "sourcesSubmitted": self.sources_submitted,
            "wikiProposalsSubmitted": self.wiki_proposals_submitted,
            "sources": self.sources,
            "wikiProposals": self.wiki_proposals,
        })
    }
}

/// 턴 완료 때 읽은 결과. 근거는 저장 형식(normalizeResponseSources)이다.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StructuredTurnResult {
    pub sources_submitted: bool,
    pub wiki_proposals_submitted: bool,
    pub sources: Vec<Value>,
    pub proposals: Vec<Value>,
    pub metadata_error: Option<String>,
}

/// inspectStructuredTurnResult.
pub fn inspect_structured_turn_result(path: &str) -> Result<StoredState, String> {
    read_stored_state(path)
}

/// readStructuredTurnResult. 파일 문제는 응답을 실패시키지 않고 metadataError로 돌린다.
pub fn read_structured_turn_result(path: Option<&str>) -> StructuredTurnResult {
    let Some(path) = path.filter(|path| !path.is_empty()) else {
        return StructuredTurnResult::default();
    };
    let read = || -> Result<StructuredTurnResult, String> {
        let state = read_stored_state(path)?;
        Ok(StructuredTurnResult {
            sources_submitted: state.sources_submitted,
            wiki_proposals_submitted: state.wiki_proposals_submitted,
            sources: normalize_response_sources(&Value::Array(state.sources), 20).map_err(|error| error.0)?,
            proposals: normalized_wiki_proposals(&Value::Array(state.wiki_proposals))?,
            metadata_error: None,
        })
    };
    read().unwrap_or_else(|message| StructuredTurnResult {
        metadata_error: Some(format!("별도 응답 메타데이터를 읽지 못했습니다. {message}")),
        ..StructuredTurnResult::default()
    })
}

/// consumeStructuredTurnResult. 읽은 뒤 파일과 잠금을 지운다.
pub fn consume_structured_turn_result(path: Option<&str>) -> StructuredTurnResult {
    let result = read_structured_turn_result(path);
    discard_structured_turn_result(path);
    result
}

/// applyStructuredTurnResult. 별도로 제출한 채널만 응답 해석값을 대체한다.
pub fn apply_structured_turn_result(decoded: &Value, structured: Option<&StructuredTurnResult>) -> Value {
    let mut result = decoded.as_object().cloned().unwrap_or_default();
    let Some(structured) = structured else {
        return Value::Object(result);
    };
    if let Some(error) = &structured.metadata_error {
        for key in ["sourceError", "wikiProposalError"] {
            if result.get(key).is_none_or(Value::is_null) {
                result.insert(key.into(), Value::String(error.clone()));
            }
        }
        return Value::Object(result);
    }
    if structured.sources_submitted {
        result.insert("sources".into(), Value::Array(structured.sources.clone()));
        result.shift_remove("sourceError");
    }
    if structured.wiki_proposals_submitted {
        result.insert("proposals".into(), Value::Array(structured.proposals.clone()));
        result.insert("wikiProposalError".into(), Value::Null);
    }
    Value::Object(result)
}

fn empty_stored_state() -> StoredState {
    StoredState { sources_submitted: false, wiki_proposals_submitted: false, sources: Vec::new(), wiki_proposals: Vec::new() }
}

fn read_stored_state(path: &str) -> Result<StoredState, String> {
    if path.is_empty() {
        return Err("OFFICESTRA_RESULT_PATH가 없습니다.".into());
    }
    let statistics = fs::symlink_metadata(path).map_err(|error| node_message(&error, "lstat", path))?;
    if !statistics.is_file() {
        return Err("응답 메타데이터 경로가 일반 파일이 아닙니다.".into());
    }
    if statistics.len() > MAX_RESULT_BYTES {
        return Err("응답 메타데이터가 허용 크기를 초과했습니다.".into());
    }
    let bytes = fs::read(path).map_err(|error| node_message(&error, "open", path))?;
    let parsed = parse_js_json(&String::from_utf8_lossy(&bytes)).map_err(|error| error.to_string())?;
    let invalid = || "응답 메타데이터 파일 형식이 올바르지 않습니다.".to_string();
    let Value::Object(parsed) = parsed else {
        return Err(invalid());
    };
    let flag = |key: &str| parsed.get(key).and_then(Value::as_bool);
    let list = |key: &str| parsed.get(key).and_then(Value::as_array).cloned();
    let (Some(sources_submitted), Some(wiki_proposals_submitted), Some(sources), Some(wiki_proposals)) =
        (flag("sourcesSubmitted"), flag("wikiProposalsSubmitted"), list("sources"), list("wikiProposals"))
    else {
        return Err(invalid());
    };
    if parsed.get("version").and_then(Value::as_f64) != Some(RESULT_VERSION) {
        return Err(invalid());
    }
    let sources = normalize_response_sources(&Value::Array(sources), 20).map_err(|error| error.0)?;
    Ok(StoredState {
        sources_submitted,
        wiki_proposals_submitted,
        sources: public_response_sources(&sources),
        wiki_proposals: normalized_wiki_proposals(&Value::Array(wiki_proposals))?,
    })
}

/// 같은 폴더의 임시 파일(0600, 새로 만들기)에 쓰고 rename으로 바꾼다.
fn write_stored_state(path: &str, state: &StoredState) -> Result<(), String> {
    let directory = std::path::Path::new(path).parent().map_or_else(|| ".".into(), |parent| parent.to_string_lossy().into_owned());
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .map_err(|error| node_message(&error, "mkdir", &directory))?;
    let temporary = format!("{path}.{}.{}.tmp", std::process::id(), random_uuid());
    let written = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options.open(&temporary).map_err(|error| node_message(&error, "open", &temporary))?;
        let body = format!("{}\n", serde_json::to_string(&state.to_json()).map_err(|error| error.to_string())?);
        io::Write::write_all(&mut file, body.as_bytes()).map_err(|error| node_message(&error, "write", &temporary))?;
        fs::rename(&temporary, path).map_err(|error| {
            let (code, description) = uv_error(&error);
            format!("{code}: {description}, rename '{temporary}' -> '{path}'")
        })?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| node_message(&error, "chmod", path))
    })();
    remove_force(&temporary);
    written
}

fn with_stored_state_lock<T>(path: &str, operation: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let lock = format!("{path}.lock");
    let mut locked = false;
    for _ in 0..200 {
        match fs::DirBuilder::new().mode(0o700).create(&lock) {
            Ok(()) => {
                locked = true;
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(node_message(&error, "mkdir", &lock)),
        }
    }
    if !locked {
        return Err("응답 메타데이터 제출 잠금을 얻지 못했습니다.".into());
    }
    let result = operation();
    remove_force(&lock);
    result
}

/// rmSync(path, { recursive: true, force: true }).
fn remove_force(path: &str) {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// publicResponseSources. 저장 형식 근거를 제출 형식(kind·ragDocumentId·workRecordId)으로 되돌린다.
pub fn public_response_sources(sources: &[Value]) -> Vec<Value> {
    sources
        .iter()
        .map(|source| {
            let mut public = Map::new();
            let field = |key: &str| source.get(key).cloned().unwrap_or(Value::Null);
            public.insert("kind".into(), field("sourceKind"));
            public.insert("title".into(), field("title"));
            public.insert("locator".into(), field("locator"));
            for (from, to) in [("excerpt", "excerpt"), ("ragDocumentID", "ragDocumentId"), ("workRecordID", "workRecordId")] {
                if let Some(value) = source.get(from).filter(|value| value.as_str().is_some_and(|text| !text.is_empty())) {
                    public.insert(to.into(), value.clone());
                }
            }
            if let Some(metadata) = source.get("metadata").filter(|metadata| metadata.as_object().is_some_and(|fields| !fields.is_empty())) {
                public.insert("metadata".into(), metadata.clone());
            }
            Value::Object(public)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAG_DOCUMENT_ID: &str = "11111111-1111-4111-8111-111111111111";
    const WORK_RECORD_ID: &str = "22222222-2222-4222-8222-222222222222";

    fn unique(name: &str) -> String {
        format!("{name}-{}-{}", std::process::id(), random_uuid())
    }

    fn cleanup(path: &str) {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn 직원_api_발신자_안내는_실제_직원_id를_쓰고_중복되지_않는다() {
        let once = identity_prompt_with_structured_result("업무 지침", Some("right-man"));
        assert!(once.contains(r#"senderCharacterId: "right-man""#));
        assert!(once.contains(r#"현재 실행 직원의 ID는 "right-man""#));
        assert!(once.contains("lifecycleState: active는 보존 중인 기록이며 실행 상태가 아니다"));
        assert!(once.contains("GET /api/agent-jobs?characterId=수신직원ID의 canReceive"));
        assert!(once.contains("DELETE는 기록 정리가 아니라 실제 업무 중단"));
        for phrase in ["자동 전달", "답변 전달 태그", "직원 메시지 API를 직접 호출하지 않는다"] {
            assert!(!once.contains(phrase));
        }
        assert_eq!(identity_prompt_with_structured_result(&once, Some("right-man")), once);
        assert!(!identity_prompt_with_structured_result("사용자 지침", None).contains("senderCharacterId"));
        assert_eq!(identity_prompt_with_structured_result("", None), format!("\n{STRUCTURED_RESULT_GUIDANCE}"));
        assert_eq!(identity_prompt_with_structured_result("  \n", Some("")), format!("\n{STRUCTURED_RESULT_GUIDANCE}"));
    }

    #[test]
    fn 시스템_지침에서_긴_json_계약을_지우고_결과_통로만_안내한다() {
        let prompt = "역할 지침\n\n근거를 쓴 응답 끝에는 실제 쓴 근거만 남긴다. 형식이 틀리면 전체가 버려진다.\n[OFFICE_SOURCES]\n[{\"kind\":\"file\"}]\n[OFFICE_WIKI_PROPOSALS]\n[]";
        let result = identity_prompt_with_structured_result(prompt, None);
        assert!(result.starts_with("역할 지침"));
        assert!(!result.contains("OFFICE_SOURCES") && !result.contains("OFFICE_WIKI_PROPOSALS"));
        assert_eq!(result.matches("officestra-result").count(), 2);
        assert_eq!(identity_prompt_with_structured_result(&result, None), result);
        assert_eq!(result, format!("역할 지침\n\n{STRUCTURED_RESULT_GUIDANCE}"));

        let other = identity_prompt_with_structured_result(
            "역할 지침\n\n완료 보고 형식을 지킨다.\n\n근거를 쓴 응답 맨 끝에는 실제 쓴 근거만 남긴다.\n[OFFICE_SOURCES]\n[]\n[OFFICE_WIKI_PROPOSALS]\n[]",
            None,
        );
        assert!(other.contains("완료 보고 형식을 지킨다"));
        assert!(!other.contains("근거를 쓴 응답") && !other.contains("OFFICE_SOURCES"));
        // 마커만 있고 앞 문장이 없으면 마커에서 자른다. 앞 문장이 마커 뒤에 있으면 마커 기준이다.
        assert_eq!(identity_prompt_with_structured_result("가\n[OFFICE_SOURCES]\n근거를 쓴 응답", None), format!("가\n\n{STRUCTURED_RESULT_GUIDANCE}"));
    }

    #[test]
    fn 결과_경로는_node와_같은_규칙으로_만든다() {
        // 해시 앞 20자는 Node createHash("sha256")로 계산한 기준값이다.
        assert_eq!(
            structured_turn_result_path("/Users/neo/office", Some("right-man"), "123"),
            format!("{}/officestra-turn-results/123/03ffd78858a801de0751/right-man.json", node_tmpdir())
        );
        assert!(structured_turn_result_path("", Some("a"), "9").ends_with("/9/e3b0c44298fc1c149afb/a.json"));
        assert!(structured_turn_result_path("", Some("가😀 a/b"), "1").ends_with("/____a_b.json"));
        assert!(structured_turn_result_path("", None, "1").ends_with("/unknown.json"));
        assert!(structured_turn_result_path("", Some(""), "1").ends_with("/unknown.json"));
        let long = "x".repeat(90);
        assert!(structured_turn_result_path("", Some(&long), "1").ends_with(&format!("/{}.json", "x".repeat(80))));
    }

    #[test]
    fn 후속_조회는_파일을_소비하지_않고_새_근거도_보존한다() {
        let workdir = unique("/followup-test");
        let path = prepare_structured_turn_result(&workdir, Some("followup-test"), "rust-unit").unwrap();
        submit_structured_response_source(&path, json!({ "kind": "file", "title": "첫 근거", "locator": "first.mjs" })).unwrap();
        assert_eq!(read_structured_turn_result(Some(&path)).sources.len(), 1);
        assert!(std::path::Path::new(&path).exists());
        submit_structured_response_source(&path, json!({ "kind": "file", "title": "후속 근거", "locator": "last.mjs" })).unwrap();
        assert_eq!(consume_structured_turn_result(Some(&path)).sources.len(), 2);
        assert!(!std::path::Path::new(&path).exists());
        cleanup(&path);
    }

    #[test]
    fn 근거와_위키_제안을_검증해_저장하고_한_번만_소비한다() {
        let path = prepare_structured_turn_result(&unique("/root"), Some("right-woman"), "rust-unit").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"version\":1,\"sourcesSubmitted\":false,\"wikiProposalsSubmitted\":false,\"sources\":[],\"wikiProposals\":[]}\n"
        );
        let rag = json!({ "kind": "rag", "title": "과거 기록", "locator": format!("rag_documents:{RAG_DOCUMENT_ID}"), "ragDocumentId": RAG_DOCUMENT_ID, "excerpt": "0.65 1위" });
        assert_eq!(submit_structured_response_source(&path, rag), Ok(1));
        let database = json!({ "kind": "database", "title": "업무 기록", "locator": format!("work_records:{WORK_RECORD_ID}"), "workRecordId": WORK_RECORD_ID });
        assert_eq!(submit_structured_response_source(&path, database), Ok(2));
        let proposal = json!({ "pageKey": "response-metadata-channel", "kind": "decision", "title": "응답 메타데이터 분리", "body": "근거와 위키 제안은 자연어 응답과 분리된 통로로 제출한다.", "approvalTier": "user" });
        assert_eq!(submit_structured_wiki_proposal(&path, proposal), Ok(1));

        let stored = inspect_structured_turn_result(&path).unwrap();
        assert!(stored.sources_submitted && stored.wiki_proposals_submitted);
        assert_eq!(stored.sources.len(), 2);
        assert_eq!(stored.wiki_proposals.len(), 1);
        assert_eq!(
            stored.sources[0],
            json!({ "kind": "rag", "title": "과거 기록", "locator": format!("rag_documents:{RAG_DOCUMENT_ID}"), "excerpt": "0.65 1위", "ragDocumentId": RAG_DOCUMENT_ID })
        );

        let consumed = consume_structured_turn_result(Some(&path));
        assert_eq!(consumed.metadata_error, None);
        assert_eq!(consumed.sources[0]["ragDocumentID"], RAG_DOCUMENT_ID);
        assert_eq!(consumed.sources[1]["workRecordID"], WORK_RECORD_ID);
        assert_eq!(consumed.proposals[0]["approvalTier"], "user");
        assert!(!std::path::Path::new(&path).exists());
        cleanup(&path);
    }

    #[test]
    fn 잘못된_제출은_거절하고_기존_값을_남긴다() {
        let path = prepare_structured_turn_result(&unique("/root"), Some("left-man"), "rust-unit").unwrap();
        let error = submit_structured_response_source(&path, json!({ "kind": "file", "title": "", "locator": "a" })).unwrap_err();
        assert!(!error.is_empty());
        assert_eq!(submit_structured_wiki_proposal(&path, json!({ "pageKey": "x" })), Err("Invalid wiki proposal fields.".into()));
        assert_eq!(inspect_structured_turn_result(&path).unwrap(), empty_stored_state());
        assert!(!std::path::Path::new(&format!("{path}.lock")).exists());
        cleanup(&path);
    }

    #[test]
    fn 별도_제출값은_이전_해석값을_대체하고_미제출_채널은_그대로_둔다() {
        let decoded = json!({
            "text": "완료했습니다.",
            "needsInput": false,
            "sources": [{ "sourceKind": "file", "title": "이전", "locator": "old.md" }],
            "proposals": [{ "pageKey": "legacy" }],
            "sourceError": "이전 오류",
            "wikiProposalError": null,
        });
        let structured = StructuredTurnResult {
            sources_submitted: true,
            sources: vec![json!({ "sourceKind": "file", "title": "새 근거", "locator": "README.md" })],
            ..StructuredTurnResult::default()
        };
        let merged = apply_structured_turn_result(&decoded, Some(&structured));
        assert_eq!(merged["sources"][0]["title"], "새 근거");
        assert!(merged.get("sourceError").is_none());
        assert_eq!(merged["proposals"], decoded["proposals"]);
        let keys: Vec<&String> = merged.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["text", "needsInput", "sources", "proposals", "wikiProposalError"]);
        let wiki = StructuredTurnResult { wiki_proposals_submitted: true, ..StructuredTurnResult::default() };
        let merged = apply_structured_turn_result(&json!({ "wikiProposalError": "x", "proposals": [1] }), Some(&wiki));
        assert_eq!(merged, json!({ "wikiProposalError": null, "proposals": [] }));
        assert_eq!(apply_structured_turn_result(&decoded, None), decoded);
    }

    #[test]
    fn 손상된_메타데이터는_응답을_실패시키지_않고_경고로_둔다() {
        let path = prepare_structured_turn_result(&unique("/root"), Some("left-man"), "rust-unit").unwrap();
        fs::write(&path, "not-json\n").unwrap();
        let consumed = consume_structured_turn_result(Some(&path));
        let merged = apply_structured_turn_result(
            &json!({ "text": "자연어 답변", "needsInput": false, "sources": [], "proposals": [], "wikiProposalError": null }),
            Some(&consumed),
        );
        assert_eq!(merged["text"], "자연어 답변");
        assert!(merged["sourceError"].as_str().unwrap().starts_with("별도 응답 메타데이터를 읽지 못했습니다. "));
        assert!(merged["wikiProposalError"].as_str().unwrap().starts_with("별도 응답 메타데이터"));
        let keys: Vec<&String> = merged.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["text", "needsInput", "sources", "proposals", "wikiProposalError", "sourceError"]);
        assert!(!std::path::Path::new(&path).exists());

        let missing = consume_structured_turn_result(Some(&path));
        assert_eq!(
            missing.metadata_error.as_deref(),
            Some(format!("별도 응답 메타데이터를 읽지 못했습니다. ENOENT: no such file or directory, lstat '{path}'").as_str())
        );
        assert_eq!(consume_structured_turn_result(None), StructuredTurnResult::default());

        let path = prepare_structured_turn_result(&unique("/root"), Some("left-man"), "rust-unit").unwrap();
        for body in [r#"{"version":2,"sourcesSubmitted":false,"wikiProposalsSubmitted":false,"sources":[],"wikiProposals":[]}"#, "[]", "null"] {
            fs::write(&path, body).unwrap();
            assert_eq!(
                read_structured_turn_result(Some(&path)).metadata_error.as_deref(),
                Some("별도 응답 메타데이터를 읽지 못했습니다. 응답 메타데이터 파일 형식이 올바르지 않습니다.")
            );
        }
        fs::write(&path, r#"{"version":1.0,"sourcesSubmitted":true,"wikiProposalsSubmitted":false,"sources":[],"wikiProposals":[]}"#).unwrap();
        assert!(read_structured_turn_result(Some(&path)).sources_submitted);
        fs::write(&path, vec![b' '; (MAX_RESULT_BYTES + 1) as usize]).unwrap();
        assert_eq!(
            read_structured_turn_result(Some(&path)).metadata_error.as_deref(),
            Some("별도 응답 메타데이터를 읽지 못했습니다. 응답 메타데이터가 허용 크기를 초과했습니다.")
        );
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", &path).unwrap();
        assert_eq!(
            read_structured_turn_result(Some(&path)).metadata_error.as_deref(),
            Some("별도 응답 메타데이터를 읽지 못했습니다. 응답 메타데이터 경로가 일반 파일이 아닙니다.")
        );
        discard_structured_turn_result(Some(&path));
        assert!(fs::symlink_metadata(&path).is_err());
        cleanup(&path);
    }

    #[test]
    fn 동시에_제출해도_값을_잃지_않는다() {
        let path = prepare_structured_turn_result(&unique("/root"), Some("boss"), "rust-unit").unwrap();
        std::thread::scope(|scope| {
            for index in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    let source = json!({ "kind": "file", "title": format!("파일 {index}"), "locator": format!("docs/{index}.md") });
                    submit_structured_response_source(path, source).unwrap();
                });
            }
        });
        assert_eq!(inspect_structured_turn_result(&path).unwrap().sources.len(), 8);
        cleanup(&path);
    }
}
