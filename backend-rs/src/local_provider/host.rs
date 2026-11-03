// 4090 원격 PC 공통 계층. Node local-provider-host.mjs의 호스트 검증·PowerShell 스크립트·SSH 실행·DB 임대·소유 기록을 옮겼다.
// 주소·사용자·키 경로는 오류 문구와 로그에 넣지 않는다. LM Studio 적재 방식은 R16에서 사용자 결정을 기다린다.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Notify;

use super::{AbortSignal, BusyDetail, LocalDefinition, LocalError};
use crate::child_process::{read_open_pipe, spawn_error_message};
use crate::js::{is_js_integer, js_compare, js_string, js_trim, now_milliseconds, parse_js_json, string_to_number, to_js_json};
use crate::local_agent_profile::LocalAgentProfile;
use crate::local_model_capabilities::DIRECT_RUNTIME;

/// 사용자가 허락한 RAM 완화 기준. 측정 뒤 멈추는 기준이지 할당 상한이 아니다.
pub const VRAM_GUARD_PERCENT: f64 = 98.0;
pub const RAM_GUARD_PERCENT: f64 = 90.0;
pub const LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT: Duration = Duration::from_millis(30_000);
pub const QWEN38_LMSTUDIO_MODEL_KEYS: [&str; 2] = ["qwen3.8-27b", "qwen3.8-27b-uncensored"];
pub const COMFY_RELEASE_MODES: [&str; 3] = ["free", "interrupt", "terminate"];
pub const DEFAULT_REMOTE_TIMEOUT: Duration = Duration::from_millis(15_000);
/// Node는 SDK 웹소켓으로 LM Studio 버전 확인·적재를 했다. R01은 lms 명령으로 바꾸기로 했지만 4090에서 lms를 확인하지 못했다.
pub const LMSTUDIO_CONTROL_UNDECIDED: &str = "LM Studio 모델 적재 방식이 아직 확인되지 않았습니다. llama.cpp 프로필을 쓰세요.";
const SSH_OUTPUT_LIMIT: usize = 1024 * 1024;
const REMOTE_COMMAND_LIMIT: usize = 7000;

pub fn is_qwen38_lmstudio_model_key(value: &str) -> bool {
    QWEN38_LMSTUDIO_MODEL_KEYS.contains(&value)
}

/// 검증한 호스트. Node `{...h}`처럼 받은 필드를 모두 지니고, 읽을 때 JS 변환 규칙을 따른다.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalHost(Map<String, Value>);

fn coerce(value: Option<&Value>) -> String {
    value.map(js_string).unwrap_or_else(|| "undefined".into())
}

fn validated_port(value: Option<&Value>) -> Option<u16> {
    value.and_then(Value::as_f64).filter(|port| is_js_integer(*port) && (1.0..=65535.0).contains(port)).map(|port| port as u16)
}

fn has_control(text: &str) -> bool {
    text.contains(['\r', '\n', '\0'])
}

impl LocalHost {
    pub fn to_value(&self) -> Value {
        Value::Object(self.0.clone())
    }

    pub fn fields(&self) -> &Map<String, Value> {
        &self.0
    }

    /// 템플릿 문자열 `${h.address}`. 검증이 String() 변환 뒤 정규식이라 빠진 값은 "undefined"다.
    pub fn address(&self) -> String {
        coerce(self.0.get("address"))
    }

    pub fn user(&self) -> String {
        coerce(self.0.get("user"))
    }

    pub fn ssh_port(&self) -> u16 {
        validated_port(self.0.get("sshPort")).unwrap_or_default()
    }

    pub fn comfy_port(&self) -> u16 {
        validated_port(self.0.get("comfyPort")).unwrap_or_default()
    }

    fn text(&self, key: &str) -> &str {
        self.0.get(key).and_then(Value::as_str).unwrap_or_default()
    }

    pub fn key_path(&self) -> &str {
        self.text("keyPath")
    }

    pub fn host_key_alias(&self) -> &str {
        self.text("hostKeyAlias")
    }

    pub fn model_key(&self) -> &str {
        self.text("modelKey")
    }

    /// validateHost({...h, address}).
    pub fn with_address(&self, address: &str) -> Result<Self, String> {
        let mut fields = self.0.clone();
        fields.insert("address".into(), json!(address));
        validate_host(&Value::Object(fields))
    }
}

static ADDRESS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9.-]+$").unwrap());
static USER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]+$").unwrap());

/// validateHost. 객체가 아니면 sshPort가 없어 Node에서도 실패한다.
pub fn validate_host(value: &Value) -> Result<LocalHost, String> {
    let Some(host) = value.as_object() else {
        return Err("Invalid local host configuration".into());
    };
    let text = |key: &str| host.get(key).and_then(Value::as_str);
    let valid = ADDRESS.is_match(&coerce(host.get("address")))
        && USER.is_match(&coerce(host.get("user")))
        && validated_port(host.get("sshPort")).is_some()
        && text("keyPath").is_some_and(|path| path.starts_with('/') && !has_control(path))
        && text("hostKeyAlias").is_some_and(|alias| !has_control(alias))
        && text("modelKey").is_some_and(|key| !key.is_empty() && !has_control(key))
        && validated_port(host.get("comfyPort")).is_some();
    if valid { Ok(LocalHost(host.clone())) } else { Err("Invalid local host configuration".into()) }
}

