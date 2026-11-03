// OFFICESTRA 백엔드 Rust 리뉴얼 실행 파일. 설정·DB·마이그레이션·HTTP 서버는 lib.rs의 run()이 맡는다.
// 첫 인자가 하위 명령 이름이면 임베딩 워커나 RAG 관리 도구로 동작한다.

use officestra_backend::{backfill_rag_embeddings, benchmark_rag_search, local_embedding};

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let code = match arguments.first().map(String::as_str) {
        // 워커는 요청을 한 줄씩 차례로 처리하므로 비동기 실행기를 띄우지 않는다.
        Some("embedding-worker") => local_embedding::run_worker(),
        Some("backfill-rag-embeddings") => block_on(backfill_rag_embeddings::run_cli(&arguments[1..])),
        Some("benchmark-rag-search") => block_on(benchmark_rag_search::run_cli()),
        _ => block_on(async {
            match officestra_backend::run().await {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("{error}");
                    1
                }
            }
        }),
    };
    std::process::exit(code);
}

fn block_on(work: impl Future<Output = i32>) -> i32 {
    match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime.block_on(work),
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}
