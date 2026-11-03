// 직원 JSON 설정을 읽어 characters 표와 동기화한다. Node의 configuration.mjs와 git-workspace.mjs의 canonicalProjectRoot를 옮겼다.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sqlx::PgConnection;
use sqlx::types::Json;

use crate::agent_provider::{agent_provider, backend_executable_name};

/// JSON 파일을 읽고 OFFICE_WORKDIR이 있으면 workdir을 덮어쓴다.
pub fn read_character_configuration(path: &Path, runtime_workdir: Option<&str>) -> Result<Value, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("직원 설정 파일을 읽을 수 없습니다: {} ({error})", path.display()))?;
    let configuration: Value = serde_json::from_str(&source)
        .map_err(|error| format!("직원 설정 파일을 해석할 수 없습니다: {} ({error})", path.display()))?;
    Ok(configuration_with_runtime_workdir(configuration, runtime_workdir))
}

pub fn configuration_with_runtime_workdir(mut configuration: Value, runtime_workdir: Option<&str>) -> Value {
    let normalized = runtime_workdir.unwrap_or("").trim();
    if normalized.is_empty() {
        return configuration;
    }
    let resolved = std::path::absolute(normalized).unwrap_or_else(|_| PathBuf::from(normalized));
    if let Some(object) = configuration.as_object_mut() {
        object.insert("workdir".to_string(), Value::String(resolved.to_string_lossy().into_owned()));
    }
    configuration
}

pub fn configuration_workdir(configuration: &Value) -> Option<PathBuf> {
    configuration.get("workdir").and_then(Value::as_str).map(PathBuf::from)
}

/// CLI 세션을 끝내야 하는 설정 변경인지 판정할 때 쓰는 조각.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTarget<'a> {
    pub backend: &'a str,
    pub local_profile_id: Option<&'a str>,
}

impl<'a> SessionTarget<'a> {
    /// Node처럼 config.localProfileId ?? localProfileId 순서로 읽는다.
    pub fn from_value(value: &'a Value) -> Self {
        let local_profile_id = value
            .get("config")
            .and_then(|config| config.get("localProfileId"))
            .filter(|id| !id.is_null())
            .or_else(|| value.get("localProfileId"))
            .and_then(Value::as_str);
        Self { backend: value.get("backend").and_then(Value::as_str).unwrap_or(""), local_profile_id }
    }
}

pub fn character_settings_require_new_session(previous: &SessionTarget<'_>, next: &SessionTarget<'_>) -> bool {
    previous.backend != next.backend || previous.local_profile_id != next.local_profile_id
}

/// Node의 access(path, X_OK)에 해당한다. 일반 파일이고 실행 비트가 하나라도 있어야 한다.
pub fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// executablePath가 비어 있거나 제공자와 맞지 않거나 실행할 수 없으면 키를 뺀다.
pub fn character_configuration_for_sync(character: &Value, executable_ok: impl Fn(&Path) -> bool) -> Value {
    let mut object: Map<String, Value> = character.as_object().cloned().unwrap_or_default();
    let executable_path = object.get("executablePath").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let backend = object.get("backend").and_then(Value::as_str).unwrap_or("");
    let expected = agent_provider(backend).map(|_| backend_executable_name(backend));
    let matches_name = expected.is_some_and(|name| Path::new(&executable_path).file_name().and_then(|n| n.to_str()) == Some(name.as_str()));
    if executable_path.is_empty() || !matches_name || !executable_ok(Path::new(&executable_path)) {
        object.remove("executablePath");
    } else {
        object.insert("executablePath".to_string(), Value::String(executable_path));
    }
    Value::Object(object)
}