/// localHostLoadConfig. 모델 풀 열쇠에 들어가므로 Node 객체 키 순서를 지킨다.
pub fn local_host_load_config(profile: &LocalAgentProfile) -> Result<Value, String> {
    let base = json!({ "gpu": { "ratio": 1 }, "gpuStrictVramCap": false, "contextLength": 32768, "tryMmap": false,
        "keepModelInMemory": false, "evalBatchSize": 128, "flashAttention": true });
    let quantized = || {
        let mut config = base.clone();
        config["contextLength"] = json!(65536);
        config["llamaKCacheQuantizationType"] = json!("q8_0");
        config["llamaVCacheQuantizationType"] = json!("q8_0");
        config
    };
    if let Some(quantization) = &profile.kv_cache_quantization {
        if profile.backend != "codex" || profile.context_window != 65536 || quantization != "q8_0" {
            return Err("Unsupported local KV configuration".into());
        }
        return Ok(quantized());
    }
    match profile.context_window {
        32768 => Ok(base),
        65536 => Ok(quantized()),
        _ => Err("Unsupported local context configuration".into()),
    }
}

/// 원격 측정값. Node `{...JSON.parse(raw), sampledAt: Date.now()}`처럼 받은 필드를 그대로 둔다.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(transparent)]
pub struct ResourceSample(pub Map<String, Value>);

impl ResourceSample {
    pub fn parse(raw: &str) -> Result<Self, LocalError> {
        let parsed = parse_js_json(raw).map_err(|error| LocalError::plain(format!("SyntaxError: {error}")))?;
        let mut fields = parsed.as_object().cloned().unwrap_or_default();
        fields.insert("sampledAt".into(), json!(now_milliseconds() as i64));
        Ok(Self(fields))
    }

    /// 시험·가짜 호스트용. 지금 시각으로 찍는다.
    pub fn of(vram_pct: f64, ram_pct: f64, busy: bool, queue_state: Option<&str>) -> Self {
        let number = |value: f64| serde_json::Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null);
        let mut fields = Map::new();
        fields.insert("vramPct".into(), number(vram_pct));
        fields.insert("ramPct".into(), number(ram_pct));
        fields.insert("busy".into(), json!(busy));
        if let Some(state) = queue_state {
            fields.insert("queueState".into(), json!(state));
        }
        fields.insert("sampledAt".into(), json!(now_milliseconds() as i64));
        Self(fields)
    }

    fn number(&self, key: &str) -> f64 {
        self.0.get(key).and_then(Value::as_f64).unwrap_or(f64::NAN)
    }

    pub fn vram_pct(&self) -> f64 {
        self.number("vramPct")
    }

    pub fn ram_pct(&self) -> f64 {
        self.number("ramPct")
    }

    pub fn sampled_at(&self) -> f64 {
        self.number("sampledAt")
    }

    /// `sample.busy` 참 판정.
    pub fn busy(&self) -> bool {
        crate::js::js_truthy(self.0.get("busy"))
    }

    /// `sample.busy === true`.
    pub fn busy_flag(&self) -> bool {
        self.0.get("busy") == Some(&Value::Bool(true))
    }

    pub fn queue_state(&self) -> Option<&str> {
        self.0.get("queueState").and_then(Value::as_str)
    }
}

pub fn local_host_memory_exceeded(sample: &ResourceSample) -> bool {
    sample.vram_pct() >= VRAM_GUARD_PERCENT || sample.ram_pct() >= RAM_GUARD_PERCENT
}

pub fn comfy_ui_state(sample: &ResourceSample) -> &'static str {
    match sample.queue_state() {
        Some("ready") if sample.busy() => "running",
        Some("ready") => "idle",
        Some("unavailable") => "unavailable",
        _ => "offline",
    }
}

/// Math.round. NaN은 JSON.stringify처럼 null이다.
fn js_round(value: f64) -> Value {
    let rounded = (value + 0.5).floor();
    if rounded.is_finite() { json!(rounded as i64) } else { Value::Null }
}

/// LocalHostGPUOccupiedError. 시작을 막은 그래픽 메모리 상태를 함께 넘긴다.
pub fn gpu_occupied(sample: &ResourceSample) -> LocalError {
    let gpu = json!({ "vramPct": js_round(sample.vram_pct()), "ramPct": js_round(sample.ram_pct()), "comfyUI": comfy_ui_state(sample) });
    LocalError {
        status: None,
        message: "4090 그래픽 메모리를 다른 작업이 쓰고 있어 로컬 모델을 시작할 수 없습니다.".into(),
        busy: Some(BusyDetail::GpuOccupied(gpu)),
    }
}

fn checked_port(port: i64, message: &str) -> Result<i64, LocalError> {
    if (1..=65535).contains(&port) { Ok(port) } else { Err(LocalError::plain(message)) }
}

