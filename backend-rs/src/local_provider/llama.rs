// 4090 llama.cpp 직접 실행 호스트. Node local-llama-host.mjs의 고정 실행 인자·PowerShell 스크립트·시작 흐름·유휴 확인을 옮겼다.
// 같은 경로·명령으로 띄운 OFFICESTRA 프로세스만 정리하고, 다른 프로그램은 건드리지 않는다.

use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::Value;

use super::host::{
    HostResource, ModelHost, ResourceSample, StartOptions, Tunnel, WindowsHost, gpu_occupied, local_host_memory_exceeded,
};
use super::{AbortSignal, LocalError};
use crate::js::{number_to_string, parse_js_json};
use crate::local_agent_profile::LocalAgentProfile;
use crate::local_model_capabilities::{
    DIRECT_RUNTIME, MEROMERO_26B_MODEL_ID, is_mero_mero, local_model_supports_vision, mero_runtime_artifacts,
};

/// 따로 설치한 고정 실행 파일. LM Studio 파일을 대신 쓰지 않는다.
pub const LLAMA_SERVER_PATH: &str = r"G:\llm\officestra-probes\llama-b10982-cuda124\bin\llama-server.exe";
pub const LLAMA_SERVER_SHA256: &str = "FFAEE576AD271EDE87B92A7D8C3863DC8F331BBAC666EE00099C250E8809E743";
pub const LLAMA_MODEL_ID: &str = "officestra-qwen38-27b-uncensored-q4km";
pub const LLAMA_MODEL_ROOT: &str = r"G:\llm\jonathancoletti\Qwen3.8-27B-Uncensored-GGUF";
pub const LLAMA_MODEL_FILE: &str = "Qwen3.8-27B-Uncensored-Q4_K_M.gguf";
pub const LLAMA_MODEL_SIZE: u64 = 16_810_714_528;
pub const LLAMA_MODEL_SHA256: &str = "4C5E2DB039E9325AC7724C8846C71356A24AD1CDFA28002D73ECB6BE645F9675";
pub const LLAMA_MMPROJ_FILE: &str = "mmproj-Qwen3.8-27B-Uncensored-F16.gguf";
pub const LLAMA_MMPROJ_SIZE: u64 = 927_606_912;
pub const LLAMA_MMPROJ_SHA256: &str = "5AC423F8A29059DC24E51BC6A43E9380DCD57A9347F28B62591E0B3F60B7081C";
pub const LLAMA_CLEANUP_REMOTE_TIMEOUT: Duration = Duration::from_millis(45_000);
pub const LLAMA_ORPHAN_CLEANUP_REMOTE_TIMEOUT: Duration = Duration::from_millis(60_000);
const QWEN_HOST_KEY: &str = "qwen3.8-27b";

/// PowerShell 작은따옴표 문자열.
fn ps(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn artifact_path(file: &str) -> String {
    format!(r"{LLAMA_MODEL_ROOT}\{file}")
}

/// WindowsLlamaCppHost 생성자가 요구하는 호스트 모델 키.
pub fn expected_model_key(profile: &LocalAgentProfile) -> &'static str {
    match mero_runtime_artifacts(profile) {
        Ok(artifacts) if is_mero_mero(profile) => artifacts.host_key,
        _ => QWEN_HOST_KEY,
    }
}

/// llamaArtifactVerificationScript. 설치·점검 도구용 전체 해시 확인이다. 프로필이 없으면 Qwen 파일을 본다.
pub fn llama_artifact_verification_script(profile: Option<&LocalAgentProfile>) -> String {
    if let Some(profile) = profile.filter(|profile| is_mero_mero(*profile))
        && let Ok(artifacts) = mero_runtime_artifacts(profile)
    {
        return [Some(artifacts.model), artifacts.vision]
            .into_iter()
            .flatten()
            .map(|artifact| {
                let path = ps(&format!(r"{}\{}", artifact.root, artifact.file));
                format!(
                    "if(!(Test-Path -LiteralPath {path})){{throw 'Pinned MeroMero artifact is not installed'}};$f=Get-Item -LiteralPath {path};if($f.Length -ne {}){{throw 'Pinned artifact size changed; revalidation required'}};if((Get-FileHash -LiteralPath {path} -Algorithm SHA256).Hash -ne '{}'){{throw 'Pinned artifact changed; revalidation required'}}",
                    artifact.size, artifact.sha256
                )
            })
            .collect::<Vec<_>>()
            .join(";");
    }
    let check = |file: &str, size: u64, sha: &str, label: &str| {
        let path = ps(&artifact_path(file));
        format!(
            "if(!(Test-Path -LiteralPath {path})){{throw '{label} is not installed'}};$f=Get-Item -LiteralPath {path};if($f.Length -ne {size}){{throw '{label} size changed; revalidation required'}};if((Get-FileHash -LiteralPath {path} -Algorithm SHA256).Hash -ne '{sha}'){{throw '{label} changed; revalidation required'}}"
        )
    };
    [check(LLAMA_MODEL_FILE, LLAMA_MODEL_SIZE, LLAMA_MODEL_SHA256, "Pinned model"), check(LLAMA_MMPROJ_FILE, LLAMA_MMPROJ_SIZE, LLAMA_MMPROJ_SHA256, "Pinned mmproj")].join(";")
}

