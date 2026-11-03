// CLI 세션 기록을 읽어 대화 시점의 컨텍스트 사용량과 한도를 계산한다. Node의 session-context-usage.mjs를 옮겼다.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use serde::Serialize;
use serde_json::Value;

use crate::antigravity_local_state::antigravity_context_usage;
use crate::js::{date_parse, join_path, js_trim, now_milliseconds};
use crate::local_artifacts::{home_directory, modified_milliseconds};

const CLAUDE_CONTEXT_WINDOWS: [(&str, f64); 10] = [
    ("claude-fable-5", 1_000_000.0),
    ("fable", 1_000_000.0),
    ("claude-mythos-5", 1_000_000.0),
    ("claude-opus-5", 1_000_000.0),
    ("claude-opus-4-8", 1_000_000.0),
    ("claude-opus-4-7", 1_000_000.0),
    ("claude-opus-4-6", 1_000_000.0),
    ("claude-sonnet-5", 1_000_000.0),
    ("claude-sonnet-4-6", 1_000_000.0),
    ("claude-haiku-4-5", 200_000.0),
];

pub const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;
const MISSING_PATH_RETRY_MS: f64 = 30_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub used_tokens: f64,
    pub limit_tokens: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextEntry {
    pub at: f64,
    pub used_tokens: f64,
    pub limit_tokens: Option<f64>,
}

#[derive(Clone, Copy, PartialEq)]
enum TranscriptKind {
    Claude,
    Codex,
}

#[derive(Default)]
struct TranscriptReader {
    offset: u64,
    remainder: String,
    entries: Vec<ContextEntry>,
}

struct CachedPath {
    path: Option<String>,
    retry_at: f64,
}

static TRANSCRIPT_READERS: LazyLock<Mutex<HashMap<String, TranscriptReader>>> = LazyLock::new(Mutex::default);
static TRANSCRIPT_PATHS: LazyLock<Mutex<HashMap<String, CachedPath>>> = LazyLock::new(Mutex::default);

pub fn claude_context_window(model: Option<&str>) -> Option<f64> {
    let id = js_trim(model.unwrap_or("")).to_lowercase();
    if id.is_empty() {
        return None;
    }
    // Claude Code 선택값의 `[1m]` 접미사는 모델과 무관하게 1M 컨텍스트 창이다.
    if id.ends_with("[1m]") {
        return Some(1_000_000.0);
    }
    CLAUDE_CONTEXT_WINDOWS.iter().find(|(prefix, _)| id.starts_with(prefix)).map(|(_, window)| *window)
}

/// 기록 위치. 기본값은 Node와 같은 홈 아래 폴더다.
pub struct TranscriptRoots {
    pub claude: String,
    pub codex: String,
    pub antigravity: String,
}

impl Default for TranscriptRoots {
    fn default() -> Self {
        let home = home_directory();
        Self {
            claude: join_path(&[&home, ".claude", "projects"]),
            codex: join_path(&[&home, ".codex", "sessions"]),
            antigravity: join_path(&[&home, ".gemini", "antigravity-cli", "conversations"]),
        }
    }
}

pub struct ContextRequest<'a> {
    pub backend: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    /// 대화 시점(밀리초). None이면 지금이다.
    pub at: Option<f64>,
    pub context_window_override: Option<f64>,
}

pub fn session_context_usage(request: &ContextRequest, roots: &TranscriptRoots) -> Option<ContextUsage> {
    session_context_usage_with(request, roots, MAX_READ_BYTES, |session_id, at, root| {
        antigravity_context_usage(session_id, at, root)
    })
}

pub fn session_context_usage_with(
    request: &ContextRequest,
    roots: &TranscriptRoots,
    max_read_bytes: u64,
    antigravity_usage_reader: impl FnOnce(&str, f64, &str) -> Option<ContextUsage>,
) -> Option<ContextUsage> {
    let kind = js_trim(request.backend.unwrap_or(""));
    if !["claude", "codex", "antigravity"].contains(&kind) {
        return None;
    }
    let id = js_trim(request.session_id.unwrap_or(""));
    if id.is_empty() {
        return None;
    }
    let boundary = request.at.unwrap_or_else(now_milliseconds);
    if !boundary.is_finite() {
        return None;
    }
    if kind == "antigravity" {
        return antigravity_usage_reader(id, boundary, &roots.antigravity);
    }

    let kind = if kind == "claude" { TranscriptKind::Claude } else { TranscriptKind::Codex };
    let path = match kind {
        TranscriptKind::Claude => claude_transcript_path(id, &roots.claude),
        TranscriptKind::Codex => codex_rollout_path(id, &roots.codex),
    }?;
    let entry = transcript_latest_entry(kind, &path, max_read_bytes, boundary)?;
    let limit_tokens = request
        .context_window_override
        .or(entry.limit_tokens)
        .or_else(|| claude_context_window(request.model))
        .filter(|limit| *limit > 0.0)?;
    Some(ContextUsage { used_tokens: entry.used_tokens.min(limit_tokens), limit_tokens })
}