/// characters 표에 직원을 넣거나, 이미 있으면 좌석과 실행 경로만 갱신한다.
pub async fn sync_characters(connection: &mut PgConnection, configuration: &Value) -> Result<(), sqlx::Error> {
    let characters = configuration.get("characters").and_then(Value::as_array).cloned().unwrap_or_default();
    for character in &characters {
        let synchronized = character_configuration_for_sync(character, is_executable_file);
        let text = |name: &str| character.get(name).and_then(Value::as_str).map(str::to_string);
        sqlx::query(
            "INSERT INTO characters (id, name, seat, backend, identity_prompt, model, effort, fast_mode, permission, config)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::jsonb)
             ON CONFLICT (id) DO UPDATE
             SET seat = EXCLUDED.seat,
                 config = CASE
                   WHEN characters.backend = EXCLUDED.backend AND EXCLUDED.config ? 'executablePath'
                     THEN jsonb_set(COALESCE(characters.config, '{}'::jsonb), '{executablePath}', EXCLUDED.config -> 'executablePath', true)
                   ELSE characters.config
                 END,
                 updated_at = now()",
        )
        .bind(text("id"))
        .bind(text("name"))
        .bind(text("seat"))
        .bind(text("backend"))
        .bind(text("identityPrompt"))
        .bind(text("model"))
        .bind(text("effort"))
        .bind(character.get("fastMode").and_then(Value::as_bool).unwrap_or(true))
        .bind(text("permission"))
        .bind(Json(synchronized))
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

/// 작업 폴더의 실제 경로를 구하고, git 저장소면 최상위 폴더를 돌려준다. Node canonicalProjectRoot와 같은 판정이다.
pub async fn canonical_project_root(workdir: &Path) -> Result<PathBuf, String> {
    crate::git_workspace::canonical_project_root(&workdir.to_string_lossy())
        .await
        .map(PathBuf::from)
        .map_err(|error| error.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 실행_workdir이_있으면_설정의_workdir을_덮어쓴다() {
        let configuration = json!({ "workdir": "/Users/your-name/Projects", "characters": [] });
        assert_eq!(configuration_with_runtime_workdir(configuration.clone(), None), configuration);
        assert_eq!(configuration_with_runtime_workdir(configuration.clone(), Some("   ")), configuration);
        let overridden = configuration_with_runtime_workdir(configuration, Some(" /Users/neo/office "));
        assert_eq!(configuration_workdir(&overridden), Some(PathBuf::from("/Users/neo/office")));
    }

    #[test]
    fn 새_세션은_cli나_로컬_프로필이_바뀔_때만_필요하다() {
        let claude = json!({ "backend": "claude" });
        let claude_config_local = json!({ "backend": "claude", "config": { "localProfileId": "local-4090" } });
        let claude_flat_local = json!({ "backend": "claude", "localProfileId": "local-4090" });
        let codex = json!({ "backend": "codex" });
        let same = |a: &Value, b: &Value| {
            character_settings_require_new_session(&SessionTarget::from_value(a), &SessionTarget::from_value(b))
        };
        assert!(!same(&claude, &claude));
        assert!(same(&claude, &codex));
        assert!(same(&claude, &claude_config_local));
        assert!(same(&claude_config_local, &claude));
        assert!(!same(&claude_config_local, &claude_flat_local));
        assert!(!same(&json!({ "backend": "claude", "config": { "localProfileId": null } }), &claude));
    }

    #[test]
    fn 실행_경로는_이름이_맞고_실행_가능할_때만_남긴다() {
        let character = |path: Value| json!({ "id": "a", "backend": "codex", "executablePath": path, "hitbox": { "x": 1 } });
        let kept = character_configuration_for_sync(&character(json!(" /opt/bin/codex ")), |_| true);
        assert_eq!(kept["executablePath"], "/opt/bin/codex");
        assert_eq!(kept["hitbox"]["x"], 1);
        for (path, ok) in [
            (json!(null), true),
            (json!("   "), true),
            (json!("/opt/bin/claude"), true),
            (json!("/opt/bin/codex"), false),
        ] {
            let synced = character_configuration_for_sync(&character(path), |_| ok);
            assert!(synced.get("executablePath").is_none(), "{synced}");
        }
        let unknown = json!({ "id": "a", "backend": "unknown", "executablePath": "/opt/bin/unknown" });
        assert!(character_configuration_for_sync(&unknown, |_| true).get("executablePath").is_none());
    }

    #[test]
    fn 실행_파일_검사는_일반_파일과_실행_비트를_본다() {
        let dir = std::env::temp_dir().join(format!("office-rs-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("codex");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable_file(&script));
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable_file(&script));
        assert!(!is_executable_file(&dir));
        assert!(!is_executable_file(&dir.join("missing")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn 저장소_루트는_git_최상위_폴더다() {
        let root = canonical_project_root(Path::new(env!("CARGO_MANIFEST_DIR"))).await.unwrap();
        assert_eq!(root, std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()).unwrap());
        assert!(canonical_project_root(Path::new("/nonexistent/office-rs")).await.is_err());
        let plain = std::env::temp_dir().join(format!("office-rs-plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        let resolved = canonical_project_root(&plain).await.unwrap();
        std::fs::remove_dir_all(&plain).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(std::env::temp_dir()).unwrap().join(plain.file_name().unwrap()));
    }
}
