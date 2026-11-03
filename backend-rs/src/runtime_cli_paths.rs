// 설치 도우미가 찾은 CLI 실행 경로를 직원 설정에 동기화한다. Node의 runtime-cli-paths.mjs와 같다.

use std::fmt;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;
use sqlx::{Connection, PgConnection, PgPool};

use crate::agent_provider::{AGENT_BACKENDS, backend_executable_name};
use crate::character_settings::with_character_session_locks;
use crate::configuration::is_executable_file;

#[derive(Debug)]
pub enum RuntimeCLIPathsError {
    /// 400. Node의 RuntimeCLIPathsValidationError.
    Validation(String),
    /// 500.
    Database(sqlx::Error),
}

impl fmt::Display for RuntimeCLIPathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(message) => f.write_str(message),
            Self::Database(error) => write!(f, "{error}"),
        }
    }
}

impl From<sqlx::Error> for RuntimeCLIPathsError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// 제공자(AGENT_BACKENDS 순서)와 검증된 절대 경로 쌍.
pub fn normalize_runtime_cli_paths(body: &Value) -> Result<Vec<(&'static str, String)>, RuntimeCLIPathsError> {
    use RuntimeCLIPathsError::Validation;
    let Some(object) = body.as_object() else {
        return Err(Validation("CLI 경로 요청은 JSON 객체여야 합니다.".to_string()));
    };
    let unknown_keys: Vec<&str> = object.keys().map(String::as_str).filter(|key| *key != "executables").collect();
    if !unknown_keys.is_empty() {
        return Err(Validation(format!("지원하지 않는 요청 항목입니다: {}", unknown_keys.join(", "))));
    }
    let Some(executables) = object.get("executables").and_then(Value::as_object) else {
        return Err(Validation("executables 객체를 입력하세요.".to_string()));
    };
    let unknown_providers: Vec<&str> =
        executables.keys().map(String::as_str).filter(|provider| !AGENT_BACKENDS.contains(provider)).collect();
    if !unknown_providers.is_empty() {
        return Err(Validation(format!("지원하지 않는 CLI입니다: {}", unknown_providers.join(", "))));
    }
    let mut normalized = Vec::new();
    for provider in AGENT_BACKENDS {
        let Some(value) = executables.get(provider) else {
            continue;
        };
        let Some(text) = value.as_str() else {
            return Err(Validation(format!("{provider} 실행 경로는 문자열이어야 합니다.")));
        };
        let executable_path = text.trim();
        if executable_path.is_empty() || !Path::new(executable_path).is_absolute() {
            return Err(Validation(format!("{provider} 실행 경로는 절대경로여야 합니다.")));
        }
        let file_name = Path::new(executable_path).file_name().and_then(|name| name.to_str());
        if file_name != Some(backend_executable_name(provider).as_str()) {
            return Err(Validation(format!("{provider} 실행 파일 이름이 올바르지 않습니다.")));
        }
        if !is_executable_file(Path::new(executable_path)) {
            return Err(Validation(format!("{provider} 실행 경로가 실행 가능한 일반 파일이 아닙니다.")));
        }
        normalized.push((provider, executable_path.to_string()));
    }
    Ok(normalized)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCLIPathsResult {
    pub ok: bool,
    pub updated_character_ids: Vec<String>,
}

/// 모든 직원 잠금을 잡고 제공자별 config.executablePath를 한 트랜잭션에서 병합한다.
pub async fn synchronize_runtime_cli_paths(pool: &PgPool, body: &Value) -> Result<RuntimeCLIPathsResult, RuntimeCLIPathsError> {
    let executables = normalize_runtime_cli_paths(body)?;
    if executables.is_empty() {
        return Ok(RuntimeCLIPathsResult { ok: true, updated_character_ids: Vec::new() });
    }
    let character_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM characters ORDER BY id").fetch_all(pool).await?;
    with_character_session_locks(pool, &character_ids, async |connection: &mut PgConnection| {
        let mut transaction = connection.begin().await?;
        let mut updated_character_ids: Vec<String> = Vec::new();
        for (provider, executable_path) in &executables {
            let outcome: Result<Vec<String>, sqlx::Error> = sqlx::query_scalar(
                "UPDATE characters
                 SET config = jsonb_set(COALESCE(config, '{}'::jsonb), '{executablePath}', to_jsonb($2::text), true),
                     updated_at = now()
                 WHERE backend = $1 AND config ->> 'executablePath' IS DISTINCT FROM $2
                 RETURNING id",
            )
            .bind(provider)
            .bind(executable_path)
            .fetch_all(&mut *transaction)
            .await;
            match outcome {
                Ok(ids) => updated_character_ids.extend(ids),
                Err(error) => {
                    transaction.rollback().await.ok();
                    return Err(error.into());
                }
            }
        }
        transaction.commit().await?;
        updated_character_ids.sort_unstable();
        updated_character_ids.dedup();
        Ok(RuntimeCLIPathsResult { ok: true, updated_character_ids })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// Node 테스트처럼 실행 파일 뒤에 symlink를 두고 링크 경로를 검증한다.
    struct Fixture {
        directory: PathBuf,
        link: PathBuf,
    }

    impl Fixture {
        fn new(provider: &str) -> Self {
            let directory = std::env::temp_dir().join(format!("officestra-runtime-{provider}-{}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            let target = directory.join(format!("{provider}-target"));
            std::fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
            let link = directory.join(provider);
            std::os::unix::fs::symlink(&target, &link).unwrap();
            Self { directory, link }
        }
        fn link(&self) -> String {
            self.link.to_string_lossy().into_owned()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.directory).ok();
        }
    }

    #[test]
    fn symlink_경로는_실행_가능한_일반_파일을_가리킬_때_보존한다() {
        let codex = Fixture::new("codex");
        let normalized = normalize_runtime_cli_paths(&json!({ "executables": { "codex": format!(" {} ", codex.link()) } })).unwrap();
        assert_eq!(normalized, vec![("codex", codex.link())]);
        let agy = Fixture::new("agy");
        let normalized = normalize_runtime_cli_paths(&json!({ "executables": { "antigravity": agy.link() } })).unwrap();
        assert_eq!(normalized, vec![("antigravity", agy.link())]);
    }

    #[test]
    fn 허용되지_않은_키와_안전하지_않은_경로는_모두_거절한다() {
        let directory = std::env::temp_dir().join(format!("officestra-runtime-invalid-{}", std::process::id()));
        let blocked = directory.join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        let non_executable = blocked.join("codex");
        std::fs::write(&non_executable, "").unwrap();
        std::fs::set_permissions(&non_executable, std::fs::Permissions::from_mode(0o644)).unwrap();
        let directory_named_claude = directory.join("directory").join("claude");
        std::fs::create_dir_all(&directory_named_claude).unwrap();
        let cases = [
            (json!(null), "CLI 경로 요청은 JSON 객체여야 합니다."),
            (json!({ "executables": {}, "extra": true }), "지원하지 않는 요청 항목입니다: extra"),
            (json!({ "executables": [] }), "executables 객체를 입력하세요."),
            (json!({ "executables": { "other": "/tmp/other" } }), "지원하지 않는 CLI입니다: other"),
            (json!({ "executables": { "codex": null } }), "codex 실행 경로는 문자열이어야 합니다."),
            (json!({ "executables": { "codex": "relative/codex" } }), "codex 실행 경로는 절대경로여야 합니다."),
            (json!({ "executables": { "codex": "/tmp/claude" } }), "codex 실행 파일 이름이 올바르지 않습니다."),
            (json!({ "executables": { "codex": non_executable.to_string_lossy() } }), "codex 실행 경로가 실행 가능한 일반 파일이 아닙니다."),
            (json!({ "executables": { "claude": directory_named_claude.to_string_lossy() } }), "claude 실행 경로가 실행 가능한 일반 파일이 아닙니다."),
        ];
        for (body, expected) in cases {
            assert_eq!(normalize_runtime_cli_paths(&body).unwrap_err().to_string(), expected, "{body}");
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
