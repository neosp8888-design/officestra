// 롤플레이 서비스 시험. 순수 함수 시험과 DB를 쓰는 통합 시험(Node roleplay.test.mjs 이식)을 담는다.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

mod http;

use super::*;
use crate::js::random_uuid;
use crate::roleplay::risu_format::fixtures::{risu_module_file, risu_preset_file, synthetic_module, synthetic_preset};

type Reply = Arc<dyn Fn(&FetchRequest) -> BoxFuture<'static, Result<FetchResponse, FetchFailure>> + Send + Sync>;

/// 부른 요청을 모두 남기고, 정해 둔 응답을 돌려주는 fetch.
struct FakeFetch {
    calls: Mutex<Vec<FetchRequest>>,
    reply: Mutex<Reply>,
}

impl FakeFetch {
    fn new(reply: impl Fn(&FetchRequest) -> Result<FetchResponse, FetchFailure> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { calls: Mutex::default(), reply: Mutex::new(sync_reply(reply)) })
    }

    fn set(&self, reply: impl Fn(&FetchRequest) -> Result<FetchResponse, FetchFailure> + Send + Sync + 'static) {
        *self.reply.lock().unwrap() = sync_reply(reply);
    }

    fn set_async(&self, reply: Reply) {
        *self.reply.lock().unwrap() = reply;
    }

    fn calls(&self) -> Vec<FetchRequest> {
        self.calls.lock().unwrap().clone()
    }
}

impl RoleplayFetch for FakeFetch {
    fn fetch(&self, request: FetchRequest) -> BoxFuture<'_, Result<FetchResponse, FetchFailure>> {
        self.calls.lock().unwrap().push(request.clone());
        let reply = self.reply.lock().unwrap().clone();
        reply(&request)
    }
}

fn sync_reply(reply: impl Fn(&FetchRequest) -> Result<FetchResponse, FetchFailure> + Send + Sync + 'static) -> Reply {
    Arc::new(move |request: &FetchRequest| {
        let result = reply(request);
        Box::pin(async move { result })
    })
}

fn respond(status: u16, body: Value) -> Result<FetchResponse, FetchFailure> {
    Ok(FetchResponse { status, body: Ok(body.to_string()) })
}

fn text_reply(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

fn header<'a>(request: &'a FetchRequest, name: &str) -> &'a str {
    request.headers.iter().find(|(key, _)| *key == name).map_or("", |(_, value)| value.as_str())
}

fn sent_body(request: &FetchRequest) -> Value {
    serde_json::from_str(request.body.as_deref().unwrap()).unwrap()
}

const DOOR_URL: &str = "http://192.0.2.20:2/v1/messages";

struct OpenedDoor {
    definition: Value,
    released: Arc<AtomicBool>,
}

/// 로컬 문을 여는 척만 한다. 연 정의와 닫았는지를 남긴다.
#[derive(Default)]
struct FakeDoors {
    context_window: Mutex<Option<f64>>,
    opened: Mutex<Vec<OpenedDoor>>,
}

impl LocalDoors for FakeDoors {
    fn open_door<'a>(&'a self, definition: &'a Value) -> BoxFuture<'a, Result<Door, String>> {
        Box::pin(async move {
            let released = Arc::new(AtomicBool::new(false));
            self.opened.lock().unwrap().push(OpenedDoor { definition: definition.clone(), released: released.clone() });
            Ok(Door {
                url: DOOR_URL.into(),
                token: "door-token".into(),
                model: definition["profile"]["model"].as_str().unwrap_or_default().into(),
                context_window: *self.context_window.lock().unwrap(),
                max_output_tokens: 1000.0,
                release: Box::new(move || {
                    Box::pin(async move {
                        released.store(true, AtomicOrdering::SeqCst);
                        Ok(())
                    })
                }),
            })
        })
    }
}

fn key_file(contents: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("roleplay-key-{}", random_uuid()));
    std::fs::write(&path, contents).unwrap();
    path
}

fn lazy_pool() -> PgPool {
    crate::db::connect_lazy("postgres://officestra@127.0.0.1:9/unused", Duration::from_millis(100)).unwrap()
}

fn offline_service(fetch: Arc<FakeFetch>, api_url: &str, api_key_file: PathBuf) -> RoleplayService {
    service_on(lazy_pool(), fetch, api_url, api_key_file)
}

fn service_on(pool: PgPool, fetch: Arc<FakeFetch>, api_url: &str, api_key_file: PathBuf) -> RoleplayService {
    RoleplayService::new(RoleplayOptions {
        pool,
        broadcast: Arc::new(|_| {}),
        fetch,
        api_url: api_url.into(),
        api_key_file,
        local_doors: Arc::new(|| None),
        proxy_starter: None,
    })
}

/// 켜 달라는 횟수를 세고, 켤 때 정해 둔 일을 한다(fetch를 살리는 등).
struct FakeStarter {
    started: AtomicUsize,
    on_start: Box<dyn Fn() -> bool + Send + Sync>,
}

impl FakeStarter {
    fn new(on_start: impl Fn() -> bool + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { started: AtomicUsize::new(0), on_start: Box::new(on_start) })
    }

    fn started(&self) -> usize {
        self.started.load(AtomicOrdering::SeqCst)
    }
}

impl ProxyStarter for FakeStarter {
    fn start(&self) -> BoxFuture<'_, bool> {
        self.started.fetch_add(1, AtomicOrdering::SeqCst);
        let ready = (self.on_start)();
        Box::pin(async move { ready })
    }
}

fn service_with_starter(fetch: Arc<FakeFetch>, api_key_file: PathBuf, starter: Arc<FakeStarter>) -> RoleplayService {
    RoleplayService::new(RoleplayOptions {
        pool: lazy_pool(),
        broadcast: Arc::new(|_| {}),
        fetch,
        api_url: DEFAULT_API_URL.into(),
        api_key_file,
        local_doors: Arc::new(|| None),
        proxy_starter: Some(starter),
    })
}

