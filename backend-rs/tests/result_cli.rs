use officestra_backend::structured_turn_result::{prepare_structured_turn_result, discard_structured_turn_result};
use serde_json::Value;
use std::process::Command;

#[test]
fn native_result_cli_roundtrip_and_rejected_input_preserve_metadata() {
    let binary = env!("CARGO_BIN_EXE_officestra-result");
    let help = Command::new(binary).arg("--help").env_remove("OFFICESTRA_RESULT_PATH").output().unwrap();
    assert!(help.status.success());
    let path = prepare_structured_turn_result("/result-cli", Some("boss"), &format!("cli-test-{}",std::process::id())).unwrap();
    let call = |args: &[&str]| Command::new(binary).args(args).env("OFFICESTRA_RESULT_PATH", &path).output().unwrap();
    assert!(call(&["source", "--kind", "file", "--title", "코드", "--locator", "src/main.rs"]).status.success());
    assert!(call(&["wiki", "--page-key", "native-result", "--kind", "decision", "--title", "제출", "--body", "네이티브 CLI를 쓴다."]).status.success());
    assert!(call(&["source", "--kind", "rag", "--title", "검색", "--rag-document-id", "11111111-1111-4111-8111-111111111111"]).status.success());
    for args in [
        vec!["source", "--kind", "file", "--title", "x"],
        vec!["source", "--kind", "file", "--kind", "web"],
        vec!["source", "--kind"],
        vec!["unknown"],
        // Node처럼 빈 ID·빈 locator로는 근거를 만들지 않는다.
        vec!["source", "--kind", "rag", "--title", "x", "--rag-document-id", ""],
        vec!["source", "--kind", "database", "--title", "x", "--work-record-id", ""],
        vec!["source", "--kind", "file", "--title", "x", "--locator", ""],
    ] {
        assert!(!call(&args).status.success());
    }
    let state: Value = serde_json::from_slice(&call(&["show"]).stdout).unwrap();
    assert_eq!(state["sources"].as_array().unwrap().len(),2);
    assert_eq!(state["sources"][1]["locator"], "rag_documents:11111111-1111-4111-8111-111111111111");
    assert_eq!(state["wikiProposals"][0]["approvalTier"], "user");
    assert_eq!(state["sourcesSubmitted"],true);
    assert_eq!(state["wikiProposalsSubmitted"],true);
    discard_structured_turn_result(Some(&path));
    let missing=Command::new(binary).arg("show").env_remove("OFFICESTRA_RESULT_PATH").output().unwrap();
    assert!(!missing.status.success());
}