/// llamaStartupArtifactScript. 평소 시작은 18GB 전체를 읽지 않고 크기만 본다.
pub fn llama_startup_artifact_script(profile: &LocalAgentProfile) -> String {
    let files: Vec<(String, u64)> = match mero_runtime_artifacts(profile) {
        Ok(artifacts) if is_mero_mero(profile) => [Some(artifacts.model), artifacts.vision]
            .into_iter()
            .flatten()
            .map(|artifact| (format!(r"{}\{}", artifact.root, artifact.file), artifact.size))
            .collect(),
        _ => vec![(artifact_path(LLAMA_MODEL_FILE), LLAMA_MODEL_SIZE), (artifact_path(LLAMA_MMPROJ_FILE), LLAMA_MMPROJ_SIZE)],
    };
    let checks = files
        .iter()
        .map(|(path, size)| format!("$f=Get-Item -LiteralPath {} -ErrorAction Stop;if($f.Length -ne {size}){{throw 'Model file size mismatch'}}", ps(path)))
        .collect::<Vec<_>>()
        .join(";");
    format!("if(!(Test-Path -LiteralPath {})){{throw 'llama.cpp runtime is not installed'}};{checks}", ps(LLAMA_SERVER_PATH))
}

/// llamaServerArguments. 추론 선택은 요청마다 정하므로 서버 인자에 넣지 않는다.
pub fn llama_server_arguments(profile: &LocalAgentProfile) -> Result<Vec<String>, String> {
    let strings = |items: &[&str]| items.iter().map(|item| item.to_string()).collect::<Vec<_>>();
    let supported_backend = profile.backend == "codex" || profile.backend == "claude";
    let quantized = profile.kv_cache_quantization.as_deref() == Some("q8_0");
    if is_mero_mero(profile) {
        if !supported_backend || ![32768, 65536].contains(&profile.context_window) || !quantized {
            return Err("Unsupported MeroMero runtime configuration".into());
        }
        let artifacts = mero_runtime_artifacts(profile)?;
        let moe = profile.model == MEROMERO_26B_MODEL_ID;
        let mut arguments = vec!["-m".to_string(), format!(r"{}\{}", artifacts.model.root, artifacts.model.file)];
        // 26B는 GPU 비전만 쓴다. CPU 투영기 발열 사고 뒤 CPU 조율을 줄였다. 31B는 텍스트 전용이라 투영기가 없다.
        if let Some(vision) = artifacts.vision {
            arguments.extend(["--mmproj".to_string(), format!(r"{}\{}", vision.root, vision.file)]);
            arguments.extend(strings(&["--mmproj-offload", "-t", "4", "-tb", "4", "--poll", "0", "--poll-batch", "0"]));
        }
        arguments.extend(["-c".to_string(), profile.context_window.to_string()]);
        arguments.extend(strings(&["-ctk", "q8_0", "-ctv", "q8_0", "-ngl", "all", "-fa", "on", "-np", "1", "-b"]));
        // Gemma 이미지 조각은 비인과 주의를 써서 마이크로 배치가 디코드 배치 전체를 담아야 한다.
        arguments.extend(strings(&[if moe { "1024" } else { "512" }, "-ub", if moe { "1024" } else { "128" }]));
        arguments.extend(strings(&["--jinja", "--host", "127.0.0.1", "--port", "18181", "--alias"]));
        arguments.push(profile.model.clone());
        arguments.extend(strings(&["--load-mode", "none", "--fit", "off"]));
        return Ok(arguments);
    }
    if profile.runtime.as_deref() != Some(DIRECT_RUNTIME) || !supported_backend || profile.context_window != 65536 || !quantized || profile.model != LLAMA_MODEL_ID {
        return Err("Unsupported direct runtime configuration".into());
    }
    // 같은 64K·KV8·비전 프로필에서 측정했다. 기본 MTP 머리가 추론 강도를 낮추지 않고 생성을 빠르게 한다.
    let mut arguments = vec!["-m".to_string(), artifact_path(LLAMA_MODEL_FILE), "--mmproj".to_string(), artifact_path(LLAMA_MMPROJ_FILE)];
    arguments.extend(strings(&["-c", "65536", "-ctk", "q8_0", "-ctv", "q8_0", "-ngl", "all", "-fa", "on", "-np", "1", "-b", "128", "-ub", "128"]));
    arguments.extend(strings(&["--jinja", "--host", "127.0.0.1", "--port", "18181", "--alias"]));
    arguments.push(profile.model.clone());
    arguments.extend(strings(&["--load-mode", "none", "--fit", "off", "--spec-type", "draft-mtp", "--spec-draft-n-max", "2"]));
    Ok(arguments)
}

pub fn llama_server_command(profile: &LocalAgentProfile) -> Result<String, String> {
    let arguments = llama_server_arguments(profile)?;
    Ok(std::iter::once(LLAMA_SERVER_PATH.to_string()).chain(arguments).map(|item| format!("\"{item}\"")).collect::<Vec<_>>().join(" "))
}

