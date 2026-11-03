// 로컬 임베딩 서비스 테스트. 가짜 워커로 Node local-embedding.test.mjs의 수명 규칙을 확인하고, 모델이 있으면 실제 워커 벡터를 Node 결과와 비교한다.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use officestra_backend::js::to_js_json;
use officestra_backend::local_embedding::{
    EmbeddingOptions, Environment, LocalEmbeddingService, ModelLoader, WorkerEvent, WorkerFactory, WorkerHandle,
    WorkerRequest, ensure_local_embedding_model, process_worker_factory,
};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

#[derive(Clone, Copy)]
enum Reply {
    Vectors,
    Fail,
    Silent,
    Exit,
}

/// Node 테스트의 FakeWorker. 문장마다 [1, 0]을 돌려주고 kill 횟수를 센다.
#[derive(Default)]
struct Workers {
    created: AtomicUsize,
    killed: AtomicUsize,
}

fn fake_factory(workers: Arc<Workers>, reply: Reply) -> WorkerFactory {
    Arc::new(move |_directory: &Path, _environment: &Environment| {
        workers.created.fetch_add(1, Ordering::SeqCst);
        let (requests, mut incoming) = mpsc::unbounded_channel::<WorkerRequest>();
        let (events, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(request) = incoming.recv().await {
                let _ = request.delivered.send(Ok(()));
                let event = match reply {
                    Reply::Vectors => WorkerEvent::Message(json!({
                        "id": request.id,
                        "ok": true,
                        "result": {
                            "vectors": request.texts.iter().map(|_| json!([1, 0])).collect::<Vec<_>>(),
                            "rssBytes": 2048,
                            "maxRSSBytes": 4096,
                        },
                    })),
                    Reply::Fail => WorkerEvent::Message(json!({ "id": request.id, "ok": false, "error": "모델 실행 실패" })),
                    Reply::Silent => continue,
                    Reply::Exit => WorkerEvent::Exit("로컬 임베딩 워커가 종료됐습니다. code=1 signal=null".into()),
                };
                let _ = events.send(event);
            }
        });
        let counter = workers.clone();
        Ok(WorkerHandle {
            requests,
            events: receiver,
            kill: Box::new(move || {
                counter.killed.fetch_add(1, Ordering::SeqCst);
            }),
        })
    })
}

fn ready_loader() -> ModelLoader {
    Arc::new(|_| async { Ok(PathBuf::from("/model")) }.boxed())
}

fn options(workers: &Arc<Workers>, reply: Reply) -> EmbeddingOptions {
    EmbeddingOptions {
        idle_timeout: Duration::from_millis(1000),
        request_timeout: Duration::from_millis(1000),
        worker_factory: fake_factory(workers.clone(), reply),
        model_loader: ready_loader(),
        environment: Environment::new(),
        logger: Arc::new(|_| {}),
    }
}

