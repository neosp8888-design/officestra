// 직원이 쓰는 CLI의 설치본과 배포 최신본을 비교하고 갱신한다. Node cli-updates.mjs와 server.mjs의 hasRunningWork를 옮겼다.

use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::{BoxFuture, join_all};
use regex::Regex;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::child_process::{DEFAULT_MAX_BUFFER, ExecFileOutput, exec_file};
use crate::js::{iso_string, js_string, js_trim, now_milliseconds};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UpdateKind {
    Npm,
    /// 실행 파일이 스스로 갱신한다(Node updateKind "self").
    SelfUpdate,
}

/// Node CLI_PACKAGES의 한 항목. 최신 버전·갱신 인자는 SelfUpdate일 때만 쓴다.
#[derive(Debug, PartialEq)]
pub struct CliPackage {
    pub id: &'static str,
    pub label: &'static str,
    pub package_name: &'static str,
    pub backend: &'static str,
    pub executable: &'static str,
    pub version_arguments: &'static [&'static str],
    pub latest_version_arguments: &'static [&'static str],
    pub update_arguments: &'static [&'static str],
    pub update_kind: UpdateKind,
}

pub static CLI_PACKAGES: [CliPackage; 3] = [
    CliPackage {
        id: "claude",
        label: "Claude Code",
        package_name: "@anthropic-ai/claude-code",
        backend: "claude",
        executable: "claude",
        version_arguments: &["--version"],
        latest_version_arguments: &[],
        update_arguments: &[],
        update_kind: UpdateKind::Npm,
    },
    CliPackage {
        id: "codex",
        label: "Codex",
        package_name: "@openai/codex",
        backend: "codex",
        executable: "codex",
        version_arguments: &["--version"],
        latest_version_arguments: &[],
        update_arguments: &[],
        update_kind: UpdateKind::Npm,
    },
    CliPackage {
        id: "antigravity",
        label: "Antigravity",
        package_name: "agy",
        backend: "antigravity",
        executable: "agy",
        version_arguments: &["--version"],
        latest_version_arguments: &["changelog"],
        update_arguments: &["update"],
        update_kind: UpdateKind::SelfUpdate,
    },
];

/// 조회는 네트워크를 타므로 화이트보드 주기보다 짧게 다시 묻지 않는다.
pub const UPDATE_CACHE_MILLISECONDS: f64 = 9.0 * 60.0 * 1_000.0;
pub const COMMAND_TIMEOUT: Duration = Duration::from_millis(20_000);
pub const INSTALL_TIMEOUT: Duration = Duration::from_millis(5 * 60 * 1_000);

/// 설치는 실제로 쓰는 실행 파일이 있는 곳에 해야 한다. 앱 번들 Node가 PATH 앞에 있으면 npm이 자기 자리를
/// 앱 안으로 판단해, 갱신이 번들 안으로 들어가고 정작 직원이 쓰는 CLI는 예전 버전으로 남는다.
pub fn npm_prefix_for_executable(executable_path: Option<&str>) -> Option<String> {
    let path = executable_path?;
    path.find("/lib/node_modules/").map(|index| path[..index].to_string())
}

static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?").expect("버전 정규식"));

/// `claude 2.1.220 (Claude Code)`나 `codex-cli 0.146.0`처럼 서로 다른 출력에서 버전만 뽑는다.
pub fn parse_installed_version(output: &str) -> Option<String> {
    VERSION.find(output).map(|found| found.as_str().to_string())
}