static CREATED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{4}-[0-9][0-9]-[0-9][0-9]T[0-9:.]+Z$").unwrap());

/// validLlamaOwnership.
pub fn valid_llama_ownership(record: &Value) -> bool {
    let pid = record.get("pid").and_then(Value::as_f64);
    pid.is_some_and(|pid| pid.fract() == 0.0 && pid.abs() <= 9_007_199_254_740_991.0 && pid > 0.0)
        && record.get("path").and_then(Value::as_str) == Some(LLAMA_SERVER_PATH)
        && record.get("created").and_then(Value::as_str).is_some_and(|created| CREATED.is_match(created))
}

/// llamaCleanupScript. Stop-Process가 메모리를 돌려주기 전에 끝날 수 있어, 그 프로세스가 사라질 때까지 기록을 남긴다.
pub fn llama_cleanup_script(record: &Value) -> Result<String, String> {
    if !valid_llama_ownership(record) {
        return Err("Invalid direct server ownership record".into());
    }
    let pid = number_to_string(record["pid"].as_f64().unwrap_or_default());
    let path = ps(record["path"].as_str().unwrap_or_default());
    let created = ps(record["created"].as_str().unwrap_or_default());
    Ok(format!(
        "$ErrorActionPreference='Stop';$process=Get-Process -Id {pid} -ErrorAction SilentlyContinue;if($process){{$identity=Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\";if($identity){{if($identity.ExecutablePath -ne {path} -or $identity.CreationDate.ToUniversalTime().ToString('o') -ne {created}){{throw 'Owned server identity changed'}};Stop-Process -Id {pid} -Force;$deadline=(Get-Date).AddSeconds(15);do{{Start-Sleep -Milliseconds 100;$process=Get-Process -Id {pid} -ErrorAction SilentlyContinue}}while($process -and (Get-Date) -lt $deadline);if($process){{throw 'Owned server did not exit'}}}}}};'released'"
    ))
}

/// llamaOrphanCleanupScript. 취소가 PID를 받기 전에 SSH를 끊었을 때, 정확히 같은 경로·명령의 OFFICESTRA 프로세스만 거둔다.
pub fn llama_orphan_cleanup_script(profile: &LocalAgentProfile) -> Result<String, String> {
    let command = ps(&llama_server_command(profile)?);
    let path = ps(LLAMA_SERVER_PATH);
    Ok(format!(
        "$ErrorActionPreference='Stop';$owned=@();$candidates=@(Get-Process -Name 'llama-server' -ErrorAction SilentlyContinue);foreach($candidate in $candidates){{$p=Get-CimInstance Win32_Process -Filter \"ProcessId=$($candidate.Id)\";if($p -and $p.ExecutablePath -eq {path} -and $p.CommandLine -eq {command}){{$owned+=$p}}}};foreach($p in $owned){{$pidToStop=[int]$p.ProcessId;Stop-Process -Id $pidToStop -Force;$deadline=(Get-Date).AddSeconds(15);do{{Start-Sleep -Milliseconds 100;$current=Get-Process -Id $pidToStop -ErrorAction SilentlyContinue}}while($current -and (Get-Date) -lt $deadline);if($current){{throw 'Orphaned direct server did not exit'}}}};@{{released=$owned.Count}}|ConvertTo-Json -Compress"
    ))
}

pub fn llama_direct_port_guard_script() -> &'static str {
    r"$listener=netstat -ano -p TCP | Select-String -Pattern '^\s*TCP\s+\S+:18181\s+\S+\s+LISTENING\s+\d+\s*$' | Select-Object -First 1;if($listener){throw 'Direct port occupied'}"
}