/// localHostResourceSampleScript. CIM 대신 .NET API로 메모리와 수신 포트를 읽는다.
pub fn local_host_resource_sample_script(comfy_port: i64) -> Result<String, LocalError> {
    let port = checked_port(comfy_port, "Invalid ComfyUI port")?.to_string();
    Ok(r"$ErrorActionPreference='Stop'; $g=(& nvidia-smi --query-gpu=memory.total,memory.used --format=csv,noheader,nounits) -split ','; Add-Type -AssemblyName Microsoft.VisualBasic; $o=New-Object Microsoft.VisualBasic.Devices.ComputerInfo; $busy=$false; $queueState='offline'; $listener=[System.Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners() | Where-Object {$_.Port -eq @PORT@} | Select-Object -First 1; if($listener){ try{$q=Invoke-RestMethod -Uri http://127.0.0.1:@PORT@/queue -TimeoutSec 2; if($null -eq $q.queue_running -or $null -eq $q.queue_pending){throw 'Invalid queue'}; $busy=($q.queue_running.Count + $q.queue_pending.Count) -gt 0; $queueState='ready'}catch{$busy=$true;$queueState='unavailable'} }; [pscustomobject]@{vramPct=100*[double]$g[1]/[double]$g[0];ramPct=100*(1-[double]$o.AvailablePhysicalMemory/[double]$o.TotalPhysicalMemory);busy=$busy;queueState=$queueState} | ConvertTo-Json -Compress"
        .replace("@PORT@", &port))
}

/// comfyReleaseScript. 같은 포트의 다른 프로그램은 끄지 않고 ComfyUI main.py만 끝낸다.
pub fn comfy_release_script(comfy_port: i64, mode: &str) -> Result<String, LocalError> {
    let port = checked_port(comfy_port, "Invalid ComfyUI port")?.to_string();
    if !COMFY_RELEASE_MODES.contains(&mode) {
        return Err(LocalError::plain("Unsupported ComfyUI release mode"));
    }
    let base = format!("http://127.0.0.1:{port}");
    let free = format!(r#"Invoke-RestMethod -Method Post -Uri {base}/free -ContentType 'application/json' -Body '{{"unload_models":true,"free_memory":true}}' -TimeoutSec 10 | Out-Null"#);
    Ok(match mode {
        "free" => format!("$ErrorActionPreference='Stop';{free};'released'"),
        // 대기열을 먼저 비워야 중단 직후 다음 작업이 모델을 다시 올리지 않는다.
        "interrupt" => format!(
            r#"$ErrorActionPreference='Stop';Invoke-RestMethod -Method Post -Uri {base}/queue -ContentType 'application/json' -Body '{{"clear":true}}' -TimeoutSec 10 | Out-Null;Invoke-RestMethod -Method Post -Uri {base}/interrupt -TimeoutSec 10 | Out-Null;{free};'released'"#
        ),
        _ => r#"$ErrorActionPreference='Stop';$line=netstat -ano -p TCP | Select-String -Pattern '^\s*TCP\s+\S+:@PORT@\s+\S+\s+LISTENING\s+(\d+)\s*$' | Select-Object -First 1;if($line -and $line.Line -match '\s+(\d+)\s*$'){$id=[int]$Matches[1];$p=Get-CimInstance Win32_Process -Filter "ProcessId=$id";if(-not $p -or $p.CommandLine -notmatch 'main\.py'){throw 'ComfyUI process not recognized'};Stop-Process -Id $id -Force;$deadline=(Get-Date).AddSeconds(15);do{Start-Sleep -Milliseconds 200;$c=Get-Process -Id $id -ErrorAction SilentlyContinue}while($c -and (Get-Date) -lt $deadline);if($c){throw 'ComfyUI did not exit'}};'released'"#
            .replace("@PORT@", &port),
    })
}

/// localHostListenerPIDScript. Get-NetTCPConnection은 CIM에서 멈출 수 있어 netstat을 쓴다.
pub fn local_host_listener_pid_script(port: i64) -> Result<String, LocalError> {
    let port = checked_port(port, "Invalid listener port")?.to_string();
    Ok(r"$line=netstat -ano -p TCP | Select-String -Pattern '^\s*TCP\s+\S+:@PORT@\s+\S+\s+LISTENING\s+(\d+)\s*$' | Select-Object -First 1;if($line -and $line.Line -match '\s+(\d+)\s*$'){$Matches[1]}else{0}"
        .replace("@PORT@", &port))
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Buffer.toString('base64').
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                output.push(BASE64[((value >> (18 - 6 * index)) & 63) as usize] as char);
            } else {
                output.push('=');
            }
        }
    }
    output
}

fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// localHostRemoteCommand. Windows OpenSSH가 cmd.exe(8191자)를 거칠 수 있어 긴 스크립트는 표준 입력으로 보낸다.
pub fn local_host_remote_command(script: &str) -> (String, Option<String>) {
    let encoded = base64_encode(&utf16le(&format!("$ProgressPreference='SilentlyContinue'; [Console]::OutputEncoding=[Text.Encoding]::UTF8; {script}")));
    let prefix = "powershell -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand ";
    if prefix.len() + encoded.len() <= REMOTE_COMMAND_LIMIT {
        return (format!("{prefix}{encoded}"), None);
    }
    let bootstrap = "$s=[Console]::In.ReadToEnd(); & ([scriptblock]::Create([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String($s))))";
    (format!("{prefix}{}", base64_encode(&utf16le(bootstrap))), Some(encoded))
}

