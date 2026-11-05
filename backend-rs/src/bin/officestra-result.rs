//! 직원 응답 메타데이터 CLI. 서버와 동일한 검증·잠금·0600 파일 쓰기를 사용한다.
use officestra_backend::structured_turn_result::{
    STRUCTURED_RESULT_ENV, inspect_structured_turn_result,
    submit_structured_response_source, submit_structured_wiki_proposal,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const HELP: &str = "OFFICESTRA 응답 메타데이터 제출\n\n자연어 답변은 평소처럼 작성하고, 실제로 사용한 근거나 위키 제안만 별도 제출합니다.\n답변 본문에 JSON이나 OFFICE_* 마커를 쓰지 않습니다.\n\n사용법:\n  officestra-result source --kind KIND --title TITLE [--locator LOCATOR]\n    [--excerpt TEXT] [--rag-document-id UUID] [--work-record-id UUID]\n  officestra-result wiki --page-key SLUG --kind KIND --title TITLE --body BODY\n  officestra-result show\n\nsource: rag, database, file, web, tool, skill. 실제 사용한 근거만 최대 20개.\nrag에는 --rag-document-id, 업무 기록 database에는 --work-record-id가 필요합니다.\ntool locator에는 원시 인자·응답 대신 도구 식별자만 씁니다.\nwiki: decision, constraint, incident. 명시적 지속 지식만 최대 3개.\n위키는 사용자 승인 후 게시하며 직원이 직접 승인·거절하지 않습니다.\n";

fn main() {
    match run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(output) => print!("{output}"),
        Err(error) => { eprintln!("{error}"); std::process::exit(1); }
    }
}

fn required<'a>(options: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, String> {
    options.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
        .ok_or_else(|| format!("--{key}가 필요합니다."))
}

fn run(args: &[String]) -> Result<String, String> {
    let command = args.first().map(String::as_str).unwrap_or("--help");
    if matches!(command, "--help" | "-h" | "help") { return Ok(HELP.into()); }
    let path = std::env::var(STRUCTURED_RESULT_ENV)
        .ok().filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{STRUCTURED_RESULT_ENV}가 없습니다. OFFICESTRA 업무 안에서 실행하세요."))?;
    if command == "show" {
        let state = inspect_structured_turn_result(&path)?;
        return Ok(format!("{}\n", serde_json::to_string_pretty(&json!({
            "version": 1, "sourcesSubmitted": state.sources_submitted,
            "wikiProposalsSubmitted": state.wiki_proposals_submitted,
            "sources": state.sources, "wikiProposals": state.wiki_proposals,
        })).map_err(|e| e.to_string())?));
    }
    let mut options = BTreeMap::new();
    for pair in args[1..].chunks(2) {
        if pair.len() != 2 || !pair[0].starts_with("--") {
            return Err(format!("옵션은 --이름 값 형식이어야 합니다: {}", pair[0]));
        }
        let key = pair[0].trim_start_matches("--");
        if options.insert(key.into(), pair[1].clone()).is_some() {
            return Err(format!("같은 옵션을 두 번 사용할 수 없습니다: {}", pair[0]));
        }
    }
    match command {
        "source" => {
            let kind = required(&options, "kind")?;
            // Node처럼 빈 locator는 거부하고, 빈 ID로는 locator를 만들지 않는다.
            let id = |option: &str| options.get(option).filter(|id| !id.is_empty());
            let locator = options.get("locator").cloned().or_else(|| match kind {
                "rag" => id("rag-document-id").map(|id| format!("rag_documents:{id}")),
                "database" => id("work-record-id").map(|id| format!("work_records:{id}")),
                _ => None,
            }).filter(|locator| !locator.is_empty()).ok_or_else(|| "--locator가 필요합니다.".to_string())?;
            let mut source = Map::new();
            source.insert("kind".into(), Value::String(kind.into()));
            source.insert("title".into(), Value::String(required(&options, "title")?.into()));
            source.insert("locator".into(), Value::String(locator));
            for (option, field) in [("excerpt", "excerpt"), ("rag-document-id", "ragDocumentId"), ("work-record-id", "workRecordId")] {
                if let Some(value) = options.get(option).filter(|v| !v.is_empty()) {
                    source.insert(field.into(), Value::String(value.clone()));
                }
            }
            let count = submit_structured_response_source(&path, Value::Object(source))?;
            Ok(format!("응답 근거 {count}개가 별도 제출됐습니다.\n"))
        }
        "wiki" => {
            let proposal = json!({
                "pageKey": required(&options, "page-key")?, "kind": required(&options, "kind")?,
                "title": required(&options, "title")?, "body": required(&options, "body")?, "approvalTier": "user",
            });
            let count = submit_structured_wiki_proposal(&path, proposal)?;
            Ok(format!("위키 제안 {count}개가 별도 제출됐습니다.\n"))
        }
        _ => Err(format!("지원하지 않는 명령입니다: {command}")),
    }
}