/// 숫자 구간으로 끊어 비교하고, 숫자가 같으면 사전 배포 꼬리표가 붙은 쪽을 낮게 본다.
/// 입력은 parse_installed_version 결과라 앞부분은 늘 숫자 세 구간이다.
pub fn is_update_available(installed: Option<&str>, latest: Option<&str>) -> bool {
    let (Some(installed), Some(latest)) = (installed, latest) else {
        return false;
    };
    if installed.is_empty() || latest.is_empty() || installed == latest {
        return false;
    }
    let parse = |value: &str| {
        let mut parts = value.splitn(2, ['-', '+']);
        let core = parts.next().unwrap_or("");
        // Number.parseInt(part, 10) || 0
        let numbers: Vec<f64> = core.split('.').map(|part| leading_integer(part).unwrap_or(0.0)).collect();
        (numbers, parts.next().is_some())
    };
    let (current, current_prerelease) = parse(installed);
    let (candidate, candidate_prerelease) = parse(latest);
    for index in 0..current.len().max(candidate.len()) {
        let left = current.get(index).copied().unwrap_or(0.0);
        let right = candidate.get(index).copied().unwrap_or(0.0);
        if left != right {
            return right > left;
        }
    }
    current_prerelease && !candidate_prerelease
}

/// parseInt(text, 10)의 앞자리 숫자. 숫자가 없거나 0이면 None(`|| 0`과 같은 결과)이다.
fn leading_integer(text: &str) -> Option<f64> {
    let trimmed = js_trim(text);
    let (sign, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (-1.0, &trimmed[1..]),
        Some(b'+') => (1.0, &trimmed[1..]),
        _ => (1.0, trimmed),
    };
    let end = digits.bytes().position(|byte| !byte.is_ascii_digit()).unwrap_or(digits.len());
    let value = digits[..end].parse::<f64>().ok()? * sign;
    (value != 0.0).then_some(value)
}

/// CLI 갱신 조회·설치가 밖에 기대는 일. Node runCommand(execFile)이며 작업 폴더는 늘 홈이다.
pub trait CliCommandRunner: Send + Sync + 'static {
    fn run<'a>(
        &'a self,
        command: &'a str,
        arguments: &'a [&'a str],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<ExecFileOutput, String>>;
}

/// 운영 실행기. execFile 기본 maxBuffer(1MiB)를 쓴다.
pub struct SystemCliCommandRunner;

impl CliCommandRunner for SystemCliCommandRunner {
    fn run<'a>(
        &'a self,
        command: &'a str,
        arguments: &'a [&'a str],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<ExecFileOutput, String>> {
        Box::pin(exec_file(command, arguments, None, timeout, DEFAULT_MAX_BUFFER))
    }
}

/// 심볼릭 링크를 따라가야 실제 설치 위치가 나온다. 어느 단계든 실패하면 모른다(None)로 본다.
async fn read_executable_path(entry: &CliPackage, runner: &dyn CliCommandRunner) -> Option<String> {
    let output = runner.run("which", &[entry.executable], COMMAND_TIMEOUT).await.ok()?;
    let path = js_trim(&output.stdout).split('\n').next().unwrap_or("").to_string();
    if path.is_empty() {
        return None;
    }
    let resolved = runner.run("readlink", &["-f", &path], COMMAND_TIMEOUT).await.ok()?;
    let resolved = js_trim(&resolved.stdout);
    Some(if resolved.is_empty() { path } else { resolved.to_string() })
}

async fn read_installed_version(entry: &CliPackage, runner: &dyn CliCommandRunner) -> Option<String> {
    let output = runner.run(entry.executable, entry.version_arguments, COMMAND_TIMEOUT).await.ok()?;
    parse_installed_version(&output.stdout)
}

async fn read_latest_version(entry: &CliPackage, runner: &dyn CliCommandRunner) -> Option<String> {
    let output = match entry.update_kind {
        UpdateKind::SelfUpdate => runner.run(entry.executable, entry.latest_version_arguments, COMMAND_TIMEOUT).await,
        UpdateKind::Npm => runner.run("npm", &["view", entry.package_name, "version"], COMMAND_TIMEOUT).await,
    }
    .ok()?;
    parse_installed_version(&output.stdout)
}

/// Node createCLIUpdateChecker. 마지막 조회 결과를 cache_duration 동안 돌려준다. 동시에 들어온 조회는
/// Node처럼 각자 다시 묻는다.
pub struct CliUpdateChecker {
    runner: Arc<dyn CliCommandRunner>,
    now: Arc<dyn Fn() -> f64 + Send + Sync>,
    cache_duration: f64,
    cached: Mutex<Option<(f64, Value)>>,
}

