// Claude 지속 세션 시험. Node claude-persistent-worker.test.mjs를 옮기고, 가짜 자식 대신 입력 줄마다 정해 둔 출력을 내는 셸 스크립트를 띄운다.

use super::*;
use crate::js::{node_tmpdir, random_uuid};

/// 사용자 메시지 n번째에는 `<폴더>/n.jsonl`을, 중단 제어 요청에는 `interrupt.jsonl`을 그대로 내보낸다. 받은 입력과 PID를 남긴다.
const FAKE_CLAUDE: &str = r#"dir="$1"; turn=0
echo $$ >> "$dir/pids"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$dir/stdin.log"
  case "$line" in
    *'"type":"control_request"'*) file="$dir/interrupt.jsonl" ;;
    *) turn=$((turn+1)); file="$dir/$turn.jsonl" ;;
  esac
  if [ -f "$file" ]; then /bin/cat "$file"; fi
done
"#;

struct Fake {
    dir: String,
    received: Arc<Mutex<Vec<Value>>>,
}

impl Fake {
    fn new(turns: &[Vec<Value>], interrupt: Option<Vec<Value>>) -> Self {
        let dir = format!("{}/claude-worker-{}", node_tmpdir(), random_uuid());
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: String, lines: &[Value]| {
            let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            std::fs::write(format!("{dir}/{name}"), text).unwrap();
        };
        for (index, lines) in turns.iter().enumerate() {
            write(format!("{}.jsonl", index + 1), lines);
        }
        if let Some(lines) = interrupt {
            write("interrupt.jsonl".into(), &lines);
        }
        Self { dir, received: Arc::default() }
    }

    fn options(&self, session_id: Option<&str>, resume_transcript_path: Option<String>, grace_milliseconds: u64) -> WorkerOptions {
        WorkerOptions {
            executable: "/bin/sh".into(),
            arguments: vec!["-c".into(), FAKE_CLAUDE.into(), "fake-claude".into(), self.dir.clone()],
            cwd: Some(self.dir.clone()),
            env: Environment::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
            signature: "same".into(),
            session_id: session_id.map(str::to_string),
            resume_transcript_path,
            on_exit: Box::new(|_| {}),
            suggestion_grace: Duration::from_millis(grace_milliseconds),
            interrupt_result_timeout: DEFAULT_INTERRUPT_RESULT_TIMEOUT,
        }
    }