fn latest_entry(entries: &[ContextEntry], boundary: f64) -> Option<ContextEntry> {
    let mut matched = None;
    let mut limit_tokens = None;
    for entry in entries {
        if entry.at > boundary {
            break;
        }
        matched = Some(*entry);
        // 압축 경계 줄에는 모델이 없으므로 직전에 기록된 한도를 이어 쓴다.
        limit_tokens = entry.limit_tokens.or(limit_tokens);
    }
    matched.map(|entry| ContextEntry { limit_tokens, ..entry })
}

/// Node의 transcriptEntries + latestEntry. 같은 경로는 읽은 위치를 기억해 덧붙은 부분만 읽는다.
fn transcript_latest_entry(kind: TranscriptKind, path: &str, max_read_bytes: u64, boundary: f64) -> Option<ContextEntry> {
    let mut readers = TRANSCRIPT_READERS.lock().unwrap_or_else(|poison| poison.into_inner());
    let Ok(metadata) = fs::metadata(path) else {
        return latest_entry(readers.get(path).map_or(&[][..], |reader| &reader.entries), boundary);
    };
    let size = metadata.len();
    if readers.get(path).is_none_or(|reader| size < reader.offset) {
        readers.insert(path.to_string(), TranscriptReader::default());
    }
    let mut length = size - readers[path].offset;
    if length > 0 {
        let mut discards_leading_fragment = false;
        if length > max_read_bytes {
            let offset = size.saturating_sub(max_read_bytes);
            readers.insert(path.to_string(), TranscriptReader { offset, ..TranscriptReader::default() });
            length = size - offset;
            discards_leading_fragment = offset > 0;
        }
        let reader = readers.get_mut(path).expect("방금 넣은 읽기 상태");
        if let Ok(file) = fs::File::open(path) {
            let mut buffer = vec![0; length as usize];
            if let Ok(read) = file.read_at(&mut buffer, reader.offset) {
                reader.offset += read as u64;
                let text = format!("{}{}", reader.remainder, String::from_utf8_lossy(&buffer[..read]));
                let mut lines: VecDeque<&str> = text.split('\n').collect();
                reader.remainder = lines.pop_back().unwrap_or("").to_string();
                if discards_leading_fragment {
                    lines.pop_front();
                }
                for line in lines {
                    let entry = match kind {
                        TranscriptKind::Claude => claude_context_entry(line),
                        TranscriptKind::Codex => codex_context_entry(line),
                    };
                    reader.entries.extend(entry);
                }
            }
        }
    }
    latest_entry(&readers[path].entries, boundary)
}

pub fn claude_context_entry(line: &str) -> Option<ContextEntry> {
    if !line.contains("\"usage\"") && !line.contains("compact_boundary") {
        return None;
    }
    let record: Value = serde_json::from_str(line).ok()?;
    if record.get("type").and_then(Value::as_str) == Some("system")
        && record.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
    {
        let metadata = nullish(record.get("compact_metadata")).or_else(|| nullish(record.get("compactMetadata")));
        let post_tokens = metadata.and_then(|metadata| {
            nullish(metadata.get("post_tokens")).or_else(|| nullish(metadata.get("postTokens")))
        });
        let used_tokens = token_count(post_tokens);
        let at = timestamp(&record)?;
        if used_tokens <= 0.0 {
            return None;
        }
        return Some(ContextEntry { at, used_tokens, limit_tokens: None });
    }
    if record.get("type").and_then(Value::as_str) != Some("assistant") || record.get("isSidechain") == Some(&Value::Bool(true)) {
        return None;
    }
    let message = record.get("message")?;
    let usage = message.get("usage")?;
    let used_tokens = token_count(usage.get("input_tokens"))
        + token_count(usage.get("cache_read_input_tokens"))
        + token_count(usage.get("cache_creation_input_tokens"));
    let at = timestamp(&record)?;
    if used_tokens <= 0.0 {
        return None;
    }
    // 설정값은 `opus` 같은 별칭일 수 있어 실제 응답에 기록된 모델 ID로 한도를 찾는다.
    Some(ContextEntry { at, used_tokens, limit_tokens: claude_context_window(message.get("model").and_then(Value::as_str)) })
}