impl Default for CliUpdateChecker {
    fn default() -> Self {
        Self::new(Arc::new(SystemCliCommandRunner), Arc::new(now_milliseconds), UPDATE_CACHE_MILLISECONDS)
    }
}

impl CliUpdateChecker {
    pub fn new(runner: Arc<dyn CliCommandRunner>, now: Arc<dyn Fn() -> f64 + Send + Sync>, cache_duration: f64) -> Self {
        Self { runner, now, cache_duration, cached: Mutex::new(None) }
    }

    /// 갱신 설치도 같은 실행기로 돌린다. Node 운영에서는 둘 다 execFile이다.
    pub fn runner(&self) -> Arc<dyn CliCommandRunner> {
        Arc::clone(&self.runner)
    }

    pub async fn read(&self, force: bool) -> Value {
        if !force
            && let Some((checked_at, payload)) = &*self.cached.lock().unwrap_or_else(PoisonError::into_inner)
            && (self.now)() - checked_at < self.cache_duration
        {
            return payload.clone();
        }
        let runner = self.runner.as_ref();
        let packages = join_all(CLI_PACKAGES.iter().map(|entry| async move {
            let (installed, latest, executable_path) = futures_util::join!(
                read_installed_version(entry, runner),
                read_latest_version(entry, runner),
                read_executable_path(entry, runner),
            );
            json!({
                "id": entry.id,
                "label": entry.label,
                "packageName": entry.package_name,
                "installedVersion": installed,
                "latestVersion": latest,
                "installPrefix": npm_prefix_for_executable(executable_path.as_deref()),
                // 조회에 실패한 것과 최신인 것은 다르다. 둘 다 배지를 감추면 사용자는 구분할 수 없다.
                "checkFailed": installed.is_none() || latest.is_none(),
                "updateAvailable": is_update_available(installed.as_deref(), latest.as_deref()),
            })
        }))
        .await;
        let any = |key: &str| packages.iter().any(|entry| entry[key] == true);
        let payload = json!({
            "checkedAt": iso_string((self.now)()),
            "updateAvailable": any("updateAvailable"),
            "checkFailed": any("checkFailed"),
            "packages": packages,
        });
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = Some(((self.now)(), payload.clone()));
        payload
    }

    pub fn invalidate(&self) {
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// 갱신 실패. 업무 중이면 409, 모르는 CLI는 400, 그 밖(명령·DB 실패, 본문 null)은 500이다.
#[derive(Clone, Debug, PartialEq)]
pub enum CliUpdateError {
    Busy(String),
    UnknownPackage(String),
    Failure(String),
}

/// 하나만 고를 수 있어야 한다. 없거나 null·빈 문자열이면 전부다. 같은 문자열만 찾는다(Node ===).
pub fn packages_for_identifier(identifier: Option<&Value>) -> Result<Vec<&'static CliPackage>, CliUpdateError> {
    match identifier {
        None | Some(Value::Null) => return Ok(CLI_PACKAGES.iter().collect()),
        Some(Value::String(text)) if text.is_empty() => return Ok(CLI_PACKAGES.iter().collect()),
        _ => {}
    }
    let wanted = identifier.and_then(Value::as_str);
    CLI_PACKAGES
        .iter()
        .find(|entry| Some(entry.id) == wanted)
        .map(|entry| vec![entry])
        .ok_or_else(|| CliUpdateError::UnknownPackage(format!("알 수 없는 CLI입니다. {}", identifier.map(js_string).unwrap_or_default())))
}

pub fn package_names_for_identifier(identifier: Option<&Value>) -> Result<Vec<&'static str>, CliUpdateError> {
    Ok(packages_for_identifier(identifier)?
        .into_iter()
        .filter(|entry| entry.update_kind == UpdateKind::Npm)
        .map(|entry| entry.package_name)
        .collect())
}

/// 갱신을 막아야 하는 것은 그 CLI를 쓰는 직원뿐이다.
pub fn backends_for_identifier(identifier: Option<&Value>) -> Result<Vec<&'static str>, CliUpdateError> {
    Ok(packages_for_identifier(identifier)?.into_iter().map(|entry| entry.backend).collect())
}