/// WindowsLMStudioHost.sshArgs. 마지막 항목이 사용자@주소다.
pub fn ssh_arguments(host: &LocalHost) -> Vec<String> {
    vec![
        "-o".into(), "BatchMode=yes".into(), "-o".into(), "ConnectTimeout=6".into(), "-o".into(), "StrictHostKeyChecking=yes".into(),
        "-o".into(), format!("HostKeyAlias={}", host.host_key_alias()), "-i".into(), host.key_path().into(), "-p".into(), host.ssh_port().to_string(),
        format!("{}@{}", host.user(), host.address()),
    ]
}

/// SSH 터널. exited는 Node 'exit'·'error' 이벤트다.
pub trait Tunnel: Send + Sync {
    fn exited(&self) -> bool;
    fn kill(&self);
}

/// 원격 PowerShell 실행과 터널. 시험은 가짜로 바꾼다.
pub trait RemoteShell: Send + Sync {
    fn run(&self, host: &LocalHost, script: String, timeout: Duration, signal: Option<AbortSignal>) -> BoxFuture<'static, Result<String, LocalError>>;
    fn open_tunnel(&self, host: &LocalHost, local_port: u16, remote_port: u16) -> Result<Arc<dyn Tunnel>, LocalError>;
}

/// 실제 ssh. 출력이 1MB를 넘거나 시간 초과·중단이면 SIGTERM을 보낸다.
pub struct SshShell;

fn terminate(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: 아직 거두지 않은 자식 프로세스 번호에 신호만 보낸다.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

impl RemoteShell for SshShell {
    fn run(&self, host: &LocalHost, script: String, timeout: Duration, signal: Option<AbortSignal>) -> BoxFuture<'static, Result<String, LocalError>> {
        let (command, input) = local_host_remote_command(&script);
        let mut arguments = ssh_arguments(host);
        arguments.push(command);
        Box::pin(async move {
            let mut child = Command::new("ssh")
                .args(&arguments)
                .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| LocalError::plain(spawn_error_message("ssh", &error)))?;
            let pid = child.id();
            if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
                // Node도 stdin 쓰기 오류를 무시한다. 출력 읽기와 겹치도록 따로 쓴다.
                tokio::spawn(async move {
                    let _ = stdin.write_all(input.as_bytes()).await;
                });
            }
            let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
            let (mut output, mut errors) = (Vec::new(), Vec::new());
            let (mut out_buffer, mut error_buffer) = ([0u8; 8192], [0u8; 8192]);
            let deadline = tokio::time::sleep(timeout);
            tokio::pin!(deadline);
            let aborted = async {
                match &signal {
                    Some(signal) => signal.aborted().await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(aborted);
            let (mut timed_out, mut abort_sent) = (false, false);
            while stdout.is_some() || stderr.is_some() {
                tokio::select! {
                    read = read_open_pipe(&mut stdout, &mut out_buffer) => match read {
                        Ok(0) | Err(_) => stdout = None,
                        Ok(count) => {
                            output.extend_from_slice(&out_buffer[..count]);
                            if output.len() > SSH_OUTPUT_LIMIT { terminate(pid); }
                        }
                    },
                    read = read_open_pipe(&mut stderr, &mut error_buffer) => match read {
                        Ok(0) | Err(_) => stderr = None,
                        Ok(count) => {
                            errors.extend_from_slice(&error_buffer[..count]);
                            if errors.len() > SSH_OUTPUT_LIMIT { terminate(pid); }
                        }
                    },
                    () = &mut deadline, if !timed_out => { timed_out = true; terminate(pid); }
                    () = &mut aborted, if !abort_sent => { abort_sent = true; terminate(pid); }
                }
            }
            let status = child.wait().await.map_err(|error| LocalError::plain(error.to_string()))?;
            if status.code() != Some(0) {
                return Err(LocalError::plain(if timed_out {
                    format!("4090 SSH operation timed out after {}ms", timeout.as_millis())
                } else {
                    "4090 SSH operation failed".into()
                }));
            }
            Ok(js_trim(&String::from_utf8_lossy(&output)).trim_start_matches('\u{feff}').to_string())
        })
    }

    fn open_tunnel(&self, host: &LocalHost, local_port: u16, remote_port: u16) -> Result<Arc<dyn Tunnel>, LocalError> {
        let mut arguments = ssh_arguments(host);
        let target = arguments.pop().unwrap_or_default();
        arguments.extend(["-N", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2", "-L"].map(String::from));
        arguments.push(format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"));
        arguments.push(target);
        let tunnel = Arc::new(SshTunnel { exited: AtomicBool::new(false), kill: Notify::new() });
        let mut child = match Command::new("ssh").args(&arguments).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn() {
            Ok(child) => child,
            Err(_) => {
                // Node는 'error' 이벤트로 exited만 세운다. 준비 확인이 실패로 끝낸다.
                tunnel.exited.store(true, Ordering::SeqCst);
                return Ok(tunnel);
            }
        };
        let watcher = Arc::clone(&tunnel);
        tokio::spawn(async move {
            let pid = child.id();
            tokio::select! {
                _ = child.wait() => {}
                () = watcher.kill.notified() => {
                    terminate(pid);
                    let _ = child.wait().await;
                }
            }
            watcher.exited.store(true, Ordering::SeqCst);
        });
        Ok(tunnel)
    }
}

struct SshTunnel {
    exited: AtomicBool,
    kill: Notify,
}

impl Tunnel for SshTunnel {
    fn exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    fn kill(&self) {
        self.kill.notify_one();
    }
}

/// pg_try_advisory_lock 임대. 같은 연결에서 풀어야 해서 연결을 쥐고 있다.
pub trait HostLease: Send {
    fn unlock(self: Box<Self>) -> BoxFuture<'static, Result<(), LocalError>>;
}

pub trait HostLeases: Send + Sync {
    /// 잠갔으면 Some, 다른 요청이 쥐고 있으면 None이다.
    fn try_lock(&self, key: String) -> BoxFuture<'static, Result<Option<Box<dyn HostLease>>, LocalError>>;
}

pub struct PgHostLeases(pub PgPool);

struct PgHostLease {
    connection: sqlx::pool::PoolConnection<sqlx::Postgres>,
    key: String,
}

impl HostLeases for PgHostLeases {
    fn try_lock(&self, key: String) -> BoxFuture<'static, Result<Option<Box<dyn HostLease>>, LocalError>> {
        let pool = self.0.clone();
        Box::pin(async move {
            let mut connection = pool.acquire().await.map_err(|error| LocalError::plain(error.to_string()))?;
            let locked: Option<bool> = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1)) AS locked")
                .bind(&key)
                .fetch_one(&mut *connection)
                .await
                .map_err(|error| LocalError::plain(error.to_string()))?;
            Ok(locked.unwrap_or(false).then(|| Box::new(PgHostLease { connection, key }) as Box<dyn HostLease>))
        })
    }
}