/// waitForLlamaIdle의 본체. read는 남은 시간을 받아 /slots 응답(정상이 아니면 None)을 돌려준다.
pub(crate) async fn wait_for_idle_with<F, Fut>(mut read: F, alive: impl Fn() -> bool, timeout: Duration, poll: Duration) -> Result<(), LocalError>
where
    F: FnMut(Duration) -> Fut,
    Fut: Future<Output = Result<Option<Value>, LocalError>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut idle = 0;
    while tokio::time::Instant::now() < deadline {
        if !alive() {
            return Err(LocalError::plain("SSH tunnel disconnected"));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(slots) = read(remaining.clamp(Duration::from_millis(1), Duration::from_millis(1000))).await? else {
            return Err(LocalError::plain("Direct slot state unavailable"));
        };
        let processing = slots.as_array().filter(|slots| slots.len() == 1).and_then(|slots| slots[0].get("is_processing")).and_then(Value::as_bool);
        let Some(processing) = processing else {
            return Err(LocalError::plain("Direct slot state invalid"));
        };
        idle = if processing { 0 } else { idle + 1 };
        if idle == 2 {
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
    Err(LocalError::plain("Direct generation did not stop"))
}

async fn fetch_json(http: &reqwest::Client, url: String, timeout: Duration) -> Result<(bool, Value), LocalError> {
    let response = http.get(url).timeout(timeout).send().await.map_err(|error| LocalError::plain(error.to_string()))?;
    let ok = response.status().is_success();
    let bytes = response.bytes().await.map_err(|error| LocalError::plain(error.to_string()))?;
    let text = String::from_utf8_lossy(&bytes);
    let value = parse_js_json(&text).map_err(|error| LocalError::plain(format!("SyntaxError: {error}")))?;
    Ok((ok, value))
}

/// waitForLlamaIdle. HTTP 끊김은 취소 요청일 뿐이라, 한 생성 슬롯이 두 번 연속 비어야 모델을 남겨 둔다.
pub async fn wait_for_llama_idle(http: reqwest::Client, base: String, alive: impl Fn() -> bool) -> Result<(), LocalError> {
    wait_for_idle_with(
        |remaining| {
            let (http, url) = (http.clone(), format!("{base}/slots"));
            async move {
                let response = http.get(url).timeout(remaining).send().await.map_err(|error| LocalError::plain(error.to_string()))?;
                if !response.status().is_success() {
                    return Ok(None);
                }
                let bytes = response.bytes().await.map_err(|error| LocalError::plain(error.to_string()))?;
                parse_js_json(&String::from_utf8_lossy(&bytes)).map(Some).map_err(|error| LocalError::plain(format!("SyntaxError: {error}")))
            }
        },
        alive,
        Duration::from_millis(5000),
        Duration::from_millis(100),
    )
    .await
}

/// WindowsLlamaCppHost.cleanupOwned. PID 재사용이 다른 프로그램을 끌 권한이 되지 않게 경로·생성 시각을 함께 본다.
pub(super) async fn cleanup_owned(host: &WindowsHost, owned: &Value) -> Result<(), LocalError> {
    let script = llama_cleanup_script(owned).map_err(LocalError::plain)?;
    host.remote(script, LLAMA_CLEANUP_REMOTE_TIMEOUT, None).await?;
    host.forget_owned().await
}

struct MemoryGuard {
    stop: AbortSignal,
    task: tokio::task::JoinHandle<()>,
}

impl MemoryGuard {
    /// 2초마다 측정해 다른 작업·메모리 초과·측정 실패면 시작을 멈춘다. 측정 중에는 다음 측정을 건너뛴다.
    fn spawn(host: Arc<WindowsHost>, controller: AbortSignal) -> Self {
        let stop = AbortSignal::new();
        let watching = stop.clone();
        let task = tokio::spawn(async move {
            let period = Duration::from_millis(2000);
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = watching.aborted() => break,
                    _ = ticks.tick() => {}
                }
                match host.sample().await {
                    Ok(sample) if !sample.busy() && !local_host_memory_exceeded(&sample) => {}
                    _ => controller.abort(),
                }
            }
        });
        Self { stop, task }
    }

    /// clearInterval 뒤 진행 중인 측정이 끝나길 기다린다.
    async fn stop(self) {
        self.stop.abort();
        let _ = self.task.await;
    }
}

struct LlamaResource {
    host: Arc<WindowsHost>,
    upstream: String,
    tunnel: Arc<dyn Tunnel>,
}

impl HostResource for LlamaResource {
    fn upstream(&self) -> String {
        self.upstream.clone()
    }

    fn sample(&self) -> BoxFuture<'static, Result<ResourceSample, LocalError>> {
        let host = Arc::clone(&self.host);
        Box::pin(async move { host.sample().await })
    }

    fn alive(&self) -> bool {
        !self.tunnel.exited()
    }

    fn release(&self) -> BoxFuture<'static, Result<(), LocalError>> {
        let host = Arc::clone(&self.host);
        Box::pin(async move { host.release().await })
    }

    fn supports_wait_for_idle(&self) -> bool {
        true
    }

    fn wait_for_idle(&self) -> BoxFuture<'static, Result<(), LocalError>> {
        let (http, base, tunnel) = (self.host.http.clone(), self.upstream.clone(), Arc::clone(&self.tunnel));
        Box::pin(async move { wait_for_llama_idle(http, base, move || !tunnel.exited()).await })
    }

    fn reconnect_address(&self, address: &str) -> Result<(), LocalError> {
        self.host.reconnect(address)
    }
}

fn probe_port() -> Result<u16, LocalError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|error| LocalError::plain(error.to_string()))?;
    Ok(listener.local_addr().map_err(|error| LocalError::plain(error.to_string()))?.port())
}

fn parse_remote_json(text: &str) -> Result<Value, LocalError> {
    parse_js_json(text).map_err(|error| LocalError::plain(format!("SyntaxError: {error}")))
}

