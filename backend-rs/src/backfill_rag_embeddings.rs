// `backfill-rag-embeddings` 하위 명령. 확인 옵션과 저장소 절대경로를 받아 남은 RAG 문서를 로컬 임베딩으로 채운다. Node backfill-rag-embeddings.mjs를 옮겼다.

use serde_json::{Value, json};

use crate::config::Config;
use crate::js::{js_trim, to_js_json_pretty};
use crate::local_embedding::LocalEmbeddingService;
use crate::rag::{DEFAULT_EMBEDDING_BATCH_SIZE, RagDatabase, backfill_rag_embeddings};
use crate::{db, migrate};

/// Node 사용법 문구에서 실행 명령만 이 실행 파일 이름으로 바꿨다.
pub const USAGE: &str = "사용법\nofficestra-backend backfill-rag-embeddings --apply --repository-root <절대경로>";

/// parseBackfillRAGEmbeddingArguments. 저장소 경로를 다듬어 돌려준다.
pub fn parse_arguments(arguments: &[String]) -> Result<String, String> {
    let mut apply = false;
    let mut repository_root = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--apply" => apply = true,
            "--repository-root" => {
                repository_root = arguments.get(index + 1).cloned();
                index += 1;
            }
            other => return Err(format!("알 수 없는 옵션입니다. {other}")),
        }
        index += 1;
    }
    if !apply {
        return Err("실제 백필에는 --apply 확인이 필요합니다.".into());
    }
    let root = repository_root.unwrap_or_default();
    let root = js_trim(&root);
    if !root.starts_with('/') {
        return Err("--repository-root는 절대경로여야 합니다.".into());
    }
    Ok(root.to_string())
}

async fn backfill(arguments: &[String], service: &LocalEmbeddingService) -> Result<Value, String> {
    let repository_root = parse_arguments(arguments)?;
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await.map_err(|error| error.to_string())?;
    let result = async {
        let migration = migrate::run(&pool, &config.migrations_dir).await.map_err(|error| error.to_string())?;
        let result = backfill_rag_embeddings(
            &mut RagDatabase::Pool(&pool),
            service,
            Some(&repository_root),
            DEFAULT_EMBEDDING_BATCH_SIZE,
            |embedded, _, _| {
                if embedded > 0 && embedded % 100 == 0 {
                    eprintln!("RAG 임베딩 백필 {embedded}건 완료");
                }
            },
        )
        .await?;
        let mut output = json!({ "migration": { "applied": migration.applied, "skipped": migration.skipped } });
        if let (Some(target), Value::Object(fields)) = (output.as_object_mut(), result) {
            target.extend(fields);
        }
        Ok(output)
    }
    .await;
    pool.close().await;
    result
}

/// 종료 코드를 돌려준다. 실패하면 사유와 사용법을 stderr에 쓴다.
pub async fn run_cli(arguments: &[String]) -> i32 {
    let service = LocalEmbeddingService::default();
    let result = backfill(arguments, &service).await;
    service.close();
    match result {
        Ok(output) => {
            println!("{}", to_js_json_pretty(&output));
            0
        }
        Err(error) => {
            eprintln!("{error}");
            eprintln!("{USAGE}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<String, String> {
        parse_arguments(&arguments.iter().map(ToString::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn 인자는_확인_옵션과_절대경로를_요구한다() {
        assert_eq!(parse(&["--apply", "--repository-root", "  /Users/neo/office "]).unwrap(), "/Users/neo/office");
        assert_eq!(parse(&["--repository-root", "/a", "--apply"]).unwrap(), "/a");
        assert_eq!(parse(&["--repository-root", "/a"]).unwrap_err(), "실제 백필에는 --apply 확인이 필요합니다.");
        assert_eq!(parse(&["--apply"]).unwrap_err(), "--repository-root는 절대경로여야 합니다.");
        assert_eq!(parse(&["--apply", "--repository-root"]).unwrap_err(), "--repository-root는 절대경로여야 합니다.");
        assert_eq!(parse(&["--apply", "--repository-root", "office"]).unwrap_err(), "--repository-root는 절대경로여야 합니다.");
        assert_eq!(parse(&["--apply", "--dry-run"]).unwrap_err(), "알 수 없는 옵션입니다. --dry-run");
        // Node처럼 --repository-root 다음 값은 옵션 이름이어도 경로로 먹는다.
        assert_eq!(parse(&["--repository-root", "--apply"]).unwrap_err(), "실제 백필에는 --apply 확인이 필요합니다.");
    }
}
