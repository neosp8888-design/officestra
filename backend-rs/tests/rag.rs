// RAG 검색·문서 추가·임베딩 저장 테스트. Node rag-search.test.mjs·rag-embeddings.test.mjs의 가짜 풀 대신 실제 테스트 DB의 되돌리는 트랜잭션으로 확인한다. DB가 필요한 경우는 OFFICE_TEST_DATABASE_URL이 있을 때만 실행한다.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::FutureExt;
use http_body_util::BodyExt;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::{
    DIMENSIONS, EmbeddingOptions, Environment, LocalEmbeddingService, MODEL_ID, WorkerEvent, WorkerHandle, WorkerRequest,
};
use officestra_backend::rag::*;
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use tokio::sync::{OnceCell, mpsc};
use tower::ServiceExt;

static READY: OnceCell<()> = OnceCell::const_new();

async fn test_pool() -> Option<PgPool> {
    let (Ok(url), Ok(migrations)) = (std::env::var("OFFICE_TEST_DATABASE_URL"), std::env::var("OFFICE_MIGRATIONS_DIR"))
    else {
        eprintln!("OFFICE_TEST_DATABASE_URL·OFFICE_MIGRATIONS_DIR이 없어 건너뜀");
        return None;
    };
    let pool = db::connect(&url).await.unwrap();
    READY
        .get_or_init(|| async {
            migrate::run(&pool, Path::new(&migrations)).await.unwrap();
        })
        .await;
    Some(pool)
}

async fn begin() -> Option<Transaction<'static, Postgres>> {
    let pool = test_pool().await?;
    Some(pool.begin().await.unwrap())
}

fn lazy_pool() -> PgPool {
    db::connect_lazy("postgres://office:x@127.0.0.1:1/none", Duration::from_secs(1)).unwrap()
}

/// 앞 두 축만 값이 있는 1024차원 벡터.
fn vector(first: f64, second: f64) -> Value {
    let mut items = vec![json!(0); DIMENSIONS];
    items[0] = json!(first);
    items[1] = json!(second);
    Value::Array(items)
}

/// 지정한 축 하나만 1인 1024차원 벡터.
fn axis(index: usize) -> Value {
    let mut items = vec![json!(0); DIMENSIONS];
    items[index] = json!(1);
    Value::Array(items)
}

/// 받은 문장을 모아 두고 문장마다 같은 벡터를 돌려주는 가짜 워커 서비스.
struct Fake {
    service: LocalEmbeddingService,
    texts: Arc<Mutex<Vec<String>>>,
    created: Arc<AtomicUsize>,
}

fn fake_service(reply: Value) -> Fake {
    let texts = Arc::new(Mutex::new(Vec::new()));
    let created = Arc::new(AtomicUsize::new(0));
    let (seen, count) = (texts.clone(), created.clone());
    let service = LocalEmbeddingService::new(EmbeddingOptions {
        idle_timeout: Duration::from_millis(1000),
        request_timeout: Duration::from_millis(1000),
        worker_factory: Arc::new(move |_directory: &Path, _environment: &Environment| {
            count.fetch_add(1, Ordering::SeqCst);
            let (requests, mut incoming) = mpsc::unbounded_channel::<WorkerRequest>();
            let (events, receiver) = mpsc::unbounded_channel();
            let (seen, reply) = (seen.clone(), reply.clone());
            tokio::spawn(async move {
                while let Some(request) = incoming.recv().await {
                    let _ = request.delivered.send(Ok(()));
                    seen.lock().unwrap().extend(request.texts.iter().cloned());
                    let vectors: Vec<Value> = request.texts.iter().map(|_| reply.clone()).collect();
                    let _ = events.send(WorkerEvent::Message(json!({
                        "id": request.id,
                        "ok": true,
                        "result": { "vectors": vectors, "rssBytes": 1, "maxRSSBytes": 1 },
                    })));
                }
            });
            Ok(WorkerHandle { requests, events: receiver, kill: Box::new(|| {}) })
        }),
        model_loader: Arc::new(|_| async { Ok(PathBuf::from("/model")) }.boxed()),
        environment: Environment::new(),
        logger: Arc::new(|_| {}),
    });
    Fake { service, texts, created }
}

/// 모델 준비가 늘 실패하는 서비스.
fn failing_service(message: &'static str) -> LocalEmbeddingService {
    LocalEmbeddingService::new(EmbeddingOptions {
        model_loader: Arc::new(move |_| async move { Err(message.to_string()) }.boxed()),
        logger: Arc::new(|_| {}),
        ..EmbeddingOptions::default()
    })
}

