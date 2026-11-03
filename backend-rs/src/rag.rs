// RAG 문서 추가·검색과 로컬 임베딩 저장·백필. Node rag-search.mjs, rag-embeddings.mjs, server.mjs의 addRAGDocument를 옮겼다.

use std::time::Instant;

use serde_json::{Value, json};
use sqlx::postgres::PgArguments;
use sqlx::{PgConnection, PgPool, Postgres};

use crate::js::{
    js_join, js_max, js_min, js_property, js_string_or_empty, js_to_number, js_trim, number_to_string, pg_text,
    string_to_number, to_js_json,
};
use crate::local_embedding::{DIMENSIONS, LocalEmbeddingService, MODEL_ID};
use crate::work_record_memory::work_record_search_ts_query;

pub const DEFAULT_VECTOR_MIN_SCORE: f64 = 0.45;
pub const STRONG_LEXICAL_SCORE: f64 = 0.2;
pub const DEFAULT_EMBEDDING_BATCH_SIZE: usize = 1;

type Rows<'q> = sqlx::query::QueryScalar<'q, Postgres, Value, PgArguments>;
type Statement<'q> = sqlx::query::Query<'q, Postgres, PgArguments>;

/// Node는 RAG 쿼리마다 pool.query를 쓴다. 운영은 풀을 그대로 쓰고, 테스트는 되돌릴 트랜잭션 연결을 넘긴다.
pub enum RagDatabase<'a> {
    Pool(&'a PgPool),
    Connection(&'a mut PgConnection),
}

impl RagDatabase<'_> {
    async fn rows(&mut self, query: Rows<'_>) -> Result<Vec<Value>, sqlx::Error> {
        match self {
            Self::Pool(pool) => query.fetch_all(*pool).await,
            Self::Connection(connection) => query.fetch_all(&mut **connection).await,
        }
    }

    async fn execute(&mut self, statement: Statement<'_>) -> Result<(), sqlx::Error> {
        match self {
            Self::Pool(pool) => statement.execute(*pool).await.map(drop),
            Self::Connection(connection) => statement.execute(&mut **connection).await.map(drop),
        }
    }
}

/// Node `error.message`. PostgreSQL 오류는 서버 문구만 쓴다.
pub fn database_message(error: &sqlx::Error) -> String {
    match error.as_database_error() {
        Some(database_error) => database_error.message().to_string(),
        None => error.to_string(),
    }
}

/// HTTP 처리기로 돌려줄 실패. 상태 코드와 Node의 오류 문구를 담는다.
#[derive(Debug)]
pub struct RagFailure {
    pub status: u16,
    pub message: String,
}

impl From<sqlx::Error> for RagFailure {
    fn from(error: sqlx::Error) -> Self {
        Self { status: 500, message: database_message(&error) }
    }
}

/// ragVectorMinimumScore. 값이 유한수면 [-1, 1]로 자르고, 없거나 숫자가 아니면 0.45다.
pub fn rag_vector_minimum_score(configured: Option<&str>) -> f64 {
    let configured = configured.map_or(f64::NAN, string_to_number);
    if configured.is_finite() { js_max(-1.0, js_min(configured, 1.0)) } else { DEFAULT_VECTOR_MIN_SCORE }
}

/// filterRAGDocumentsByScore. 점수가 없거나 숫자가 아니면 NaN이라 빠진다.
pub fn filter_by_score(documents: Vec<Value>, minimum_score: f64) -> Vec<Value> {
    documents
        .into_iter()
        .filter(|document| document.get("score").map_or(f64::NAN, js_to_number) >= minimum_score)
        .collect()
}

/// `[${embedding.join(",")}]`.
fn joined_vector(items: &[Value]) -> String {
    format!("[{}]", js_join(items, ","))
}

const EXPLICIT_VECTOR_SQL: &str = r#"
      SELECT row_to_json(found) FROM (
        SELECT
          id,
          id AS "ragDocumentId",
          source,
          title,
          content,
          metadata,
          work_record_id AS "workRecordId",
          1 - (embedding <=> $1::vector) AS score
        FROM searchable_rag_documents
        WHERE embedding IS NOT NULL
        ORDER BY embedding <=> $1::vector
        LIMIT $2::bigint
      ) AS found
