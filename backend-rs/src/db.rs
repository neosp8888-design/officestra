// PostgreSQL 연결 풀. Node의 pg Pool과 같은 기본 최대 연결 수 10을 쓴다.

use std::time::Duration;

use sqlx::PgConnection;
use sqlx::postgres::{PgPool, PgPoolOptions};

pub const MAX_CONNECTIONS: u32 = 10;
/// 연결을 못 얻으면 이 시간 안에 포기한다. DB가 죽었을 때 /health가 바로 503을 내도록 짧게 둔다.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn connect(url: &str) -> Result<PgPool, sqlx::Error> {
    options().connect(url).await
}

/// 첫 질의 때 연결한다. 테스트에서 DB 없이 라우터를 만들 때 쓴다.
pub fn connect_lazy(url: &str, acquire_timeout: Duration) -> Result<PgPool, sqlx::Error> {
    options().acquire_timeout(acquire_timeout).connect_lazy(url)
}

/// Node의 withTransaction. 실패하면 커밋하지 않고 버려 롤백한다.
pub async fn with_transaction<T, E: From<sqlx::Error>>(
    pool: &PgPool,
    operation: impl AsyncFnOnce(&mut PgConnection) -> Result<T, E>,
) -> Result<T, E> {
    let mut transaction = pool.begin().await?;
    let result = operation(&mut transaction).await?;
    transaction.commit().await?;
    Ok(result)
}

fn options() -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .acquire_timeout(ACQUIRE_TIMEOUT)
}
