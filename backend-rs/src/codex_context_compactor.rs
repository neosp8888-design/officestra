// Codex app-server의 네이티브 thread/compact/start를 한 번 실행한다. Node codex-context-compactor.mjs와 같다.

use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin};
use tokio::time::Instant;

use crate::agent_runtime::launch::Environment;
use crate::agent_runtime::lifecycle::LineSplitter;
use crate::child_process::{read_open_pipe, spawn_error_message};
use crate::js::{js_string, js_trim, js_truthy, parse_js_json, to_js_json, utf16_suffix};
use crate::local_embedding::signal_name;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const STDERR_TAIL_UNITS: usize = 8_192;
const TIMED_OUT: &str = "Codex 컨텍스트 압축 시간이 초과됐습니다.";
const INPUT_CLOSED: &str = "Codex app-server 입력 스트림이 닫혔습니다.";
const REQUEST_FAILED: &str = "Codex 요청이 실패했습니다.";
const COMPACTION_FAILED: &str = "Codex 컨텍스트 압축이 실패했습니다.";
const INTERACTION_REFUSED: &str = "OFFICESTRA 압축 호출은 상호작용을 지원하지 않습니다.";
const INTERACTION_REQUIRED: &str = "Codex 압축 중 사용자 상호작용이 필요합니다.";

pub struct CodexCompactRequest<'a> {
    pub executable: &'a str,
    pub thread_id: &'a str,
    pub cwd: &'a str,
    pub env: &'a Environment,
    pub timeout: Duration,
}

/// 압축 완료 알림의 turnId. 없으면 null이다.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexCompaction {
    pub turn_id: Value,
}

/// compactCodexThread. initialize → initialized → thread/resume → thread/compact/start를 차례로 보내고
/// 같은 스레드의 압축 완료 알림을 기다린다. 어느 단계든 제한 시간 하나로 끊는다.
pub async fn compact_codex_thread(request: CodexCompactRequest<'_>) -> Result<CodexCompaction, String> {
    let deadline = Instant::now() + request.timeout;
    let mut child = tokio::process::Command::new(request.executable)
        .args(["app-server", "--stdio"])
        .current_dir(request.cwd)
        .env_clear()
        .envs(request.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| spawn_error_message(request.executable, &error))?;
    let mut session = Session {
        thread_id: request.thread_id,
        cwd: request.cwd,
        stdin: child.stdin.take(),
        next_id: 1,
        pending: None,
        step: Step::Initialize,
        compacted: None,
    };
    let result = session.run(&mut child, deadline).await;
    // Node finally처럼 입력을 닫고, 아직 살아 있으면 SIGTERM만 보낸다. 끝난 프로세스는 뒤에서 거둔다.
    drop(session.stdin.take());
    if matches!(child.try_wait(), Ok(None))
        && let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok())
    {
        // SAFETY: 아직 거두지 않은 자식 프로세스 번호에 신호만 보낸다.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    result
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    Initialize,
    Resume,
    Compact,
    Compaction,
}

struct Session<'a> {
    thread_id: &'a str,
    cwd: &'a str,
    stdin: Option<ChildStdin>,
    next_id: u64,
    pending: Option<u64>,
    step: Step,
    /// 압축 완료 알림을 받았으면 그 turnId. 첫 알림만 쓴다.
    compacted: Option<Value>,
}