#[test]
fn base64_matches_node_lenient_decoding() {
    // Node 24 Buffer.from(text, "base64") 결과(16진).
    for (text, hex) in [
        ("", ""),
        ("AAAA", "000000"),
        ("YWJj", "616263"),
        ("YW Jj", "616263"),
        ("YWJ", "6162"),
        ("YQ==", "61"),
        ("YQ=a", "61"),
        ("Y=WJj", ""),
        ("-_-_", "fbffbf"),
        ("+/+/", "fbffbf"),
        ("가나", ""),
        ("YWJjZA", "61626364"),
        ("YWJjZA=", "61626364"),
        ("=YWJj", ""),
        ("YW\nJj\n", "616263"),
        ("Y", ""),
        ("YW", "61"),
        ("*YWJj", "616263"),
        ("YWJj*", "616263"),
        ("YWJj=YWJj", "616263"),
        ("\u{1F600}YQ", ""),
    ] {
        let decoded: String = base64url_decode(text).iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(decoded, hex, "{text:?}");
    }
}

#[test]
fn locale_compare_sorts_like_node_for_ascii() {
    let mut ids = [
        "gemini-4-argon-high", "claude-opus-5-5-low", "B", "a", "A", "b", "_x", "1", "a-b", "a_b", "ab", "aB", "Ab", "a b", "a.b", "~", "$",
    ];
    ids.sort_by(|left, right| locale_compare(left, right));
    assert_eq!(
        ids,
        ["_x", "~", "$", "1", "a", "A", "a b", "a_b", "a-b", "a.b", "ab", "aB", "Ab", "b", "B", "claude-opus-5-5-low", "gemini-4-argon-high"]
    );
}

#[test]
fn validators_keep_node_messages() {
    fn message<T>(result: Result<T, RoleplayError>) -> String {
        result.map(|_| ()).unwrap_err().message().to_string()
    }
    assert_eq!(message(text(None, "메시지", 10, true)), "메시지는 문자열이어야 합니다.");
    assert_eq!(message(text(Some(&json!("😀😀😀")), "메시지", 5, true)), "메시지가 너무 깁니다.");
    assert_eq!(message(text(Some(&json!(" \u{feff}\n")), "메시지", 10, true)), "메시지를 입력하세요.");
    assert_eq!(text(Some(&json!(" ")), "메시지", 10, false).unwrap(), " ");

    assert!(is_uuid("0F0E0D0C-0B0A-4908-8706-050403020100"));
    assert!(!is_uuid("0f0e0d0c-0b0a-4908-8706-05040302010"));
    assert!(!is_uuid("0f0e0d0c-0b0a-4908-8706-05040302010g"));
    assert!(!is_uuid("0f0e0d0c00b0a-4908-8706-050403020100"));
    assert_eq!(message(uuid(&json!(1), "presetId")), "presetId는 UUID여야 합니다.");

    assert_eq!(message(toggle_values(&json!(["a"]))), "토글 값은 객체여야 합니다.");
    assert_eq!(message(toggle_values(&json!(null))), "토글 값은 객체여야 합니다.");
    assert_eq!(message(toggle_values(&json!({ "": "1" }))), "토글 이름이 올바르지 않습니다.");
    let long_key = Value::Object(Map::from_iter([("가".repeat(201), json!("1"))]));
    assert_eq!(message(toggle_values(&long_key)), "토글 이름이 올바르지 않습니다.");
    assert!(toggle_values(&Value::Object(Map::from_iter([("가".repeat(200), json!("1"))]))).is_ok());
    assert_eq!(message(toggle_values(&json!({ "on": 1 }))), "토글 on는 문자열이어야 합니다.");
    assert_eq!(message(toggle_values(&json!({ "__proto__": 1 }))), "토글 __proto__는 문자열이어야 합니다.");
    // Object.entries 순서(정수 모양 키 먼저)로 검사하고, __proto__는 결과에 남지 않는다.
    let values = toggle_values(&json!({ "b": "1", "2": "x", "__proto__": "p", "a": "y" })).unwrap();
    assert_eq!(values.keys().collect::<Vec<_>>(), ["2", "b", "a"]);
    assert_eq!(message(toggle_values(&json!({ "b": 1, "2": 2 }))), "토글 2는 문자열이어야 합니다.");
}

#[test]
fn decodes_assets_by_extension_and_falls_back_to_file_name() {
    let preset = risu_preset_file(&synthetic_preset("decode-test"));
    let (kind, name, data) = decode_roleplay_asset("합성.risup", &preset).unwrap();
    assert_eq!((kind, name.as_str(), &data["moduleIntergration"]), ("preset", "합성 프리셋", &json!("decode-test")));
    // 값이 undefined인 칸은 JSON에서 빠진다.
    assert!(data.get("localStopStrings").is_none());
    // 확장자를 모르면 모듈 다음 프리셋으로 풀어 본다.
    assert_eq!(decode_roleplay_asset("이름없음", &preset).unwrap().0, "preset");

    let unnamed = risu_module_file(&json!({ "namespace": "x", "name": "  " }));
    assert_eq!(decode_roleplay_asset("My.RISUM", &unnamed).unwrap().1, "my");
    assert_eq!(decode_roleplay_asset(".risum", &unnamed).unwrap().1, "module");
    let numbered = risu_module_file(&json!({ "name": 7 }));
    assert_eq!(decode_roleplay_asset("a.risum", &numbered).unwrap().1, "7");

    let error = decode_roleplay_asset("x.risup", &[0, 0, 0]).unwrap_err();
    assert_eq!(error.status(), 400);
    assert!(!error.message().is_empty());
    // 모듈로 먼저 풀다 실패해도 마지막(프리셋) 오류를 알린다.
    let module_only = risu_module_file(&synthetic_module("decode-test"));
    assert_eq!(decode_roleplay_asset("x.risup", &module_only).unwrap_err().status(), 400);
    assert_eq!(decode_roleplay_asset("x.bin", &module_only).unwrap().0, "module");
}