async fn insert_document(
    c: &mut PgConnection,
    title: &str,
    content: &str,
    embedding: Option<(&Value, &str)>,
    work_record: Option<&str>,
) -> String {
    sqlx::query_scalar(
        "INSERT INTO rag_documents (source, title, content, embedding, embedding_model, work_record_id)
         VALUES ('rag-test', $1, $2, $3::vector, $4, $5::uuid) RETURNING id::text",
    )
    .bind(title)
    .bind(content)
    .bind(embedding.map(|(vector, _)| vector_literal(vector).unwrap()))
    .bind(embedding.map(|(_, model)| model))
    .bind(work_record)
    .fetch_one(c)
    .await
    .unwrap()
}

/// 새 프로젝트와 그 프로젝트의 업무 기록 하나를 만들고 (저장소 경로, 기록 id)를 돌려준다.
async fn project_record(c: &mut PgConnection, title: &str, body: &str) -> (String, String) {
    let (project, root): (String, String) = sqlx::query_as(
        "INSERT INTO projects (repository_root) VALUES ('/tmp/officestra-rag-test-' || gen_random_uuid())
         RETURNING id::text, repository_root",
    )
    .fetch_one(&mut *c)
    .await
    .unwrap();
    let record: String = sqlx::query_scalar(
        "INSERT INTO work_records (project_id, record_type, title, body) VALUES ($1::uuid, 'result', $2, $3) RETURNING id::text",
    )
    .bind(project)
    .bind(title)
    .bind(body)
    .fetch_one(&mut *c)
    .await
    .unwrap();
    (root, record)
}

async fn document_state(c: &mut PgConnection, id: &str) -> Value {
    sqlx::query_scalar(
        "SELECT json_build_object(
           'model', embedding_model,
           'error', embedding_error,
           'hasEmbedding', embedding IS NOT NULL,
           'stamped', embedding_updated_at IS NOT NULL)
         FROM rag_documents WHERE id = $1::uuid",
    )
    .bind(id)
    .fetch_one(c)
    .await
    .unwrap()
}

fn ids(documents: &[Value]) -> Vec<String> {
    documents.iter().map(|document| document["id"].as_str().unwrap().to_string()).collect()
}

fn id_set(documents: &[Value]) -> BTreeSet<String> {
    ids(documents).into_iter().collect()
}

fn score(document: &Value) -> f64 {
    document["score"].as_f64().unwrap()
}

/// 검색 시험 문서. 벡터 문서 넷은 같은 본문이라 전문검색 점수가 같다.
struct Seeded {
    near: String,
    middle: String,
    far: String,
    other_model: String,
    symbol_title: String,
    short: String,
}

async fn seed(c: &mut PgConnection) -> Seeded {
    let long = "벡터 검색 확인 본문 ".repeat(10);
    let model = |vector| Some((vector, MODEL_ID));
    let (e0, mid, e1) = (axis(0), vector(0.6, 0.8), axis(1));
    Seeded {
        near: insert_document(c, "near 가까운", &long, model(&e0), None).await,
        middle: insert_document(c, "middle 중간", &long, model(&mid), None).await,
        far: insert_document(c, "far 먼", &long, model(&e1), None).await,
        other_model: insert_document(c, "other 다른 모델", &long, Some((&e0, "external:unspecified")), None).await,
        symbol_title: insert_document(c, "!!", &long, model(&e0), None).await,
        short: insert_document(c, "ab", "짧은 본문", model(&e0), None).await,
    }
}

fn request<'a>(
    query: Option<&'a Value>,
    embedding: Option<&'a Value>,
    limit: Option<&'a Value>,
    service: Option<&'a LocalEmbeddingService>,
) -> SearchRequest<'a> {
    SearchRequest { query, embedding, limit, service, minimum_score: 0.45 }
}