impl Session<'_> {
    async fn run(&mut self, child: &mut Child, deadline: Instant) -> Result<CodexCompaction, String> {
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut stderr_tail = String::new();
        let mut splitter = LineSplitter::default();
        let mut closed = false;
        let (mut output, mut diagnostics) = (vec![0_u8; 64 * 1024], vec![0_u8; 8 * 1024]);
        self.request(
            "initialize",
            json!({
                "clientInfo": { "name": "officestra", "title": "OFFICESTRA", "version": "1" },
                "capabilities": { "experimentalApi": false, "requestAttestation": false },
            }),
        )
        .await?;
        loop {
            tokio::select! {
                read = read_open_pipe(&mut stdout, &mut output) => {
                    let lines = match read {
                        Ok(0) | Err(_) => {
                            stdout = None;
                            splitter.finish().into_iter().collect()
                        }
                        Ok(count) => splitter.push(&output[..count]),
                    };
                    for line in lines {
                        if let Some(result) = self.handle_line(&line).await {
                            return result;
                        }
                    }
                }
                read = read_open_pipe(&mut stderr, &mut diagnostics) => match read {
                    Ok(0) | Err(_) => stderr = None,
                    Ok(count) => {
                        let joined = format!("{stderr_tail}{}", String::from_utf8_lossy(&diagnostics[..count]));
                        stderr_tail = utf16_suffix(&joined, STDERR_TAIL_UNITS);
                    }
                },
                // Node close는 종료와 출력 파이프 닫힘을 모두 기다린다. 압축을 이미 받았으면 무시하고 남은 응답을 기다린다.
                status = child.wait(), if !closed && stdout.is_none() && stderr.is_none() => {
                    closed = true;
                    if self.compacted.is_none() {
                        return Err(close_message(status.ok(), &stderr_tail));
                    }
                }
                () = tokio::time::sleep_until(deadline) => return Err(TIMED_OUT.into()),
            }
        }
    }

    /// 한 줄을 처리한다. Some이면 Node의 압축 호출이 그 값으로 끝난다.
    async fn handle_line(&mut self, line: &str) -> Option<Result<CodexCompaction, String>> {
        let Ok(message) = parse_js_json(line) else {
            return None;
        };
        let fields = match &message {
            // Object.prototype.hasOwnProperty.call(null, "id")의 TypeError로 출력 읽기가 끝난다.
            Value::Null => return Some(Err("Cannot convert undefined or null to object".into())),
            Value::Object(fields) => fields,
            _ => return None,
        };
        if let Some(id) = fields.get("id") {
            return self.handle_response(fields, id).await;
        }
        let params = fields.get("params");
        let same_thread = field(params, "threadId").and_then(Value::as_str) == Some(self.thread_id);
        match fields.get("method").and_then(Value::as_str) {
            Some("item/completed")
                if same_thread && field(field(params, "item"), "type").and_then(Value::as_str) == Some("contextCompaction") =>
            {
                self.finish(params)
            }
            Some("thread/compacted") if same_thread => self.finish(params),
            Some("error") if same_thread && field(params, "willRetry") != Some(&Value::Bool(true)) => {
                Some(Err(message_or(field(field(params, "error"), "message"), COMPACTION_FAILED)))
            }
            Some("turn/completed")
                if same_thread && field(field(params, "turn"), "status").and_then(Value::as_str) == Some("failed") =>
            {
                Some(Err(message_or(field(field(field(params, "turn"), "error"), "message"), COMPACTION_FAILED)))
            }
            _ => None,
        }
    }

    async fn handle_response(&mut self, fields: &Map<String, Value>, id: &Value) -> Option<Result<CodexCompaction, String>> {
        // Map.get(message.id)는 숫자 id만 찾는다.
        if let Some(pending) = self.pending
            && id.as_f64() == Some(pending as f64)
        {
            self.pending = None;
            if js_truthy(fields.get("error")) {
                return Some(Err(message_or(field(fields.get("error"), "message"), REQUEST_FAILED)));
            }
            return self.advance().await;
        }
        // 압축 중 서버가 권한이나 사용자 입력을 요구하면 headless 호출은 응답할 수 없어 바로 거절한다.
        if js_truthy(fields.get("method")) {
            let reply = json!({ "id": id, "error": { "code": -32601, "message": INTERACTION_REFUSED } });
            if let Err(error) = self.send(&reply).await {
                return Some(Err(error));
            }
            return Some(Err(INTERACTION_REQUIRED.into()));
        }
        None
    }

    async fn advance(&mut self) -> Option<Result<CodexCompaction, String>> {
        let sent = match self.step {
            Step::Initialize => {
                self.step = Step::Resume;
                match self.send(&json!({ "method": "initialized" })).await {
                    Ok(()) => self.request("thread/resume", json!({ "threadId": self.thread_id, "cwd": self.cwd })).await,
                    Err(error) => Err(error),
                }
            }
            Step::Resume => {
                self.step = Step::Compact;
                self.request("thread/compact/start", json!({ "threadId": self.thread_id })).await
            }
            Step::Compact | Step::Compaction => {
                self.step = Step::Compaction;
                return self.compacted.clone().map(|turn_id| Ok(CodexCompaction { turn_id }));
            }
        };
        sent.err().map(Err)
    }

    fn finish(&mut self, params: Option<&Value>) -> Option<Result<CodexCompaction, String>> {
        if self.compacted.is_none() {
            self.compacted = Some(field(params, "turnId").filter(|value| !value.is_null()).cloned().unwrap_or(Value::Null));
        }
        (self.step == Step::Compaction).then(|| Ok(CodexCompaction { turn_id: self.compacted.clone().unwrap_or(Value::Null) }))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        self.pending = Some(id);
        let sent = self.send(&json!({ "id": id, "method": method, "params": params })).await;
        if sent.is_err() {
            self.pending = None;
        }
        sent
    }

    async fn send(&mut self, object: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or(INPUT_CLOSED)?;
        let mut line = to_js_json(object).map_err(|error| error.to_string())?;
        line.push(b'\n');
        stdin.write_all(&line).await.map_err(|_| INPUT_CLOSED.to_string())?;
        stdin.flush().await.map_err(|_| INPUT_CLOSED.to_string())
    }
}