#[tokio::test]
async fn import_checks_size_before_touching_database() {
    let service = offline_service(FakeFetch::new(|_| respond(200, json!({}))), DEFAULT_API_URL, PathBuf::from("/nonexistent"));
    let status = async |body: Value| {
        let error = service.import_asset(&body).await.unwrap_err();
        (error.status(), error.message().to_string())
    };
    assert_eq!(status(json!({})).await, (400, "파일 내용는 문자열이어야 합니다.".into()));
    assert_eq!(status(json!(null)).await, (400, "파일 내용는 문자열이어야 합니다.".into()));
    assert_eq!(status(json!({ "dataBase64": "  " })).await, (400, "파일 내용를 입력하세요.".into()));
    assert_eq!(status(json!({ "dataBase64": "====" })).await, (400, "파일 크기가 올바르지 않습니다.".into()));
    assert_eq!(status(json!({ "fileName": "x.risup", "dataBase64": "AAAA" })).await.0, 400);
}

#[tokio::test]
async fn proxy_models_send_the_key_and_never_return_it() {
    let keys = key_file("secret-key\n");
    let fetch = FakeFetch::new(|_| {
        respond(
            200,
            json!({ "data": [
                { "id": "gemini-4-argon-high", "description": "Gemini 4 Argon (High)" },
                { "id": "claude-opus-5-5-low" },
                { "id": "" },
                { "id": " ", "description": "공백" },
                { "id": 3 },
                "문자열",
            ] }),
        )
    });
    let service = offline_service(fetch.clone(), "http://proxy.test/v1/messages", keys.clone());
    let result = service.list_proxy_models().await.unwrap();
    let calls = fetch.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!((calls[0].method, calls[0].url.as_str(), header(&calls[0], "x-api-key")), ("GET", "http://proxy.test/v1/models", "secret-key"));
    assert_eq!(header(&calls[0], "anthropic-version"), "2023-06-01");
    assert_eq!(calls[0].timeout, MODEL_LIST_TIMEOUT);
    assert_eq!(
        result,
        json!({ "models": [
            { "id": "claude-opus-5-5-low", "title": "claude-opus-5-5-low" },
            { "id": "gemini-4-argon-high", "title": "Gemini 4 Argon (High)" },
        ] })
    );
    assert!(!result.to_string().contains("secret-key"));

    let down = offline_service(FakeFetch::new(|_| Err(FetchFailure::Failed)), DEFAULT_API_URL, keys.clone());
    assert_eq!(down.list_proxy_models().await.unwrap_err(), RoleplayError::new(502, PROXY_UNREACHABLE));
    let refused = offline_service(FakeFetch::new(|_| respond(401, json!({}))), DEFAULT_API_URL, keys.clone());
    assert_eq!(refused.list_proxy_models().await.unwrap_err(), RoleplayError::new(502, "모델 목록을 받지 못했습니다(401)."));
    let broken = offline_service(FakeFetch::new(|_| Ok(FetchResponse { status: 200, body: Err("끊김".into()) })), DEFAULT_API_URL, keys.clone());
    assert_eq!(broken.list_proxy_models().await.unwrap_err(), RoleplayError::new(502, "모델 목록을 받지 못했습니다(200)."));
    let bad_url = offline_service(FakeFetch::new(|_| respond(200, json!({ "data": [] }))), "상대 주소", keys.clone());
    assert_eq!(bad_url.list_proxy_models().await.unwrap_err(), RoleplayError::new(502, PROXY_UNREACHABLE));
    let no_key = offline_service(FakeFetch::new(|_| respond(200, json!({ "data": [] }))), DEFAULT_API_URL, keys.with_extension("missing"));
    assert_eq!(no_key.list_proxy_models().await.unwrap_err(), RoleplayError::new(503, KEY_FILE_UNREADABLE));
    std::fs::remove_file(keys).unwrap();
}

