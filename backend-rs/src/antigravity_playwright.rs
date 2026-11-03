// Antigravity CLI가 요구하는 Playwright 1.57 드라이버 경로를 자식 환경에 넣는다. Node antigravity-playwright.mjs와 같다.

use std::collections::BTreeMap;
use std::path::Path;

pub const ANTIGRAVITY_PLAYWRIGHT_VERSION: &str = "1.57.0";

/// 드라이버 폴더 이름. Node는 모듈 옆 `playwright-driver-1.57.0`을 쓴다.
pub fn antigravity_playwright_driver_folder_name() -> String {
    format!("playwright-driver-{ANTIGRAVITY_PLAYWRIGHT_VERSION}")
}

/// antigravityPlaywrightEnvironment. 드라이버의 package/cli.js가 있을 때만 두 값을 채우고,
/// 이미 있는 값은 덮지 않는다(`??=`). Node 실행 파일을 모르면 드라이버 경로만 넣는다.
pub fn antigravity_playwright_environment(
    base: &BTreeMap<String, String>,
    driver_directory: &str,
    node_executable: Option<&str>,
) -> BTreeMap<String, String> {
    let mut environment = base.clone();
    if !Path::new(driver_directory).join("package").join("cli.js").exists() {
        return environment;
    }
    environment.entry("PLAYWRIGHT_DRIVER_PATH".to_string()).or_insert_with(|| driver_directory.to_string());
    if let Some(node) = node_executable {
        environment.entry("PLAYWRIGHT_NODEJS_PATH".to_string()).or_insert_with(|| node.to_string());
    }
    environment
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 드라이버가_있을_때만_비어_있는_값을_채운다() {
        let root = std::env::temp_dir().join(format!("antigravity-playwright-{}", std::process::id()));
        let driver = root.join(antigravity_playwright_driver_folder_name());
        let driver_text = driver.to_str().unwrap().to_string();
        let base = BTreeMap::from([("PATH".to_string(), "/tmp/bin".to_string())]);

        let missing = antigravity_playwright_environment(&base, &driver_text, Some("/bin/node"));
        assert_eq!(missing, base);

        std::fs::create_dir_all(driver.join("package")).unwrap();
        std::fs::write(driver.join("package").join("cli.js"), "").unwrap();
        let filled = antigravity_playwright_environment(&base, &driver_text, Some("/bin/node"));
        assert_eq!(filled["PLAYWRIGHT_DRIVER_PATH"], driver_text);
        assert!(filled["PLAYWRIGHT_DRIVER_PATH"].ends_with("playwright-driver-1.57.0"));
        assert_eq!(filled["PLAYWRIGHT_NODEJS_PATH"], "/bin/node");

        let mut preset = base.clone();
        preset.insert("PLAYWRIGHT_DRIVER_PATH".into(), String::new());
        let kept = antigravity_playwright_environment(&preset, &driver_text, None);
        assert_eq!(kept["PLAYWRIGHT_DRIVER_PATH"], "", "빈 문자열도 이미 있는 값이라 덮지 않는다");
        assert!(!kept.contains_key("PLAYWRIGHT_NODEJS_PATH"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