    fn collector(&self) -> impl FnMut(String) -> std::future::Ready<Result<(), String>> + Send + 'static {
        let received = Arc::clone(&self.received);
        move |line| {
            received.lock().unwrap().push(serde_json::from_str(&line).unwrap());
            std::future::ready(Ok(()))
        }
    }

    fn results(&self) -> Vec<Value> {
        self.received.lock().unwrap().iter().filter(|event| event["type"] == "result").cloned().collect()
    }

    fn stdin(&self) -> Vec<Value> {
        std::fs::read_to_string(format!("{}/stdin.log", self.dir))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    async fn wait_for_input(&self, count: usize) {
        for _ in 0..500 {
            if self.stdin().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("입력 {count}줄을 기다리다 끝났습니다.");
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: 신호 없이 프로세스가 있는지만 본다.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn wait_until_gone(pid: i32) {
    for _ in 0..500 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("프로세스 {pid}가 끝나지 않았습니다.");
}

fn close_to(value: &Value, expected: f64) -> bool {
    value.as_f64().is_some_and(|value| (value - expected).abs() < 1e-9)
}

#[tokio::test]
async fn 같은_프로세스가_연속_턴을_받고_누적_사용량은_턴별_증분으로_전달한다() {
    let fake = Fake::new(
        &[
            vec![
                json!({ "type": "system", "subtype": "init", "session_id": "session-1" }),
                json!({ "type": "result", "subtype": "success", "is_error": false, "result": "첫 답", "total_cost_usd": 1.2,
                    "usage": { "input_tokens": 10, "output_tokens": 2 },
                    "modelUsage": { "claude-opus-5": { "inputTokens": 10, "outputTokens": 2, "cacheReadInputTokens": 100, "cacheCreationInputTokens": 900, "costUSD": 1.2 } } }),
                json!({ "type": "prompt_suggestion", "suggestion": "첫 추천", "uuid": "suggestion-1" }),
            ],
            vec![
                json!({ "type": "result", "subtype": "success", "is_error": false, "result": "둘째 답", "total_cost_usd": 1.5,
                    "usage": { "input_tokens": 1, "output_tokens": 3 },
                    "modelUsage": { "claude-opus-5": { "inputTokens": 11, "outputTokens": 5, "cacheReadInputTokens": 1_050, "cacheCreationInputTokens": 920, "costUSD": 1.5 } } }),
                json!({ "type": "prompt_suggestion", "suggestion": "둘째 추천", "uuid": "suggestion-2" }),
            ],
        ],
        None,
    );
    let worker = ClaudePersistentWorker::spawn(fake.options(None, None, 100));
    worker.run_turn("첫 질문", fake.collector()).await.unwrap();
    worker.run_turn("둘째 질문", fake.collector()).await.unwrap();

    let input = fake.stdin();
    assert_eq!(input[0], json!({ "type": "user", "message": { "role": "user", "content": "첫 질문" }, "parent_tool_use_id": null }));
    assert_eq!(input[1]["message"]["content"], "둘째 질문");
    assert_eq!(std::fs::read_to_string(format!("{}/pids", fake.dir)).unwrap().lines().count(), 1, "프로세스는 하나다");
    assert_eq!(worker.session_id().as_deref(), Some("session-1"));
    assert!(worker.matches("same", Some("session-1")));
    assert!(!worker.matches("same", None));
    assert!(!worker.matches("same", Some("session-2")));
    assert!(!worker.matches("other", Some("session-1")));
    let results = fake.results();
    assert_eq!(results[0]["total_cost_usd"], json!(1.2));
    assert!(close_to(&results[1]["total_cost_usd"], 0.3));
    assert_eq!(results[1]["usage"], json!({ "input_tokens": 1, "output_tokens": 3 }));
    let model = &results[1]["modelUsage"]["claude-opus-5"];
    assert_eq!(
        (&model["inputTokens"], &model["outputTokens"], &model["cacheReadInputTokens"], &model["cacheCreationInputTokens"]),
        (&json!(1), &json!(3), &json!(950), &json!(20))
    );
    assert!(close_to(&model["costUSD"], 0.3));
    assert_eq!(fake.received.lock().unwrap().iter().filter(|event| event["type"] == "prompt_suggestion").count(), 2);

    let pid = worker.pid().unwrap();
    worker.close(DEFAULT_CLOSE_REASON);
    wait_until_gone(pid).await;
    assert_eq!(worker.run_turn("셋째", fake.collector()).await, Err(CLOSED.into()));
}

#[tokio::test]
async fn 재개한_첫_턴은_저장된_누적_비용과_모델_집계를_뺀다() {
    let fake = Fake::new(
        &[vec![json!({ "type": "result", "subtype": "success", "total_cost_usd": 40.4383378,
            "modelUsage": {
                "opus": { "inputTokens": 75453, "outputTokens": 573128, "cacheReadInputTokens": 81699169, "cacheCreationInputTokens": 1522487, "costUSD": 40.2743398 },
                "sonnet": { "inputTokens": 47259, "outputTokens": 6948, "costUSD": 0.163998 } } })]],
        None,
    );
    let path = format!("{}/session.jsonl", fake.dir);
    let state = json!({ "type": "cost-state", "sessionId": "session", "totalCostUSD": 39.985636,
        "modelUsage": {
            "opus": { "inputTokens": 75437, "outputTokens": 567902, "cacheReadInputTokens": 80719380, "cacheCreationInputTokens": 1503467, "costUSD": 39.821638 },
            "sonnet": { "inputTokens": 47259, "outputTokens": 6948, "costUSD": 0.163998 } } });
    std::fs::write(&path, format!("{state}\n")).unwrap();
    let worker = ClaudePersistentWorker::spawn(fake.options(Some("session"), Some(path), 1));
    worker.run_turn("검토", fake.collector()).await.unwrap();
    let result = &fake.results()[0];
    assert!(close_to(&result["total_cost_usd"], 0.4527018));
    let opus = &result["modelUsage"]["opus"];
    assert_eq!(
        (&opus["inputTokens"], &opus["outputTokens"], &opus["cacheReadInputTokens"], &opus["cacheCreationInputTokens"]),
        (&json!(16), &json!(5226), &json!(979789), &json!(19020))
    );
    assert_eq!(result["modelUsage"]["sonnet"]["outputTokens"], json!(0));
    worker.close(DEFAULT_CLOSE_REASON);
}

#[tokio::test]
async fn 재개_기준을_모르면_첫_비용은_비우고_다음_턴부터_증분으로_센다() {
    let turn = |total: f64| {
        vec![json!({ "type": "result", "subtype": "success", "total_cost_usd": total, "usage": { "input_tokens": 10, "output_tokens": 5 },
            "modelUsage": { "opus": { "costUSD": total, "inputTokens": total * 100.0 } } })]
    };
    let fake = Fake::new(&[turn(40.0), turn(40.5)], None);
    let mut options = fake.options(Some("session"), None, 1);
    options.cwd = Some(fake.dir.clone());
    let worker = ClaudePersistentWorker::spawn(options);
    worker.run_turn("검토", fake.collector()).await.unwrap();
    worker.run_turn("검토", fake.collector()).await.unwrap();
    let results = fake.results();
    assert_eq!(results[0]["total_cost_usd"], Value::Null);
    assert!(results[0].get("modelUsage").is_none());
    assert_eq!(results[0]["usage"], json!({ "input_tokens": 10, "output_tokens": 5 }));
    assert_eq!(results[1]["total_cost_usd"], json!(0.5));
    assert_eq!(results[1]["modelUsage"]["opus"]["inputTokens"], json!(50));
    worker.close(DEFAULT_CLOSE_REASON);
}

#[tokio::test]
async fn 이전_턴에서_늦은_추천은_다음_턴에_붙이지_않는다() {
    let fake = Fake::new(
        &[
            vec![json!({ "type": "result", "subtype": "success", "is_error": false, "result": "첫 답", "total_cost_usd": 0.1 })],
            vec![
                json!({ "type": "prompt_suggestion", "suggestion": "늦은 첫 추천", "uuid": "late-1" }),
                json!({ "type": "result", "subtype": "success", "is_error": false, "result": "둘째 답", "total_cost_usd": 0.2 }),
                json!({ "type": "prompt_suggestion", "suggestion": "둘째 추천", "uuid": "suggestion-2" }),
            ],
        ],
        None,
    );
    let worker = ClaudePersistentWorker::spawn(fake.options(None, None, 5));
    worker.run_turn("첫 질문", fake.collector()).await.unwrap();
    let first = fake.received.lock().unwrap().len();
    worker.run_turn("둘째 질문", fake.collector()).await.unwrap();
    let second: Vec<Value> = fake.received.lock().unwrap()[first..].to_vec();
    assert!(!second.iter().any(|event| event["suggestion"] == "늦은 첫 추천"));
    assert!(second.iter().any(|event| event["suggestion"] == "둘째 추천"));
    worker.close(DEFAULT_CLOSE_REASON);
}

#[test]
fn 누적_집계가_줄면_새_query_기준으로_그대로_쓴다() {
    let previous = CumulativeUsage {
        total_cost_usd: Some(2.0),
        model_usage: json!({ "model": { "inputTokens": 200, "costUSD": 2 } }).as_object().unwrap().clone(),
    };
    let (scoped, cumulative) = scoped_claude_cumulative_result(
        &json!({ "type": "result", "total_cost_usd": 0.4, "modelUsage": { "model": { "inputTokens": 40, "costUSD": 0.4 } } }),
        Some(&previous),
    );
    assert_eq!(scoped["total_cost_usd"], json!(0.4));
    assert_eq!(scoped["modelUsage"]["model"]["inputTokens"], json!(40.0));
    assert_eq!(scoped["modelUsage"]["model"]["costUSD"], json!(0.4));
    assert_eq!(cumulative.total_cost_usd, Some(0.4));
}

#[tokio::test]
async fn 수동_압축은_compact와_완료_경계의_토큰을_쓴다() {
    let fake = Fake::new(
        &[vec![
            json!({ "type": "system", "subtype": "compact_boundary", "compact_metadata": { "pre_tokens": 901_000, "post_tokens": 42_000 } }),
            json!({ "type": "result", "subtype": "success", "is_error": false, "result": "Compacted conversation", "total_cost_usd": 0.1 }),
            json!({ "type": "prompt_suggestion", "suggestion": "계속할까요?" }),
        ]],
        None,
    );
    let worker = ClaudePersistentWorker::spawn(fake.options(Some("session-1"), None, 5));
    assert_eq!(worker.compact().await, Ok(CompactTokens { pre_tokens: Some(901_000.0), post_tokens: Some(42_000.0) }));
    let content = fake.stdin()[0]["message"]["content"].as_str().unwrap().to_string();
    assert!(content.starts_with("/compact OFFICESTRA 직원 이름과 직급 호칭을 쓰지 말고"));
    assert!(!content.contains("백부장") && !content.contains("클대리"));
    worker.close(DEFAULT_CLOSE_REASON);
}

#[tokio::test]
async fn 압축_결과가_실패면_그_문구로_실패한다() {
    let fake = Fake::new(&[vec![json!({ "type": "result", "subtype": "error_during_execution", "is_error": true, "error": "한도 초과" })]], None);
    let worker = ClaudePersistentWorker::spawn(fake.options(None, None, 5));
    assert_eq!(worker.compact().await, Err("한도 초과".into()));
    worker.close(DEFAULT_CLOSE_REASON);
}

#[tokio::test]
async fn 중단은_interrupt를_먼저_보내고_비용이_담긴_result를_받은_뒤_종료한다() {
    // 실제 Claude Code 2.1.261은 interrupt 뒤 이 형태의 result를 보낸다.
    let fake = Fake::new(
        &[],
        Some(vec![json!({ "type": "result", "subtype": "error_during_execution", "is_error": true, "result": "", "total_cost_usd": 0.002706,
            "usage": { "input_tokens": 0, "output_tokens": 0 },
            "modelUsage": { "claude-sonnet-5": { "inputTokens": 1_218, "outputTokens": 27, "costUSD": 0.002706 } } })]),
    );
    let worker = ClaudePersistentWorker::spawn(fake.options(None, None, 100));
    let running = Arc::clone(&worker);
    let collector = fake.collector();
    let turn = tokio::spawn(async move { running.run_turn("긴 작업", collector).await });
    fake.wait_for_input(1).await;
    let pid = worker.pid().unwrap();
    assert!(alive(pid));
    worker.cancel_current().await;
    assert_eq!(turn.await.unwrap(), Ok(()), "중단 result를 받아 턴이 정상으로 끝났다");
    let input = fake.stdin();
    assert_eq!((&input[0]["type"], &input[1]["type"], &input[1]["request"]["subtype"]), (&json!("user"), &json!("control_request"), &json!("interrupt")));
    let result = &fake.results()[0];
    assert_eq!(result["total_cost_usd"], json!(0.002706));
    assert_eq!(result["modelUsage"]["claude-sonnet-5"]["inputTokens"], json!(1_218));
    wait_until_gone(pid).await;
}

#[tokio::test]
async fn 중단_result가_제한_시간_안에_오지_않으면_그대로_종료한다() {
    let fake = Fake::new(&[], None);
    let mut options = fake.options(None, None, 100);
    options.interrupt_result_timeout = Duration::from_millis(20);
    let worker = ClaudePersistentWorker::spawn(options);
    let running = Arc::clone(&worker);
    let collector = fake.collector();
    let turn = tokio::spawn(async move { running.run_turn("긴 작업", collector).await });
    fake.wait_for_input(1).await;
    let pid = worker.pid().unwrap();
    worker.cancel_current().await;
    assert_eq!(fake.stdin()[1]["type"], "control_request");
    let rejected = turn.await.unwrap().unwrap_err();
    assert!(rejected.contains("중단했습니다"), "{rejected}");
    wait_until_gone(pid).await;
}

#[tokio::test]
async fn 실행하지_못하면_첫_턴을_실행_실패_문구로_거절하고_종료를_알린다() {
    let fake = Fake::new(&[], None);
    let (sender, receiver) = oneshot::channel();
    let mut options = fake.options(None, None, 5);
    options.executable = "/no/such/claude".into();
    options.on_exit = Box::new(move |id| {
        let _ = sender.send(id);
    });
    let worker = ClaudePersistentWorker::spawn(options);
    assert_eq!(worker.run_turn("질문", fake.collector()).await, Err("spawn /no/such/claude ENOENT".into()));
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), receiver).await.unwrap(), Ok(worker.id));
    assert!(!worker.matches("same", None));
}

#[tokio::test]
async fn 예상하지_못한_종료는_종료_코드와_stderr로_실패한다() {
    let fake = Fake::new(&[], None);
    let mut options = fake.options(None, None, 5);
    options.arguments = vec!["-c".into(), "read line; echo '설정 오류' >&2; exit 3".into()];
    let worker = ClaudePersistentWorker::spawn(options);
    assert_eq!(worker.run_turn("질문", fake.collector()).await, Err("Claude Code 지속 세션이 종료 코드 3로 끝났습니다. 설정 오류".into()));
}