"#;

const LOCAL_VECTOR_SQL: &str = r#"
      SELECT row_to_json(found) FROM (
        SELECT
          id,
          id AS "ragDocumentId",
          source,
          title,
          content,
          metadata,
          work_record_id AS "workRecordId",
          1 - (embedding <=> $1::vector) AS score
        FROM searchable_rag_documents
        WHERE embedding IS NOT NULL
          AND embedding_model = $3
          AND COALESCE(title, '') ~ '[[:alnum:]]'
          AND (
            char_length(COALESCE(title, '')) >= 4
            OR char_length(content) >= 80
          )
        ORDER BY embedding <=> $1::vector
        LIMIT $2::bigint
      ) AS found
"#;

const LEXICAL_SQL: &str = r#"
      SELECT row_to_json(found) FROM (
        SELECT
          id,
          id AS "ragDocumentId",
          source,
          title,
          content,
          metadata,
          work_record_id AS "workRecordId",
          ts_rank(
            search_document,
            to_tsquery('simple', $1)
          ) AS score
        FROM searchable_rag_documents
        WHERE search_document @@ to_tsquery('simple', $1)
          AND COALESCE(title, '') ~ '[[:alnum:]]'
        ORDER BY score DESC
        LIMIT $2::bigint
      ) AS found
"#;

async fn explicit_vector_search(db: &mut RagDatabase<'_>, literal: String, limit: f64) -> Result<Vec<Value>, sqlx::Error> {
    db.rows(sqlx::query_scalar(EXPLICIT_VECTOR_SQL).bind(literal).bind(number_to_string(limit))).await
}

async fn local_vector_search(
    db: &mut RagDatabase<'_>,
    literal: String,
    limit: f64,
    minimum_score: f64,
) -> Result<Vec<Value>, sqlx::Error> {
    let rows = db
        .rows(sqlx::query_scalar(LOCAL_VECTOR_SQL).bind(literal).bind(number_to_string(limit)).bind(MODEL_ID))
        .await?;
    Ok(filter_by_score(rows, minimum_score))
}

async fn lexical_search(db: &mut RagDatabase<'_>, query: &str, limit: f64) -> Result<Vec<Value>, sqlx::Error> {
    let ts_query = work_record_search_ts_query(query);
    if ts_query.is_empty() {
        return Ok(Vec::new());
    }
    db.rows(sqlx::query_scalar(LEXICAL_SQL).bind(ts_query).bind(number_to_string(limit))).await
}

pub struct RagSearch {
    pub documents: Vec<Value>,
    pub mode: &'static str,
    pub fallback_error: Option<String>,
}

/// searchRAGDocuments의 인자. None은 JS undefined다.
pub struct SearchRequest<'a> {
    pub query: Option<&'a Value>,
    pub embedding: Option<&'a Value>,
    pub limit: Option<&'a Value>,
    pub service: Option<&'a LocalEmbeddingService>,
    pub minimum_score: f64,
}

/// `Math.max(1, Math.min(Number(limit) || 5, 20))`.
fn bounded_search_limit(limit: Option<&Value>) -> f64 {
    let number = limit.map_or(5.0, js_to_number);
    let number = if number.is_nan() || number == 0.0 { 5.0 } else { number };
    js_max(1.0, js_min(number, 20.0))
}

