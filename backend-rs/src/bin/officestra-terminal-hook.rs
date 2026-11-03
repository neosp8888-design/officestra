// Claude Code 훅(stdin)과 Codex notify(첫 인자)를 터미널 사건 수신 API로 넘기는 실행 파일. Node officestra-terminal-hook과 같다.
// 기록 실패가 CLI 입력을 막으면 안 되므로 어떤 실패든 종료 코드는 0이다.

use std::io::Read;
use std::time::Duration;

use officestra_backend::js::{js_property, js_string_or_empty, js_trim, parse_js_json, to_js_json};
use serde_json::json;

fn main() {
    if let Err(message) = run() {
        eprintln!("OFFICESTRA 터미널 이벤트 전달 실패: {message}");
    }
    std::process::exit(0);
}

fn run() -> Result<(), String> {
    let url = std::env::var("OFFICESTRA_TERMINAL_EVENTS_URL").unwrap_or_default();
    let url = js_trim(&url);
    if url.is_empty() {
        return Ok(());
    }
    let argument = std::env::args().nth(1).unwrap_or_default();
    let argument = js_trim(&argument).to_string();
    let source = if argument.is_empty() { "claude" } else { "codex" };
    let raw = if argument.is_empty() {
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).map_err(|error| error.to_string())?;
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        argument
    };
    if raw.is_empty() {
        return Ok(());
    }
    let payload = parse_js_json(&raw).map_err(|error| error.to_string())?;
    let field = if source == "claude" { "hook_event_name" } else { "type" };
    let event = js_string_or_empty(js_property(&payload, field)?);
    let body = to_js_json(&json!({ "source": source, "event": event, "payload": payload })).map_err(|error| error.to_string())?;
    // reqwest는 rustls 공급자를 직접 설치해야 한다. 백엔드(lib.rs)와 같은 ring 공급자다.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let client = reqwest::Client::builder().build().map_err(|error| error.to_string())?;
        client
            .post(url)
            .header("content-type", "application/json")
            .body(body)
            .timeout(Duration::from_millis(2_500))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    })
}