impl HostLease for PgHostLease {
    fn unlock(self: Box<Self>) -> BoxFuture<'static, Result<(), LocalError>> {
        Box::pin(async move {
            let PgHostLease { mut connection, key } = *self;
            let result = sqlx::query("SELECT pg_advisory_unlock(hashtext($1))").bind(&key).execute(&mut *connection).await;
            match result {
                Ok(_) => Ok(()),
                Err(error) => {
                    // 풀지 못한 잠금이 풀에 돌아가 남지 않게 연결을 닫는다.
                    let _ = connection.close().await;
                    Err(LocalError::plain(error.to_string()))
                }
            }
        })
    }
}

pub struct StartOptions {
    pub signal: AbortSignal,
    pub comfy_release: Option<String>,
}

/// host.start()가 돌려주는 자원. 모델 풀이 빌려 주고 돌려받는다.
pub trait HostResource: Send + Sync {
    fn upstream(&self) -> String;
    fn sample(&self) -> BoxFuture<'static, Result<ResourceSample, LocalError>>;
    fn alive(&self) -> bool;
    fn release(&self) -> BoxFuture<'static, Result<(), LocalError>>;
    /// waitForIdle이 있는 자원(llama.cpp)만 확인된 취소 뒤 모델을 데워 둔다.
    fn supports_wait_for_idle(&self) -> bool {
        false
    }
    fn wait_for_idle(&self) -> BoxFuture<'static, Result<(), LocalError>> {
        Box::pin(async { Ok(()) })
    }
    /// reconnectAddress. 설정이 유휴·고정 호스트 키를 확인한 뒤에만 부른다.
    fn reconnect_address(&self, _address: &str) -> Result<(), LocalError> {
        Ok(())
    }
}

pub trait ModelHost: Send + Sync {
    fn start(self: Arc<Self>, options: StartOptions) -> BoxFuture<'static, Result<Arc<dyn HostResource>, LocalError>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    LmStudio,
    Llama,
}

/// ComfyUI 해제 뒤 메모리가 내려가길 기다리는 한도. 시험은 짧게 바꾼다.
#[derive(Debug, Clone, Copy)]
pub struct HostTimings {
    pub comfy_release_timeout: Duration,
    pub comfy_release_poll: Duration,
}

impl Default for HostTimings {
    fn default() -> Self {
        Self { comfy_release_timeout: Duration::from_millis(20_000), comfy_release_poll: Duration::from_millis(1000) }
    }
}

pub(super) struct HostInner {
    pub(super) host: LocalHost,
    pub(super) owned: Option<Value>,
    pub(super) lease: Option<Box<dyn HostLease>>,
    pub(super) tunnel: Option<Arc<dyn Tunnel>>,
}