/// searchRAGDocuments. 벡터를 직접 주면 그 벡터로, 아니면 로컬 임베딩과 전문검색을 함께 보고, 임베딩이 실패하면 전문검색으로 대체한다.
pub async fn search_rag_documents(db: &mut RagDatabase<'_>, request: SearchRequest<'_>) -> Result<RagSearch, sqlx::Error> {
    let limit = bounded_search_limit(request.limit);
    if let Some(Value::Array(embedding)) = request.embedding {
        return Ok(RagSearch {
            documents: explicit_vector_search(db, joined_vector(embedding), limit).await?,
            mode: "explicit_vector",
            fallback_error: None,
        });
    }
    let query = js_string_or_empty(request.query);
    let query = js_trim(&query);
    if query.is_empty() {
        return Ok(RagSearch { documents: Vec::new(), mode: "empty", fallback_error: None });
    }
    let Some(service) = request.service else {
        return Ok(RagSearch { documents: lexical_search(db, query, limit).await?, mode: "lexical", fallback_error: None });
    };
    let attempt: Result<RagSearch, String> = async {
        let vectors = service.embed(&[query.to_string()]).await?;
        let literal = match vectors.first() {
            Some(Value::Array(items)) => joined_vector(items),
            None => return Err("Cannot read properties of undefined (reading 'join')".to_string()),
            Some(Value::Null) => return Err("Cannot read properties of null (reading 'join')".to_string()),
            Some(_) => return Err("embedding.join is not a function".to_string()),
        };
        let vector_documents =
            local_vector_search(db, literal, limit, request.minimum_score).await.map_err(|error| database_message(&error))?;
        let lexical_documents = lexical_search(db, query, limit).await.map_err(|error| database_message(&error))?;
        let top_lexical = lexical_documents.first().and_then(|document| document.get("score")).map_or(f64::NAN, js_to_number);
        if top_lexical >= STRONG_LEXICAL_SCORE {
            return Ok(RagSearch { documents: lexical_documents, mode: "strong_lexical", fallback_error: None });
        }
        Ok(RagSearch { documents: vector_documents, mode: "local_vector", fallback_error: None })
    }
    .await;
    match attempt {
        Ok(search) => Ok(search),
        Err(error) => Ok(RagSearch {
            documents: lexical_search(db, query, limit).await?,
            mode: "lexical_fallback",
            fallback_error: Some(error),
        }),
    }
}

const INSERT_DOCUMENT_SQL: &str = r#"
      INSERT INTO rag_documents (
        source,
        title,
        content,
        metadata,
        embedding,
        embedding_model,
        embedding_updated_at
      )
      VALUES ($1, $2, $3, $4::jsonb, $5::vector, $6, CASE
        WHEN $5::vector IS NULL THEN NULL
        ELSE now()
      END)
      RETURNING json_build_object('id', id)
"#;

/// addRAGDocument. 본문 속성을 Node와 같은 순서로 읽고 새 문서 id를 돌려준다.
pub async fn add_rag_document(db: &mut RagDatabase<'_>, body: &Value) -> Result<Value, RagFailure> {
    let failure = |message: String| RagFailure { status: 500, message };
    let content = js_string_or_empty(js_property(body, "content").map_err(failure)?);
    let content = js_trim(&content).to_string();
    if content.is_empty() {
        return Err(RagFailure { status: 400, message: "RAG 문서 내용이 필요합니다.".into() });
    }
    let embedding = match js_property(body, "embedding").map_err(failure)? {
        Some(Value::Array(items)) => Some(joined_vector(items)),
        _ => None,
    };
    let embedding_model = match &embedding {
        Some(_) => {
            let model = match js_property(body, "embeddingModel").map_err(failure)? {
                None | Some(Value::Null) => "external:unspecified".to_string(),
                Some(model) => js_string_or_empty(Some(model)),
            };
            let model = js_trim(&model);
            Some(if model.is_empty() { "external:unspecified".to_string() } else { model.to_string() })
        }
        None => None,
    };
    let text_of = |value: Option<&Value>| value.and_then(pg_text);
    let source = text_of(js_property(body, "source").map_err(failure)?);
    let title = text_of(js_property(body, "title").map_err(failure)?);
    let metadata = match js_property(body, "metadata").map_err(failure)? {
        None | Some(Value::Null) => "{}".to_string(),
        Some(metadata) => String::from_utf8(to_js_json(metadata).unwrap_or_default()).unwrap_or_default(),
    };
    let mut rows = db
        .rows(
            sqlx::query_scalar(INSERT_DOCUMENT_SQL)
                .bind(source)
                .bind(title)
                .bind(content)
                .bind(metadata)
                .bind(embedding)
                .bind(embedding_model),
        )
        .await?;
    Ok(rows.pop().unwrap_or(Value::Null))
}