pub fn codex_context_entry(line: &str) -> Option<ContextEntry> {
    if !line.contains("\"token_count\"") {
        return None;
    }
    let record: Value = serde_json::from_str(line).ok()?;
    let payload = record.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let info = payload.get("info");
    let used_tokens = token_count(info.and_then(|info| info.get("last_token_usage")?.get("input_tokens")));
    let limit_tokens = token_count(info.and_then(|info| info.get("model_context_window")));
    let at = timestamp(&record)?;
    if used_tokens <= 0.0 {
        return None;
    }
    Some(ContextEntry { at, used_tokens, limit_tokens: (limit_tokens > 0.0).then_some(limit_tokens) })
}

fn nullish(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// `Date.parse(record.timestamp)`. 문자열이 아니거나 ISO 꼴이 아니면 NaN이라 줄을 버린다.
fn timestamp(record: &Value) -> Option<f64> {
    date_parse(record.get("timestamp")?.as_str()?)
}

fn token_count(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).filter(|count| count.is_finite() && *count > 0.0).unwrap_or(0.0)
}

fn claude_transcript_path(session_id: &str, root: &str) -> Option<String> {
    if fs::metadata(root).is_err() {
        return None;
    }
    let projects = fs::read_dir(root).ok()?.collect::<Result<Vec<_>, _>>().ok()?;
    let mut candidates: Vec<(String, u64, f64)> = Vec::new();
    for project in projects {
        if !project.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = join_path(&[root, &project.file_name().to_string_lossy(), &format!("{session_id}.jsonl")]);
        // 동시에 정리된 과거 worktree 기록은 건너뛴다.
        if let Ok(metadata) = fs::metadata(&path)
            && metadata.is_file()
        {
            candidates.push((path, metadata.len(), modified_milliseconds(&metadata)));
        }
    }
    // Node는 같은 시각·크기일 때 localeCompare로 고른다. 여기서는 바이트 순서로 둔다.
    candidates.sort_by(|left, right| {
        right.2.total_cmp(&left.2).then(right.1.cmp(&left.1)).then_with(|| left.0.cmp(&right.0))
    });
    candidates.into_iter().next().map(|(path, _, _)| path)
}

fn codex_rollout_path(session_id: &str, root: &str) -> Option<String> {
    cached_path(&format!("codex:{session_id}:{root}"), || {
        if fs::metadata(root).is_err() {
            return None;
        }
        let relative = recursive_entries(root)?
            .into_iter()
            .find(|entry| entry.ends_with(".jsonl") && entry.contains(session_id))?;
        Some(join_path(&[root, &relative]))
    })
}

/// fs.readdirSync(root, { recursive: true }). 폴더마다 이름순(libuv scandir)으로 읽고 너비 우선으로 내려간다.
pub(crate) fn recursive_entries(root: &str) -> Option<Vec<String>> {
    let mut results = Vec::new();
    let mut queue = VecDeque::from([root.to_string()]);
    while let Some(directory) = queue.pop_front() {
        let mut names: Vec<String> = fs::read_dir(&directory)
            .ok()?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()
            .ok()?;
        names.sort();
        for name in names {
            let path = join_path(&[&directory, &name]);
            let relative = path.strip_prefix(root).unwrap_or(&path).trim_start_matches('/').to_string();
            results.push(relative);
            if Path::new(&path).is_dir() {
                queue.push_back(path);
            }
        }
    }
    Some(results)
}