#[tokio::test]
async fn 로컬_벡터_검색은_같은_모델에서_최소_점수를_넘는_문서만_가까운_순으로_돌려준다() {
    let Some(mut transaction) = begin().await else { return };
    let c = &mut *transaction;
    let seeded = seed(c).await;
    let fake = fake_service(axis(0));
    let query = json!("  벡터 검색  ");

    let search = search_rag_documents(&mut RagDatabase::Connection(c), request(Some(&query), None, None, Some(&fake.service)))
        .await
        .unwrap();
    assert_eq!(search.mode, "local_vector");
    assert!(search.fallback_error.is_none());
    assert_eq!(ids(&search.documents), [seeded.near.clone(), seeded.middle.clone()]);
    assert_eq!(*fake.texts.lock().unwrap(), ["벡터 검색"]);
    let keys: Vec<&str> = search.documents[0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["id", "ragDocumentId", "source", "title", "content", "metadata", "workRecordId", "score"]);
    assert_eq!(search.documents[0]["ragDocumentId"], search.documents[0]["id"]);
    assert_eq!(search.documents[0]["workRecordId"], Value::Null);
    assert_eq!(search.documents[0]["metadata"], json!({}));
    assert!((score(&search.documents[0]) - 1.0).abs() < 1e-6);
    assert!((score(&search.documents[1]) - 0.6).abs() < 1e-6);

    let strict = SearchRequest { minimum_score: 0.7, ..request(Some(&query), None, None, Some(&fake.service)) };
    let search = search_rag_documents(&mut RagDatabase::Connection(c), strict).await.unwrap();
    assert_eq!(ids(&search.documents), [seeded.near.clone()]);

    // 같은 질의를 전문검색만으로 찾으면 제목에 영숫자가 있는 네 문서가 나온다. 같은 낱말만 되풀이해 점수가 0.2에 못 미치므로 위에서 벡터 결과가 이겼다.
    let lexical = search_rag_documents(&mut RagDatabase::Connection(c), request(Some(&query), None, None, None)).await.unwrap();
    assert_eq!(lexical.mode, "lexical");
    assert_eq!(
        id_set(&lexical.documents),
        BTreeSet::from([seeded.near, seeded.middle, seeded.far, seeded.other_model])
    );
    assert!(lexical.documents.iter().all(|document| score(document) > 0.0 && score(document) < STRONG_LEXICAL_SCORE));
    assert!(!id_set(&lexical.documents).contains(&seeded.symbol_title));
    assert!(!id_set(&lexical.documents).contains(&seeded.short));
    fake.service.close();
}

#[tokio::test]
async fn 접두어가_여러_낱말에_걸려_전문검색_점수가_강하면_벡터보다_앞선다() {
    let Some(mut transaction) = begin().await else { return };
    let c = &mut *transaction;
    let vector_document = insert_document(c, "tunnel 벡터", &"관련 없는 본문 ".repeat(10), Some((&axis(0), MODEL_ID)), None).await;
    let weak = insert_document(c, "tunnel 약함", "터널 점검", None, None).await;
    let fake = fake_service(axis(0));
    let query = json!("터널");

    // 낱말 하나만 걸리면 ts_rank가 0.1 아래라 벡터 결과를 쓴다.
    let search = search_rag_documents(&mut RagDatabase::Connection(c), request(Some(&query), None, None, Some(&fake.service)))
        .await
        .unwrap();
    assert_eq!((search.mode, ids(&search.documents)), ("local_vector", vec![vector_document.clone()]));

    // "터널:*"이 조사 붙은 낱말 여섯에 걸리면 점수가 더해져 0.2를 넘는다.
    let strong = insert_document(c, "tunnel 강함", "터널 터널이 터널을 터널에서 터널로 터널은", None, None).await;
    let search = search_rag_documents(&mut RagDatabase::Connection(c), request(Some(&query), None, None, Some(&fake.service)))
        .await
        .unwrap();
    assert_eq!(search.mode, "strong_lexical");
    assert_eq!(ids(&search.documents), [strong, weak]);
    assert!(score(&search.documents[0]) >= STRONG_LEXICAL_SCORE);
    assert!(score(&search.documents[1]) < STRONG_LEXICAL_SCORE);
    fake.service.close();
}

#[tokio::test]
async fn 모델을_준비하지_못하면_전문검색으로_대체하고_사유를_남긴다() {
    let Some(mut transaction) = begin().await else { return };
    let c = &mut *transaction;
    let seeded = seed(c).await;
    let service = failing_service("모델 없음");
    let query = json!("벡터 검색");
    let limit = json!(2);

    let search = search_rag_documents(&mut RagDatabase::Connection(c), request(Some(&query), None, Some(&limit), Some(&service)))
        .await
        .unwrap();
    assert_eq!(search.mode, "lexical_fallback");
    assert_eq!(search.fallback_error.as_deref(), Some("모델 없음"));
    assert_eq!(search.documents.len(), 2);
    let candidates = BTreeSet::from([seeded.near, seeded.middle, seeded.far, seeded.other_model]);
    assert!(id_set(&search.documents).is_subset(&candidates));
    service.close();
}