/// ragEmbeddingText. 제목을 두 번, 본문 끝 900자와 앞 600자를 이어 붙인다. 길이는 JS처럼 UTF-16 단위로 센다.
pub fn rag_embedding_text(document: &Value) -> String {
    let title = js_string_or_empty(document.get("title"));
    let title = js_trim(&title);
    let content = js_string_or_empty(document.get("content"));
    let units: Vec<u16> = js_trim(&content).encode_utf16().collect();
    let beginning = String::from_utf16_lossy(&units[..units.len().min(600)]);
    let ending =
        if units.len() > 600 { String::from_utf16_lossy(&units[units.len().saturating_sub(900)..]) } else { String::new() };
    [title, title, ending.as_str(), beginning.as_str()].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n")
}

/// vectorLiteral. 1024개의 유한 숫자 배열만 pgvector 문자열로 바꾼다.
pub fn vector_literal(vector: &Value) -> Result<String, String> {
    match vector {
        Value::Array(items) if items.len() == DIMENSIONS && items.iter().all(|item| item.as_f64().is_some_and(f64::is_finite)) => {
            Ok(joined_vector(items))
        }
        _ => Err(format!("로컬 임베딩은 {DIMENSIONS}차원 유한 숫자여야 합니다.")),
    }
}

/// listPendingRAGEmbeddingDocuments의 조건. None은 Node의 null이다.
#[derive(Default, Clone, Copy)]
pub struct PendingFilter<'a> {
    pub repository_root: Option<&'a str>,
    pub document_ids: Option<&'a [String]>,
    pub work_record_ids: Option<&'a [String]>,
    /// JS `limit`. Node는 null도 Number(null)=0이라 1건으로 자르고, NaN·무한대만 제한하지 않는다.
    pub limit: Option<f64>,
}

const LIST_PENDING_SQL: &str = r#"
      SELECT row_to_json(pending) FROM (
        SELECT
          document.id::text AS id,
          document.title,
          document.content
        FROM rag_documents AS document
        LEFT JOIN work_records AS record ON record.id = document.work_record_id
        LEFT JOIN projects AS project ON project.id = record.project_id
        WHERE (document.embedding IS NULL
            OR document.embedding_model IS DISTINCT FROM $1)
          AND ($2::text IS NULL OR project.repository_root = $2)
          AND ($3::uuid[] IS NULL OR document.id = ANY($3::uuid[]))
          AND ($5::uuid[] IS NULL OR document.work_record_id = ANY($5::uuid[]))
        ORDER BY document.updated_at, document.id
        LIMIT COALESCE($4::integer, 2147483647)
      ) AS pending
"#;

pub async fn list_pending_rag_embedding_documents(
    db: &mut RagDatabase<'_>,
    filter: PendingFilter<'_>,
) -> Result<Vec<Value>, sqlx::Error> {
    let non_empty = |ids: Option<&[String]>| ids.filter(|ids| !ids.is_empty()).map(<[String]>::to_vec);
    let limit = filter.limit.unwrap_or(0.0);
    let bounded = limit.is_finite().then(|| number_to_string(js_max(1.0, limit)));
    db.rows(
        sqlx::query_scalar(LIST_PENDING_SQL)
            .bind(MODEL_ID)
            .bind(filter.repository_root)
            .bind(non_empty(filter.document_ids))
            .bind(bounded)
            .bind(non_empty(filter.work_record_ids)),
    )
    .await
}

const STORE_EMBEDDING_SQL: &str = r#"
        UPDATE rag_documents
        SET embedding = $2::vector,
            embedding_model = $3,
            embedding_updated_at = now(),
            embedding_error = NULL
        WHERE id = $1::uuid
"#;