fn texts(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

fn js(value: &Value) -> String {
    String::from_utf8(to_js_json(value).unwrap()).unwrap()
}

#[tokio::test]
async fn 임베딩_워커는_필요할_때_한_번_만들고_유휴_제한_뒤_종료한다() {
    let workers = Arc::new(Workers::default());
    let service = LocalEmbeddingService::new(EmbeddingOptions {
        idle_timeout: Duration::from_millis(20),
        ..options(&workers, Reply::Vectors)
    });
    assert!(!service.has_active_worker());
    assert_eq!(service.embed(&texts(&["검색 질문"])).await.unwrap(), vec![json!([1, 0])]);
    assert!(service.has_active_worker());
    assert_eq!(workers.created.load(Ordering::SeqCst), 1);
    assert_eq!(js(&service.metrics()), r#"{"active":true,"peakWorkerRSSBytes":4096,"lastWorkerRSSBytes":2048}"#);

    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!service.has_active_worker());
    assert_eq!(workers.killed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn 동시에_요청해도_모델_준비_후_워커는_하나만_생성한다() {
    let workers = Arc::new(Workers::default());
    let gate = Arc::new(Semaphore::new(0));
    let loads = Arc::new(AtomicUsize::new(0));
    let (loader_gate, loader_count) = (gate.clone(), loads.clone());
    let service = LocalEmbeddingService::new(EmbeddingOptions {
        model_loader: Arc::new(move |_| {
            loader_count.fetch_add(1, Ordering::SeqCst);
            let gate = loader_gate.clone();
            async move {
                let _permit = gate.acquire().await.map_err(|error| error.to_string())?;
                Ok(PathBuf::from("/model"))
            }
            .boxed()
        }),
        ..options(&workers, Reply::Vectors)
    });
    let (first_service, second_service) = (service.clone(), service.clone());
    let first = tokio::spawn(async move { first_service.embed(&texts(&["첫 질문"])).await });
    let second = tokio::spawn(async move { second_service.embed(&texts(&["둘째 질문"])).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    gate.add_permits(1);
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
    assert_eq!(workers.created.load(Ordering::SeqCst), 1);
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    service.close();
}

#[tokio::test]
async fn 워커_준비_실패는_임베딩_비활성_사유를_로그로_남기고_재시도한다() {
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let (logged, counted) = (warnings.clone(), attempts.clone());
    let workers = Arc::new(Workers::default());
    let service = LocalEmbeddingService::new(EmbeddingOptions {
        model_loader: Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            async { Err("모델 파일 검증 실패".to_string()) }.boxed()
        }),
        logger: Arc::new(move |message| logged.lock().unwrap().push(message.to_string())),
        ..options(&workers, Reply::Vectors)
    });
    assert_eq!(service.embed(&texts(&["첫 질문"])).await.unwrap_err(), "모델 파일 검증 실패");
    assert_eq!(service.embed(&texts(&["둘째 질문"])).await.unwrap_err(), "모델 파일 검증 실패");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(*warnings.lock().unwrap(), vec!["임베딩 비활성(사유): 모델 파일 검증 실패"]);
    assert_eq!(workers.created.load(Ordering::SeqCst), 0);
    service.close();
}

#[tokio::test]
async fn 빈_문장과_종료한_서비스는_워커를_만들지_않고_거절한다() {
    let workers = Arc::new(Workers::default());
    let service = LocalEmbeddingService::new(options(&workers, Reply::Vectors));
    let blank = "임베딩할 비어 있지 않은 텍스트가 필요합니다.";
    assert_eq!(service.embed(&[]).await.unwrap_err(), blank);
    assert_eq!(service.embed(&texts(&["질문", " \n\u{3000}"])).await.unwrap_err(), blank);
    service.close();
    assert_eq!(service.embed(&texts(&["질문"])).await.unwrap_err(), "로컬 임베딩 서비스를 종료했습니다.");
    assert_eq!(workers.created.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn 워커_오류·시간_초과·비정상_종료는_사유를_돌려주고_다음_요청에서_새_워커를_띄운다() {
    for (reply, expected, killed) in [
        (Reply::Fail, "모델 실행 실패", 1),
        (Reply::Silent, "로컬 임베딩 응답 시간이 초과됐습니다.", 1),
        (Reply::Exit, "로컬 임베딩 워커가 종료됐습니다. code=1 signal=null", 0),
    ] {
        let workers = Arc::new(Workers::default());
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let logged = warnings.clone();
        let service = LocalEmbeddingService::new(EmbeddingOptions {
            request_timeout: Duration::from_millis(30),
            logger: Arc::new(move |message| logged.lock().unwrap().push(message.to_string())),
            ..options(&workers, reply)
        });
        assert_eq!(service.embed(&texts(&["질문"])).await.unwrap_err(), expected);
        assert!(!service.has_active_worker());
        assert_eq!(workers.killed.load(Ordering::SeqCst), killed);
        assert_eq!(*warnings.lock().unwrap(), vec![format!("임베딩 비활성(사유): {expected}")]);
        assert_eq!(service.embed(&texts(&["다시"])).await.unwrap_err(), expected);
        assert_eq!(workers.created.load(Ordering::SeqCst), 2);
        // 같은 사유는 한 번만 경고한다.
        assert_eq!(warnings.lock().unwrap().len(), 1);
        service.close();
    }
}

#[tokio::test]
async fn 모델_파일이_없고_내려받기를_막으면_첫_파일_이름으로_실패한다() {
    let directory = std::env::temp_dir().join(format!("office-rs-model-{}", std::process::id()));
    let environment = Environment::from([
        ("OFFICE_EMBEDDING_MODEL_DIR".to_string(), directory.to_string_lossy().into_owned()),
        ("OFFICE_EMBEDDING_ALLOW_MODEL_DOWNLOAD".to_string(), "0".to_string()),
    ]);
    let error = ensure_local_embedding_model(&environment).await.err().unwrap();
    assert_eq!(error, "로컬 임베딩 모델 파일이 없습니다. config.json");
    assert!(directory.is_dir(), "Node처럼 모델 폴더는 먼저 만든다");
    std::fs::remove_dir_all(&directory).unwrap();
}

const ORT_LIBRARY: &str =
    "/Users/neo/office/backend/node_modules/onnxruntime-node/bin/napi-v6/darwin/arm64/libonnxruntime.1.24.3.dylib";

fn real_model_directory() -> Option<PathBuf> {
    let directory = std::env::home_dir()?.join(".officestra/models/bge-m3-onnx-25b9af8e87a3");
    let ready = directory.join("onnx/model_quantized.onnx").is_file() && Path::new(ORT_LIBRARY).is_file();
    if !ready {
        eprintln!("건너뜀: 로컬 bge-m3 모델이나 ONNX Runtime 라이브러리가 없습니다.");
    }
    ready.then_some(directory)
}

#[tokio::test]
async fn 실제_워커_벡터는_node_워커와_같다() {
    let Some(directory) = real_model_directory() else { return };
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/node-embeddings.json")).expect("Node 임베딩 고정값");
    let texts: Vec<String> = fixture["texts"].as_array().unwrap().iter().map(|text| text.as_str().unwrap().into()).collect();
    let loader_directory = directory.clone();
    let service = LocalEmbeddingService::new(EmbeddingOptions {
        idle_timeout: Duration::from_secs(60),
        request_timeout: Duration::from_secs(120),
        worker_factory: process_worker_factory(PathBuf::from(env!("CARGO_BIN_EXE_officestra-backend"))),
        model_loader: Arc::new(move |_| {
            let directory = loader_directory.clone();
            async move { Ok(directory) }.boxed()
        }),
        environment: Environment::from([("ORT_DYLIB_PATH".to_string(), ORT_LIBRARY.to_string())]),
        logger: Arc::new(|message| panic!("예상하지 못한 경고: {message}")),
    });
    assert_eq!(fixture["modelID"], service.model_id());
    for (text, expected) in texts.iter().zip(fixture["vectors"].as_array().unwrap()) {
        let vectors = service.embed(std::slice::from_ref(text)).await.unwrap();
        let (actual, expected) = (vectors[0].as_array().unwrap(), expected.as_array().unwrap());
        assert_eq!(actual.len(), 1024);
        let dot: f64 = actual.iter().zip(expected).map(|(left, right)| left.as_f64().unwrap() * right.as_f64().unwrap()).sum();
        assert!(dot >= 0.999, "코사인 {dot}");
        assert_eq!(actual, expected, "Node 워커와 같은 CPU 실행이면 벡터가 비트 단위로 같다");
    }
    let metrics = service.metrics();
    assert_eq!(metrics["active"], true);
    assert!(metrics["lastWorkerRSSBytes"].as_f64().unwrap() > 0.0);
    assert!(metrics["peakWorkerRSSBytes"].as_f64() >= metrics["lastWorkerRSSBytes"].as_f64());
    service.close();
    assert!(!service.has_active_worker());
}