/// WindowsLMStudioHost·WindowsLlamaCppHost. 시작 흐름만 다르고 원격 실행·측정·정리는 같다.
pub struct WindowsHost {
    pub(super) kind: HostKind,
    pub(super) profile: LocalAgentProfile,
    pub(super) key: String,
    pub(super) directory: PathBuf,
    pub(super) state_path: PathBuf,
    pub(super) shell: Arc<dyn RemoteShell>,
    pub(super) leases: Arc<dyn HostLeases>,
    pub(super) timings: HostTimings,
    pub(super) http: reqwest::Client,
    pub(super) inner: Mutex<HostInner>,
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

impl WindowsHost {
    /// 생성자. llama.cpp는 실행 인자와 고정 모델 키를 먼저 확인한다.
    pub fn new(
        definition: &LocalDefinition,
        shell: Arc<dyn RemoteShell>,
        leases: Arc<dyn HostLeases>,
        state_directory: PathBuf,
        timings: HostTimings,
    ) -> Result<Arc<Self>, LocalError> {
        let host = definition.host.clone();
        let kind = if definition.profile.runtime.as_deref() == Some(DIRECT_RUNTIME) { HostKind::Llama } else { HostKind::LmStudio };
        let key = sha256_hex(&format!("{}:{}", host.address(), host.ssh_port()));
        let mut state_path = state_directory.join(format!("{key}.json"));
        if kind == HostKind::Llama {
            super::llama::llama_server_arguments(&definition.profile).map_err(LocalError::plain)?;
            if host.model_key() != super::llama::expected_model_key(&definition.profile) {
                return Err(LocalError::plain("Unsupported model key"));
            }
            state_path = state_directory.join(format!("{key}.llama.json"));
        }
        let http = crate::http_client_builder().no_proxy().build().map_err(|error| LocalError::plain(error.to_string()))?;
        Ok(Arc::new(Self {
            kind,
            profile: definition.profile.clone(),
            key,
            directory: state_directory,
            state_path,
            shell,
            leases,
            timings,
            http,
            inner: Mutex::new(HostInner { host, owned: None, lease: None, tunnel: None }),
        }))
    }

    pub(super) fn current_host(&self) -> LocalHost {
        self.inner.lock().unwrap().host.clone()
    }

    pub fn remote(&self, script: String, timeout: Duration, signal: Option<AbortSignal>) -> BoxFuture<'static, Result<String, LocalError>> {
        self.shell.run(&self.current_host(), script, timeout, signal)
    }

    pub async fn sample(&self) -> Result<ResourceSample, LocalError> {
        let script = local_host_resource_sample_script(i64::from(self.current_host().comfy_port()))?;
        ResourceSample::parse(&self.remote(script, LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT, None).await?)
    }

    /// Number(netstat 결과). 숫자가 아니면 NaN이다.
    pub async fn server_pid(&self) -> Result<f64, LocalError> {
        Ok(string_to_number(&self.remote(local_host_listener_pid_script(1234)?, DEFAULT_REMOTE_TIMEOUT, None).await?))
    }

    pub fn gpu_available(&self, sample: &ResourceSample) -> bool {
        let base = !sample.busy() && !local_host_memory_exceeded(sample);
        match self.kind {
            HostKind::LmStudio => base,
            // llama.cpp는 빈 그래픽 메모리에서만 시작한다(20% 규칙).
            HostKind::Llama => base && sample.vram_pct() <= 20.0,
        }
    }