pub async fn store_rag_document_embeddings(
    db: &mut RagDatabase<'_>,
    documents: &[Value],
    vectors: &[Value],
) -> Result<(), String> {
    if documents.len() != vectors.len() {
        return Err("RAG 문서 수와 임베딩 수가 일치하지 않습니다.".into());
    }
    for (document, vector) in documents.iter().zip(vectors) {
        let id = document.get("id").and_then(pg_text);
        let literal = vector_literal(vector)?;
        db.execute(sqlx::query(STORE_EMBEDDING_SQL).bind(id).bind(literal).bind(MODEL_ID))
            .await
            .map_err(|error| database_message(&error))?;
    }
    Ok(())
}

const MARK_FAILURE_SQL: &str = r#"
      UPDATE rag_documents
      SET embedding_error = $2,
          embedding_updated_at = now()
      WHERE id = ANY($1::uuid[])
"#;

pub async fn mark_rag_embedding_failure(db: &mut RagDatabase<'_>, documents: &[Value], error: &str) -> Result<(), sqlx::Error> {
    if documents.is_empty() {
        return Ok(());
    }
    let ids: Vec<Option<String>> = documents.iter().map(|document| document.get("id").and_then(pg_text)).collect();
    db.execute(sqlx::query(MARK_FAILURE_SQL).bind(ids).bind(error)).await
}

pub struct EmbeddingBatch {
    pub embedded: usize,
    pub failed: usize,
    pub documents: Vec<Value>,
    pub error: Option<String>,
}

impl EmbeddingBatch {
    pub fn to_json(&self) -> Value {
        let mut value = json!({ "embedded": self.embedded, "failed": self.failed, "documents": self.documents });
        if let Some(error) = &self.error {
            value["error"] = Value::String(error.clone());
        }
        value
    }
}

/// embedRAGDocumentsBestEffort. 임베딩·저장이 실패하면 문서에 사유를 남기고 실패 건수로 돌려준다. 목록 조회·사유 기록 실패만 오류다.
pub async fn embed_rag_documents_best_effort(
    db: &mut RagDatabase<'_>,
    service: &LocalEmbeddingService,
    filter: PendingFilter<'_>,
) -> Result<EmbeddingBatch, sqlx::Error> {
    let documents = list_pending_rag_embedding_documents(db, filter).await?;
    if documents.is_empty() {
        return Ok(EmbeddingBatch { embedded: 0, failed: 0, documents, error: None });
    }
    let attempt = async {
        let texts: Vec<String> = documents.iter().map(rag_embedding_text).collect();
        let vectors = service.embed(&texts).await?;
        store_rag_document_embeddings(db, &documents, &vectors).await
    }
    .await;
    match attempt {
        Ok(()) => Ok(EmbeddingBatch { embedded: documents.len(), failed: 0, documents, error: None }),
        Err(error) => {
            mark_rag_embedding_failure(db, &documents, &error).await?;
            Ok(EmbeddingBatch { embedded: 0, failed: documents.len(), documents, error: Some(error) })
        }
    }
}