#[tokio::test]
async fn call_model_reports_node_messages() {
    let keys = key_file("\u{feff} test-key \n");
    let fetch = FakeFetch::new(|_| respond(200, json!({ "content": [{ "type": "thinking", "thinking": "숨김" }, { "type": "text", "text": "답" }, { "type": "text", "text": 1 }, { "type": "text", "text": "!" }] })));
    let service = offline_service(fetch.clone(), DEFAULT_API_URL, keys.clone());
    assert_eq!(service.call_model(&json!({ "model": "m" }), None).await.unwrap(), "답!");
    let call = &fetch.calls()[0];
    assert_eq!((call.url.as_str(), header(call, "x-api-key"), call.timeout), (DEFAULT_API_URL, "test-key", REQUEST_TIMEOUT));
    assert_eq!(call.body.as_deref(), Some(r#"{"model":"m"}"#));

    let error = async |reply: Result<FetchResponse, FetchFailure>, local: bool| {
        fetch.set(move |_| reply.clone());
        let target = local.then(|| CallTarget { url: DOOR_URL.into(), key: "door-token".into(), timeout: LOCAL_REQUEST_TIMEOUT });
        service.call_model(&json!({}), target).await.unwrap_err()
    };
    assert_eq!(error(Err(FetchFailure::Timeout), false).await, RoleplayError::new(502, "모델 응답이 너무 오래 걸려 멈췄습니다."));
    assert_eq!(error(Err(FetchFailure::Failed), false).await, RoleplayError::new(502, PROXY_UNREACHABLE));
    assert_eq!(error(Err(FetchFailure::Failed), true).await, RoleplayError::new(502, "로컬 모델에 연결하지 못했습니다."));
    assert_eq!(
        error(respond(401, json!({ "error": { "message": "invalid key" } })), false).await,
        RoleplayError::new(502, "모델 서버가 401로 거절했습니다. invalid key")
    );
    assert_eq!(error(respond(503, json!({ "error": "로컬 연결 실패" })), true).await, RoleplayError::new(502, "모델 서버가 503로 거절했습니다. 로컬 연결 실패"));
    assert_eq!(error(respond(500, json!({ "error": { "message": 1 } })), false).await, RoleplayError::new(502, "모델 서버가 500로 거절했습니다."));
    assert_eq!(error(respond(500, json!({ "error": "" })), false).await, RoleplayError::new(502, "모델 서버가 500로 거절했습니다."));
    let long = "가".repeat(400);
    assert_eq!(
        error(respond(429, json!({ "error": long })), false).await.message(),
        format!("모델 서버가 429로 거절했습니다. {}", "가".repeat(300))
    );
    assert_eq!(error(respond(200, json!({ "content": [{ "type": "text", "text": " \n" }] })), false).await, RoleplayError::new(502, "모델이 빈 답을 보냈습니다."));
    assert_eq!(error(respond(200, json!(null)), false).await, RoleplayError::new(502, "모델이 빈 답을 보냈습니다."));
    assert_eq!(error(Ok(FetchResponse { status: 200, body: Err("끊김".into()) }), false).await, RoleplayError::Internal("끊김".into()));
    std::fs::remove_file(&keys).unwrap();
    assert_eq!(error(respond(200, text_reply("답")), false).await, RoleplayError::new(503, KEY_FILE_UNREADABLE));
}

#[tokio::test]
async fn unreachable_proxy_is_started_then_the_same_request_is_resent() {
    let keys = key_file("k");
    let fetch = FakeFetch::new(|_| Err(FetchFailure::Unreachable));
    // 켜면 그 뒤로는 응답한다.
    let starter = {
        let fetch = fetch.clone();
        FakeStarter::new(move || {
            fetch.set(|request| match request.method {
                "GET" => respond(200, json!({ "data": [{ "id": "gemini-4-argon-low" }] })),
                _ => respond(200, text_reply("켜진 뒤 답")),
            });
            true
        })
    };
    let service = service_with_starter(fetch.clone(), keys.clone(), starter.clone());
    let models = service.list_proxy_models().await.unwrap();
    assert_eq!(models, json!({ "models": [{ "id": "gemini-4-argon-low", "title": "gemini-4-argon-low" }] }));
    let calls = fetch.calls();
    assert_eq!((starter.started(), calls.len()), (1, 2));
    assert_eq!(calls[0], calls[1]);

    fetch.set(|_| Err(FetchFailure::Unreachable));
    assert_eq!(service.call_model(&json!({ "model": "m" }), None).await.unwrap(), "켜진 뒤 답");
    let calls = fetch.calls();
    assert_eq!((starter.started(), calls.len()), (2, 4));
    assert_eq!(calls[2], calls[3]);
    assert_eq!(calls[3].url, DEFAULT_API_URL);

    // 연결된 뒤 끊기거나 늦은 요청은 이미 나갔을 수 있어 켜지도 다시 보내지도 않는다.
    for failure in [FetchFailure::Failed, FetchFailure::Timeout] {
        fetch.set(move |_| Err(failure));
        assert!(service.call_model(&json!({}), None).await.is_err());
        assert!(service.list_proxy_models().await.is_err());
    }
    assert_eq!((starter.started(), fetch.calls().len()), (2, 8));

    // 로컬 문은 이 프록시와 무관하다.
    fetch.set(|_| Err(FetchFailure::Unreachable));
    let target = CallTarget { url: DOOR_URL.into(), key: "door-token".into(), timeout: LOCAL_REQUEST_TIMEOUT };
    assert_eq!(service.call_model(&json!({}), Some(target)).await.unwrap_err(), RoleplayError::new(502, "로컬 모델에 연결하지 못했습니다."));
    assert_eq!((starter.started(), fetch.calls().len()), (2, 9));

    // 켜지 못하면 다시 보내지 않고 지금 안내를 보여 준다.
    let dead = FakeStarter::new(|| false);
    let fetch = FakeFetch::new(|_| Err(FetchFailure::Unreachable));
    let service = service_with_starter(fetch.clone(), keys.clone(), dead.clone());
    assert_eq!(service.list_proxy_models().await.unwrap_err(), RoleplayError::new(502, PROXY_UNREACHABLE));
    assert_eq!(service.call_model(&json!({}), None).await.unwrap_err(), RoleplayError::new(502, PROXY_UNREACHABLE));
    assert_eq!((dead.started(), fetch.calls().len()), (2, 2));
    std::fs::remove_file(keys).unwrap();
}

#[tokio::test]
async fn script_starter_runs_the_script_only_while_the_port_is_closed() {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir().join(format!("roleplay-proxy-{}", random_uuid()));
    std::fs::create_dir_all(&directory).unwrap();
    let ran = directory.join("ran");
    let script = directory.join("start.sh");
    // 상대 경로로 남겨 스크립트 폴더에서 도는지도 본다.
    std::fs::write(&script, "#!/bin/sh\necho run >> ran\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let free = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = free.local_addr().unwrap().to_string();
    drop(free);
    // 스크립트가 돌면 그 포트를 연다(프록시가 뜬 것처럼).
    let opener = {
        let (ran, address) = (ran.clone(), address.clone());
        tokio::spawn(async move {
            while !ran.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            TcpListener::bind(address).await.unwrap()
        })
    };
    let starter = ScriptProxyStarter::new(script.clone(), address.clone(), Duration::from_secs(5), Duration::from_secs(3));
    assert!(starter.start().await);
    let listener = opener.await.unwrap();
    // 이미 열려 있으면 스크립트를 다시 돌리지 않는다.
    assert!(starter.start().await);
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "run\n");
    drop(listener);

    // 스크립트가 실패하거나 늦거나, 끝나도 포트가 안 열리면 켜지 못한 것이다.
    let closed = ScriptProxyStarter::new(script.clone(), address.clone(), Duration::from_secs(5), Duration::from_millis(300));
    std::fs::write(&script, "#!/bin/sh\nexit 3\n").unwrap();
    assert!(!closed.start().await);
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    assert!(!closed.start().await);
    let slow = ScriptProxyStarter::new(script.clone(), address.clone(), Duration::from_millis(300), Duration::from_millis(300));
    std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
    assert!(!slow.start().await);
    let missing = ScriptProxyStarter::new(directory.join("없음.sh"), address, Duration::from_secs(5), Duration::from_millis(300));
    assert!(!missing.start().await);
    std::fs::remove_dir_all(directory).unwrap();
}

/// 요청 하나를 받아 본문까지 읽고 정해 둔 응답을 보낸 뒤, 받은 요청을 돌려준다.
async fn one_shot_server(response: &'static [u8]) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&request).into_owned();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .to_lowercase()
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:").map(|value| value.trim().parse::<usize>().unwrap()))
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            if read == 0 {
                break;
            }
        }
        socket.write_all(response).await.unwrap();
        socket.shutdown().await.unwrap();
        String::from_utf8_lossy(&request).into_owned()
    });
    (format!("http://{address}"), task)
}