fn cached_path(key: &str, resolve: impl FnOnce() -> Option<String>) -> Option<String> {
    let now = now_milliseconds();
    {
        let paths = TRANSCRIPT_PATHS.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(cached) = paths.get(key) {
            if let Some(path) = &cached.path
                && Path::new(path).exists()
            {
                return Some(path.clone());
            }
            if cached.path.is_none() && now < cached.retry_at {
                return None;
            }
        }
    }
    let path = resolve();
    TRANSCRIPT_PATHS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(key.to_string(), CachedPath { path: path.clone(), retry_at: now_milliseconds() + MISSING_PATH_RETRY_MS });
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    struct TempDirectory(String);

    impl TempDirectory {
        fn new(prefix: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("{prefix}{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path.to_string_lossy().into_owned())
        }

        fn join(&self, parts: &[&str]) -> String {
            let mut all = vec![self.0.as_str()];
            all.extend(parts);
            join_path(&all)
        }

        fn roots(&self) -> TranscriptRoots {
            TranscriptRoots { claude: self.0.clone(), codex: self.0.clone(), antigravity: self.0.clone() }
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn claude_assistant_line(timestamp: &str, input: u64, cache_read: u64, cache_write: u64, is_sidechain: bool) -> String {
        json!({
            "type": "assistant",
            "isSidechain": is_sidechain,
            "timestamp": timestamp,
            "message": {
                "model": "claude-opus-5",
                "usage": {
                    "input_tokens": input,
                    "cache_read_input_tokens": cache_read,
                    "cache_creation_input_tokens": cache_write,
                    "output_tokens": 100,
                },
            },
        })
        .to_string()
    }

    fn codex_token_count_line(timestamp: &str, last_input: u64, window: u64) -> String {
        json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": { "input_tokens": 999_999, "output_tokens": 1 },
                    "last_token_usage": { "input_tokens": last_input, "output_tokens": 1 },
                    "model_context_window": window,
                },
            },
        })
        .to_string()
    }

    fn at(iso: &str) -> Option<f64> {
        date_parse(iso)
    }

    fn usage(used: f64, limit: f64) -> Option<ContextUsage> {
        Some(ContextUsage { used_tokens: used, limit_tokens: limit })
    }

    fn request<'a>(backend: &'a str, session_id: Option<&'a str>, model: Option<&'a str>, when: &str) -> ContextRequest<'a> {
        ContextRequest { backend: Some(backend), session_id, model, at: at(when), context_window_override: None }
    }

    fn set_modified(path: &str, milliseconds: u64) {
        let time = SystemTime::UNIX_EPOCH + Duration::from_millis(milliseconds);
        fs::File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
    }

    #[test]
    fn claude_기록은_입력과_캐시를_합쳐_컨텍스트_점유로_계산한다() {
        let entry = claude_context_entry(&claude_assistant_line("2026-07-31T19:40:22.710Z", 1, 37_274, 231_269, false)).unwrap();
        assert_eq!(entry.used_tokens, 268_544.0);
        assert_eq!(entry.limit_tokens, Some(1_000_000.0));
    }

    #[test]
    fn claude_압축_경계는_압축_뒤_컨텍스트_점유를_반영한다() {
        let line = json!({
            "type": "system",
            "subtype": "compact_boundary",
            "timestamp": "2026-08-20T01:02:03.000Z",
            "compact_metadata": { "pre_tokens": 905_000, "post_tokens": 47_500 },
        });
        let entry = claude_context_entry(&line.to_string()).unwrap();
        assert_eq!(entry.used_tokens, 47_500.0);
        assert_eq!(entry.limit_tokens, None);
        assert_eq!(Some(entry.at), at("2026-08-20T01:02:03.000Z"));
    }

    #[test]
    fn claude_부속_대화와_사용량_없는_줄은_건너뛴다() {
        assert_eq!(claude_context_entry(&claude_assistant_line("2026-07-31T19:40:22.710Z", 1, 10, 0, true)), None);
        let user = json!({ "type": "user", "timestamp": "2026-07-31T19:40:00.000Z" });
        assert_eq!(claude_context_entry(&user.to_string()), None);
    }

    #[test]
    fn codex_기록은_마지막_요청_입력과_모델_한도를_사용한다() {
        let entry = codex_context_entry(&codex_token_count_line("2026-08-01T05:05:06.005Z", 57_218, 258_400)).unwrap();
        assert_eq!(entry.used_tokens, 57_218.0);
        assert_eq!(entry.limit_tokens, Some(258_400.0));
    }

    #[test]
    fn antigravity_세션은_로컬_대화_메타데이터의_실제_한도를_사용한다() {
        let mut seen = None;
        let roots = TranscriptRoots {
            claude: String::new(),
            codex: String::new(),
            antigravity: "/tmp/antigravity-conversations".into(),
        };
        let result = session_context_usage_with(
            &request("antigravity", Some("11111111-2222-4333-8444-555555555555"), Some("gemini-3.7-flash"), "2026-08-28T00:00:00Z"),
            &roots,
            MAX_READ_BYTES,
            |session_id, boundary, root| {
                seen = Some((session_id.to_string(), boundary, root.to_string()));
                usage(58_559.0, 256_000.0)
            },
        );
        assert_eq!(result, usage(58_559.0, 256_000.0));
        let (session_id, boundary, root) = seen.unwrap();
        assert_eq!(session_id, "11111111-2222-4333-8444-555555555555");
        assert_eq!(root, "/tmp/antigravity-conversations");
        assert_eq!(Some(boundary), at("2026-08-28T00:00:00Z"));
    }

    #[test]
    fn 누적_사용량은_컨텍스트_점유로_오인하지_않는다() {
        let entry = codex_context_entry(&codex_token_count_line("2026-08-01T05:05:06.005Z", 38_195, 258_400)).unwrap();
        assert_ne!(entry.used_tokens, 999_999.0);
    }

    #[test]
    fn claude_모델_문자열로_컨텍스트_한도를_찾는다() {
        assert_eq!(claude_context_window(Some("fable")), Some(1_000_000.0));
        assert_eq!(claude_context_window(Some("claude-opus-5")), Some(1_000_000.0));
        assert_eq!(claude_context_window(Some("claude-sonnet-5")), Some(1_000_000.0));
        assert_eq!(claude_context_window(Some("claude-haiku-4-5-20251001")), Some(200_000.0));
        assert_eq!(claude_context_window(Some("gpt-5.6-sol")), None);
        assert_eq!(claude_context_window(None), None);
    }

    #[test]
    fn claude_세션은_모델_기본_컨텍스트_한도를_사용한다() {
        let root = TempDirectory::new("officellm-claude-");
        fs::create_dir(root.join(&["-Users-neo-office"])).unwrap();
        let session_id = "2206f58e-0bd8-43dd-8068-780090adbaa8";
        let lines = [
            claude_assistant_line("2026-07-31T19:30:00.000Z", 2, 36_310, 964, false),
            claude_assistant_line("2026-07-31T19:40:22.710Z", 1, 37_274, 231_269, false),
            claude_assistant_line("2026-07-31T20:10:00.000Z", 5, 900_000, 0, false),
        ];
        fs::write(root.join(&["-Users-neo-office", &format!("{session_id}.jsonl")]), lines.join("\n") + "\n").unwrap();
        assert_eq!(
            session_context_usage(&request("claude", Some(session_id), Some("claude-opus-5"), "2026-07-31T19:40:23.259Z"), &root.roots()),
            usage(268_544.0, 1_000_000.0)
        );
    }

    #[test]
    fn claude_세션은_설정이_별칭이어도_기록된_모델_id로_한도를_찾는다() {
        let root = TempDirectory::new("officellm-claude-alias-");
        fs::create_dir(root.join(&["-Users-neo-office"])).unwrap();
        let session_id = "dcff4a46-7a34-459a-baa8-8ee75956377f";
        let compact = json!({
            "type": "system",
            "subtype": "compact_boundary",
            "timestamp": "2026-09-27T11:00:00.000Z",
            "compact_metadata": { "pre_tokens": 800_000, "post_tokens": 40_000 },
        });
        let lines = [claude_assistant_line("2026-09-27T10:00:00.000Z", 3, 120_000, 500, false), compact.to_string()];
        fs::write(root.join(&["-Users-neo-office", &format!("{session_id}.jsonl")]), lines.join("\n") + "\n").unwrap();
        let read = |when: &str| session_context_usage(&request("claude", Some(session_id), Some("opus"), when), &root.roots());
        assert_eq!(read("2026-09-27T10:30:00.000Z"), usage(120_503.0, 1_000_000.0));
        assert_eq!(read("2026-09-27T11:30:00.000Z"), usage(40_000.0, 1_000_000.0));
    }

    #[test]
    fn claude_세션은_중복_기록_중_마지막으로_쓰인_경로를_다시_고른다() {
        let root = TempDirectory::new("officellm-claude-duplicates-");
        fs::create_dir(root.join(&["a-stale"])).unwrap();
        fs::create_dir(root.join(&["z-active"])).unwrap();
        let session_id = "2206f58e-0bd8-43dd-8068-780090adbaa8";
        let stale = root.join(&["a-stale", &format!("{session_id}.jsonl")]);
        let active = root.join(&["z-active", &format!("{session_id}.jsonl")]);
        fs::write(
            &stale,
            claude_assistant_line("2026-07-31T19:30:00.000Z", 2, 100, 0, false) + "\n" + &"x".repeat(4_000) + "\n",
        )
        .unwrap();
        set_modified(&stale, 1_000);
        let read = || session_context_usage(&request("claude", Some(session_id), Some("claude-opus-5"), "2026-07-31T20:00:00.000Z"), &root.roots());
        assert_eq!(read(), usage(102.0, 1_000_000.0));

        let lines = [
            claude_assistant_line("2026-07-31T19:30:00.000Z", 2, 100, 0, false),
            claude_assistant_line("2026-07-31T19:40:00.000Z", 3, 700, 11, false),
        ];
        fs::write(&active, lines.join("\n") + "\n").unwrap();
        set_modified(&active, 2_000);
        assert_eq!(read(), usage(714.0, 1_000_000.0));
    }

    #[test]
    fn 큰_claude_기록은_마지막_구간을_읽어_최신_점유를_유지한다() {
        let root = TempDirectory::new("officellm-claude-large-");
        fs::create_dir(root.join(&["project"])).unwrap();
        let session_id = "2206f58e-0bd8-43dd-8068-780090adbaa8";
        fs::write(
            root.join(&["project", &format!("{session_id}.jsonl")]),
            format!("{}\n{}\n", "x".repeat(2_000), claude_assistant_line("2026-07-31T19:40:00.000Z", 4, 800, 20, false)),
        )
        .unwrap();
        let result = session_context_usage_with(
            &request("claude", Some(session_id), Some("claude-opus-5"), "2026-07-31T20:00:00.000Z"),
            &root.roots(),
            1_024,
            |_, _, _| None,
        );
        assert_eq!(result, usage(824.0, 1_000_000.0));
    }

    #[test]
    fn 이어붙은_기록은_다시_읽어_최신_점유를_반영한다() {
        let root = TempDirectory::new("officellm-codex-");
        let day = root.join(&["2026", "08", "01"]);
        fs::create_dir_all(&day).unwrap();
        let session_id = "019fbbb6-0c8f-7ac2-b1c7-755cf64a5440";
        let path = join_path(&[&day, &format!("rollout-2026-08-01T14-04-58-{session_id}.jsonl")]);
        fs::write(&path, codex_token_count_line("2026-08-01T05:05:06.005Z", 38_195, 258_400) + "\n").unwrap();
        let read = || session_context_usage(&request("codex", Some(session_id), None, "2026-08-01T06:00:00.000Z"), &root.roots());
        assert_eq!(read(), usage(38_195.0, 258_400.0));

        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all((codex_token_count_line("2026-08-01T05:30:00.000Z", 57_218, 258_400) + "\n").as_bytes()).unwrap();
        assert_eq!(read(), usage(57_218.0, 258_400.0));
    }

    #[test]
    fn 기록이나_한도를_찾지_못하면_값을_만들지_않는다() {
        let root = TempDirectory::new("officellm-empty-");
        let roots = root.roots();
        assert_eq!(
            session_context_usage(&request("claude", Some("missing-session"), Some("claude-opus-5"), "2026-08-01T06:00:00.000Z"), &roots),
            None
        );
        assert_eq!(session_context_usage(&request("claude", None, Some("claude-opus-5"), "2026-08-01T06:00:00.000Z"), &roots), None);
    }

    #[test]
    fn 재귀_목록은_node처럼_너비_우선과_이름순이다() {
        let root = TempDirectory::new("officellm-recursive-");
        fs::create_dir_all(root.join(&["b", "z"])).unwrap();
        fs::create_dir_all(root.join(&["a"])).unwrap();
        fs::write(root.join(&["c.jsonl"]), "").unwrap();
        fs::write(root.join(&["a", "y.jsonl"]), "").unwrap();
        fs::write(root.join(&["b", "z", "x.jsonl"]), "").unwrap();
        assert_eq!(
            recursive_entries(&root.0).unwrap(),
            ["a", "b", "c.jsonl", "a/y.jsonl", "b/z", "b/z/x.jsonl"]
        );
    }
}
