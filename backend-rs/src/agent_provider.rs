// 직원별 AI CLI의 실행 파일과 허용 설정을 한곳에서 정의한다. Node의 agent-provider.mjs와 같은 목록이다.

pub struct Provider {
    pub executable: &'static str,
    pub models: &'static [&'static str],
    pub efforts: &'static [&'static str],
    pub permissions: &'static [&'static str],
}

const CODEX: Provider = Provider {
    executable: "codex",
    models: &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"],
    efforts: &["low", "medium", "high", "xhigh", "max", "ultra"],
    permissions: &["read-only", "workspace-write", "danger-full-access"],
};

const CLAUDE: Provider = Provider {
    executable: "claude",
    models: &["claude-opus-5", "fable", "claude-sonnet-5"],
    efforts: &["high", "xhigh", "max"],
    permissions: &["plan", "auto", "acceptEdits", "bypassPermissions"],
};

// 앞 두 모델이 현재 고르는 목록이다. 아래 셋은 목록에서 내렸지만 이미 직원 설정과 지난 턴에
// 남아 있으므로, 그 값을 다시 저장할 때 검증에서 막히지 않도록 허용 목록에는 남긴다.
const ANTIGRAVITY: Provider = Provider {
    executable: "agy",
    models: &["gemini-3.8-flash", "gemini-3.1-pro", "gemini-3.7-flash", "gemini-3.6-flash", "gemini-3.5-flash"],
    efforts: &["low", "medium", "high"],
    permissions: &["plan", "accept-edits", "dangerously-skip-permissions"],
};

/// Node의 AGENT_BACKENDS 순서(codex, claude, antigravity)를 그대로 둔다.
pub const AGENT_BACKENDS: [&str; 3] = ["codex", "claude", "antigravity"];

pub fn agent_provider(backend: &str) -> Option<&'static Provider> {
    match backend {
        "codex" => Some(&CODEX),
        "claude" => Some(&CLAUDE),
        "antigravity" => Some(&ANTIGRAVITY),
        _ => None,
    }
}

pub fn backend_executable_name(backend: &str) -> String {
    agent_provider(backend).map(|p| p.executable.to_string()).unwrap_or_else(|| backend.to_string())
}

pub fn backend_models(backend: &str) -> &'static [&'static str] {
    agent_provider(backend).map(|p| p.models).unwrap_or(&[])
}

pub fn backend_efforts(backend: &str, model: Option<&str>) -> &'static [&'static str] {
    if backend == "antigravity" && model == Some("gemini-3.1-pro") {
        return &["low", "high"];
    }
    agent_provider(backend).map(|p| p.efforts).unwrap_or(&[])
}

pub fn backend_permissions(backend: &str) -> &'static [&'static str] {
    agent_provider(backend).map(|p| p.permissions).unwrap_or(&[])
}

pub fn backend_supports_fast_mode(backend: &str, model: &str) -> bool {
    backend == "codex" || (backend == "claude" && model == "claude-opus-5")
}

pub fn normalized_backend_permission(backend: &str, permission: &str) -> String {
    if backend == "claude" && permission == "acceptEdits" {
        return "auto".to_string();
    }
    permission.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 제공자_목록과_실행_파일_이름() {
        assert_eq!(AGENT_BACKENDS, ["codex", "claude", "antigravity"]);
        assert_eq!(backend_executable_name("antigravity"), "agy");
        assert_eq!(backend_executable_name("unknown"), "unknown");
        assert!(agent_provider("unknown").is_none());
        assert!(backend_models("unknown").is_empty());
    }

    #[test]
    fn 모델별_추론_단계와_fast_모드() {
        assert_eq!(backend_efforts("antigravity", Some("gemini-3.1-pro")), ["low", "high"]);
        assert_eq!(backend_efforts("antigravity", Some("gemini-3.8-flash")), ["low", "medium", "high"]);
        assert_eq!(backend_efforts("claude", None), ["high", "xhigh", "max"]);
        assert!(backend_supports_fast_mode("codex", "gpt-5.6-sol"));
        assert!(backend_supports_fast_mode("claude", "claude-opus-5"));
        assert!(!backend_supports_fast_mode("claude", "claude-sonnet-5"));
        assert!(!backend_supports_fast_mode("antigravity", "gemini-3.8-flash"));
    }

    #[test]
    fn claude_accept_edits는_auto로_저장한다() {
        assert_eq!(normalized_backend_permission("claude", "acceptEdits"), "auto");
        assert_eq!(normalized_backend_permission("codex", "workspace-write"), "workspace-write");
    }
}