#[tokio::test]
async fn http_fetch_sends_headers_and_reads_text_like_node() {
    let fetch = HttpFetch::new().unwrap();
    let (url, server) = one_shot_server(b"HTTP/1.1 201 Created\r\ncontent-length: 14\r\nconnection: close\r\n\r\n\xEF\xBB\xBF{\"ok\":true}").await;
    let response = fetch
        .fetch(FetchRequest {
            method: "POST",
            url: format!("{url}/v1/messages"),
            headers: vec![("content-type", "application/json".into()), ("anthropic-version", "2023-06-01".into()), ("x-api-key", "k".into())],
            body: Some(r#"{"a":1}"#.into()),
            timeout: Duration::from_secs(5),
        })
        .await
        .unwrap();
    assert_eq!(response, FetchResponse { status: 201, body: Ok(r#"{"ok":true}"#.into()) });
    assert!(response.ok());
    let request = server.await.unwrap();
    assert!(request.starts_with("POST /v1/messages HTTP/1.1\r\n"), "{request}");
    let lower = request.to_lowercase();
    for line in ["\r\ncontent-type: application/json\r\n", "\r\nanthropic-version: 2023-06-01\r\n", "\r\nx-api-key: k\r\n"] {
        assert!(lower.contains(line), "{line:?} 없음");
    }
    assert!(request.ends_with("\r\n\r\n{\"a\":1}"));

    // 응답 머리글이 오지 않으면 시간 초과, 받는 쪽이 없으면 연결부터 못 한 것이다.
    // 요청을 받은 뒤 답 없이 끊기면 요청은 나갔으므로 그냥 실패다(다시 보내지 않는다).
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hold = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(socket);
    });
    let request = |url: String, timeout| FetchRequest { method: "GET", url, headers: Vec::new(), body: None, timeout };
    assert_eq!(fetch.fetch(request(format!("http://{address}/v1/models"), Duration::from_millis(200))).await, Err(FetchFailure::Timeout));
    hold.abort();
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    assert_eq!(fetch.fetch(request(format!("http://{closed}/v1/models"), Duration::from_secs(5))).await, Err(FetchFailure::Unreachable));
    let (dropped, server) = one_shot_server(b"").await;
    assert_eq!(fetch.fetch(request(format!("{dropped}/v1/models"), Duration::from_secs(5))).await, Err(FetchFailure::Failed));
    assert!(server.await.unwrap().starts_with("GET /v1/models HTTP/1.1\r\n"));
}

static MIGRATED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn database() -> Option<PgPool> {
    let (Ok(url), Ok(migrations)) = (std::env::var("OFFICE_TEST_DATABASE_URL"), std::env::var("OFFICE_MIGRATIONS_DIR")) else {
        eprintln!("OFFICE_TEST_DATABASE_URL·OFFICE_MIGRATIONS_DIR이 없어 롤플레이 DB 시험 건너뜀");
        return None;
    };
    let pool = crate::db::connect(&url).await.unwrap();
    MIGRATED
        .get_or_init(|| async {
            crate::migrate::run(&pool, std::path::Path::new(&migrations)).await.unwrap();
        })
        .await;
    Some(pool)
}

fn local_definition(id: &str, backend: &str, usage_protocol: &str) -> Value {
    json!({
        "profile": { "id": id, "providerKind": "local", "backend": backend, "model": "test-model", "endpoint": "http://127.0.0.1:41235",
            "credentialEnv": "OFFICESTRA_LOCAL_TEST_TOKEN", "credentialVersion": "v1", "contextWindow": 32768, "maxOutputTokens": 4096,
            "usageProtocol": usage_protocol },
        "host": { "address": "192.0.2.10", "user": "test", "sshPort": 2222, "keyPath": "/tmp/test-key", "hostKeyAlias": "test-host",
            "modelKey": "test/model", "comfyPort": 8188 },
    })
}

fn conversation(state: &Value) -> Vec<(String, String)> {
    state["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| (message["role"].as_str().unwrap().to_string(), message["content"].as_str().unwrap().to_string()))
        .collect()
}

/// Node "롤플레이 서비스 DB 통합". 시험용 직원·로컬 프로필·합성 자산만 만들고 끝나면 지운다.
#[tokio::test]
async fn service_round_trip_with_database() {
    use futures_util::FutureExt;

    let Some(pool) = database().await else { return };
    let suffix = random_uuid();
    let id = format!("roleplay-test-{suffix}");
    let local_test = format!("roleplay-local-test-{suffix}");
    let local_codex = format!("roleplay-local-codex-{suffix}");
    let local_off = format!("roleplay-local-off-{suffix}");
    let namespace = "roleplay-service-test";
    let preset_file = risu_preset_file(&synthetic_preset(namespace));
    let module_file = risu_module_file(&synthetic_module(namespace));
    let hashes = vec![sha256_hex(&preset_file), sha256_hex(&module_file)];
    let profiles = vec![local_test.clone(), local_codex.clone(), local_off.clone()];
    let cleanup = async || {
        // 직원을 지우면 롤플레이 설정·대화·메시지가 외래 키로 함께 지워진다.
        sqlx::query("DELETE FROM characters WHERE id = $1").bind(&id).execute(&pool).await.unwrap();
        sqlx::query("DELETE FROM local_agent_profiles WHERE id = ANY($1)").bind(&profiles).execute(&pool).await.unwrap();
        sqlx::query("DELETE FROM roleplay_assets WHERE source_sha256 = ANY($1)").bind(&hashes).execute(&pool).await.unwrap();
    };
    cleanup().await;
    sqlx::query("INSERT INTO characters (id, name, seat, backend, permission, config) VALUES ($1, '코대리', '상단', 'claude', 'acceptEdits', '{}')")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    for (profile_id, backend, protocol, enabled) in [
        (&local_test, "claude", "anthropic-normalized-v1", true),
        (&local_codex, "codex", "openai-responses-v1", true),
        (&local_off, "claude", "anthropic-normalized-v1", false),
    ] {
        sqlx::query("INSERT INTO local_agent_profiles (id, definition, enabled) VALUES ($1, $2, $3)")
            .bind(profile_id)
            .bind(local_definition(profile_id, backend, protocol))
            .bind(enabled)
            .execute(&pool)
            .await
            .unwrap();
    }
    let keys = key_file("test-key\n");
    let api_url = "http://192.0.2.1:1/v1/messages";
    let fetch = FakeFetch::new(|_| {
        respond(200, json!({ "content": [{ "type": "thinking", "thinking": "숨김" }, { "type": "text", "text": "답 123" }] }))
    });
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let doors_slot: Arc<Mutex<Option<Arc<dyn LocalDoors>>>> = Arc::default();
    let service = RoleplayService::new(RoleplayOptions {
        pool: pool.clone(),
        broadcast: {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        },
        fetch: fetch.clone(),
        api_url: api_url.into(),
        api_key_file: keys.clone(),
        local_doors: {
            let slot = doors_slot.clone();
            Arc::new(move || slot.lock().unwrap().clone())
        },
        proxy_starter: None,
    });

    let scenarios = async {
        let send = async |content: &str| {
            let accepted = service.send_message(&id, &json!({ "content": content })).await.unwrap();
            service.wait_for_reply(&id).await;
            accepted
        };
        let read = async || service.read_profile(&id).await.unwrap();
        let update = async |body: Value| service.update_profile(&id, &body).await;
        let import = async |file_name: &str, bytes: &[u8]| {
            let encoded = crate::local_provider::host::base64_encode(bytes);
            service.import_asset(&json!({ "fileName": file_name, "dataBase64": encoded })).await
        };

        // 가져오기: 같은 파일은 이름이 달라도 한 번만 저장한다.
        let first = import("합성.risup", &preset_file).await.unwrap();
        let again = import("다른이름.risup", &preset_file).await.unwrap();
        let module = import("합성.risum", &module_file).await.unwrap();
        assert_eq!((&first["created"], &again["created"]), (&json!(true), &json!(false)));
        assert_eq!(again["asset"]["id"], first["asset"]["id"]);
        let summary = first["asset"].as_object().unwrap();
        assert_eq!(summary.keys().collect::<Vec<_>>(), ["id", "kind", "name", "importedAt", "promptCount", "regexCount", "toggleCount"]);
        assert_eq!(
            [&summary["kind"], &summary["name"], &summary["promptCount"], &summary["regexCount"], &summary["toggleCount"]],
            [&json!("preset"), &json!("합성 프리셋"), &json!(6), &json!(1), &json!(2)]
        );
        assert!(summary["importedAt"].as_str().unwrap().ends_with('Z'));
        assert_eq!((&module["asset"]["kind"], &module["asset"]["regexCount"]), (&json!("module"), &json!(2)));
        assert_eq!(import("x.risup", b"AAAA").await.unwrap_err().status(), 400);
        let preset_id = first["asset"]["id"].as_str().unwrap().to_string();
        let module_id = module["asset"]["id"].as_str().unwrap().to_string();
        let listed = service.list_assets().await.unwrap();
        assert_eq!(listed.iter().filter(|asset| asset["id"] == preset_id || asset["id"] == module_id).count(), 2);

        // 프리셋 없이 보내면 거절하고 잘못된 자산 종류도 거절한다.
        let default_profile = read().await;
        assert_eq!(
            default_profile["profile"].to_string(),
            json!({ "characterId": id, "enabled": false, "presetId": null, "moduleIds": [], "toggles": {}, "characterDescription": "",
                "firstMessage": "", "userName": "사용자", "persona": "", "model": "gemini-3.8-flash-high", "localProfileId": null,
                "speakerCue": false, "currentChatId": null, "updatedAt": null })
            .to_string()
        );
        assert_eq!(
            default_profile.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["profile", "toggleDefinitions", "activeModuleIds", "messages", "generating", "lastError"]
        );
        assert_eq!(service.send_message(&id, &json!({ "content": "안녕" })).await.unwrap_err(), RoleplayError::new(409, "롤플레이 프리셋을 먼저 고르세요."));
        assert_eq!(update(json!({ "presetId": module_id })).await.unwrap_err(), RoleplayError::new(400, "가져온 프리셋이 아닙니다."));
        assert_eq!(update(json!({ "moduleIds": [preset_id] })).await.unwrap_err(), RoleplayError::new(400, "가져온 모듈이 아닙니다."));
        assert_eq!(update(json!({ "moduleIds": "x" })).await.unwrap_err(), RoleplayError::new(400, "moduleIds는 배열이어야 합니다."));
        assert_eq!(update(json!({ "moduleIds": ["x"] })).await.unwrap_err(), RoleplayError::new(400, "moduleIds는 UUID여야 합니다."));
        assert_eq!(update(json!({ "enabled": 1 })).await.unwrap_err(), RoleplayError::new(400, "enabled는 참/거짓이어야 합니다."));
        assert_eq!(update(json!({ "userName": " " })).await.unwrap_err(), RoleplayError::new(400, "사용자 이름를 입력하세요."));
        assert_eq!(update(json!(null)).await.unwrap_err().status(), 500);
        assert_eq!(service.update_profile("roleplay-missing", &json!({})).await.unwrap_err(), RoleplayError::new(404, "직원을 찾을 수 없습니다."));
        // 내용 검사가 직원 확인보다 먼저다.
        assert_eq!(service.send_message("roleplay-missing", &json!({})).await.unwrap_err(), RoleplayError::new(400, "메시지는 문자열이어야 합니다."));
        assert_eq!(service.send_message("roleplay-missing", &json!({ "content": "x" })).await.unwrap_err().status(), 404);
        assert_eq!(service.start_new_chat("roleplay-missing").await.unwrap_err().status(), 404);
        assert!(events.lock().unwrap().is_empty());

        // 설정을 저장하고 통합 모듈의 토글까지 보여 준다.
        let saved = update(json!({
            "enabled": true,
            "presetId": preset_id,
            "toggles": { "on": "1" },
            "characterDescription": "명랑한 개발자",
            "userName": " 민지 ",
        }))
        .await
        .unwrap();
        assert_eq!((&saved["profile"]["enabled"], &saved["profile"]["speakerCue"]), (&json!(true), &json!(false)));
        assert_eq!((&saved["profile"]["userName"], &saved["profile"]["toggles"]), (&json!("민지"), &json!({ "on": "1" })));
        assert!(saved["profile"]["updatedAt"].as_str().unwrap().ends_with('Z'));
        assert_eq!(update(json!({ "speakerCue": true })).await.unwrap()["profile"]["speakerCue"], json!(true));
        assert_eq!(update(json!({ "speakerCue": false })).await.unwrap()["profile"]["speakerCue"], json!(false));
        assert_eq!(update(json!({ "speakerCue": "1" })).await.unwrap_err(), RoleplayError::new(400, "speakerCue는 참/거짓이어야 합니다."));
        assert_eq!(saved["activeModuleIds"], json!([module_id]));
        let keys_of = |state: &Value| state["toggleDefinitions"].as_array().unwrap().iter().map(|toggle| toggle["key"].clone()).collect::<Vec<_>>();
        assert_eq!(keys_of(&saved), [json!("on"), json!("mode"), json!("")]);
        let chosen = update(json!({ "moduleIds": [module_id, module_id] })).await.unwrap();
        assert_eq!((&chosen["profile"]["moduleIds"], &chosen["activeModuleIds"]), (&json!([module_id]), &json!([module_id])));
        assert_eq!(update(json!({ "moduleIds": [] })).await.unwrap()["profile"]["moduleIds"], json!([]));
        assert_eq!(events.lock().unwrap().last().unwrap(), &json!({ "type": "roleplay.changed", "characterId": id }));

        // 메시지를 보내면 입력·출력 정규식을 거쳐 대화에 저장한다.
        let accepted = send("/안녕").await;
        assert_eq!(accepted.as_object().unwrap().keys().collect::<Vec<_>>(), ["accepted", "chatId"]);
        let state = read().await;
        assert_eq!(accepted["chatId"], state["profile"]["currentChatId"]);
        assert_eq!(conversation(&state), [("user".into(), "안녕".into()), ("char".into(), "답 [123]".into())]);
        assert_eq!((&state["generating"], &state["lastError"]), (&json!(false), &Value::Null));
        assert!(state["messages"][0]["createdAt"].as_str().unwrap().ends_with('Z'));
        let sent = fetch.calls().pop().unwrap();
        assert_eq!((sent.url.as_str(), header(&sent, "x-api-key"), sent.timeout), (api_url, "test-key", REQUEST_TIMEOUT));
        let body = sent_body(&sent);
        assert_eq!(body["model"], json!("gemini-3.8-flash-high"));
        assert!(body["system"].as_str().unwrap().starts_with("시작 켜짐"), "{}", body["system"]);
        assert_eq!(body["messages"][0]["content"], json!("안녕"));

        // 답을 만드는 동안 다음 메시지와 새 대화는 막는다.
        let gate = Arc::new(tokio::sync::Notify::new());
        fetch.set_async({
            let gate = gate.clone();
            Arc::new(move |_| {
                let gate = gate.clone();
                Box::pin(async move {
                    gate.notified().await;
                    respond(200, text_reply("늦은 답"))
                })
            })
        });
        service.send_message(&id, &json!({ "content": "기다려" })).await.unwrap();
        assert_eq!(service.send_message(&id, &json!({ "content": "또" })).await.unwrap_err(), RoleplayError::new(409, "이전 답을 만드는 중입니다."));
        assert_eq!(service.start_new_chat(&id).await.unwrap_err(), RoleplayError::new(409, "답을 만드는 중에는 새 대화를 시작할 수 없습니다."));
        assert_eq!(read().await["generating"], json!(true));
        gate.notify_one();
        service.wait_for_reply(&id).await;
        assert_eq!(conversation(&read().await).last().unwrap().1, "늦은 답");

        // 모델 오류는 마지막 오류로 남고 키는 노출하지 않는다.
        fetch.set(|_| respond(401, json!({ "error": { "message": "invalid key" } })));
        send("실패").await;
        let state = read().await;
        assert_eq!(state["lastError"], json!("모델 서버가 401로 거절했습니다. invalid key"));
        assert!(!state.to_string().contains("test-key"));
        assert_eq!(conversation(&state).last().unwrap().0, "user");

        // 로컬 프로필은 켜진 Messages 형식만 고를 수 있다.
        assert_eq!(
            update(json!({ "localProfileId": local_codex })).await.unwrap_err(),
            RoleplayError::new(400, "Messages 형식을 쓰는 로컬 프로필만 롤플레이에 연결할 수 있습니다.")
        );
        assert_eq!(update(json!({ "localProfileId": local_off })).await.unwrap_err(), RoleplayError::new(400, "로컬 프로필이 꺼져 있거나 없습니다."));
        assert_eq!(update(json!({ "localProfileId": "missing" })).await.unwrap_err(), RoleplayError::new(400, "로컬 프로필이 꺼져 있거나 없습니다."));
        assert_eq!(update(json!({ "localProfileId": 3 })).await.unwrap_err(), RoleplayError::new(400, "로컬 프로필는 문자열이어야 합니다."));
        assert_eq!(update(json!({ "localProfileId": local_test })).await.unwrap()["profile"]["localProfileId"], json!(local_test));

        // 로컬 프로필이면 로컬 문으로 보내고 모델명과 출력 한도를 맞춘다.
        fetch.set(|_| respond(200, text_reply("로컬 답")));
        send("서비스 없음").await;
        assert_eq!(read().await["lastError"], json!("로컬 모델 서비스가 준비되지 않았습니다."));
        let doors = Arc::new(FakeDoors::default());
        *doors_slot.lock().unwrap() = Some(doors.clone());
        send("로컬").await;
        let state = read().await;
        assert_eq!((&state["lastError"], &conversation(&state).last().unwrap().1), (&Value::Null, &"로컬 답".to_string()));
        let sent = fetch.calls().pop().unwrap();
        assert_eq!((sent.url.as_str(), header(&sent, "x-api-key"), sent.timeout), (DOOR_URL, "door-token", LOCAL_REQUEST_TIMEOUT));
        let body = sent_body(&sent);
        assert_eq!((&body["model"], &body["max_tokens"]), (&json!("test-model"), &json!(1000)));
        {
            let opened = doors.opened.lock().unwrap();
            assert_eq!(opened.len(), 1);
            assert_eq!((&opened[0].definition["profile"]["id"], &opened[0].definition["host"]["address"]), (&json!(local_test), &json!("192.0.2.10")));
            assert!(opened[0].released.load(AtomicOrdering::SeqCst));
        }

        // 직원에게 다른 프로필이 배정돼 있어도 저장한 PC 주소는 롤플레이도 따른다.
        sqlx::query("UPDATE characters SET config = $2 WHERE id = $1")
            .bind(&id)
            .bind(json!({ "localProfileId": local_codex, "localHostAddress": "192.0.2.11", "localReasoning": "high" }))
            .execute(&pool)
            .await
            .unwrap();
        fetch.set(|_| respond(503, json!({ "error": "로컬 연결 실패: 시간 초과" })));
        send("로컬 실패").await;
        assert_eq!(read().await["lastError"], json!("모델 서버가 503로 거절했습니다. 로컬 연결 실패: 시간 초과"));
        {
            let opened = doors.opened.lock().unwrap();
            let last = opened.last().unwrap();
            assert_eq!(last.definition["host"]["address"], json!("192.0.2.11"));
            assert_ne!(last.definition["profile"]["reasoning"], json!("high"));
            assert!(last.released.load(AtomicOrdering::SeqCst));
        }
        assert_eq!(update(json!({ "localProfileId": "" })).await.unwrap()["profile"]["localProfileId"], Value::Null);
        fetch.set(|_| respond(200, text_reply("프록시 답")));
        send("프록시").await;
        assert_eq!(fetch.calls().pop().unwrap().url, api_url);
        assert_eq!(doors.opened.lock().unwrap().len(), 2);

        // 로컬 한도를 넘으면 오래된 대화부터 빼고 보낸다. 메시지 하나를 400토큰으로 세며 한도는 3000 - 답 1000 - 여유 256 = 1744다.
        update(json!({ "localProfileId": local_test })).await.unwrap();
        *doors.context_window.lock().unwrap() = Some(3000.0);
        fetch.set(|request| {
            if request.url.ends_with("/count_tokens") {
                respond(200, json!({ "input_tokens": sent_body(request)["messages"].as_array().unwrap().len() * 400 }))
            } else {
                respond(200, text_reply("잘린 답"))
            }
        });
        let before = fetch.calls().len();
        send("길어진 대화").await;
        let state = read().await;
        assert_eq!((&state["lastError"], &conversation(&state).last().unwrap().1), (&Value::Null, &"잘린 답".to_string()));
        let sent = fetch.calls().split_off(before);
        let counts: Vec<_> = sent.iter().filter(|call| call.url.ends_with("/count_tokens")).collect();
        assert!(counts.len() >= 2, "센 횟수 {}", counts.len());
        assert!(counts.iter().all(|call| call.url == format!("{DOOR_URL}/count_tokens") && header(call, "x-api-key") == "door-token"));
        let generated = sent.last().unwrap();
        assert_eq!(generated.url, DOOR_URL);
        let messages = sent_body(generated)["messages"].clone();
        let count = messages.as_array().unwrap().len();
        assert!(count >= 1 && count * 400 <= 1744, "보낸 메시지 {count}개");
        assert!(messages.to_string().contains("길어진 대화"));
        assert!(!messages.to_string().contains("\"안녕\""));

        // 마지막 메시지만 남겨도 넘으면 생성 요청 없이 오류로 남긴다. 한도는 1300 - 1000 - 256 = 44다.
        *doors.context_window.lock().unwrap() = Some(1300.0);
        let before = fetch.calls().len();
        send("너무 작음").await;
        let error = read().await["lastError"].as_str().unwrap().to_string();
        assert!(error.starts_with("프리셋과 마지막 메시지만으로도 로컬 모델 한도를 넘습니다(") && error.ends_with("토큰, 한도 44토큰)."), "{error}");
        assert!(fetch.calls()[before..].iter().all(|call| call.url.ends_with("/count_tokens")));
        assert_eq!(update(json!({ "localProfileId": "" })).await.unwrap()["profile"]["localProfileId"], Value::Null);

        // 새 대화를 시작하면 빈 대화로 바뀌고 지난 대화는 지워진다.
        let count_in = async |chat_id: &str| -> i64 {
            sqlx::query_scalar("SELECT count(*) FROM roleplay_messages WHERE chat_id = $1::uuid").bind(chat_id).fetch_one(&pool).await.unwrap()
        };
        let previous = read().await["profile"]["currentChatId"].as_str().unwrap().to_string();
        assert!(count_in(&previous).await > 0);
        let state = service.start_new_chat(&id).await.unwrap();
        assert_eq!((&state["messages"], &state["lastError"]), (&json!([]), &Value::Null));
        let chats: Vec<String> =
            sqlx::query_scalar("SELECT id::text FROM roleplay_chats WHERE character_id = $1").bind(&id).fetch_all(&pool).await.unwrap();
        assert_eq!(chats, [state["profile"]["currentChatId"].as_str().unwrap().to_string()]);
        assert_eq!(count_in(&previous).await, 0);
        assert!(events.lock().unwrap().iter().all(|event| event == &json!({ "type": "roleplay.changed", "characterId": id })));
    };
    let outcome = std::panic::AssertUnwindSafe(scenarios).catch_unwind().await;
    cleanup().await;
    std::fs::remove_file(&keys).unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
