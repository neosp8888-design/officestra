// database/migrations의 SQL을 이름순으로 적용하고 schema_migrations 표에 기록한다.
// Node의 backend/src/migrate.mjs와 같은 표·같은 이름을 써서 두 백엔드가 한 DB를 공유할 수 있다.

use std::fmt;
use std::path::{Path, PathBuf};

use sqlx::{AssertSqlSafe, PgPool};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub applied: Vec<String>,
    pub skipped: usize,
}

#[derive(Debug)]
pub enum MigrateError {
    Io { path: PathBuf, source: std::io::Error },
    Database(sqlx::Error),
    Apply { file: String, source: sqlx::Error },
}

impl fmt::Display for MigrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "마이그레이션 폴더를 읽을 수 없습니다 ({}). {source}", path.display())
            }
            Self::Database(source) => write!(f, "마이그레이션 기록 표를 준비할 수 없습니다. {source}"),
            Self::Apply { file, source } => {
                write!(f, "마이그레이션 {file} 적용에 실패했습니다. {source}")
            }
        }
    }
}

impl std::error::Error for MigrateError {}

impl From<sqlx::Error> for MigrateError {
    fn from(source: sqlx::Error) -> Self {
        Self::Database(source)
    }
}

/// 폴더의 `.sql` 파일 이름을 바이트 순으로 정렬해 돌려준다. JS의 기본 sort와 같은 순서다.
pub fn migration_files(directory: &Path) -> Result<Vec<String>, MigrateError> {
    let io = |source| MigrateError::Io { path: directory.to_path_buf(), source };
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).map_err(io)? {
        let name = entry.map_err(io)?.file_name();
        let name = name.to_string_lossy().into_owned();
        if name.ends_with(".sql") {
            files.push(name);
        }
    }
    files.sort();
    Ok(files)
}

pub async fn run(pool: &PgPool, directory: &Path) -> Result<Outcome, MigrateError> {
    let files = migration_files(directory)?;
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            name text PRIMARY KEY,
            applied_at timestamptz NOT NULL DEFAULT now()
        )",
    )
    .execute(pool)
    .await?;
    let already: Vec<String> = sqlx::query_scalar("SELECT name FROM schema_migrations")
        .fetch_all(pool)
        .await?;

    let mut outcome = Outcome::default();
    for file in files {
        if already.contains(&file) {
            outcome.skipped += 1;
            continue;
        }
        let path = directory.join(&file);
        let sql = std::fs::read_to_string(&path).map_err(|source| MigrateError::Io { path, source })?;
        let apply = |source| MigrateError::Apply { file: file.clone(), source };
        let mut transaction = pool.begin().await.map_err(apply)?;
        // 저장소의 마이그레이션 파일 자체가 SQL이므로 그대로 실행한다. 사용자 입력이 섞이지 않는다.
        sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut *transaction).await.map_err(apply)?;
        sqlx::query("INSERT INTO schema_migrations (name) VALUES ($1)")
            .bind(&file)
            .execute(&mut *transaction)
            .await
            .map_err(apply)?;
        transaction.commit().await.map_err(apply)?;
        outcome.applied.push(file);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_파일만_이름순으로_고른다() {
        let dir = std::env::temp_dir().join(format!("office-rs-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["010_b.sql", "002_a.sql", "readme.md", "001_z.sql"] {
            std::fs::write(dir.join(name), "SELECT 1;").unwrap();
        }
        let files = migration_files(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(files, vec!["001_z.sql", "002_a.sql", "010_b.sql"]);
    }

    #[test]
    fn 없는_폴더는_경로를_담은_오류다() {
        let error = migration_files(Path::new("/nonexistent/office-rs")).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/office-rs"));
    }
}