#[tokio::test]
async fn 벡터를_직접_주면_서비스를_부르지_않고_모델과_제목을_가리지_않는다() {
    let Some(mut transaction) = begin().await else { return };
    let c = &mut *transaction;
    let seeded = seed(c).await;
    let fake = fake_service(axis(0));
    let query = json!("벡터 검색");

    let (embedding, limit) = (axis(1), json!(2));
    let search = search_rag_documents(
        &mut RagDatabase::Connection(c),
        request(Some(&query), Some(&embedding), Some(&limit), Some(&fake.service)),
    )
    .await
    .unwrap();
    assert_eq!(search.mode, "explicit_vector");
    assert_eq!(ids(&search.documents), [seeded.far.clone(), seeded.middle.clone()]);
    assert!((score(&search.documents[1]) - 0.8).abs() < 1e-6);

    let (embedding, limit) = (axis(0), json!(5));
    let search = search_rag_documents(&mut RagDatabase::Connection(c), request(None, Some(&embedding), Some(&limit), None))
        .await
        .unwrap();
    let found = ids(&search.documents);
    assert_eq!(
        found[..4].iter().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([seeded.near, seeded.other_model, seeded.symbol_title, seeded.short])
    );
    assert_eq!(found[4], seeded.middle);
    assert_eq!(fake.created.load(Ordering::SeqCst), 0);
    assert!(fake.texts.lock().unwrap().is_empty());
    fake.service.close();
}

#[tokio::test]
async fn 빈_질의는_db를_보지_않고_빈_결과다() {
    let pool = lazy_pool();
    let fake = fake_service(axis(0));
    for query in [None, Some(json!(null)), Some(json!("   ")), Some(json!(""))] {
        let search =
            search_rag_documents(&mut RagDatabase::Pool(&pool), request(query.as_ref(), None, None, Some(&fake.service)))
                .await
                .unwrap();
        assert_eq!((search.mode, search.documents.len()), ("empty", 0));
    }
    // 배열이 아닌 벡터는 무시하고 질의를 본다.
    let not_array = json!("[1,0]");
    let search = search_rag_documents(&mut RagDatabase::Pool(&pool), request(None, Some(&not_array), None, None)).await.unwrap();
    assert_eq!(search.mode, "empty");
    assert_eq!(fake.created.load(Ordering::SeqCst), 0);
    fake.service.close();
}

#[tokio::test]
async fn 문서_추가는_내용을_요구하고_외부_벡터의_모델_이름을_채운다() {
    let Some(mut transaction) = begin().await else { return };
    let c = &mut *transaction;

    for body in [json!({}), json!({ "content": "   " }), json!({ "content": null })] {
        let blank = add_rag_document(&mut RagDatabase::Connection(c), &body).await.unwrap_err();
        assert_eq!((blank.status, blank.message.as_str()), (400, "RAG 문서 내용이 필요합니다."));
    }

    let created = add_rag_document(
        &mut RagDatabase::Connection(c),
        &json!({
            "source": "manual",
            "title": "외부 벡터",
            "content": "  외부 벡터 본문  ",
            "metadata": { "b": 1, "a": 2 },
            "embedding": axis(2),
        }),
    )
    .await
    .unwrap();
    let keys: Vec<&String> = created.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["id"]);
    let id = created["id"].as_str().unwrap().to_string();
    let row: Value = sqlx::query_scalar(
        "SELECT json_build_object('source', source, 'title', title, 'content', content, 'metadata', metadata,
                                  'matches', embedding = $2::vector)
         FROM rag_documents WHERE id = $1::uuid",
    )
    .bind(&id)
    .bind(vector_literal(&axis(2)).unwrap())
    .fetch_one(&mut *c)
    .await
    .unwrap();
    assert_eq!(
        row,
        json!({ "source": "manual", "title": "외부 벡터", "content": "외부 벡터 본문", "metadata": { "a": 2, "b": 1 }, "matches": true })
    );
    assert_eq!(
        document_state(c, &id).await,
        json!({ "model": "external:unspecified", "error": null, "hasEmbedding": true, "stamped": true })
    );

    let named = |model: Value| json!({ "content": "이름 있는 벡터", "embedding": axis(3), "embeddingModel": model });
    for (model, expected) in [(json!("  "), "external:unspecified"), (json!(" custom:model "), "custom:model"), (json!(null), "external:unspecified")] {
        let created = add_rag_document(&mut RagDatabase::Connection(c), &named(model)).await.unwrap();
        let state = document_state(c, created["id"].as_str().unwrap()).await;
        assert_eq!(state["model"], json!(expected));
    }

    // 배열이 아닌 벡터는 없는 것으로 보고 모델 이름과 시각을 비운다.
    let plain = add_rag_document(&mut RagDatabase::Connection(c), &json!({ "content": "벡터 없음", "embedding": "[1,2]", "embeddingModel": "x" }))
        .await
        .unwrap();
    assert_eq!(
        document_state(c, plain["id"].as_str().unwrap()).await,
        json!({ "model": null, "error": null, "hasEmbedding": false, "stamped": false })
    );

    // 차원이 틀린 벡터는 PostgreSQL 오류 문장 그대로 500이다. 트랜잭션이 깨지므로 마지막에 둔다.
    let wrong = add_rag_document(&mut RagDatabase::Connection(c), &json!({ "content": "틀린 차원", "embedding": [1, 2, 3] }))
        .await
        .unwrap_err();
    assert_eq!((wrong.status, wrong.message.as_str()), (500, "expected 1024 dimensions, not 3"));
}