    /// ComfyUI 해제 요청 뒤 메모리가 내려갈 때까지만 기다린다. 시작 판단은 호출한 쪽이 다시 한다.
    pub async fn release_comfy_ui(&self, mode: &str, signal: Option<AbortSignal>) -> Result<(), LocalError> {
        let script = comfy_release_script(i64::from(self.current_host().comfy_port()), mode)?;
        self.remote(script, Duration::from_millis(30_000), signal.clone()).await?;
        let deadline = tokio::time::Instant::now() + self.timings.comfy_release_timeout;
        let mut sample = self.sample().await?;
        while !signal.as_ref().is_some_and(AbortSignal::is_aborted) && !self.gpu_available(&sample) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(self.timings.comfy_release_poll).await;
            sample = self.sample().await?;
        }
        Ok(())
    }

    pub(super) async fn take_lease(&self) -> Result<(), LocalError> {
        let lease = self.leases.try_lock(format!("local-host:{}", self.key)).await?;
        let Some(lease) = lease else {
            return Err(LocalError::busy("4090 is leased by another request"));
        };
        self.inner.lock().unwrap().lease = Some(lease);
        Ok(())
    }

    /// 상태 폴더를 만들고 남은 소유 기록을 읽는다.
    pub(super) async fn load_owned(&self) -> Result<(), LocalError> {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&self.directory).map_err(|error| LocalError::plain(error.to_string()))?;
        match tokio::fs::read_to_string(&self.state_path).await {
            Ok(text) => {
                let owned = parse_js_json(&text).map_err(|error| LocalError::plain(format!("SyntaxError: {error}")))?;
                self.inner.lock().unwrap().owned = Some(owned);
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(LocalError::plain(error.to_string())),
        }
    }

    pub(super) async fn write_owned(&self, owned: &Value) -> Result<(), LocalError> {
        use std::os::unix::fs::OpenOptionsExt;
        let bytes = to_js_json(owned).map_err(|error| LocalError::plain(error.to_string()))?;
        let path = self.state_path.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
            file.write_all(&bytes)
        })
        .await
        .map_err(|error| LocalError::plain(error.to_string()))?
        .map_err(|error| LocalError::plain(error.to_string()))?;
        self.inner.lock().unwrap().owned = Some(owned.clone());
        Ok(())
    }

    pub(super) async fn forget_owned(&self) -> Result<(), LocalError> {
        match tokio::fs::remove_file(&self.state_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LocalError::plain(error.to_string())),
        }
        self.inner.lock().unwrap().owned = None;
        Ok(())
    }

    async fn cleanup_owned(&self) -> Result<(), LocalError> {
        let owned = self.inner.lock().unwrap().owned.clone();
        let Some(owned) = owned else {
            return Ok(());
        };
        match self.kind {
            HostKind::Llama => super::llama::cleanup_owned(self, &owned).await,
            // LM Studio 소유 기록은 lms 확인 뒤에만 정리할 수 있다. 기록을 지우지 않고 멈춘다.
            HostKind::LmStudio => Err(LocalError::plain(LMSTUDIO_CONTROL_UNDECIDED)),
        }
    }

    /// release. 소유 자원 정리 실패보다 임대 해제 실패를 먼저 알린다(Node try/finally 순서).
    pub async fn release(&self) -> Result<(), LocalError> {
        let failure = self.cleanup_owned().await.err();
        let (tunnel, lease) = {
            let mut inner = self.inner.lock().unwrap();
            (inner.tunnel.take(), inner.lease.take())
        };
        if let Some(tunnel) = tunnel {
            tunnel.kill();
        }
        if let Some(lease) = lease {
            lease.unlock().await?;
        }
        failure.map_or(Ok(()), Err)
    }

    pub(super) fn reconnect(&self, address: &str) -> Result<(), LocalError> {
        let mut inner = self.inner.lock().unwrap();
        inner.host = inner.host.with_address(address).map_err(LocalError::plain)?;
        Ok(())
    }
}

impl ModelHost for WindowsHost {
    fn start(self: Arc<Self>, options: StartOptions) -> BoxFuture<'static, Result<Arc<dyn HostResource>, LocalError>> {
        Box::pin(async move {
            match self.kind {
                HostKind::Llama => super::llama::start(self, options).await,
                HostKind::LmStudio => Err(LocalError::plain(LMSTUDIO_CONTROL_UNDECIDED)),
            }
        })
    }
}

