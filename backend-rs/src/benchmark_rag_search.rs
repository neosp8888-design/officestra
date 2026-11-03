// `benchmark-rag-search` 하위 명령. 고정 질의 10개를 전문검색과 자동(임베딩) 검색으로 돌려 상위 3건을 비교한다. Node benchmark-rag-search.mjs를 옮겼다.

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::config::Config;
use crate::db;
use crate::js::{js_to_number, to_js_json_pretty};
use crate::local_embedding::LocalEmbeddingService;
use crate::rag::{RagDatabase, SearchRequest, database_message, rag_vector_minimum_score, search_rag_documents};

pub const RAG_COMPARISON_QUERIES: [&str; 10] = [
    "만화 콘티",
    "웹툰 그림 순서",
    "ComfyUI",
    "그림 만드는 프로그램",
    "임베딩",
    "검색이 잘 안 되는 문제",
    "RAM 95%",
    "메모리 부족으로 중단",
    "터널",
    "원격 접속 끊김",
];

fn compact(documents: &[Value]) -> Vec<Value> {
    let field = |document: &Value, key: &str| document.get(key).filter(|value| !value.is_null()).cloned();
    documents
        .iter()
        .map(|document| {
            json!({
                "ragDocumentId": field(document, "ragDocumentId").or_else(|| document.get("id").cloned()),
                "workRecordId": field(document, "workRecordId"),
                "title": document.get("title"),
                "score": document.get("score").map_or(f64::NAN, js_to_number),
            })
        })
        .collect()
}

/// benchmarkRAGSearch. 끝나면 임베딩 워커를 닫는다.
pub async fn benchmark_rag_search(pool: &PgPool, service: &LocalEmbeddingService, limit: u32) -> Result<Value, String> {
    let limit = json!(limit);
    let mut results = Vec::new();
    let outcome = async {
        for query in RAG_COMPARISON_QUERIES {
            let query_value = json!(query);
            let minimum_score = rag_vector_minimum_score(std::env::var("OFFICE_RAG_VECTOR_MIN_SCORE").ok().as_deref());
            let search = |service| SearchRequest {
                query: Some(&query_value),
                embedding: None,
                limit: Some(&limit),
                service,
                minimum_score,
            };
            let lexical = search_rag_documents(&mut RagDatabase::Pool(pool), search(None))
                .await
                .map_err(|error| database_message(&error))?;
            let automatic = search_rag_documents(&mut RagDatabase::Pool(pool), search(Some(service)))
                .await
                .map_err(|error| database_message(&error))?;
            results.push(json!({
                "query": query,
                "lexical": compact(&lexical.documents),
                "automaticMode": automatic.mode,
                "automatic": compact(&automatic.documents),
            }));
        }
        Ok(json!({ "results": results, "metrics": service.metrics() }))
    }
    .await;
    service.close();
    outcome
}

pub async fn run_cli() -> i32 {
    let result = async {
        let config = Config::from_env()?;
        let pool = db::connect(&config.database_url).await.map_err(|error| error.to_string())?;
        let result = benchmark_rag_search(&pool, &LocalEmbeddingService::default(), 3).await;
        pool.close().await;
        result
    }
    .await;
    match result {
        Ok(output) => {
            println!("{}", to_js_json_pretty(&output));
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}