#[tokio::test]
async fn 대기_문서는_업무_기록_id로_골라_임베딩하고_모델_id로_저장한다() {
    let Some(mut transaction) = begin().await else { return };
    let c = &mut *transaction;
    let (_root, record) = project_record(c, "메모리 부족", "최종 결과 본문").await;
    let content = format!("{}최종 결과", "앞부분 기록 ".repeat(200));
    let linked = insert_document(c, "메모리 부족", &content, None, Some(&record)).await;
    let other = insert_document(c, "other 문서", "다른 본문", None, None).await;
    let fake = fake_service(axis(3));

    let records = [record.clone()];
    let filter = PendingFilter { work_record_ids: Some(&records), ..PendingFilter::default() };
    let batch = embed_rag_documents_best_effort(&mut RagDatabase::Connection(c), &fake.service, filter).await.unwrap();
    assert_eq!((batch.embedded, batch.failed, batch.error.as_deref()), (1, 0, None));
    assert_eq!(ids(&batch.documents), [linked.clone()]);
    let summary = batch.to_json();
    let keys: Vec<&String> = summary.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["embedded", "failed", "documents"]);
    let pending_keys: Vec<&String> = batch.documents[0].as_object().unwrap().keys().collect();
    assert_eq!(pending_keys, ["id", "title", "content"]);

    let texts = fake.texts.lock().unwrap().clone();
    assert_eq!(texts, [rag_embedding_text(&batch.documents[0])]);
    assert!(texts[0].starts_with("메모리 부족\n메모리 부족\n"));
    assert!(texts[0].contains("최종 결과"));
    assert!(texts[0].encode_utf16().count() < 1700);

    let stored: bool = sqlx::query_scalar("SELECT embedding = $2::vector FROM rag_documents WHERE id = $1::uuid")
        .bind(&linked)
        .bind(vector_literal(&axis(3)).unwrap())
        .fetch_one(&mut *c)
        .await
        .unwrap();
    assert!(stored);
    assert_eq!(document_state(c, &linked).await, json!({ "model": MODEL_ID, "error": null, "hasEmbedding": true, "stamped": true }));
    assert_eq!(document_state(c, &other).await["hasEmbedding"], json!(false));

    // 이미 같은 모델로 저장한 문서는 다시 대기 목록에 오르지 않는다.
    let again = embed_rag_documents_best_effort(&mut RagDatabase::Connection(c), &fake.service, filter).await.unwrap();
    assert_eq!((again.embedded, again.failed, again.documents.len()), (0, 0, 0));
    assert_eq!(again.to_json(), json!({ "embedded": 0, "failed": 0, "documents": [] }));
    fake.service.close();
}