/// 객체의 속성. 객체가 아니면 JS처럼 undefined다.
fn field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    value?.as_object()?.get(key)
}

/// String(value ?? fallback).
fn message_or(value: Option<&Value>, fallback: &str) -> String {
    value.filter(|value| !value.is_null()).map_or_else(|| fallback.to_string(), js_string)
}

fn close_message(status: Option<ExitStatus>, stderr_tail: &str) -> String {
    let diagnostic = js_trim(stderr_tail);
    let suffix = if diagnostic.is_empty() { String::new() } else { format!(" {diagnostic}") };
    match (status.and_then(|status| status.signal()), status.and_then(|status| status.code())) {
        (Some(signal), _) => format!("Codex app-server가 {} 신호로 종료됐습니다.{suffix}", signal_name(signal)),
        (None, Some(code)) => format!("Codex app-server가 종료 코드 {code}로 끝났습니다.{suffix}"),
        (None, None) => format!("Codex app-server가 종료 코드 unknown로 끝났습니다.{suffix}"),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::*;

    const FAKE_APP_SERVER: &str = r#"#!/bin/sh
trap '' TERM
pwd > "$LOG_DIR/cwd.txt"
printf '%s|%s\n' "$*" "$TEST" > "$LOG_DIR/args.txt"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG_DIR/input.log"
  case "$line" in
    *'"method":"initialize"'*)
      case "$MODE" in
        exit) printf 'boom\n' >&2; exit 7 ;;
        signal) kill -KILL $$ ;;
        hang) ;;
        null) printf 'null\n' ;;
        *) printf '%s\n' '{"id":1,"result":{"userAgent":"test"}}' ;;
      esac ;;
    *'"method":"thread/resume"'*)
      case "$MODE" in
        resume-error) printf '%s\n' '{"id":2,"error":{"message":"스레드를 찾을 수 없습니다."}}' ;;
        resume-error-default) printf '%s\n' '{"id":"2","result":{}}' '{"id":2,"error":{"code":1}}' ;;
        *) printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-1"}}}' ;;
      esac ;;
    *'"method":"thread/compact/start"'*)
      case "$MODE" in
        early) printf '%s\n' '{"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-early","item":{"type":"contextCompaction"}}}' '{"id":3,"result":{}}' ;;
        ask) printf '%s\n' '{"id":3,"result":{}}' '{"id":"srv-1","method":"item/commandExecution/requestApproval","params":{}}' ;;
        retry) printf '%s\n' '{"id":3,"result":{}}' '{"method":"error","params":{"threadId":"thread-1","willRetry":true,"error":{"message":"다시 시도합니다."}}}' '{"method":"error","params":{"threadId":"other","error":{"message":"남의 스레드"}}}' '{"method":"error","params":{"threadId":"thread-1","willRetry":false,"error":{"message":"압축에 실패했습니다."}}}' ;;
        turn-failed) printf '%s\n' '{"id":3,"result":{}}' '{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"status":"failed","error":null}}}' ;;
        compacted) printf '%s\r\n' 'not json' '5' '{"id":3,"result":{}}' '{"method":"item/completed","params":{"threadId":"thread-1","item":{"type":"agentMessage"}}}' '{"method":"thread/compacted","params":{"threadId":"thread-1"}}' ;;
        exit-after) printf '%s\n' '{"id":3,"result":{}}'; printf 'last words\n' >&2; exit 0 ;;
        *) printf '%s\n' '{"id":3,"result":{}}' '{"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-compact","item":{"type":"contextCompaction","id":"item-1"},"completedAtMs":1}}' ;;
      esac ;;
  esac
