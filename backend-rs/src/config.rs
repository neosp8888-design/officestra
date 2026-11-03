// 환경 변수에서 서버 설정을 읽는다. 기본값은 Node 백엔드(server.mjs·db.mjs·configuration.mjs·roleplay-service.mjs)와 같다.

use std::env;
use std::fmt;
use std::path::PathBuf;

pub const DEFAULT_DATABASE_URL: &str = "postgres://office:office-local@127.0.0.1:54329/office";
pub const DEFAULT_PORT: u16 = 4317;
pub const DEFAULT_MIGRATIONS_DIR: &str = "database/migrations";
pub const DEFAULT_CHARACTER_CONFIG_PATH: &str = "Sources/OfficeCore/Resources/characters.json";
pub const DEFAULT_ROLEPLAY_API_URL: &str = "http://127.0.0.1:8080/v1/messages";

/// 로그나 Debug 출력에 값이 찍히지 않는 비밀 문자열.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<비공개>")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub release_id: Option<String>,
    pub workdir: Option<PathBuf>,
    pub migrations_dir: PathBuf,
    /// 직원 JSON 경로. 상대 경로는 Node에서 저장소 루트 기준이었고, 여기서는 OFFICE_WORKDIR(없으면 현재 폴더) 기준이다.
    pub character_config_path: PathBuf,
    pub embedding_model_dir: Option<PathBuf>,
    pub worktree_root: Option<PathBuf>,
    pub claude_config_dir: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    /// 네이티브 officestra-result가 있는 폴더. 없으면 현재 실행 파일 폴더를 쓴다.
    pub result_tool_dir: Option<PathBuf>,
    /// 외부 Antigravity CLI의 Playwright용 Node. 앱에는 포함하지 않으며 없으면 PATH에 더하지 않는다.
    pub node_executable: Option<PathBuf>,
    /// Antigravity용 Playwright 1.57 드라이버 폴더. 없으면 결과 도구 폴더의 playwright-driver-1.57.0이다.
    pub playwright_driver_dir: Option<PathBuf>,
    pub roleplay_api_url: String,
    pub roleplay_api_key_file: PathBuf,
    pub claude_code_oauth_token: Option<Secret>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|key| env::var(key).ok(), env::home_dir())
    }

    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        home: Option<PathBuf>,
    ) -> Result<Self, String> {
        let port = match get("OFFICE_BACKEND_PORT") {
            None => DEFAULT_PORT,
            Some(value) => value
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("OFFICE_BACKEND_PORT 값이 올바르지 않습니다: {value}"))?,
        };
        let workdir = get("OFFICE_WORKDIR")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let config_base = workdir.clone().unwrap_or_else(|| PathBuf::from("."));
        let character_config_path = config_base.join(
            get("CHARACTER_CONFIG_PATH").unwrap_or_else(|| DEFAULT_CHARACTER_CONFIG_PATH.to_string()),
        );
        let home = home.unwrap_or_else(|| PathBuf::from("."));
        let roleplay_api_key_file = get("OFFICE_ROLEPLAY_API_KEY_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config").join("antigravity-proxy").join("local-api-key"));
        let optional_path = |key: &str| get(key).map(PathBuf::from);
        Ok(Self {
            port,
            database_url: get("DATABASE_URL").unwrap_or_else(|| DEFAULT_DATABASE_URL.to_string()),
            release_id: get("OFFICESTRA_RELEASE_ID"),
            workdir,
            migrations_dir: get("OFFICE_MIGRATIONS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_MIGRATIONS_DIR)),
            character_config_path,
            embedding_model_dir: optional_path("OFFICE_EMBEDDING_MODEL_DIR"),
            worktree_root: optional_path("OFFICE_WORKTREE_ROOT"),
            claude_config_dir: optional_path("CLAUDE_CONFIG_DIR"),
            codex_home: optional_path("CODEX_HOME"),
            result_tool_dir: optional_path("OFFICE_RESULT_TOOL_DIR"),
            node_executable: optional_path("OFFICE_NODE_EXECUTABLE"),
            playwright_driver_dir: optional_path("OFFICE_PLAYWRIGHT_DRIVER_DIR"),
            roleplay_api_url: get("OFFICE_ROLEPLAY_API_URL").unwrap_or_else(|| DEFAULT_ROLEPLAY_API_URL.to_string()),
            roleplay_api_key_file,
            claude_code_oauth_token: get("CLAUDE_CODE_OAUTH_TOKEN").map(Secret),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn 기본값은_node_백엔드와_같다() {
        let config = Config::from_lookup(lookup(&[]), Some(PathBuf::from("/Users/x"))).unwrap();
        assert_eq!(config.port, 4317);
        assert_eq!(config.database_url, DEFAULT_DATABASE_URL);
        assert_eq!(config.release_id, None);
        assert_eq!(config.workdir, None);
        assert_eq!(config.migrations_dir, PathBuf::from("database/migrations"));
        assert_eq!(config.character_config_path, PathBuf::from("./Sources/OfficeCore/Resources/characters.json"));
        assert_eq!(config.roleplay_api_url, DEFAULT_ROLEPLAY_API_URL);
        assert_eq!(config.roleplay_api_key_file, PathBuf::from("/Users/x/.config/antigravity-proxy/local-api-key"));
        assert_eq!(config.claude_code_oauth_token, None);
        assert_eq!(config.codex_home, None);
    }

    #[test]
    fn 환경_변수를_읽는다() {
        let config = Config::from_lookup(
            lookup(&[
                ("OFFICE_BACKEND_PORT", "4318"),
                ("DATABASE_URL", "postgres://x"),
                ("OFFICESTRA_RELEASE_ID", "abc"),
                ("OFFICE_WORKDIR", " /tmp/w "),
                ("OFFICE_MIGRATIONS_DIR", "/tmp/m"),
                ("CHARACTER_CONFIG_PATH", "/abs/characters.json"),
                ("OFFICE_EMBEDDING_MODEL_DIR", "/tmp/e"),
                ("OFFICE_WORKTREE_ROOT", "/tmp/t"),
                ("CLAUDE_CONFIG_DIR", "/tmp/c"),
                ("CODEX_HOME", "/tmp/x"),
                ("OFFICE_RESULT_TOOL_DIR", "/tmp/r"),
                ("OFFICE_NODE_EXECUTABLE", "/tmp/n/node"),
                ("OFFICE_PLAYWRIGHT_DRIVER_DIR", "/tmp/p"),
                ("OFFICE_ROLEPLAY_API_URL", "http://127.0.0.1:1/v1/messages"),
                ("OFFICE_ROLEPLAY_API_KEY_FILE", "/tmp/k"),
                ("CLAUDE_CODE_OAUTH_TOKEN", "secret-value"),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(config.port, 4318);
        assert_eq!(config.database_url, "postgres://x");
        assert_eq!(config.release_id.as_deref(), Some("abc"));
        assert_eq!(config.workdir, Some(PathBuf::from("/tmp/w")));
        assert_eq!(config.migrations_dir, PathBuf::from("/tmp/m"));
        // 절대 경로를 join하면 절대 경로가 그대로 남는다.
        assert_eq!(config.character_config_path, PathBuf::from("/abs/characters.json"));
        assert_eq!(config.embedding_model_dir, Some(PathBuf::from("/tmp/e")));
        assert_eq!(config.worktree_root, Some(PathBuf::from("/tmp/t")));
        assert_eq!(config.claude_config_dir, Some(PathBuf::from("/tmp/c")));
        assert_eq!(config.codex_home, Some(PathBuf::from("/tmp/x")));
        assert_eq!(config.result_tool_dir, Some(PathBuf::from("/tmp/r")));
        assert_eq!(config.node_executable, Some(PathBuf::from("/tmp/n/node")));
        assert_eq!(config.playwright_driver_dir, Some(PathBuf::from("/tmp/p")));
        assert_eq!(config.roleplay_api_url, "http://127.0.0.1:1/v1/messages");
        assert_eq!(config.roleplay_api_key_file, PathBuf::from("/tmp/k"));
        assert_eq!(config.claude_code_oauth_token.as_ref().unwrap().expose(), "secret-value");
        assert!(!format!("{config:?}").contains("secret-value"));
    }

    #[test]
    fn 상대_직원_json_경로는_workdir_기준이다() {
        let config = Config::from_lookup(
            lookup(&[("OFFICE_WORKDIR", "/Users/neo/office"), ("CHARACTER_CONFIG_PATH", "x/c.json")]),
            None,
        )
        .unwrap();
        assert_eq!(config.character_config_path, PathBuf::from("/Users/neo/office/x/c.json"));
    }

    #[test]
    fn 포트가_숫자가_아니면_거부한다() {
        let error = Config::from_lookup(lookup(&[("OFFICE_BACKEND_PORT", "abc")]), None).unwrap_err();
        assert!(error.contains("OFFICE_BACKEND_PORT"));
    }
}