#[tokio::test]
async fn 대기_목록은_저장소_문서_id_개수_조건을_node와_같이_적용한다() {
    let Some(mut transaction) = begin().await else { return };
    let c = &mut *transaction;
    let (root, record) = project_record(c, "기록", "본문").await;
    let linked = insert_document(c, "linked", "기록 문서", None, Some(&record)).await;
    let first = insert_document(c, "first", "대기 문서", None, None).await;
    let second = insert_document(c, "second", "대기 문서", Some((&axis(0), "external:unspecified")), None).await;
    let done = insert_document(c, "done", "끝난 문서", Some((&axis(0), MODEL_ID)), None).await;

    let all = [linked.clone(), first.clone(), second.clone(), done.clone()];
    let mut expected = vec![linked.clone(), first.clone(), second.clone()];
    expected.sort();
    let cases: Vec<(PendingFilter, Vec<String>)> = vec![
        // Node는 limit null을 Number(null)=0으로 보고 1건으로 자른다.
        (PendingFilter { document_ids: Some(&all), ..PendingFilter::default() }, expected[..1].to_vec()),
        (PendingFilter { document_ids: Some(&all), limit: Some(f64::INFINITY), ..PendingFilter::default() }, expected.clone()),
        (PendingFilter { document_ids: Some(&all), limit: Some(f64::NAN), ..PendingFilter::default() }, expected.clone()),
        (PendingFilter { document_ids: Some(&all), limit: Some(2.0), ..PendingFilter::default() }, expected[..2].to_vec()),
        (PendingFilter { document_ids: Some(&all), limit: Some(-3.0), ..PendingFilter::default() }, expected[..1].to_vec()),
        (PendingFilter { repository_root: Some(&root), limit: Some(f64::INFINITY), ..PendingFilter::default() }, vec![linked.clone()]),
        (
            PendingFilter { document_ids: Some(&[]), work_record_ids: Some(&[]), repository_root: Some(&root), limit: Some(9.0) },
            vec![linked.clone()],
        ),
    ];
    for (filter, wanted) in cases {
        let found = ids(&list_pending_rag_embedding_documents(&mut RagDatabase::Connection(c), filter).await.unwrap());
        assert_eq!(found, wanted);
    }
}

#[tokio::test]
async fn 임베딩이_실패하면_사유를_문서에_남기고_실패로_센다() {
    let Some(mut transaction) = begin().await else { return };
    let c = &mut *transaction;
    let document = insert_document(c, "실패 문서", "실패 본문", None, None).await;
    let documents = [document.clone()];
    let filter = PendingFilter { document_ids: Some(&documents), ..PendingFilter::default() };

    let service = failing_service("모델 로드 실패");
    let batch = embed_rag_documents_best_effort(&mut RagDatabase::Connection(c), &service, filter).await.unwrap();
    assert_eq!((batch.embedded, batch.failed, batch.error.as_deref()), (0, 1, Some("모델 로드 실패")));
    let summary = batch.to_json();
    let keys: Vec<&String> = summary.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["embedded", "failed", "documents", "error"]);
    assert_eq!(
        document_state(c, &document).await,
        json!({ "model": null, "error": "모델 로드 실패", "hasEmbedding": false, "stamped": true })
    );
    service.close();

    // 차원이 틀린 벡터는 저장 전에 거른다.
    let short = fake_service(json!([1, 0]));
    let batch = embed_rag_documents_best_effort(&mut RagDatabase::Connection(c), &short.service, filter).await.unwrap();
    assert_eq!(batch.error.as_deref(), Some("로컬 임베딩은 1024차원 유한 숫자여야 합니다."));
    assert_eq!(document_state(c, &document).await["error"], json!("로컬 임베딩은 1024차원 유한 숫자여야 합니다."));
    short.service.close();

    // 성공하면 실패 사유를 지운다.
    let fake = fake_service(axis(4));
    let batch = embed_rag_documents_best_effort(&mut RagDatabase::Connection(c), &fake.service, filter).await.unwrap();
    assert_eq!((batch.embedded, batch.failed), (1, 0));
    assert_eq!(document_state(c, &document).await, json!({ "model": MODEL_ID, "error": null, "hasEmbedding": true, "stamped": true }));
    fake.service.close();
}