/// 대상 npm CLI들이 같은 곳에 설치돼 있을 때만 그 위치를 쓴다. 서로 다르면 npm 기본 판단에 맡긴다.
pub fn shared_install_prefix(status: &Value, identifier: Option<&Value>) -> Result<Option<String>, CliUpdateError> {
    let wanted: Vec<&str> = packages_for_identifier(identifier)?
        .into_iter()
        .filter(|entry| entry.update_kind == UpdateKind::Npm)
        .map(|entry| entry.id)
        .collect();
    let mut prefixes: Vec<&str> = Vec::new();
    for entry in status.get("packages").and_then(Value::as_array).into_iter().flatten() {
        let id = entry.get("id").and_then(Value::as_str);
        if let Some(prefix) = entry.get("installPrefix").and_then(Value::as_str)
            && !prefix.is_empty()
            && id.is_some_and(|id| wanted.contains(&id))
            && !prefixes.contains(&prefix)
        {
            prefixes.push(prefix);
        }
    }
    Ok((prefixes.len() == 1).then(|| prefixes[0].to_string()))
}

/// Node applyCLIUpdates(server.mjs가 부르는 형태). 업무 확인이 먼저이고, npm 패키지를 한 번에 설치한 뒤
/// 자체 갱신 CLI를 차례로 돌린다. 명령이 실패하면 그 오류로 멈춘다.
/// 업무 확인은 Future를 돌려주는 클로저로 받는다. async 클로저로 받으면 axum 처리기의 Send 추론이 막힌다.
pub async fn apply_cli_updates<Running>(
    runner: &dyn CliCommandRunner,
    packages: &[&CliPackage],
    backends: Vec<&'static str>,
    prefix: Option<&str>,
    has_running_work: impl FnOnce(Vec<&'static str>) -> Running,
) -> Result<Value, CliUpdateError>
where
    Running: Future<Output = Result<bool, String>>,
{
    if has_running_work(backends).await.map_err(CliUpdateError::Failure)? {
        return Err(CliUpdateError::Busy("진행 중인 업무가 끝난 뒤에 업데이트할 수 있습니다.".into()));
    }
    let output_text = |output: ExecFileOutput| {
        let stdout = js_trim(&output.stdout);
        if stdout.is_empty() { js_trim(&output.stderr).to_string() } else { stdout.to_string() }
    };
    let mut outputs = Vec::new();
    let npm_packages: Vec<&str> =
        packages.iter().filter(|entry| entry.update_kind == UpdateKind::Npm).map(|entry| entry.package_name).collect();
    if !npm_packages.is_empty() {
        let mut arguments = vec!["install", "-g"];
        if let Some(prefix) = prefix.filter(|prefix| !prefix.is_empty()) {
            arguments.extend(["--prefix", prefix]);
        }
        arguments.extend(npm_packages);
        outputs.push(output_text(runner.run("npm", &arguments, INSTALL_TIMEOUT).await.map_err(CliUpdateError::Failure)?));
    }
    for entry in packages.iter().filter(|entry| entry.update_kind == UpdateKind::SelfUpdate) {
        let output = runner.run(entry.executable, entry.update_arguments, INSTALL_TIMEOUT).await;
        outputs.push(output_text(output.map_err(CliUpdateError::Failure)?));
    }
    outputs.retain(|output| !output.is_empty());
    Ok(json!({ "ok": true, "output": outputs.join("\n") }))
}

/// server.mjs hasRunningWork. 대기·실행 중인 대화 중 그 CLI를 쓰는 직원 것이 있는지 본다.
pub async fn has_running_work(pool: &PgPool, backends: &[&str]) -> Result<bool, String> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM turns AS turn \
         JOIN cli_sessions AS session ON session.id = turn.cli_session_id \
         JOIN characters AS character ON character.id = session.character_id \
         WHERE turn.status IN ('pending', 'running') \
           AND ($1::text[] IS NULL OR character.backend = ANY($1)) \
         LIMIT 1",
    )
    .bind(backends)
    .fetch_optional(pool)
    .await
    .map_err(|error| crate::rag::database_message(&error))?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use std::future::ready;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn 서로_다른_출력_형식에서_설치_버전만_뽑는다() {
        assert_eq!(parse_installed_version("2.1.220 (Claude Code)").as_deref(), Some("2.1.220"));
        assert_eq!(parse_installed_version("codex-cli 0.146.0").as_deref(), Some("0.146.0"));
        assert_eq!(parse_installed_version("Antigravity CLI 1.1.22").as_deref(), Some("1.1.22"));
        assert_eq!(parse_installed_version("codex-cli 0.148.0-alpha.9").as_deref(), Some("0.148.0-alpha.9"));
        assert_eq!(parse_installed_version("버전 없음"), None);
        assert_eq!(parse_installed_version(""), None);
    }

    #[test]
    fn 버전_비교는_사전순이_아니라_숫자_구간으로_한다() {
        assert!(is_update_available(Some("2.1.9"), Some("2.1.10")));
        assert!(!is_update_available(Some("2.1.10"), Some("2.1.9")));
        assert!(is_update_available(Some("2.1.220"), Some("2.1.233")));
        assert!(!is_update_available(Some("2.1.233"), Some("2.1.233")));
        assert!(is_update_available(Some("0.146.0"), Some("0.147.0")));
        assert!(is_update_available(Some("1.9.0"), Some("2.0.0")));
        assert!(!is_update_available(None, Some("2.0.0")));
        assert!(!is_update_available(Some("2.0.0"), None));
        // 사전 배포본은 같은 숫자의 정식 배포본보다 낮게 본다.
        assert!(is_update_available(Some("0.148.0-alpha.9"), Some("0.148.0")));
        assert!(!is_update_available(Some("0.148.0"), Some("0.148.0-alpha.9")));
    }

    /// 명령별 정해 둔 출력. 받은 명령과 인자를 차례로 남긴다.
    struct FakeRunner {
        reply: Box<dyn Fn(&str, &[&str]) -> Result<ExecFileOutput, String> + Send + Sync>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(reply: impl Fn(&str, &[&str]) -> Result<ExecFileOutput, String> + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self { reply: Box::new(reply), calls: Mutex::default() })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CliCommandRunner for FakeRunner {
        fn run<'a>(
            &'a self,
            command: &'a str,
            arguments: &'a [&'a str],
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<ExecFileOutput, String>> {
            let call = std::iter::once(command).chain(arguments.iter().copied()).map(str::to_string).collect();
            self.calls.lock().unwrap().push(call);
            let reply = (self.reply)(command, arguments);
            Box::pin(async move { reply })
        }
    }

    fn stdout(text: &str) -> Result<ExecFileOutput, String> {
        Ok(ExecFileOutput { stdout: text.into(), stderr: String::new() })
    }

    #[tokio::test]
    async fn 조회_결과는_캐시하고_강제_조회는_다시_묻는다() {
        let runner = FakeRunner::new(|command, _| stdout(if command == "npm" { "2.1.233\n" } else { "2.1.220 (Claude Code)\n" }));
        let clock = Arc::new(AtomicUsize::new(0));
        let now = Arc::clone(&clock);
        let checker = CliUpdateChecker::new(runner.clone(), Arc::new(move || now.load(Ordering::SeqCst) as f64), 1_000.0);

        let first = checker.read(false).await;
        assert_eq!(first["updateAvailable"], true);
        assert_eq!(first["packages"].as_array().unwrap().len(), 3);
        let after_first = runner.calls().len();
        checker.read(false).await;
        assert_eq!(runner.calls().len(), after_first, "캐시 동안에는 다시 묻지 않는다.");
        checker.read(true).await;
        assert!(runner.calls().len() > after_first, "강제 조회는 다시 물어야 한다.");
        clock.fetch_add(2_000, Ordering::SeqCst);
        let before_expiry = runner.calls().len();
        checker.read(false).await;
        assert!(runner.calls().len() > before_expiry, "캐시가 만료되면 다시 물어야 한다.");
    }

    #[tokio::test]
    async fn 버전_조회가_실패해도_갱신_여부만_모른_채_응답한다() {
        let runner = FakeRunner::new(|_, _| Err("명령을 찾을 수 없습니다".into()));
        let status = CliUpdateChecker::new(runner, Arc::new(|| 0.0), UPDATE_CACHE_MILLISECONDS).read(false).await;
        assert_eq!(status["updateAvailable"], false);
        for entry in status["packages"].as_array().unwrap() {
            assert_eq!((&entry["installedVersion"], &entry["latestVersion"]), (&Value::Null, &Value::Null));
            assert_eq!(entry["updateAvailable"], false);
        }
    }

    #[tokio::test]
    async fn 조회_실패는_최신_상태와_구분하고_정상_조회는_실패로_표시하지_않는다() {
        let runner = FakeRunner::new(|command, _| {
            if command == "npm" { Err("spawn npm ENOENT".into()) } else { stdout("2.1.233 (Claude Code)\n") }
        });
        let status = CliUpdateChecker::new(runner, Arc::new(|| 0.0), UPDATE_CACHE_MILLISECONDS).read(false).await;
        assert_eq!((&status["checkFailed"], &status["updateAvailable"]), (&json!(true), &json!(false)));
        for entry in status["packages"].as_array().unwrap() {
            assert_eq!(entry["checkFailed"], entry["id"] != "antigravity", "{entry}");
        }

        let runner = FakeRunner::new(|command, _| stdout(if command == "npm" { "2.1.233\n" } else { "2.1.220 (Claude Code)\n" }));
        let status = CliUpdateChecker::new(runner, Arc::new(|| 0.0), UPDATE_CACHE_MILLISECONDS).read(false).await;
        assert_eq!((&status["checkFailed"], &status["updateAvailable"]), (&json!(false), &json!(true)));
    }

    #[tokio::test]
    async fn 조회는_실제_설치_위치와_시각을_남긴다() {
        let runner = FakeRunner::new(|command, arguments| match (command, arguments) {
            ("which", ["claude"]) => stdout("/Users/u/.nvm/versions/node/v24/bin/claude\n/other\n"),
            ("readlink", ["-f", path]) if path.ends_with("bin/claude") => {
                stdout("/Users/u/.nvm/versions/node/v24/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe\n")
            }
            ("which", ["codex"]) => stdout("/opt/homebrew/bin/codex\n"),
            ("readlink", _) => stdout(" \n"),
            ("which", _) => stdout(""),
            _ => stdout("1.0.0\n"),
        });
        let status = CliUpdateChecker::new(runner.clone(), Arc::new(|| 1_700_000_000_123.0), UPDATE_CACHE_MILLISECONDS)
            .read(false)
            .await;
        assert_eq!(status["checkedAt"], "2023-11-14T22:13:20.123Z");
        let keys: Vec<&String> = status["packages"][0].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            ["id", "label", "packageName", "installedVersion", "latestVersion", "installPrefix", "checkFailed", "updateAvailable"]
        );
        let prefixes: Vec<&Value> = status["packages"].as_array().unwrap().iter().map(|entry| &entry["installPrefix"]).collect();
        assert_eq!(prefixes, [&json!("/Users/u/.nvm/versions/node/v24"), &Value::Null, &Value::Null]);
        let calls = runner.calls();
        assert!(calls.contains(&vec!["npm".into(), "view".into(), "@openai/codex".into(), "version".into()]));
        assert!(calls.contains(&vec!["agy".into(), "changelog".into()]));
        // readlink가 빈 출력이면 which 경로를 쓴다.
        assert!(calls.contains(&vec!["readlink".into(), "-f".into(), "/opt/homebrew/bin/codex".into()]));
    }

    #[tokio::test]
    async fn 진행_중_업무가_있으면_갱신을_거부한다() {
        let runner = FakeRunner::new(|_, _| stdout(""));
        let all = packages_for_identifier(None).unwrap();
        let result = apply_cli_updates(runner.as_ref(), &all, backends_for_identifier(None).unwrap(), None, |_| ready(Ok(true))).await;
        assert_eq!(result, Err(CliUpdateError::Busy("진행 중인 업무가 끝난 뒤에 업데이트할 수 있습니다.".into())));
        assert!(runner.calls().is_empty(), "세션이 쓰는 실행 파일을 도중에 갈아 끼우면 안 됩니다.");
    }

    #[tokio::test]
    async fn 업무가_없으면_npm_두_패키지와_antigravity_자체_업데이트를_실행한다() {
        let runner = FakeRunner::new(|_, _| stdout("updated 2 packages\n"));
        let all = packages_for_identifier(None).unwrap();
        let result = apply_cli_updates(runner.as_ref(), &all, backends_for_identifier(None).unwrap(), None, |_| ready(Ok(false))).await;
        assert_eq!(result, Ok(json!({ "ok": true, "output": "updated 2 packages\nupdated 2 packages" })));
        assert_eq!(
            runner.calls(),
            [vec!["npm", "install", "-g", "@anthropic-ai/claude-code", "@openai/codex"], vec!["agy", "update"]]
        );
    }

    #[test]
    fn 업데이트는_cli별로_따로_고를_수_있다() {
        let id = |text: &str| Value::from(text);
        assert_eq!(package_names_for_identifier(Some(&id("claude"))), Ok(vec!["@anthropic-ai/claude-code"]));
        assert_eq!(package_names_for_identifier(Some(&id("codex"))), Ok(vec!["@openai/codex"]));
        assert_eq!(package_names_for_identifier(Some(&id("antigravity"))), Ok(vec![]));
        assert_eq!(packages_for_identifier(Some(&id("antigravity"))).unwrap()[0].update_kind, UpdateKind::SelfUpdate);
        assert_eq!(package_names_for_identifier(None), Ok(vec!["@anthropic-ai/claude-code", "@openai/codex"]));
        assert_eq!(package_names_for_identifier(Some(&id(""))), Ok(vec!["@anthropic-ai/claude-code", "@openai/codex"]));
        assert_eq!(
            package_names_for_identifier(Some(&id("gemini"))),
            Err(CliUpdateError::UnknownPackage("알 수 없는 CLI입니다. gemini".into()))
        );
        // 문자열이 아닌 값은 String()으로 문구에 들어간다.
        for (value, text) in [(json!(5), "5"), (json!({}), "[object Object]"), (json!(["codex"]), "codex"), (json!(false), "false")] {
            assert_eq!(
                packages_for_identifier(Some(&value)),
                Err(CliUpdateError::UnknownPackage(format!("알 수 없는 CLI입니다. {text}")))
            );
        }
    }

    #[tokio::test]
    async fn 한쪽만_고르면_그_패키지만_설치하고_위치를_알면_명시한다() {
        let runner = FakeRunner::new(|_, _| stdout(""));
        let codex = packages_for_identifier(Some(&json!("codex"))).unwrap();
        let result = apply_cli_updates(runner.as_ref(), &codex, vec!["codex"], None, |_| ready(Ok(false))).await;
        assert_eq!(result, Ok(json!({ "ok": true, "output": "" })));
        let prefix = Some("/Users/neo/.nvm/versions/node/v24.14.0");
        apply_cli_updates(runner.as_ref(), &codex, vec!["codex"], prefix, |_| ready(Ok(false))).await.unwrap();
        assert_eq!(
            runner.calls(),
            [vec!["npm", "install", "-g", "@openai/codex"], vec!["npm", "install", "-g", "--prefix", prefix.unwrap(), "@openai/codex"]]
        );
    }

    #[tokio::test]
    async fn antigravity는_agy_자체_업데이트_명령만_사용한다() {
        let runner = FakeRunner::new(|_, _| Ok(ExecFileOutput { stdout: " \n".into(), stderr: " Already on the latest version.\n".into() }));
        let antigravity = packages_for_identifier(Some(&json!("antigravity"))).unwrap();
        let result = apply_cli_updates(runner.as_ref(), &antigravity, vec!["antigravity"], None, |_| ready(Ok(false))).await;
        // 표준 출력이 비면 표준 오류를 남긴다.
        assert_eq!(result, Ok(json!({ "ok": true, "output": "Already on the latest version." })));
        assert_eq!(runner.calls(), [vec!["agy", "update"]]);
    }

    #[tokio::test]
    async fn 갱신을_막는_판정은_그_cli를_쓰는_직원만_본다() {
        assert_eq!(backends_for_identifier(Some(&json!("claude"))), Ok(vec!["claude"]));
        assert_eq!(backends_for_identifier(Some(&json!("codex"))), Ok(vec!["codex"]));
        assert_eq!(backends_for_identifier(Some(&json!("antigravity"))), Ok(vec!["antigravity"]));
        assert_eq!(backends_for_identifier(None), Ok(vec!["claude", "codex", "antigravity"]));

        let runner = FakeRunner::new(|_, _| stdout(""));
        let asked = Mutex::new(Vec::new());
        let running = |backends: Vec<&'static str>| {
            asked.lock().unwrap().push(backends.clone());
            backends.contains(&"claude")
        };
        let codex = packages_for_identifier(Some(&json!("codex"))).unwrap();
        let result = apply_cli_updates(runner.as_ref(), &codex, vec!["codex"], None, |backends| ready(Ok(running(backends)))).await;
        assert!(result.is_ok());
        assert_eq!(*asked.lock().unwrap(), [vec!["codex"]]);
        let claude = packages_for_identifier(Some(&json!("claude"))).unwrap();
        let result = apply_cli_updates(runner.as_ref(), &claude, vec!["claude"], None, |backends| ready(Ok(running(backends)))).await;
        assert!(matches!(result, Err(CliUpdateError::Busy(_))));
    }

    #[tokio::test]
    async fn 설치_명령이_실패하면_그_오류로_멈춘다() {
        let runner = FakeRunner::new(|command, _| {
            if command == "npm" { Err("Command failed: npm install -g @openai/codex\nE404\n".into()) } else { stdout("ok") }
        });
        let all = packages_for_identifier(None).unwrap();
        let result = apply_cli_updates(runner.as_ref(), &all, vec![], None, |_| ready(Ok(false))).await;
        assert_eq!(result, Err(CliUpdateError::Failure("Command failed: npm install -g @openai/codex\nE404\n".into())));
        assert_eq!(runner.calls().len(), 1);
        let result = apply_cli_updates(runner.as_ref(), &all, vec![], None, |_| ready(Err("DB 오류".into()))).await;
        assert_eq!(result, Err(CliUpdateError::Failure("DB 오류".into())));
    }

    #[test]
    fn 실행_파일_경로에서_설치_위치를_뽑고_같은_곳일_때만_쓴다() {
        assert_eq!(
            npm_prefix_for_executable(Some(
                "/Users/neo/.nvm/versions/node/v24.14.0/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe"
            ))
            .as_deref(),
            Some("/Users/neo/.nvm/versions/node/v24.14.0")
        );
        assert_eq!(npm_prefix_for_executable(Some("/opt/homebrew/bin/codex")), None);
        assert_eq!(npm_prefix_for_executable(None), None);
        assert_eq!(npm_prefix_for_executable(Some("/lib/node_modules/x")).as_deref(), Some(""));

        let status = json!({ "packages": [{ "id": "claude", "installPrefix": "/nvm/v24" }, { "id": "codex", "installPrefix": "/nvm/v24" }] });
        assert_eq!(shared_install_prefix(&status, Some(&json!("claude"))), Ok(Some("/nvm/v24".into())));
        assert_eq!(shared_install_prefix(&status, None), Ok(Some("/nvm/v24".into())));
        let mixed = json!({ "packages": [{ "id": "claude", "installPrefix": "/nvm/v24" }, { "id": "codex", "installPrefix": "/opt/homebrew" }] });
        assert_eq!(shared_install_prefix(&mixed, None), Ok(None));
        assert_eq!(shared_install_prefix(&mixed, Some(&json!("codex"))), Ok(Some("/opt/homebrew".into())));
        assert_eq!(shared_install_prefix(&json!({ "packages": [] }), None), Ok(None));
        assert_eq!(shared_install_prefix(&mixed, Some(&json!("antigravity"))), Ok(None));
    }
}