/// backfillRAGEmbeddings. 한 묶음씩 임베딩하고, 실패하면 그 사유로 멈춘다. progress는 (누적 성공, 누적 실패, 이번 묶음)이다.
pub async fn backfill_rag_embeddings(
    db: &mut RagDatabase<'_>,
    service: &LocalEmbeddingService,
    repository_root: Option<&str>,
    batch_size: usize,
    mut progress: impl FnMut(usize, usize, &EmbeddingBatch),
) -> Result<Value, String> {
    let started = Instant::now();
    let (mut embedded, mut failed) = (0, 0);
    loop {
        let filter = PendingFilter { repository_root, limit: Some(batch_size as f64), ..PendingFilter::default() };
        let result =
            embed_rag_documents_best_effort(db, service, filter).await.map_err(|error| database_message(&error))?;
        embedded += result.embedded;
        failed += result.failed;
        progress(embedded, failed, &result);
        if let Some(error) = result.error {
            return Err(error);
        }
        if result.documents.len() < batch_size {
            break;
        }
    }
    let mut summary = json!({
        "embedded": embedded,
        "failed": failed,
        "elapsedSeconds": started.elapsed().as_secs_f64(),
    });
    if let (Some(target), Value::Object(metrics)) = (summary.as_object_mut(), service.metrics()) {
        target.extend(metrics);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 벡터_최소_점수는_유한수만_받아_범위로_자른다() {
        assert_eq!(rag_vector_minimum_score(None), 0.45);
        assert_eq!(rag_vector_minimum_score(Some("")), 0.0);
        assert_eq!(rag_vector_minimum_score(Some(" 0.3 ")), 0.3);
        assert_eq!(rag_vector_minimum_score(Some("2")), 1.0);
        assert_eq!(rag_vector_minimum_score(Some("-5")), -1.0);
        assert_eq!(rag_vector_minimum_score(Some("abc")), 0.45);
        assert_eq!(rag_vector_minimum_score(Some("Infinity")), 0.45);
    }

    #[test]
    fn 검색_개수는_1에서_20_사이고_0과_숫자가_아닌_값은_5다() {
        assert_eq!(bounded_search_limit(None), 5.0);
        assert_eq!(bounded_search_limit(Some(&json!(null))), 5.0);
        assert_eq!(bounded_search_limit(Some(&json!(0))), 5.0);
        assert_eq!(bounded_search_limit(Some(&json!("abc"))), 5.0);
        assert_eq!(bounded_search_limit(Some(&json!("3"))), 3.0);
        assert_eq!(bounded_search_limit(Some(&json!(99))), 20.0);
        assert_eq!(bounded_search_limit(Some(&json!(-4))), 1.0);
        assert_eq!(bounded_search_limit(Some(&json!(2.5))), 2.5);
        assert_eq!(bounded_search_limit(Some(&json!(true))), 1.0);
    }

    #[test]
    fn 점수_거르기는_숫자로_바꿔_비교하고_없는_점수는_뺀다() {
        let documents = vec![json!({ "score": 0.5 }), json!({ "score": "0.46" }), json!({ "score": 0.44 }), json!({})];
        assert_eq!(filter_by_score(documents, 0.45), vec![json!({ "score": 0.5 }), json!({ "score": "0.46" })]);
    }

    #[test]
    fn 임베딩_문장은_제목_둘과_본문_끝_앞을_utf16_길이로_잇는다() {
        assert_eq!(rag_embedding_text(&json!({ "title": "  제목 ", "content": " 본문 " })), "제목\n제목\n본문");
        assert_eq!(rag_embedding_text(&json!({ "title": null, "content": "" })), "");
        let long: String = (0..1000).map(|index| char::from(b'a' + (index % 26) as u8)).collect();
        let text = rag_embedding_text(&json!({ "title": "t", "content": long }));
        let parts: Vec<&str> = text.split('\n').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[2], &long[100..]);
        assert_eq!(parts[3], &long[..600]);
        // 이모지는 UTF-16 두 단위다. 601단위가 되면 끝 구간이 생긴다.
        // 앞 600단위에서 잘린 반쪽 대리 문자를 JS는 그대로 넘기고 여기서는 U+FFFD로 바꾼다(R22 차이 후보).
        let emoji = format!("{}😀", "가".repeat(599));
        let parts: Vec<String> = rag_embedding_text(&json!({ "content": emoji })).split('\n').map(str::to_string).collect();
        assert_eq!(parts[0], emoji);
        assert_eq!(parts[1], format!("{}\u{FFFD}", "가".repeat(599)));
    }

    #[test]
    fn 저장용_벡터는_1024개의_숫자여야_한다() {
        let mut vector = vec![json!(0.5); DIMENSIONS];
        let literal = vector_literal(&Value::Array(vector.clone())).unwrap();
        assert!(literal.starts_with("[0.5,0.5,") && literal.ends_with(",0.5]"));
        let error = "로컬 임베딩은 1024차원 유한 숫자여야 합니다.";
        vector[3] = json!("0.5");
        assert_eq!(vector_literal(&Value::Array(vector.clone())).unwrap_err(), error);
        vector.truncate(3);
        assert_eq!(vector_literal(&Value::Array(vector)).unwrap_err(), error);
        assert_eq!(vector_literal(&json!(null)).unwrap_err(), error);
    }
}