#[tokio::test]
async fn 백필은_저장소_문서를_한_묶음씩_끝까지_임베딩하고_실패하면_멈춘다() {
    let Some(mut transaction) = begin().await else { return };
    let c = &mut *transaction;
    let (root, record) = project_record(c, "백필", "본문").await;
    // 업무 기록 하나에는 RAG 문서가 하나뿐이라 같은 프로젝트에 기록을 하나 더 만든다.
    let second: String = sqlx::query_scalar(
        "INSERT INTO work_records (project_id, record_type, title, body)
         SELECT project_id, 'result', '백필 둘', '본문' FROM work_records WHERE id = $1::uuid RETURNING id::text",
    )
    .bind(&record)
    .fetch_one(&mut *c)
    .await
    .unwrap();
    insert_document(c, "backfill 하나", "백필 본문 하나", None, Some(&record)).await;
    insert_document(c, "backfill 둘", "백필 본문 둘", None, Some(&second)).await;

    let failing = failing_service("모델 로드 실패");
    let mut calls = Vec::new();
    let failed = backfill_rag_embeddings(&mut RagDatabase::Connection(c), &failing, Some(&root), 1, |embedded, failed, batch| {
        calls.push((embedded, failed, batch.documents.len()));
    })
    .await;
    assert_eq!(failed.unwrap_err(), "모델 로드 실패");
    assert_eq!(calls, [(0, 1, 1)]);
    failing.close();

    let fake = fake_service(axis(5));
    let mut calls = Vec::new();
    let summary = backfill_rag_embeddings(&mut RagDatabase::Connection(c), &fake.service, Some(&root), 1, |embedded, failed, batch| {
        calls.push((embedded, failed, batch.documents.len()));
    })
    .await
    .unwrap();
    assert_eq!(calls, [(1, 0, 1), (2, 0, 1), (2, 0, 0)]);
    assert_eq!((summary["embedded"].clone(), summary["failed"].clone()), (json!(2), json!(0)));
    let keys: Vec<&String> = summary.as_object().unwrap().keys().take(3).collect();
    assert_eq!(keys, ["embedded", "failed", "elapsedSeconds"]);
    assert!(summary["elapsedSeconds"].as_f64().unwrap() >= 0.0);
    fake.service.close();
}

fn state(pool: PgPool) -> AppState {
    AppState {
        pool,
        runtime: Arc::new(Runtime::new(None, None)),
        release_id: None,
        cost_summary: Arc::default(),
        realtime: Arc::default(),
        embedding: LocalEmbeddingService::default(),
        model_catalog: None,
        cli_updates: Arc::default(),
        usage_summary: None,
        roleplay: None,
        voice_follow: Arc::default(),
    }
}

async fn call(state: &AppState, path: &str, body: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn rag_http는_null_본문에서_node와_같은_오류를_낸다() {
    let state = state(lazy_pool());
    assert_eq!(
        call(&state, "/api/rag/search", "null").await,
        (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": "Cannot read properties of null (reading 'query')" }))
    );
    assert_eq!(
        call(&state, "/api/rag/documents", "null").await,
        (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": "Cannot read properties of null (reading 'content')" }))
    );
    assert_eq!(
        call(&state, "/api/rag/documents", r#"{"content":" "}"#).await,
        (StatusCode::BAD_REQUEST, json!({ "error": "RAG 문서 내용이 필요합니다." }))
    );
    assert_eq!(call(&state, "/api/rag/search", r#"{"query":"  "}"#).await, (StatusCode::OK, json!({ "documents": [] })));
}

#[tokio::test]
async fn rag_http로_문서를_추가하고_벡터로_다시_찾는다() {
    let Some(pool) = test_pool().await else { return };
    let state = state(pool.clone());
    // 다른 테스트의 축(0~5)과 겹치지 않는 축을 쓴다.
    let embedding = axis(7);
    let body = json!({ "source": "rag-http-test", "title": "http 경로", "content": "http 경로 확인 문서", "embedding": embedding });
    let (status, created) = call(&state, "/api/rag/documents", &body.to_string()).await;
    let id = created["id"].as_str().map(str::to_string);
    let found = call(&state, "/api/rag/search", &json!({ "embedding": embedding, "limit": 1 }).to_string()).await;
    if let Some(id) = &id {
        sqlx::query("DELETE FROM rag_documents WHERE id = $1::uuid").bind(id).execute(&pool).await.unwrap();
    }
    assert_eq!(status, StatusCode::CREATED);
    let id = id.unwrap();
    assert_eq!(found.0, StatusCode::OK);
    let documents = found.1["documents"].as_array().unwrap();
    assert_eq!(ids(documents), [id]);
    assert_eq!(found.1.as_object().unwrap().len(), 1);
    assert!((score(&documents[0]) - 1.0).abs() < 1e-6);
}