/// WindowsLlamaCppHost.start. 실패하면 release()로 소유 프로세스·터널·임대를 정리하고, 정리 오류가 있으면 그것을 알린다.
pub(super) async fn start(host: Arc<WindowsHost>, options: StartOptions) -> Result<Arc<dyn HostResource>, LocalError> {
    let controller = AbortSignal::new();
    let link = {
        let (caller, controller) = (options.signal.clone(), controller.clone());
        tokio::spawn(async move {
            caller.aborted().await;
            controller.abort();
        })
    };
    let mut guard = None;
    let result = start_steps(&host, options.comfy_release.as_deref(), &controller, &mut guard).await;
    if let Some(guard) = guard {
        guard.stop().await;
    }
    link.abort();
    match result {
        Ok(resource) => Ok(resource),
        Err(error) => {
            host.release().await?;
            Err(error)
        }
    }
}

async fn start_steps(
    host: &Arc<WindowsHost>,
    comfy_release: Option<&str>,
    controller: &AbortSignal,
    guard: &mut Option<MemoryGuard>,
) -> Result<Arc<dyn HostResource>, LocalError> {
    host.take_lease().await?;
    host.load_owned().await?;
    let owned = host.inner.lock().unwrap().owned.clone();
    if let Some(owned) = owned {
        cleanup_owned(host, &owned).await?;
    }
    if let Some(mode) = comfy_release {
        host.release_comfy_ui(mode, Some(controller.clone())).await?;
    }
    let (sample, studio_pid) = tokio::join!(host.sample(), host.server_pid());
    let (mut sample, studio_pid) = (sample?, studio_pid?);
    if !sample.busy() && sample.vram_pct() > 20.0 {
        host.remote(llama_orphan_cleanup_script(&host.profile)?, LLAMA_ORPHAN_CLEANUP_REMOTE_TIMEOUT, None).await?;
        sample = host.sample().await?;
    }
    if !host.gpu_available(&sample) {
        return Err(gpu_occupied(&sample));
    }
    if studio_pid != 0.0 && !studio_pid.is_nan() {
        let script = "Invoke-RestMethod 'http://127.0.0.1:1234/api/v1/models' -TimeoutSec 3 | ConvertTo-Json -Depth 8 -Compress".to_string();
        let models = parse_remote_json(&host.remote(script, super::host::DEFAULT_REMOTE_TIMEOUT, None).await?)?;
        let idle = models.get("models").and_then(Value::as_array).is_some_and(|models| {
            models.iter().all(|model| model.get("loaded_instances").and_then(Value::as_array).is_some_and(Vec::is_empty))
        });
        if !idle {
            return Err(LocalError::busy("LM Studio model is in use"));
        }
    }
    if controller.is_aborted() {
        return Err(LocalError::plain("Direct startup cancelled"));
    }
    let command = llama_server_command(&host.profile)?;
    let directory = &LLAMA_SERVER_PATH[..LLAMA_SERVER_PATH.rfind('\\').unwrap_or_default()];
    // WMI는 Windows OpenSSH가 분리된 자식을 기다리지 않게 한다. PID를 받기 전에는 중단 신호를 넘기지 않는다.
    let script = format!(
        "$ErrorActionPreference='Stop';$null=& {{{}}};{};{};$r=Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{CommandLine={};CurrentDirectory={}}};if($r.ReturnValue -ne 0){{throw 'Direct server launch failed'}};$p=Get-CimInstance Win32_Process -Filter \"ProcessId=$($r.ProcessId)\";@{{pid=[int]$r.ProcessId;path=$p.ExecutablePath;created=$p.CreationDate.ToUniversalTime().ToString('o')}} | ConvertTo-Json -Compress",
        llama_orphan_cleanup_script(&host.profile)?,
        llama_direct_port_guard_script(),
        llama_startup_artifact_script(&host.profile),
        ps(&command),
        ps(directory),
    );
    let owned = parse_remote_json(&host.remote(script, Duration::from_millis(60_000), None).await?)?;
    if !valid_llama_ownership(&owned) {
        return Err(LocalError::plain("Direct server identity unavailable"));
    }
    host.write_owned(&owned).await?;
    if controller.is_aborted() {
        return Err(LocalError::plain("Direct startup cancelled"));
    }
    let port = probe_port()?;
    let tunnel = host.shell.open_tunnel(&host.current_host(), port, 18181)?;
    host.inner.lock().unwrap().tunnel = Some(Arc::clone(&tunnel));
    *guard = Some(MemoryGuard::spawn(Arc::clone(host), controller.clone()));
    let base = format!("http://127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + Duration::from_millis(120_000);
    let mut ready = false;
    while !controller.is_aborted() && !tunnel.exited() && tokio::time::Instant::now() < deadline {
        ready = host.http.get(format!("{base}/health")).timeout(Duration::from_millis(1000)).send().await.is_ok_and(|response| response.status().is_success());
        if ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if !ready || controller.is_aborted() || tunnel.exited() {
        return Err(LocalError::plain("Direct server readiness or memory guard failed"));
    }
    let (_, props) = fetch_json(&host.http, format!("{base}/props"), Duration::from_millis(3000)).await?;
    let context = props.get("default_generation_settings").and_then(|settings| settings.get("n_ctx")).and_then(Value::as_f64);
    if context != Some(host.profile.context_window as f64) || props.get("total_slots").and_then(Value::as_f64) != Some(1.0) {
        return Err(LocalError::plain("Direct server context mismatch"));
    }
    let vision = props.get("modalities").and_then(|modalities| modalities.get("vision")) == Some(&Value::Bool(true));
    if vision != local_model_supports_vision(&host.profile) {
        return Err(LocalError::plain("Direct server vision capability mismatch"));
    }
    let last = host.sample().await?;
    if last.busy() || local_host_memory_exceeded(&last) || controller.is_aborted() {
        return Err(LocalError::plain("Direct server memory guard failed"));
    }
    Ok(Arc::new(LlamaResource { host: Arc::clone(host), upstream: base, tunnel }))
}

/// ModelHost가 dyn으로 쓰이도록 WindowsHost를 그대로 내보낸다.
pub fn as_model_host(host: Arc<WindowsHost>) -> Arc<dyn ModelHost> {
    host
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use serde_json::json;

    use super::super::host::{
        HostLease, HostLeases, HostTimings, LocalHost, RemoteShell, comfy_release_script, local_host_listener_pid_script,
        local_host_resource_sample_script,
    };
    use super::super::{LocalDefinition, normalize_local_definition};
    use super::*;

    fn definition() -> LocalDefinition {
        normalize_local_definition(&json!({
            "profile": { "id": "direct-test", "providerKind": "local", "backend": "codex", "runtime": DIRECT_RUNTIME, "model": LLAMA_MODEL_ID,
                "endpoint": "http://127.0.0.1:41235", "credentialEnv": "OFFICESTRA_LOCAL_TEST_TOKEN", "credentialVersion": "v1", "contextWindow": 65536,
                "maxOutputTokens": 4096, "usageProtocol": "openai-responses-v1", "kvCacheQuantization": "q8_0", "reasoning": "medium" },
            "host": { "address": "127.0.0.1", "user": "test", "sshPort": 2222, "keyPath": "/tmp/test-key", "hostKeyAlias": "test", "modelKey": "qwen3.8-27b", "comfyPort": 8188 },
        }))
        .unwrap()
    }

    fn with(profile: &LocalAgentProfile, change: impl FnOnce(&mut LocalAgentProfile)) -> LocalAgentProfile {
        let mut copy = profile.clone();
        change(&mut copy);
        copy
    }

    #[test]
    fn direct_server_pins_measured_arguments() {
        let profile = definition().profile;
        let arguments = llama_server_arguments(&profile).unwrap();
        let expected: Vec<String> = [
            "-m", &artifact_path(LLAMA_MODEL_FILE), "--mmproj", &artifact_path(LLAMA_MMPROJ_FILE), "-c", "65536", "-ctk", "q8_0", "-ctv", "q8_0", "-ngl", "all", "-fa", "on",
            "-np", "1", "-b", "128", "-ub", "128", "--jinja", "--host", "127.0.0.1", "--port", "18181", "--alias", LLAMA_MODEL_ID, "--load-mode", "none", "--fit", "off",
            "--spec-type", "draft-mtp", "--spec-draft-n-max", "2",
        ]
        .map(String::from)
        .into();
        assert_eq!(arguments, expected);
        for reasoning in ["default", "low", "medium", "xhigh"] {
            assert_eq!(llama_server_arguments(&with(&profile, |p| p.reasoning = Some(reasoning.into()))).unwrap(), arguments);
        }
        assert_eq!(llama_server_arguments(&with(&profile, |p| p.backend = "claude".into())).unwrap(), arguments);
        assert!(llama_server_arguments(&with(&profile, |p| p.kv_cache_quantization = Some("q4_0".into()))).is_err());
        assert!(llama_server_arguments(&with(&profile, |p| p.model = "officestra-qwen38-27b".into())).is_err());
        let command = llama_server_command(&profile).unwrap();
        assert!(command.contains("llama-server.exe"));
        assert!(command.contains(r#"--port" "18181"#));
        assert!(command.contains(r#""--spec-type" "draft-mtp" "--spec-draft-n-max" "2""#));
        let orphan = llama_orphan_cleanup_script(&profile).unwrap();
        assert!(orphan.contains(&command));
        for part in ["Get-Process -Name 'llama-server'", ".ExecutablePath -eq", ".CommandLine -eq", "Orphaned direct server did not exit", "released=$owned.Count"] {
            assert!(orphan.contains(part), "{part}");
        }
        let verification = llama_artifact_verification_script(None);
        for value in [LLAMA_MODEL_FILE, &LLAMA_MODEL_SIZE.to_string(), LLAMA_MODEL_SHA256, LLAMA_MMPROJ_FILE, &LLAMA_MMPROJ_SIZE.to_string(), LLAMA_MMPROJ_SHA256] {
            assert!(verification.contains(value), "{value}");
        }
        let startup = llama_startup_artifact_script(&profile);
        assert!(startup.contains("Get-Item") && startup.contains(".Length"));
        assert!(!startup.contains("Get-FileHash") && !startup.contains("SHA256"));
        let guard = llama_direct_port_guard_script();
        assert!(guard.contains("netstat -ano -p TCP") && guard.contains(":18181") && !guard.contains("Get-NetTCPConnection"));
        assert_eq!(LLAMA_CLEANUP_REMOTE_TIMEOUT.as_millis(), 45_000);
        assert_eq!(LLAMA_ORPHAN_CLEANUP_REMOTE_TIMEOUT.as_millis(), 60_000);
    }

    #[test]
    fn ownership_records_and_cleanup_scripts() {
        let record = json!({ "pid": 12, "path": LLAMA_SERVER_PATH, "created": "2026-09-15T00:00:00.0000000Z" });
        assert!(valid_llama_ownership(&record));
        assert!(!valid_llama_ownership(&json!({ "pid": 12, "path": r"C:\other.exe", "created": "2026-09-15T00:00:00Z" })));
        assert!(!valid_llama_ownership(&json!({ "pid": 0, "path": LLAMA_SERVER_PATH, "created": "2026-09-15T00:00:00Z" })));
        assert!(!valid_llama_ownership(&json!({ "pid": 12, "path": LLAMA_SERVER_PATH, "created": "2026-09-15T00:00:00" })));
        let cleanup = llama_cleanup_script(&record).unwrap();
        for part in ["Get-Process -Id 12", "Stop-Process -Id 12 -Force", "AddSeconds(15)", "Owned server did not exit"] {
            assert!(cleanup.contains(part), "{part}");
        }
        assert!(llama_cleanup_script(&json!({ "pid": 12, "path": r"C:\other.exe", "created": "2026-09-15T00:00:00Z" })).is_err());
    }

    #[test]
    fn mero_arguments_follow_artifacts() {
        use crate::local_model_capabilities::{MEROMERO_26B_HOST_KEY, MEROMERO_HOST_KEY, MEROMERO_MODEL_ID};
        let base = definition().profile;
        let vision = with(&base, |p| p.model = MEROMERO_26B_MODEL_ID.into());
        let arguments = llama_server_arguments(&vision).unwrap();
        assert!(arguments.contains(&"--mmproj-offload".to_string()));
        assert_eq!(arguments[arguments.iter().position(|a| a == "-b").unwrap() + 1], "1024");
        assert_eq!(expected_model_key(&vision), MEROMERO_26B_HOST_KEY);
        let text = with(&base, |p| {
            p.model = MEROMERO_MODEL_ID.into();
            p.context_window = 32768;
        });
        let arguments = llama_server_arguments(&text).unwrap();
        assert!(!arguments.contains(&"--mmproj".to_string()));
        assert_eq!(arguments[arguments.iter().position(|a| a == "-ub").unwrap() + 1], "128");
        assert_eq!(expected_model_key(&text), MEROMERO_HOST_KEY);
        assert!(llama_server_arguments(&with(&text, |p| p.kv_cache_quantization = None)).is_err());
        assert_eq!(expected_model_key(&base), "qwen3.8-27b");
    }

    #[tokio::test]
    async fn idle_needs_two_consecutive_readings() {
        let states = Mutex::new(VecDeque::from([true, false, true, false, false]));
        let checks = Mutex::new(0);
        let read = |_| {
            *checks.lock().unwrap() += 1;
            let next = states.lock().unwrap().pop_front().unwrap();
            async move { Ok(Some(json!([{ "is_processing": next }]))) }
        };
        wait_for_idle_with(read, || true, Duration::from_millis(1000), Duration::from_millis(1)).await.unwrap();
        assert_eq!(*checks.lock().unwrap(), 5);
        let busy = wait_for_idle_with(|_| async { Ok(Some(json!([{ "is_processing": true }]))) }, || true, Duration::from_millis(20), Duration::from_millis(1)).await;
        assert_eq!(busy.unwrap_err().message, "Direct generation did not stop");
        for invalid in [json!([]), json!({}), json!([{ "is_processing": "x" }]), json!([{ "is_processing": false }, { "is_processing": false }])] {
            let result = wait_for_idle_with(|_| { let v = invalid.clone(); async move { Ok(Some(v)) } }, || true, Duration::from_millis(100), Duration::from_millis(1)).await;
            assert!(result.unwrap_err().message.contains("invalid"));
        }
        let unavailable = wait_for_idle_with(|_| async { Ok(None) }, || true, Duration::from_millis(100), Duration::from_millis(1)).await;
        assert!(unavailable.unwrap_err().message.contains("unavailable"));
        let disconnected = wait_for_idle_with(|_| async { Ok(None) }, || false, Duration::from_millis(100), Duration::from_millis(1)).await;
        assert!(disconnected.unwrap_err().message.contains("disconnected"));
    }

    /// 실제 SSH·DB 대신 원격 명령과 측정을 기록한다. 측정은 마지막 값을 계속 돌려준다.
    struct FakeShell {
        remotes: Mutex<Vec<String>>,
        samples: Mutex<VecDeque<ResourceSample>>,
        taken: Mutex<Vec<f64>>,
    }

    impl RemoteShell for FakeShell {
        fn run(&self, _host: &LocalHost, script: String, _timeout: Duration, _signal: Option<AbortSignal>) -> BoxFuture<'static, Result<String, LocalError>> {
            let reply = if script == local_host_resource_sample_script(8188).unwrap() {
                let mut queue = self.samples.lock().unwrap();
                let sample = if queue.len() > 1 { queue.pop_front().unwrap() } else { queue[0].clone() };
                self.taken.lock().unwrap().push(sample.vram_pct());
                serde_json::to_string(&sample).unwrap()
            } else if script == local_host_listener_pid_script(1234).unwrap() {
                "0".into()
            } else {
                self.remotes.lock().unwrap().push(script);
                r#"{"released":0}"#.into()
            };
            Box::pin(async move { Ok(reply) })
        }

        fn open_tunnel(&self, _host: &LocalHost, _local: u16, _remote: u16) -> Result<Arc<dyn Tunnel>, LocalError> {
            Err(LocalError::plain("tunnel not expected"))
        }
    }

    #[derive(Default)]
    struct FakeLeases(Arc<Mutex<Vec<&'static str>>>);
    struct FakeLease(Arc<Mutex<Vec<&'static str>>>);

    impl HostLeases for FakeLeases {
        fn try_lock(&self, _key: String) -> BoxFuture<'static, Result<Option<Box<dyn HostLease>>, LocalError>> {
            self.0.lock().unwrap().push("advisory_lock");
            let log = Arc::clone(&self.0);
            Box::pin(async move { Ok(Some(Box::new(FakeLease(log)) as Box<dyn HostLease>)) })
        }
    }

    impl HostLease for FakeLease {
        fn unlock(self: Box<Self>) -> BoxFuture<'static, Result<(), LocalError>> {
            self.0.lock().unwrap().push("advisory_unlock");
            Box::pin(async { Ok(()) })
        }
    }

    fn occupied_host(samples: Vec<ResourceSample>) -> (Arc<WindowsHost>, Arc<FakeShell>, Arc<Mutex<Vec<&'static str>>>) {
        let shell = Arc::new(FakeShell { remotes: Mutex::default(), samples: Mutex::new(samples.into()), taken: Mutex::default() });
        let leases = FakeLeases::default();
        let log = Arc::clone(&leases.0);
        let directory = PathBuf::from(crate::js::node_tmpdir()).join(format!("officestra-comfy-release-test-{}", crate::js::random_uuid()));
        let timings = HostTimings { comfy_release_timeout: Duration::from_millis(30), comfy_release_poll: Duration::from_millis(1) };
        let host = WindowsHost::new(&definition(), shell.clone(), Arc::new(leases), directory, timings).unwrap();
        (host, shell, log)
    }

    #[tokio::test]
    async fn gpu_held_by_comfy_reports_state_and_unlocks() {
        let (host, shell, log) = occupied_host(vec![ResourceSample::of(79.6, 40.0, false, Some("ready"))]);
        let error = as_model_host(Arc::clone(&host)).start(StartOptions { signal: AbortSignal::new(), comfy_release: None }).await.err().unwrap();
        assert!(error.is_busy());
        assert_eq!(error.gpu(), Some(&json!({ "vramPct": 80, "ramPct": 40, "comfyUI": "idle" })));
        assert!(!shell.remotes.lock().unwrap().iter().any(|script| script.contains("/free")));
        assert!(log.lock().unwrap().contains(&"advisory_unlock"));
        let _ = std::fs::remove_dir(&host.directory);
    }

    #[tokio::test]
    async fn chosen_comfy_release_runs_inside_lease_before_gpu_check() {
        let (host, shell, _) = occupied_host(vec![ResourceSample::of(80.0, 40.0, true, Some("ready")), ResourceSample::of(79.0, 40.0, true, Some("ready"))]);
        let error = as_model_host(Arc::clone(&host)).start(StartOptions { signal: AbortSignal::new(), comfy_release: Some("interrupt".into()) }).await.err().unwrap();
        assert_eq!(shell.remotes.lock().unwrap()[0], comfy_release_script(8188, "interrupt").unwrap());
        assert_eq!(error.gpu().unwrap()["comfyUI"], "running");
        let _ = std::fs::remove_dir(&host.directory);
    }

    #[tokio::test]
    async fn comfy_release_waits_for_direct_runtime_limit() {
        let (host, shell, _) = occupied_host(vec![
            ResourceSample::of(80.0, 40.0, false, Some("ready")),
            ResourceSample::of(45.0, 35.0, false, Some("ready")),
            ResourceSample::of(9.0, 30.0, false, Some("ready")),
        ]);
        let timings = HostTimings { comfy_release_timeout: Duration::from_secs(5), comfy_release_poll: Duration::from_millis(1) };
        let host = WindowsHost::new(&definition(), shell.clone(), Arc::new(FakeLeases::default()), host.directory.clone(), timings).unwrap();
        host.release_comfy_ui("free", None).await.unwrap();
        assert!(shell.remotes.lock().unwrap()[0].contains("/free"));
        assert_eq!(*shell.taken.lock().unwrap(), [80.0, 45.0, 9.0]);
        assert!(!host.gpu_available(&ResourceSample::of(25.0, 30.0, false, None)));
    }
}