/// localModelPoolKey의 호스트 정체. 키를 JS 기본 정렬로 늘어놓는다.
pub(super) fn sorted_host_identity(host: &LocalHost) -> Value {
    let mut keys: Vec<&String> = host.fields().keys().collect();
    keys.sort_by(|left, right| js_compare(left, right));
    Value::Object(keys.into_iter().map(|key| (key.clone(), host.fields()[key].clone())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Value {
        json!({ "address": "127.0.0.1", "user": "test", "sshPort": 2222, "keyPath": "/tmp/test-key", "hostKeyAlias": "test", "modelKey": "qwen3.8-27b", "comfyPort": 8188 })
    }

    fn decode_base64(text: &str) -> Vec<u8> {
        let value = |c: u8| BASE64.iter().position(|&b| b == c).unwrap() as u32;
        let bytes: Vec<u8> = text.bytes().filter(|&b| b != b'=').collect();
        let mut output = Vec::new();
        for chunk in bytes.chunks(4) {
            let mut acc = 0u32;
            for (index, &byte) in chunk.iter().enumerate() {
                acc |= value(byte) << (18 - 6 * index);
            }
            for index in 0..chunk.len() - 1 {
                output.push((acc >> (16 - 8 * index)) as u8);
            }
        }
        output
    }

    fn utf16_decode(bytes: &[u8]) -> String {
        String::from_utf16(&bytes.chunks(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn validates_hosts_like_node() {
        assert!(validate_host(&host()).is_ok());
        let mut extra = host();
        extra["note"] = json!("kept");
        assert_eq!(validate_host(&extra).unwrap().fields()["note"], "kept");
        // String(undefined)는 "undefined"라 정규식을 지난다.
        let mut missing = host();
        missing.as_object_mut().unwrap().shift_remove("address");
        assert_eq!(validate_host(&missing).unwrap().address(), "undefined");
        for (key, value) in [
            ("address", json!("1.2.3.4:22")),
            ("user", json!("a b")),
            ("sshPort", json!(0)),
            ("sshPort", json!(22.5)),
            ("sshPort", json!("22")),
            ("keyPath", json!("relative")),
            ("keyPath", json!("/a\nb")),
            ("hostKeyAlias", json!(1)),
            ("modelKey", json!("")),
            ("comfyPort", json!(70000)),
        ] {
            let mut bad = host();
            bad[key] = value;
            assert!(validate_host(&bad).is_err(), "{key}");
        }
        assert!(validate_host(&Value::Null).is_err());
        assert!(validate_host(&json!([])).is_err());
        let mut float_port = host();
        float_port["sshPort"] = json!(2222.0);
        assert_eq!(validate_host(&float_port).unwrap().ssh_port(), 2222);
    }

    #[test]
    fn long_scripts_go_through_stdin_without_changing_unicode() {
        let long = format!("Write-Output '시작'\n#{}\nWrite-Output '끝'", "한글 $x ' \" ".repeat(400));
        for (index, script) in ["Write-Output '짧은 명령'".to_string(), long].iter().enumerate() {
            let (command, input) = local_host_remote_command(script);
            assert!(command.len() < 7001);
            let payload = command.split(" -EncodedCommand ").nth(1).unwrap();
            let decoded = utf16_decode(&decode_base64(input.as_deref().unwrap_or(payload)));
            assert!(decoded.ends_with(script.as_str()));
            if index == 0 {
                assert_eq!(input, None);
            } else {
                assert!(input.is_some());
                assert!(utf16_decode(&decode_base64(payload)).contains("ReadToEnd"));
            }
        }
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
        assert_eq!(base64_encode(b"a"), "YQ==");
    }

    #[test]
    fn comfy_release_scripts_and_states() {
        let free = comfy_release_script(8188, "free").unwrap();
        assert!(free.contains("http://127.0.0.1:8188/free"));
        assert!(free.contains(r#""unload_models":true"#));
        let interrupt = comfy_release_script(8188, "interrupt").unwrap();
        assert!(interrupt.find("/queue").unwrap() < interrupt.find("/interrupt").unwrap());
        assert!(interrupt.find("/interrupt").unwrap() < interrupt.find("/free").unwrap());
        assert!(interrupt.contains(r#""clear":true"#));
        let terminate = comfy_release_script(8188, "terminate").unwrap();
        assert!(terminate.contains(":8188"));
        assert!(terminate.contains(r"main\.py"));
        assert!(terminate.contains("Stop-Process -Id $id -Force"));
        assert_eq!(comfy_release_script(8188, "kill-all").unwrap_err().message, "Unsupported ComfyUI release mode");
        assert_eq!(comfy_release_script(0, "free").unwrap_err().message, "Invalid ComfyUI port");
        assert_eq!(comfy_ui_state(&ResourceSample::of(1.0, 1.0, false, Some("offline"))), "offline");
        assert_eq!(comfy_ui_state(&ResourceSample::of(1.0, 1.0, true, Some("unavailable"))), "unavailable");
        assert_eq!(comfy_ui_state(&ResourceSample::of(1.0, 1.0, false, Some("ready"))), "idle");
        assert_eq!(comfy_ui_state(&ResourceSample::of(1.0, 1.0, true, Some("ready"))), "running");
        let occupied = gpu_occupied(&ResourceSample::of(79.6, 40.0, false, Some("ready")));
        assert!(occupied.is_busy());
        assert_eq!(occupied.gpu(), Some(&json!({ "vramPct": 80, "ramPct": 40, "comfyUI": "idle" })));
        let sample = local_host_resource_sample_script(8188).unwrap();
        assert!(sample.contains("$_.Port -eq 8188"));
        assert!(sample.contains("http://127.0.0.1:8188/queue"));
        assert!(local_host_listener_pid_script(1234).unwrap().contains(r"\S+:1234\s+"));
        assert!(local_host_resource_sample_script(65536).is_err());
    }

    #[test]
    fn load_config_and_identity_order() {
        let profile = |context: u64, kv: Option<&str>, backend: &str| LocalAgentProfile {
            id: "p".into(),
            provider_kind: "local",
            backend: backend.into(),
            model: "m".into(),
            endpoint: "http://127.0.0.1:1".into(),
            credential_env: "OFFICESTRA_LOCAL_X".into(),
            credential_version: "v1".into(),
            context_window: context,
            max_output_tokens: 4096,
            usage_protocol: "openai-responses-v1".into(),
            reasoning: None,
            runtime: None,
            kv_cache_quantization: kv.map(String::from),
        };
        assert_eq!(
            String::from_utf8(to_js_json(&local_host_load_config(&profile(65536, None, "claude")).unwrap()).unwrap()).unwrap(),
            r#"{"gpu":{"ratio":1},"gpuStrictVramCap":false,"contextLength":65536,"tryMmap":false,"keepModelInMemory":false,"evalBatchSize":128,"flashAttention":true,"llamaKCacheQuantizationType":"q8_0","llamaVCacheQuantizationType":"q8_0"}"#
        );
        assert_eq!(local_host_load_config(&profile(32768, None, "claude")).unwrap()["contextLength"], 32768);
        assert!(local_host_load_config(&profile(65536, Some("q8_0"), "codex")).is_ok());
        assert_eq!(local_host_load_config(&profile(65536, Some("q8_0"), "claude")).unwrap_err(), "Unsupported local KV configuration");
        assert_eq!(local_host_load_config(&profile(16384, None, "codex")).unwrap_err(), "Unsupported local context configuration");
        let identity = sorted_host_identity(&validate_host(&host()).unwrap());
        assert_eq!(identity.as_object().unwrap().keys().collect::<Vec<_>>(), ["address", "comfyPort", "hostKeyAlias", "keyPath", "modelKey", "sshPort", "user"]);
    }
}
