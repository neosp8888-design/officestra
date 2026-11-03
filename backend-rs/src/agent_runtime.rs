// 직원 CLI 실행기. Node agent-runtime.mjs를 옮기며, 실행 인자·첨부 같은 묶음은 하위 모듈에 둔다.

pub mod attachments;
pub mod claude_session;
pub mod compaction;
pub mod file_changes;
pub mod launch;
pub mod lifecycle;
pub mod process_group;
pub mod rollout;
pub mod service;
pub mod terminal;
pub mod turn;
pub mod usage;
pub mod workspace;

use std::ffi::CString;
use std::path::Path;

use serde_json::Value;

use crate::agent_provider::backend_executable_name;
use crate::js::{join_path, js_compare, uv_error};
use crate::local_artifacts::home_directory;

/// configuredExecutableForCharacter. 직원 설정의 executablePath가 있고, 이름이 제공자 실행 파일과 같고,
/// 실행할 수 있는 일반 파일일 때만 쓴다.
pub fn configured_executable_for_character(backend: &str, config: &Value) -> Option<String> {
    let configured = config.get("executablePath")?.as_str().filter(|path| !path.is_empty())?;
    let path = Path::new(configured);
    if !path.exists() {
        return None;
    }
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    if name != backend_executable_name(backend).to_lowercase() {
        return None;
    }
    if !std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) || !executable_by_process(configured) {
        return None;
    }
    Some(configured.to_string())
}

/// accessSync(path, X_OK). 실행 비트만 보지 않고 이 프로세스의 사용자 기준으로 판단한다.
fn executable_by_process(path: &str) -> bool {
    let Ok(path) = CString::new(path) else {
        return false;
    };
    // SAFETY: path는 NUL로 끝나는 올바른 C 문자열이고 access는 그 값을 읽기만 한다.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

/// locateExecutable. 설정값이 없으면 ~/.local/bin, /opt/homebrew/bin, /usr/local/bin 순으로 찾고,
/// Claude는 nvm 판 폴더를 이름 역순으로 먼저 본다. 아무것도 없으면 이름만 돌려 PATH 검색에 맡긴다.
/// nvm 폴더를 읽지 못하면 Node readdirSync처럼 오류다.
pub fn locate_executable(backend: &str, config: &Value) -> Result<String, String> {
    if let Some(configured) = configured_executable_for_character(backend, config) {
        return Ok(configured);
    }
    let home = home_directory();
    let name = backend_executable_name(backend);
    let mut candidates = vec![
        join_path(&[&home, ".local", "bin", &name]),
        join_path(&["/opt/homebrew/bin", &name]),
        join_path(&["/usr/local/bin", &name]),
    ];
    if backend == "claude" {
        let versions = join_path(&[&home, ".nvm", "versions", "node"]);
        if Path::new(&versions).exists() {
            let entries = std::fs::read_dir(&versions).map_err(|error| {
                let (code, description) = uv_error(&error);
                format!("{code}: {description}, scandir '{versions}'")
            })?;
            let mut names: Vec<String> =
                entries.filter_map(Result::ok).map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
            names.sort_by(|left, right| js_compare(left, right));
            names.reverse();
            let nvm = names.iter().map(|version| join_path(&[&versions, version, "bin", "claude"]));
            candidates.splice(0..0, nvm);
        }
    }
    Ok(candidates.into_iter().find(|candidate| Path::new(candidate).exists()).unwrap_or(name))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::*;

    #[test]
    fn configured_path_needs_matching_executable_file() {
        let directory = std::env::temp_dir().join(format!("agent-runtime-{}", std::process::id()));
        let bin = directory.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let claude = bin.join("Claude");
        std::fs::write(&claude, "#!/bin/sh\n").unwrap();
        let text = claude.to_str().unwrap();
        let config = json!({ "executablePath": text });

        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(configured_executable_for_character("claude", &config), None);
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(configured_executable_for_character("claude", &config).as_deref(), Some(text));
        assert_eq!(configured_executable_for_character("codex", &config), None);
        assert_eq!(configured_executable_for_character("claude", &json!({ "executablePath": 5 })), None);
        assert_eq!(configured_executable_for_character("claude", &json!("x")), None);
        let folder = directory.join("folder").join("claude");
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(configured_executable_for_character("claude", &json!({ "executablePath": folder.to_str().unwrap() })), None);
        assert_eq!(locate_executable("claude", &config).as_deref(), Ok(text));
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