done
"#;

    struct Fixture {
        root: PathBuf,
        executable: String,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("officestra-codex-compactor-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("bin")).unwrap();
            let root = root.canonicalize().unwrap();
            let executable = root.join("bin").join("codex");
            std::fs::write(&executable, FAKE_APP_SERVER).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { root, executable: executable.to_str().unwrap().to_string() }
        }

        async fn compact(&self, mode: &str, timeout: Duration) -> (PathBuf, Result<CodexCompaction, String>) {
            let directory = self.root.join(mode);
            std::fs::create_dir_all(&directory).unwrap();
            let text = directory.to_str().unwrap().to_string();
            let env = Environment::from([
                ("TEST".to_string(), "1".to_string()),
                ("MODE".to_string(), mode.to_string()),
                ("LOG_DIR".to_string(), text.clone()),
            ]);
            let result = compact_codex_thread(CodexCompactRequest {
                executable: &self.executable,
                thread_id: "thread-1",
                cwd: &text,
                env: &env,
                timeout,
            })
            .await;
            (directory, result)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn read(directory: &Path, name: &str) -> String {
        std::fs::read_to_string(directory.join(name)).unwrap_or_default()
    }

    async fn wait_for_line(directory: &Path, expected: &str) -> String {
        for _ in 0..100 {
            let log = read(directory, "input.log");
            if log.lines().any(|line| line == expected) {
                return log;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        read(directory, "input.log")
    }

    // Node "Codex 수동 압축은 app-server의 네이티브 compact 요청을 사용한다".
    #[tokio::test]
    async fn native_compact_request_sequence() {
        let fixture = Fixture::new("sequence");
        let (directory, result) = fixture.compact("ok", Duration::from_secs(5)).await;
        assert_eq!(result, Ok(CodexCompaction { turn_id: json!("turn-compact") }));
        let cwd = directory.to_str().unwrap();
        assert_eq!(read(&directory, "args.txt"), "app-server --stdio|1\n");
        assert_eq!(read(&directory, "cwd.txt"), format!("{cwd}\n"));
        let expected = [
            r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"officestra","title":"OFFICESTRA","version":"1"},"capabilities":{"experimentalApi":false,"requestAttestation":false}}}"#.to_string(),
            r#"{"method":"initialized"}"#.to_string(),
            format!(r#"{{"id":2,"method":"thread/resume","params":{{"threadId":"thread-1","cwd":"{cwd}"}}}}"#),
            r#"{"id":3,"method":"thread/compact/start","params":{"threadId":"thread-1"}}"#.to_string(),
        ];
        let log = wait_for_line(&directory, &expected[3]).await;
        assert_eq!(log.lines().collect::<Vec<_>>(), expected);
    }

    #[tokio::test]
    async fn completion_variants_and_failures_follow_node() {
        let fixture = Fixture::new("variants");
        let second = Duration::from_secs(5);
        let cases: [(&str, Result<CodexCompaction, String>); 10] = [
            ("early", Ok(CodexCompaction { turn_id: json!("turn-early") })),
            ("compacted", Ok(CodexCompaction { turn_id: Value::Null })),
            ("resume-error", Err("스레드를 찾을 수 없습니다.".into())),
            ("resume-error-default", Err(REQUEST_FAILED.into())),
            ("retry", Err("압축에 실패했습니다.".into())),
            ("turn-failed", Err(COMPACTION_FAILED.into())),
            ("null", Err("Cannot convert undefined or null to object".into())),
            ("exit", Err("Codex app-server가 종료 코드 7로 끝났습니다. boom".into())),
            ("exit-after", Err("Codex app-server가 종료 코드 0로 끝났습니다. last words".into())),
            ("signal", Err("Codex app-server가 SIGKILL 신호로 종료됐습니다.".into())),
        ];
        for (mode, expected) in cases {
            let (_, result) = fixture.compact(mode, second).await;
            assert_eq!(result, expected, "{mode}");
        }

        let started = std::time::Instant::now();
        let (_, result) = fixture.compact("hang", Duration::from_millis(300)).await;
        assert_eq!(result, Err(TIMED_OUT.into()));
        assert!(started.elapsed() < Duration::from_secs(3));

        let (directory, result) = fixture.compact("ask", second).await;
        assert_eq!(result, Err(INTERACTION_REQUIRED.into()));
        let reply = r#"{"id":"srv-1","error":{"code":-32601,"message":"OFFICESTRA 압축 호출은 상호작용을 지원하지 않습니다."}}"#;
        assert!(wait_for_line(&directory, reply).await.lines().any(|line| line == reply));
    }

    #[tokio::test]
    async fn missing_executable_is_a_spawn_error() {
        let env = Environment::new();
        let result = compact_codex_thread(CodexCompactRequest {
            executable: "/nonexistent/officestra/codex",
            thread_id: "thread-1",
            cwd: "/",
            env: &env,
            timeout: Duration::from_secs(1),
        })
        .await;
        assert_eq!(result, Err("spawn /nonexistent/officestra/codex ENOENT".into()));
    }
}
