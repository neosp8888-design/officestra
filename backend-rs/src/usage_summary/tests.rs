// 사용량 요약 모듈 시험. 기대값은 Node usage-summary.test.mjs와 Node 24에서 직접 계산한 값이다.

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::{HeaderMap, Method, StatusCode, Uri};

use super::*;

fn window(remaining: Option<i64>, reset_at: Option<&str>) -> Option<LimitWindow> {
    Some(LimitWindow { remaining, reset_at: reset_at.map(str::to_string) })
}

fn text(value: &str) -> Value {
    Value::String(value.into())
}

fn milliseconds(iso: &str) -> f64 {
    date_parse(iso).unwrap()
}

/// Node Buffer.toString("base64url")와 같은 패딩 없는 인코딩.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let value =
            chunk.iter().enumerate().fold(0_u32, |value, (index, byte)| value | (u32::from(*byte) << (16 - 8 * index)));
        for index in 0..=chunk.len() {
            output.push(char::from(ALPHABET[((value >> (18 - 6 * index)) & 63) as usize]));
        }
    }
    output
}

// ── 파서 ──

#[test]
fn codex_windows_follow_window_length() {
    let limits = parse_codex_rate_limits(Some(&json!({
        "rateLimits": {
            "planType": "pro",
            "primary": {"usedPercent": 24.6, "windowDurationMins": 300, "resetsAt": 1_800_000_000},
            "secondary": {"usedPercent": 41, "windowDurationMins": 10_080, "resetsAt": 1_800_600_000},
        },
    })));
    assert_eq!(
        limits,
        CodexLimits {
            five_hour: window(Some(75), Some("2027-01-15T08:00:00.000Z")),
            weekly: window(Some(59), Some("2027-01-22T06:40:00.000Z")),
            plan: Some("Pro 20x".into()),
            subscription_expires_at: None,
        }
    );
}

#[test]
fn codex_parser_coerces_like_node() {
    let loose = parse_codex_rate_limits(Some(&json!({
        "rateLimits": {
            "planType": "prolite",
            "primary": {"usedPercent": "40", "windowDurationMins": "300", "resetsAt": null},
            "secondary": "x",
        },
    })));
    assert_eq!(loose.five_hour, window(Some(60), Some("1970-01-01T00:00:00.000Z")));
    assert_eq!(loose.weekly, None);
    assert_eq!(loose.plan.as_deref(), Some("Pro 5x"));

    let empty = CodexLimits::default();
    assert_eq!(parse_codex_rate_limits(Some(&text("abc"))), empty);
    assert_eq!(parse_codex_rate_limits(Some(&Value::Null)), empty);
    assert_eq!(parse_codex_rate_limits(None), empty);

    let top_level = parse_codex_rate_limits(Some(&json!({
        "primary": {"usedPercent": -5, "windowDurationMins": 10_080, "resetsAt": "1800000000"},
        "planType": " team_plus ",
    })));
    assert_eq!(top_level.five_hour, None);
    assert_eq!(top_level.weekly, window(Some(100), Some("2027-01-15T08:00:00.000Z")));
    assert_eq!(top_level.plan.as_deref(), Some("Team Plus"));
}

#[test]
fn codex_subscription_expiration_reads_only_the_claim() {
    let claims = json!({
        "https://api.openai.com/auth": {"chatgpt_subscription_active_until": "2026-08-31T14:54:17+00:00"},
        "exp": 1_900_000_000,
    });
    let raw = json!({"tokens": {"id_token": format!("header.{}.signature", base64url(claims.to_string().as_bytes()))}});
    assert_eq!(parse_codex_subscription_expiration(&raw.to_string()).as_deref(), Some("2026-08-31T14:54:17.000Z"));
    assert_eq!(parse_codex_subscription_expiration("{}"), None);
    assert_eq!(parse_codex_subscription_expiration("not-json"), None);

    let numeric = json!({"https://api.openai.com/auth": {"chatgpt_subscription_active_until": 1_900_000_000}});
    let raw = json!({"tokens": {"id_token": format!("a.{}.c", base64url(numeric.to_string().as_bytes()))}});
    assert_eq!(parse_codex_subscription_expiration(&raw.to_string()).as_deref(), Some("2030-03-17T17:46:40.000Z"));
    assert_eq!(parse_codex_subscription_expiration(r#"{"tokens":{"id_token":"a..c"}}"#), None);
}

#[test]
fn base64url_decode_matches_node_buffer() {
    let cases: [(&str, &[u8]); 9] = [
        ("TWFu", &[77, 97, 110]),
        ("TWE", &[77, 97]),
        ("TW=Fu", &[77]),
        ("T😀WFu", &[]),
        ("TW Fu", &[77, 97, 110]),
        ("TWFuTWFu==", &[77, 97, 110, 77, 97, 110]),
        ("-_-_", &[251, 255, 191]),
        ("TĽWFu", &[]),
        ("QUJDĽREVG", &[65, 66, 67]),
    ];
    for (input, expected) in cases {
        assert_eq!(base64url_decode(input), expected, "{input}");
    }
}

#[test]
fn claude_utilization_becomes_remaining_percent() {
    let limits = parse_claude_rate_limits(
        &json!({
            "five_hour": {"utilization": 63, "resets_at": "2026-08-12T10:00:00Z"},
            "seven_day": {"utilization": 11, "resets_at": "2026-08-13T00:00:00Z"},
        }),
        None,
        Some(&text("max")),
    );
    assert_eq!(
        limits,
        ClaudeLimits {
            five_hour: window(Some(37), Some("2026-08-12T10:00:00.000Z")),
            weekly: window(Some(89), Some("2026-08-13T00:00:00.000Z")),
            model_weekly_limits: Vec::new(),
            plan: Some("Max".into()),
        }
    );
}

fn model_limit(id: &str, name: &str, remaining: Option<i64>, reset_at: Option<&str>) -> ModelWeeklyLimit {
    ModelWeeklyLimit { id: id.into(), name: name.into(), remaining, reset_at: reset_at.map(str::to_string) }
}

#[test]
fn claude_model_weekly_limits_keep_inactive_zero() {
    let limits = parse_claude_rate_limits(
        &json!({
            "five_hour": {"utilization": 0, "resets_at": null},
            "seven_day": {"utilization": 18, "resets_at": "2026-10-22T00:00:00Z"},
            "limits": [
                {"kind": "weekly_all", "percent": 18, "scope": null},
                {"kind": "weekly_scoped", "percent": 0, "is_active": false,
                 "resets_at": "2026-10-22T00:00:00Z", "scope": {"model": {"id": null, "display_name": "Fable"}}},
                {"kind": "weekly_scoped", "percent": 100, "scope": {"model": {"id": "opus", "display_name": "Opus"}}},
                {"kind": "weekly_scoped", "percent": null, "scope": {"model": {"id": "unknown", "display_name": "Unknown"}}},
                {"kind": "weekly_scoped", "percent": 25, "scope": {"surface": "cowork"}},
            ],
        }),
        None,
        None,
    );
    assert_eq!(limits.weekly.and_then(|weekly| weekly.remaining), Some(82));
    assert_eq!(
        limits.model_weekly_limits,
        [
            model_limit("Fable", "Fable", Some(100), Some("2026-10-22T00:00:00.000Z")),
            model_limit("opus", "Opus", Some(0), None),
            model_limit("unknown", "Unknown", None, None),
        ]
    );

    let coerced = parse_claude_rate_limits(
        &json!({"limits": [
            {"kind": "weekly_scoped", "percent": "", "resets_at": true, "scope": {"model": {"id": 0, "display_name": 7}}},
            {"kind": "weekly_scoped", "percent": 12.5, "resets_at": ["2026-01-01"],
             "scope": {"model": {"id": "m", "display_name": {}}}},
            {"kind": "weekly_scoped", "percent": [30], "resets_at": 1.5, "scope": {"model": {"display_name": "x"}}},
            {"kind": "weekly_scoped", "percent": false, "resets_at": {}, "scope": {"model": {"display_name": "y"}}},
        ]}),
        None,
        None,
    );
    assert_eq!(
        coerced.model_weekly_limits,
        [
            model_limit("7", "7", None, Some("1970-01-01T00:00:00.001Z")),
            model_limit("m", "[object Object]", Some(88), Some("2026-01-01T00:00:00.000Z")),
            model_limit("x", "x", Some(70), Some("1970-01-01T00:00:01.500Z")),
            model_limit("y", "y", Some(100), None),
        ]
    );
}

#[test]
fn claude_plan_badge_follows_tier_then_subscription() {
    let empty = json!({"five_hour": null, "seven_day": null});
    let plan = |tier: Option<&str>, kind: Option<&str>| {
        let (tier, kind) = (tier.map(text), kind.map(text));
        parse_claude_rate_limits(&empty, tier.as_ref(), kind.as_ref()).plan
    };
    assert_eq!(plan(Some("default_claude_max_5x"), None).as_deref(), Some("Max 5x"));
    // 티어 문자열을 그대로 쓰면 "Default Claude Ai"가 배지에 뜬다.
    assert_eq!(plan(Some("default_claude_ai"), Some("pro")).as_deref(), Some("Pro"));
    assert_eq!(plan(Some("default_claude_ai"), None), None);
    assert_eq!(plan(Some("MAX 20X"), None).as_deref(), Some("Max 20x"));
    assert_eq!(plan(None, Some(" team-plan_x ")).as_deref(), Some("Team Plan X"));
    assert_eq!(plan(None, Some("-_-")).as_deref(), Some(""));
    assert_eq!(plan(None, Some("\u{10428}abc \u{df}eta")).as_deref(), Some("\u{10428}abc SSeta"));

    let odd = parse_claude_rate_limits(&json!({"five_hour": [], "seven_day": "x"}), None, None);
    assert_eq!(odd.five_hour, window(None, None));
    assert_eq!(odd.weekly, None);
    assert_eq!(parse_claude_rate_limits(&json!([]), None, None), ClaudeLimits::default());
}

fn quota_bucket_value(id: &str, window: &str, remaining: Option<i64>, reset_at: Option<&str>) -> QuotaBucket {
    QuotaBucket {
        id: id.into(),
        name: id.into(),
        window: window.into(),
        remaining,
        reset_at: reset_at.map(str::to_string),
    }
}

#[test]
fn antigravity_weekly_comes_from_the_gemini_bucket() {
    let limits = parse_antigravity_rate_limits(
        &json!({"response": {"groups": [{
            "displayName": "Gemini Models",
            "buckets": [{"bucketId": "gemini-weekly", "window": "weekly", "remainingFraction": 0.852_948_8,
                         "resetTime": "2026-09-03T13:44:43Z"}],
        }]}}),
        "",
    )
    .unwrap();
    assert_eq!(
        limits,
        AntigravityLimits {
            five_hour: None,
            weekly: window(Some(85), Some("2026-09-03T13:44:43.000Z")),
            plan: Some("Free".into()),
            groups: vec![QuotaGroup {
                id: "gemini".into(),
                name: "Gemini Models".into(),
                buckets: vec![quota_bucket_value("gemini-weekly", "weekly", Some(85), Some("2026-09-03T13:44:43.000Z"))],
            }],
            image_reset_at: None,
        }
    );
}

#[test]
fn antigravity_five_hour_bucket_means_google_ai_pro() {
    let limits = parse_antigravity_rate_limits(
        &json!({"response": {"groups": [{"displayName": "Gemini Models", "buckets": [
            {"bucketId": "gemini-weekly", "window": "weekly", "remainingFraction": 0.91},
            {"bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.73},
        ]}]}}),
        "",
    )
    .unwrap();
    assert_eq!(limits.five_hour.and_then(|window| window.remaining), Some(73));
    assert_eq!(limits.plan.as_deref(), Some("Google AI Pro"));

    let third_party = parse_antigravity_rate_limits(
        &json!({"response": {"groups": [
            {"displayName": "Gemini Models", "buckets": [{"bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.96}]},
            {"displayName": "Claude and GPT models", "buckets": [{"bucketId": "3p-5h", "window": "5h", "remainingFraction": 1}]},
        ]}}),
        "",
    )
    .unwrap();
    assert_eq!(third_party.plan.as_deref(), Some("Google AI Pro"));
}

fn argon_fixture() -> Value {
    json!({"response": {"groups": [
        {"displayName": "Gemini Models", "buckets": [
            {"bucketId": "gemini-weekly", "window": "weekly", "remainingFraction": 1},
            {"bucketId": "gemini-5h", "window": "5h", "remainingFraction": 1},
        ]},
        {"displayName": "Claude and GPT models", "buckets": [
            {"bucketId": "3p-weekly", "window": "weekly", "remainingFraction": 1},
        ]},
        {"displayName": "Gemini 4 Argon", "buckets": [
            {"bucketId": "gemini-argon-wtu-weekly", "window": "weekly",
             "remainingFraction": 0.486_284, "resetTime": "2026-10-24T05:44:32Z"},
        ]},
    ]}})
}

fn groups_mut(payload: &mut Value) -> &mut Vec<Value> {
    payload["response"]["groups"].as_array_mut().unwrap()
}

#[test]
fn antigravity_argon_reads_its_own_weekly_limit() {
    let limits = parse_antigravity_rate_limits(&argon_fixture(), "gemini-4-argon-high").unwrap();
    assert_eq!(limits.five_hour, None);
    assert_eq!(limits.weekly, window(Some(49), Some("2026-10-24T05:44:32.000Z")));
    assert_eq!(limits.plan.as_deref(), Some("Google AI Pro"));
    let ids: Vec<&str> = limits.groups.iter().map(|group| group.id.as_str()).collect();
    assert_eq!(ids, ["gemini", "third-party", "argon"]);
    let first_remaining: Vec<Option<i64>> = limits.groups.iter().map(|group| group.buckets[0].remaining).collect();
    assert_eq!(first_remaining, [Some(100), Some(100), Some(49)]);

    let mut reversed = argon_fixture();
    groups_mut(&mut reversed).reverse();
    let gemini = parse_antigravity_rate_limits(&reversed, "gemini-3.8-flash").unwrap();
    assert_eq!(gemini.weekly.and_then(|window| window.remaining), Some(100));
    assert_eq!(gemini.five_hour.and_then(|window| window.remaining), Some(100));

    let mut without_argon = argon_fixture();
    groups_mut(&mut without_argon).pop();
    let missing = parse_antigravity_rate_limits(&without_argon, "gemini-4-argon").unwrap();
    assert_eq!((missing.weekly, missing.five_hour, missing.groups.len()), (None, None, 2));
}

#[test]
fn antigravity_argon_missing_fraction_is_not_zero() {
    let mut payload = argon_fixture();
    payload["response"]["groups"][2]["buckets"][0]["remainingFraction"] = Value::Null;
    let missing = parse_antigravity_rate_limits(&payload, "gemini-4-argon").unwrap();
    assert_eq!(missing.weekly, None);
    assert_eq!(missing.groups[2].buckets[0].remaining, None);
    payload["response"]["groups"][2]["buckets"][0]["remainingFraction"] = json!(0);
    let zero = parse_antigravity_rate_limits(&payload, "gemini-4-argon").unwrap();
    assert_eq!(zero.weekly.and_then(|window| window.remaining), Some(0));
}

#[test]
fn antigravity_groups_keep_resets_and_new_groups() {
    let mut payload = argon_fixture();
    payload["response"]["groups"][0]["buckets"][0]["resetTime"] = text("2026-10-25T01:00:00Z");
    groups_mut(&mut payload).push(json!({"displayName": "New Model", "buckets": [
        {"bucketId": "new-weekly", "window": "weekly", "remainingFraction": 0.25, "resetTime": "2026-10-26T02:00:00Z"},
    ]}));
    let limits = parse_antigravity_rate_limits(&payload, "claude-opus-5-5").unwrap();
    assert_eq!(limits.weekly.and_then(|window| window.remaining), Some(100));
    assert_eq!(limits.groups.len(), 4);
    assert_eq!(limits.groups[0].buckets[0].reset_at.as_deref(), Some("2026-10-25T01:00:00.000Z"));
    assert_eq!(limits.groups[2].buckets[0].reset_at.as_deref(), Some("2026-10-24T05:44:32.000Z"));
    assert_eq!(limits.groups[3].buckets[0].remaining, Some(25));
}

#[test]
fn antigravity_rejects_payloads_like_node() {
    let no_remaining = json!({"groups": [{"displayName": "Gemini Models", "buckets": [{"bucketId": "gemini-weekly"}]}]});
    let message = "Antigravity 계정 한도 응답에 Gemini 잔여량이 없습니다.";
    assert_eq!(parse_antigravity_rate_limits(&no_remaining, ""), Err(message.into()));
    assert_eq!(parse_antigravity_rate_limits(&Value::Null, ""), Err(message.into()));
    assert_eq!(parse_antigravity_rate_limits(&json!({"groups": {}}), ""), Err("groups.map is not a function".into()));
    assert_eq!(
        parse_antigravity_rate_limits(
            &json!({"groups": [{"buckets": [{"bucketId": "x", "remainingFraction": 1}]}, {"buckets": {}}]}),
            ""
        ),
        Err("((intermediate value) ?? []).map is not a function".into())
    );
}

#[test]
fn antigravity_odd_groups_follow_node() {
    let third_party = parse_antigravity_rate_limits(
        &json!({"response": {"groups": null}, "groups": [
            {"displayName": "Mystery", "buckets": [
                {"bucketId": "", "displayName": null, "window": "MONTH", "remainingFraction": "0.285", "resetTime": 0},
            ]},
            {"buckets": [{"bucketId": "3p-5h", "remainingFraction": -1}]},
            {"displayName": "", "buckets": []},
        ]}),
        "GPT-5",
    )
    .unwrap();
    assert_eq!(
        third_party,
        AntigravityLimits {
            five_hour: window(Some(0), None),
            weekly: None,
            plan: Some("Free".into()),
            groups: vec![
                QuotaGroup {
                    id: "Mystery".into(),
                    name: "Mystery".into(),
                    buckets: vec![quota_bucket_value("", "month", Some(28), Some("1970-01-01T00:00:00.000Z"))],
                },
                QuotaGroup {
                    id: "third-party".into(),
                    name: String::new(),
                    buckets: vec![quota_bucket_value("3p-5h", "fiveHour", Some(0), None)],
                },
                QuotaGroup { id: String::new(), name: String::new(), buckets: Vec::new() },
            ],
            image_reset_at: None,
        }
    );

    let missing_group = parse_antigravity_rate_limits(
        &json!({"groups": [{"displayName": "x", "buckets": [
            {"bucketId": "gemini-5h", "remainingFraction": null},
            {"bucketId": "gemini-five-hour", "remainingFraction": 0.5},
            {"bucketId": "a", "window": "Week", "remainingFraction": 2},
        ]}]}),
        "o3-mini",
    )
    .unwrap();
    assert_eq!((missing_group.five_hour, missing_group.weekly), (None, None));
    assert_eq!(missing_group.plan.as_deref(), Some("Google AI Pro"));
    assert_eq!(
        missing_group.groups[0].buckets,
        [
            quota_bucket_value("gemini-5h", "fiveHour", None, None),
            quota_bucket_value("gemini-five-hour", "fiveHour", Some(50), None),
            quota_bucket_value("a", "weekly", Some(100), None),
        ]
    );
}

#[test]
fn js_round_is_math_round() {
    assert_eq!(js_round(2.5), 3.0);
    assert_eq!(js_round(-2.5), -2.0);
    assert_eq!(js_round(1234.4), 1234.0);
}

// ── 프로세스 ──

/// 시험마다 따로 쓰는 임시 폴더. 가짜 실행 파일과 기록 파일을 담는다.
struct Scratch(PathBuf);

impl Scratch {
    fn new(prefix: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{prefix}{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn text(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    fn script(&self, name: &str, body: &str) -> String {
        let path = self.path(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }

    fn write(&self, name: &str, body: &str) {
        let path = self.path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 가짜 app-server는 요청 세 줄을 먼저 다 읽는다. 일찍 끝나 stdin 쓰기가 EPIPE와 겨루지 않게 한다.
const READ_REQUEST: &str = "read -r first; read -r second; read -r third";

#[tokio::test]
async fn codex_app_server_runs_in_home_and_skips_noise_lines() {
    let (scratch, home) = (Scratch::new("officestra-codex-"), Scratch::new("officestra-codex-home-"));
    let record = scratch.text();
    // Node는 "null" 줄에서 프로세스가 죽는다. Rust는 다른 잡음 줄처럼 건너뛴다(기록된 차이).
    let codex = scratch.script(
        "codex",
        &format!(
            r#"pwd -P > '{record}/cwd'
printf '%s|' "$@" > '{record}/args'
{READ_REQUEST}
printf '%s\n%s\n%s\n' "$first" "$second" "$third" > '{record}/request'
echo 'garbage'
echo '5'
echo 'null'
echo '{{"id":"2","result":{{}}}}'
echo '{{"id":1,"result":{{}}}}'
echo '{{"id":2.0,"result":{{"rateLimits":{{"planType":"pro","primary":{{"usedPercent":34,"windowDurationMins":10080,"resetsAt":1800000000}}}}}}}}'
exec sleep 30"#
        ),
    );
    let result = read_codex_app_server(&codex, &home.text(), Duration::from_secs(5)).await.unwrap();
    let limits = parse_codex_rate_limits(result.as_ref());
    assert_eq!(limits.weekly, window(Some(66), Some("2027-01-15T08:00:00.000Z")));
    assert_eq!(limits.plan.as_deref(), Some("Pro 20x"));
    let home_path = std::fs::canonicalize(&home.0).unwrap();
    assert_eq!(scratch.read("cwd").trim_end(), home_path.to_string_lossy());
    assert_eq!(scratch.read("args"), "app-server|--stdio|");
    assert_eq!(scratch.read("request"), CODEX_REQUEST);
}

async fn codex_outcome(body: &str, timeout: Duration) -> Result<Option<Value>, String> {
    let scratch = Scratch::new("officestra-codex-case-");
    let codex = scratch.script("codex", &format!("{READ_REQUEST}\n{body}"));
    read_codex_app_server(&codex, &scratch.text(), timeout).await
}

#[tokio::test]
async fn codex_app_server_failures_use_node_messages() {
    let long = Duration::from_secs(5);
    assert_eq!(codex_outcome("exit 3", long).await, Err("Codex 계정 한도 조회가 실패했습니다 (3).".into()));
    assert_eq!(
        codex_outcome("printf '  로그인 필요 \\n' >&2\nexit 1", long).await,
        Err("Codex 계정 한도 조회가 실패했습니다: 로그인 필요".into())
    );
    assert_eq!(codex_outcome("kill -9 $$", long).await, Err("Codex 계정 한도 조회가 실패했습니다 (종료).".into()));
    assert_eq!(
        codex_outcome(r#"echo '{"id":2,"error":{"message":"x"}}'; exec sleep 30"#, long).await,
        Err("Codex 계정 한도 응답이 실패했습니다.".into())
    );
    assert_eq!(codex_outcome(r#"echo '{"id":2}'; exec sleep 30"#, long).await, Ok(None));

    let started = Instant::now();
    assert_eq!(
        codex_outcome("exec sleep 30", Duration::from_millis(300)).await,
        Err("Codex 계정 한도 조회 시간이 초과됐습니다.".into())
    );
    assert!(started.elapsed() < Duration::from_secs(3));

    let home = Scratch::new("officestra-codex-spawn-");
    assert_eq!(
        read_codex_app_server("/nonexistent/officestra-codex", &home.text(), long).await,
        Err("Codex CLI를 실행할 수 없습니다.".into())
    );
    assert_eq!(read_codex_app_server("/etc/hosts/codex", &home.text(), long).await, Err("spawn ENOTDIR".into()));
}

type Seen = Arc<Mutex<Vec<Value>>>;

/// 받은 요청을 기록하고 정해 둔 답을 차례로 돌려주는 로컬 HTTP 서버. 마지막 답은 계속 반복한다.
async fn fake_server(replies: Vec<(u16, String)>, delay: Duration) -> (u16, Seen) {
    let seen: Seen = Arc::default();
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let recorded = Arc::clone(&seen);
    let router = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: String| {
        let (recorded, replies) = (Arc::clone(&recorded), Arc::clone(&replies));
        async move {
            let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_string);
            recorded.lock().unwrap().push(json!({
                "method": method.as_str(),
                "path": uri.path(),
                "body": body,
                "authorization": header("authorization"),
                "beta": header("anthropic-beta"),
                "agent": header("user-agent"),
                "csrf": header("x-codeium-csrf-token"),
                "type": header("content-type"),
            }));
            tokio::time::sleep(delay).await;
            let (status, body) = {
                let mut replies = replies.lock().unwrap();
                if replies.len() > 1 { replies.pop_front().unwrap() } else { replies.front().cloned().unwrap() }
            };
            (StatusCode::from_u16(status).unwrap(), body)
        }
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (port, seen)
}

fn requests(seen: &Seen) -> Vec<Value> {
    seen.lock().unwrap().clone()
}

#[tokio::test]
async fn antigravity_probe_matches_token_flag_and_header() {
    let payload = json!({"response": {"groups": [{"displayName": "Gemini Models", "buckets": [
        {"bucketId": "gemini-weekly", "remainingFraction": 0.8, "resetTime": "2026-09-18T02:00:00.000Z"},
    ]}]}});
    // 첫 답은 실패, 둘째는 그룹이 비어 있어 계속 묻는다.
    let replies = vec![(503, "{}".to_string()), (200, r#"{"response":{"groups":[]}}"#.to_string()), (200, payload.to_string())];
    let (port, seen) = fake_server(replies, Duration::ZERO).await;
    let scratch = Scratch::new("officestra-agy-");
    let record = scratch.text();
    let agy = scratch.script(
        "agy",
        &format!(
            r#"pwd -P > '{record}/cwd'
printf '%s|' "$@" > '{record}/args'
printf 'starting\nLanguage server listening on random port at {port} for HTTP\n' > "$4"
exec sleep 30"#
        ),
    );
    let home = Scratch::new("officestra-agy-home-");
    let result = probe_antigravity_quota(&agy, &home.text(), Duration::from_secs(5), &BTreeMap::new()).await.unwrap();
    assert_eq!(result, payload);

    let arguments = scratch.read("args");
    let arguments: Vec<&str> = arguments.trim_end_matches('|').split('|').collect();
    assert_eq!(arguments.len(), 5);
    assert_eq!((arguments[0], arguments[2], arguments[4]), ("--csrf_token", "--log-file", "models"));
    let token = arguments[1];
    assert_eq!((token.len(), &token[14..15], &token[8..9]), (36, "4", "-"));
    let log = Path::new(arguments[3]);
    assert_eq!(log.file_name().and_then(|name| name.to_str()), Some("agy.log"));
    let root = log.parent().unwrap();
    assert!(root.file_name().unwrap().to_string_lossy().starts_with("officestra-antigravity-quota-"));
    assert!(!root.exists(), "끝나면 임시 폴더를 지운다");
    assert_eq!(scratch.read("cwd").trim_end(), std::fs::canonicalize(&home.0).unwrap().to_string_lossy());

    let requests = requests(&seen);
    assert_eq!(requests.len(), 3);
    for request in requests {
        assert_eq!(request["method"], "POST");
        assert_eq!(request["path"], "/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary");
        assert_eq!(request["csrf"], token);
        assert_eq!(request["type"], "application/json");
        assert_eq!(request["body"], "{}");
    }
}

#[tokio::test]
async fn antigravity_probe_failures_use_node_messages() {
    let scratch = Scratch::new("officestra-agy-case-");
    let long = Duration::from_secs(10);
    let timeout = "Antigravity 계정 한도 조회 시간이 초과됐습니다.";

    let none = BTreeMap::new();

    let started = Instant::now();
    let quiet = scratch.script("quiet", "exit 0");
    assert_eq!(probe_antigravity_quota(&quiet, &scratch.text(), long, &none).await, Err(timeout.into()));
    assert!(started.elapsed() < Duration::from_secs(3), "끝난 자식은 시간 제한까지 기다리지 않는다");

    let failing = scratch.script("failing", "printf ' 로그인 필요 \\n' >&2\nexit 1");
    assert_eq!(
        probe_antigravity_quota(&failing, &scratch.text(), long, &none).await,
        Err("Antigravity 계정 한도 조회가 실패했습니다: 로그인 필요".into())
    );

    let started = Instant::now();
    let slow = scratch.script("slow", "exec sleep 30");
    assert_eq!(
        probe_antigravity_quota(&slow, &scratch.text(), Duration::from_millis(300), &none).await,
        Err(timeout.into())
    );
    assert!(started.elapsed() < Duration::from_secs(3));

    assert_eq!(
        probe_antigravity_quota("/nonexistent/officestra-agy", &scratch.text(), long, &none).await,
        Err("Antigravity CLI를 실행할 수 없습니다.".into())
    );
    assert_eq!(
        probe_antigravity_quota("/etc/hosts/agy", &scratch.text(), long, &none).await,
        Err("spawn ENOTDIR".into())
    );
}

/// Node `env: antigravityPlaywrightEnvironment(process.env)`. 드라이버가 있을 때만 PLAYWRIGHT_* 값을 agy 자식에게 넘긴다.
#[tokio::test]
async fn antigravity_probe_passes_playwright_environment_like_node() {
    // 시험 프로세스에 이미 값이 있으면 `??=` 규칙상 더하지 않는 것이 맞으므로 이 시험의 전제가 깨진다.
    assert!(std::env::var_os("PLAYWRIGHT_DRIVER_PATH").is_none() && std::env::var_os("PLAYWRIGHT_NODEJS_PATH").is_none());
    let scratch = Scratch::new("officestra-agy-playwright-");
    let driver = scratch.path("playwright-driver-1.57.0").to_string_lossy().into_owned();
    let mut sources = claude_sources(&scratch, 9);
    sources.playwright_driver_directory = Some(driver.clone());
    sources.node_executable = Some("/opt/officestra/node/bin/node".into());
    assert_eq!(antigravity_probe_environment(&sources), BTreeMap::new(), "package/cli.js가 없으면 더하지 않는다");

    scratch.write("playwright-driver-1.57.0/package/cli.js", "");
    let environment = antigravity_probe_environment(&sources);
    assert_eq!(
        environment,
        BTreeMap::from([
            ("PLAYWRIGHT_DRIVER_PATH".to_string(), driver.clone()),
            ("PLAYWRIGHT_NODEJS_PATH".to_string(), "/opt/officestra/node/bin/node".to_string()),
        ])
    );
    let record = scratch.text();
    let agy = scratch.script(
        "agy",
        &format!("printf '%s|%s|%s' \"$PLAYWRIGHT_DRIVER_PATH\" \"$PLAYWRIGHT_NODEJS_PATH\" \"${{PATH:+있음}}\" > '{record}/env'\nexit 0"),
    );
    let _ = probe_antigravity_quota(&agy, &scratch.text(), Duration::from_secs(5), &environment).await;
    assert_eq!(scratch.read("env"), format!("{driver}|/opt/officestra/node/bin/node|있음"), "백엔드 환경도 그대로 물려준다");

    sources.node_executable = None;
    assert_eq!(
        antigravity_probe_environment(&sources),
        BTreeMap::from([("PLAYWRIGHT_DRIVER_PATH".to_string(), driver)]),
        "Node 실행 파일을 모르면 드라이버 경로만 넣는다"
    );
    sources.playwright_driver_directory = None;
    assert_eq!(antigravity_probe_environment(&sources), BTreeMap::new());
}

// ── Claude ──

/// 실제 계정을 부르지 않는 Claude 조회 구성. Keychain은 없는 경로이고 사용량 주소는 로컬 서버다.
fn claude_sources(home: &Scratch, port: u16) -> UsageSources {
    UsageSources {
        pool: crate::db::connect_lazy("postgres://officestra@127.0.0.1:9/unused", Duration::from_millis(100)).unwrap(),
        home: home.text(),
        codex_home: None,
        claude_config_dir: Some(home.path("config")),
        claude_code_oauth_token: None,
        keychain_command: "/nonexistent/officestra-security".into(),
        claude_usage_url: format!("http://127.0.0.1:{port}/api/oauth/usage"),
        timeout: Duration::from_secs(5),
        playwright_driver_directory: None,
        node_executable: None,
    }
}

const CLAUDE_USAGE: &str =
    r#"{"five_hour":{"utilization":10,"resets_at":null},"seven_day":{"utilization":20,"resets_at":null}}"#;

#[tokio::test]
async fn claude_reads_usage_with_the_existing_token() {
    let (port, seen) = fake_server(vec![(200, CLAUDE_USAGE.into())], Duration::ZERO).await;
    let home = Scratch::new("officestra-claude-");
    home.write(
        "config/.credentials.json",
        r#"{"claudeAiOauth":{"accessToken":" test-token ","subscriptionType":"max","scopes":["user:inference","user:profile"]}}"#,
    );
    let limits = read_claude_rate_limits(&claude_sources(&home, port)).await.unwrap();
    assert_eq!(limits.five_hour, window(Some(90), None));
    assert_eq!(limits.weekly, window(Some(80), None));
    assert_eq!(limits.plan.as_deref(), Some("Max"));
    let requests = requests(&seen);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "GET");
    assert_eq!(requests[0]["path"], "/api/oauth/usage");
    assert_eq!(requests[0]["authorization"], "Bearer test-token");
    assert_eq!(requests[0]["beta"], "oauth-2025-04-20");
    assert_eq!(requests[0]["agent"], "OFFICESTRA/1");
}

#[tokio::test]
async fn claude_plan_prefers_the_account_profile() {
    let (port, _) = fake_server(vec![(200, r#"{"five_hour":null,"seven_day":null}"#.into())], Duration::ZERO).await;
    let home = Scratch::new("officestra-claude-plan-");
    let credential = r#"{"claudeAiOauth":{"accessToken":"t","rateLimitTier":"default_claude_max_5x","subscriptionType":"max"}}"#;
    home.write("config/.credentials.json", credential);
    let sources = claude_sources(&home, port);
    let plan = || async { read_claude_rate_limits(&sources).await.unwrap().plan };

    home.write(
        "config/.claude.json",
        r#"{"oauthAccount":{"organizationType":" claude_pro ","organizationRateLimitTier":"default_claude_ai"}}"#,
    );
    assert_eq!(plan().await.as_deref(), Some("Pro"));
    // 프로필을 못 읽으면 로그인 정보로 되돌아간다.
    home.write("config/.claude.json", r#"{"oauthAccount":{}}"#);
    assert_eq!(plan().await.as_deref(), Some("Max 5x"));
    std::fs::remove_file(home.path("config/.claude.json")).unwrap();
    assert_eq!(plan().await.as_deref(), Some("Max 5x"));

    // CLAUDE_CONFIG_DIR가 없으면 ~/.claude/.credentials.json과 ~/.claude.json이다.
    home.write(".claude/.credentials.json", credential);
    home.write(".claude.json", r#"{"oauthAccount":{"organizationType":"claude_team"}}"#);
    let mut default_paths = claude_sources(&home, port);
    default_paths.claude_config_dir = None;
    assert_eq!(read_claude_rate_limits(&default_paths).await.unwrap().plan.as_deref(), Some("Team"));
}

#[tokio::test]
async fn claude_environment_token_comes_first() {
    let (port, seen) = fake_server(vec![(200, CLAUDE_USAGE.into())], Duration::ZERO).await;
    let home = Scratch::new("officestra-claude-env-");
    home.write("config/.credentials.json", r#"{"accessToken":"file-token","subscriptionType":"pro"}"#);
    let token = |value: &'static str| {
        Config::from_lookup(|key| (key == "CLAUDE_CODE_OAUTH_TOKEN").then(|| value.to_string()), Some(home.0.clone()))
            .unwrap()
            .claude_code_oauth_token
    };
    let mut sources = claude_sources(&home, port);
    sources.claude_code_oauth_token = token("  env-token \n");
    assert_eq!(read_claude_rate_limits(&sources).await.unwrap().plan, None);
    sources.claude_code_oauth_token = token("   ");
    assert_eq!(read_claude_rate_limits(&sources).await.unwrap().plan.as_deref(), Some("Pro"));
    let authorizations: Vec<Value> = requests(&seen).iter().map(|request| request["authorization"].clone()).collect();
    assert_eq!(authorizations, [json!("Bearer env-token"), json!("Bearer file-token")]);
}

#[tokio::test]
async fn claude_credential_fallbacks_follow_node() {
    let (port, seen) = fake_server(vec![(200, CLAUDE_USAGE.into())], Duration::ZERO).await;
    let home = Scratch::new("officestra-claude-keychain-");
    let record = home.text();
    let mut sources = claude_sources(&home, port);

    home.write("config/.credentials.json", "{not json");
    assert_eq!(read_claude_rate_limits(&sources).await.err().as_deref(), Some("Claude Code 로그인 정보를 읽을 수 없습니다."));

    // 권한이 모자란 로그인 정보와 토큰 없는 정보는 건너뛰고 Keychain을 본다.
    sources.keychain_command = home.script(
        "security",
        &format!(r#"printf '%s|' "$@" > '{record}/keychain-args'
printf '{{"accessToken":"keychain-token"}}\n'"#),
    );
    home.write("config/.credentials.json", r#"{"claudeAiOauth":{"accessToken":"file-token","scopes":["user:inference"]}}"#);
    assert!(read_claude_rate_limits(&sources).await.is_ok());
    home.write("config/.credentials.json", r#"{"claudeAiOauth":{}}"#);
    assert!(read_claude_rate_limits(&sources).await.is_ok());
    assert_eq!(home.read("keychain-args"), "find-generic-password|-s|Claude Code-credentials|-w|");
    let authorizations: Vec<Value> = requests(&seen).iter().map(|request| request["authorization"].clone()).collect();
    assert_eq!(authorizations, [json!("Bearer keychain-token"), json!("Bearer keychain-token")]);

    std::fs::remove_file(home.path("config/.credentials.json")).unwrap();
    sources.keychain_command = home.script("security-garbage", "echo 'not json'");
    assert_eq!(
        read_claude_rate_limits(&sources).await.err().as_deref(),
        Some("Claude Code Keychain 로그인 정보를 읽을 수 없습니다.")
    );
    let not_found = "Claude Code 로그인 정보를 찾을 수 없습니다.";
    sources.keychain_command = home.script("security-missing", "exit 44");
    assert_eq!(read_claude_rate_limits(&sources).await.err().as_deref(), Some(not_found));
    sources.keychain_command = "/nonexistent/officestra-security".into();
    assert_eq!(read_claude_rate_limits(&sources).await.err().as_deref(), Some(not_found));
    assert_eq!(requests(&seen).len(), 2, "로그인 정보가 없으면 사용량 주소를 부르지 않는다");
}

#[tokio::test]
async fn claude_http_failures_use_node_messages() {
    let home = Scratch::new("officestra-claude-http-");
    home.write("config/.credentials.json", r#"{"accessToken":"t"}"#);

    let (port, _) = fake_server(vec![(429, "{}".into())], Duration::ZERO).await;
    assert_eq!(
        read_claude_rate_limits(&claude_sources(&home, port)).await.err().as_deref(),
        Some("Claude Code 계정 한도 조회가 실패했습니다 (HTTP 429).")
    );

    let (port, _) = fake_server(vec![(200, CLAUDE_USAGE.into())], Duration::from_secs(3)).await;
    let mut slow = claude_sources(&home, port);
    slow.timeout = Duration::from_millis(200);
    let started = Instant::now();
    assert_eq!(read_claude_rate_limits(&slow).await.err().as_deref(), Some("Claude Code 계정 한도 조회 시간이 초과됐습니다."));
    assert!(started.elapsed() < Duration::from_secs(2));

    let closed = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    assert_eq!(read_claude_rate_limits(&claude_sources(&home, closed_port)).await.err().as_deref(), Some("fetch failed"));
}

#[tokio::test]
async fn antigravity_image_cooldown_reads_only_future_resets() {
    let home = Scratch::new("officestra-cooldown-");
    let now = milliseconds("2026-08-30T16:00:00Z");
    assert_eq!(read_antigravity_image_cooldown(&home.text(), now).await, None);
    let file = ".gemini/antigravity-cli/image-cooldown.json";
    home.write(file, r#"{"resetAt":"2026-08-30T16:54:02Z"}"#);
    assert_eq!(read_antigravity_image_cooldown(&home.text(), now).await.as_deref(), Some("2026-08-30T16:54:02.000Z"));
    assert_eq!(read_antigravity_image_cooldown(&home.text(), milliseconds("2026-08-30T17:00:00Z")).await, None);
    home.write(file, r#"{"resetAt":1788108842000}"#);
    assert_eq!(read_antigravity_image_cooldown(&home.text(), now).await.as_deref(), Some("2026-08-30T16:54:02.000Z"));
    for body in [r#"{"resetAt":0}"#, r#"{"resetAt":"invalid"}"#, "not json", "null"] {
        home.write(file, body);
        assert_eq!(read_antigravity_image_cooldown(&home.text(), now).await, None, "{body}");
    }
}

// ── DB 통계 ──

#[test]
fn activity_rows_are_normalized_per_provider() {
    let activity = activity_from_rows(vec![
        (Some("codex".into()), Some(1.25), Some(1234.4), Some(9.5), Some(9875.5)),
        (Some("claude".into()), None, Some(f64::NAN), Some(f64::INFINITY), Some(2.5)),
        (Some("other".into()), Some(7.0), Some(7.0), Some(7.0), Some(7.0)),
        (None, Some(8.0), Some(8.0), Some(8.0), Some(8.0)),
    ]);
    let provider = |today_cost_usd, recent_tokens, last_30_days_cost_usd, last_30_days_tokens| ProviderActivity {
        today_cost_usd,
        recent_tokens,
        last_30_days_cost_usd,
        last_30_days_tokens,
        cost_estimate_supported: true,
    };
    assert_eq!(activity.codex, provider(1.25, 1234.0, 9.5, 9876.0));
    assert_eq!(activity.claude, provider(0.0, 0.0, 0.0, 3.0));
    assert_eq!(activity.antigravity, ProviderActivity::default());
}

#[test]
fn local_day_start_is_local_midnight() {
    let now = milliseconds("2026-08-12T03:00:00Z");
    let start = local_day_start(now);
    assert!(start <= now && now - start < 25.0 * 3_600_000.0);
    assert_eq!(local_day_start(start), start);
    assert_eq!(start % 1_000.0, 0.0);
}

// ── 요약기 ──

fn counted<T: Send + 'static>(
    reads: &Arc<AtomicUsize>,
    answer: impl Fn(usize) -> Result<Option<T>, String> + Send + Sync + 'static,
) -> ProviderRead<T> {
    let reads = Arc::clone(reads);
    Arc::new(move || {
        let result = answer(reads.fetch_add(1, Ordering::SeqCst) + 1);
        async move { result }.boxed()
    })
}

fn empty_activity(reads: &Arc<AtomicUsize>) -> ActivityRead {
    let reads = Arc::clone(reads);
    Arc::new(move |_| {
        let count = reads.fetch_add(1, Ordering::SeqCst) + 1;
        let codex = ProviderActivity { today_cost_usd: count as f64, ..ProviderActivity::default() };
        async move { Ok(UsageActivity { codex, ..UsageActivity::default() }) }.boxed()
    })
}

struct Clock(Arc<Mutex<f64>>);

impl Clock {
    fn new(iso: &str) -> Self {
        Self(Arc::new(Mutex::new(milliseconds(iso))))
    }

    fn set(&self, iso: &str) {
        *self.0.lock().unwrap() = milliseconds(iso);
    }

    fn options(&self) -> UsageSummaryOptions {
        let current = Arc::clone(&self.0);
        UsageSummaryOptions { now: Arc::new(move || *current.lock().unwrap()), ..UsageSummaryOptions::default() }
    }
}

fn reads() -> Arc<AtomicUsize> {
    Arc::default()
}

fn count(reads: &Arc<AtomicUsize>) -> usize {
    reads.load(Ordering::SeqCst)
}

#[tokio::test]
async fn limits_are_cached_but_activity_is_read_every_time() {
    let (codex_reads, claude_reads, antigravity_reads, activity_reads) = (reads(), reads(), reads(), reads());
    let clock = Clock::new("2026-08-12T00:00:00Z");
    let reader = UsageSummaryReader::new(
        UsageProviders {
            codex: counted(&codex_reads, |_| {
                Ok(Some(CodexLimits {
                    five_hour: window(Some(70), None),
                    weekly: window(Some(60), None),
                    plan: Some("Pro".into()),
                    subscription_expires_at: Some("2026-08-31T14:54:17.000Z".into()),
                }))
            }),
            claude: counted(&claude_reads, |_| {
                Ok(Some(ClaudeLimits {
                    five_hour: window(Some(50), None),
                    weekly: window(Some(40), None),
                    model_weekly_limits: Vec::new(),
                    plan: Some("Max".into()),
                }))
            }),
            antigravity: counted(&antigravity_reads, |_| {
                Ok(Some(AntigravityLimits { weekly: window(Some(85), None), ..AntigravityLimits::default() }))
            }),
            activity: empty_activity(&activity_reads),
        },
        clock.options(),
    );
    let first = reader.read(false).await.unwrap();
    let second = reader.read(false).await.unwrap();
    let forced = reader.read(true).await.unwrap();
    assert_eq!(first["codexFiveHour"], 70);
    assert_eq!(first["codexSubscriptionExpiresAt"], "2026-08-31T14:54:17.000Z");
    assert_eq!(second["codexActivity"]["todayCostUSD"].as_f64(), Some(2.0));
    assert_eq!(forced["codexActivity"]["todayCostUSD"].as_f64(), Some(3.0));
    assert_eq!(count(&codex_reads), 2);
    // Claude·Antigravity는 force여도 캐시 수명 안에서는 다시 부르지 않고 직전 값을 쓴다.
    assert_eq!((count(&claude_reads), count(&antigravity_reads)), (1, 1));
    assert_eq!(forced["claudeFiveHour"], 50);
    assert_eq!(forced["antigravityWeekly"], 85);
    assert_eq!(count(&activity_reads), 3);
}

#[tokio::test]
async fn cached_antigravity_keeps_every_group() {
    let (antigravity_reads, activity_reads) = (reads(), reads());
    let reader = UsageSummaryReader::new(
        UsageProviders {
            codex: counted(&reads(), |_| Ok(None)),
            claude: counted(&reads(), |_| Ok(None)),
            antigravity: counted(&antigravity_reads, |_| {
                parse_antigravity_rate_limits(&argon_fixture(), "gemini-4-argon").map(Some)
            }),
            activity: empty_activity(&activity_reads),
        },
        Clock::new("2026-08-12T00:00:00Z").options(),
    );
    let first = reader.read(false).await.unwrap();
    let cached = reader.read(true).await.unwrap();
    assert_eq!(count(&antigravity_reads), 1);
    let ids: Vec<&Value> = first["antigravityQuotaGroups"].as_array().unwrap().iter().map(|group| &group["id"]).collect();
    assert_eq!(ids, [&json!("gemini"), &json!("third-party"), &json!("argon")]);
    assert_eq!(cached["antigravityQuotaGroups"], first["antigravityQuotaGroups"]);
}

#[tokio::test]
async fn claude_failure_keeps_the_last_good_value() {
    let claude_reads = reads();
    let clock = Clock::new("2026-08-23T00:00:00Z");
    let reader = UsageSummaryReader::new(
        UsageProviders {
            codex: counted(&reads(), |_| {
                Ok(Some(CodexLimits {
                    five_hour: window(Some(70), None),
                    weekly: window(Some(60), None),
                    plan: Some("Pro".into()),
                    subscription_expires_at: None,
                }))
            }),
            claude: counted(&claude_reads, |count| {
                if count > 1 {
                    return Err("Claude 계정 한도 조회가 실패했습니다 (HTTP 429).".into());
                }
                Ok(Some(ClaudeLimits {
                    five_hour: window(Some(48), None),
                    weekly: window(Some(13), None),
                    model_weekly_limits: vec![model_limit("Fable", "Fable", Some(70), None)],
                    plan: Some("Max".into()),
                }))
            }),
            antigravity: counted(&reads(), |_| {
                Ok(Some(AntigravityLimits { weekly: window(Some(85), None), ..AntigravityLimits::default() }))
            }),
            activity: empty_activity(&reads()),
        },
        clock.options(),
    );
    let healthy = reader.read(false).await.unwrap();
    assert_eq!(healthy["claudeFiveHour"], 48);
    assert_eq!(healthy["claudeModelWeeklyLimits"], json!([{"id": "Fable", "name": "Fable", "remaining": 70, "resetAt": null}]));
    assert_eq!((&healthy["claudeLimitStale"], &healthy["claudeLimitError"]), (&json!(false), &Value::Null));

    // 캐시 수명이 지난 뒤 429가 나도 값은 남고 오류만 붙는다.
    clock.set("2026-08-23T00:06:00Z");
    let failed = reader.read(true).await.unwrap();
    assert_eq!(count(&claude_reads), 2);
    assert_eq!(failed["claudeFiveHour"], 48, "직전 정상값을 유지한다");
    assert_eq!(failed["claudeWeekly"], 13);
    assert_eq!(failed["claudeModelWeeklyLimits"], healthy["claudeModelWeeklyLimits"]);
    assert_eq!(failed["claudeLimitStale"], true);
    assert_eq!(failed["claudeLimitError"], "Claude 계정 한도 조회가 실패했습니다 (HTTP 429).");
    assert_eq!(failed["codexFiveHour"], 70, "코덱스 한도는 영향받지 않는다");
}

#[tokio::test]
async fn claude_failure_waits_for_the_backoff() {
    let claude_reads = reads();
    let clock = Clock::new("2026-08-23T00:00:00Z");
    let reader = UsageSummaryReader::new(
        UsageProviders {
            codex: counted(&reads(), |_| Ok(None)),
            claude: counted(&claude_reads, |_| Err::<Option<ClaudeLimits>, _>("HTTP 429".into())),
            antigravity: counted(&reads(), |_| Ok(None)),
            activity: empty_activity(&reads()),
        },
        clock.options(),
    );
    reader.read(false).await.unwrap();
    assert_eq!(count(&claude_reads), 1);
    // 백오프(90초) 안에서는 강제 갱신을 눌러도 업스트림을 부르지 않는다.
    clock.set("2026-08-23T00:00:30Z");
    reader.read(true).await.unwrap();
    assert_eq!(count(&claude_reads), 1);
    clock.set("2026-08-23T00:02:00Z");
    reader.read(true).await.unwrap();
    assert_eq!(count(&claude_reads), 2);
}

#[tokio::test]
async fn one_provider_failure_does_not_hide_the_others() {
    let reader = UsageSummaryReader::new(
        UsageProviders {
            codex: counted(&reads(), |_| Err::<Option<CodexLimits>, _>("codex unavailable".into())),
            claude: counted(&reads(), |_| {
                Ok(Some(ClaudeLimits {
                    five_hour: window(Some(80), None),
                    weekly: window(Some(90), None),
                    model_weekly_limits: Vec::new(),
                    plan: Some("Max".into()),
                }))
            }),
            antigravity: counted(&reads(), |_| {
                Ok(Some(AntigravityLimits { weekly: window(Some(85), None), ..AntigravityLimits::default() }))
            }),
            activity: empty_activity(&reads()),
        },
        Clock::new("2026-08-12T00:00:00Z").options(),
    );
    let summary = reader.read(false).await.unwrap();
    assert_eq!(summary["codexFiveHour"], Value::Null);
    assert_eq!(summary["codexLimitError"], "codex unavailable");
    assert_eq!(summary["claudeFiveHour"], 80);
    assert_eq!(summary["claudeLimitError"], Value::Null);
    assert_eq!(summary["codexActivity"]["todayCostUSD"].as_f64(), Some(1.0));
}

#[tokio::test]
async fn concurrent_reads_share_one_refresh_and_activity_errors_fail_the_read() {
    let codex_reads = reads();
    let slow_reads = Arc::clone(&codex_reads);
    let codex: ProviderRead<CodexLimits> = Arc::new(move || {
        slow_reads.fetch_add(1, Ordering::SeqCst);
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Some(CodexLimits::default()))
        }
        .boxed()
    });
    let activity_reads = reads();
    let providers = UsageProviders {
        codex,
        claude: counted(&reads(), |_| Ok(None)),
        antigravity: counted(&reads(), |_| Ok(None)),
        activity: empty_activity(&activity_reads),
    };
    let reader = UsageSummaryReader::new(providers.clone(), Clock::new("2026-08-12T00:00:00Z").options());
    let (first, second) = tokio::join!(reader.read(false), reader.read(false));
    assert_eq!(first.unwrap()["fetchedAt"], second.unwrap()["fetchedAt"]);
    assert_eq!((count(&codex_reads), count(&activity_reads)), (1, 2));

    let failing = UsageSummaryReader::new(
        UsageProviders { activity: Arc::new(|_| async { Err("relation missing".to_string()) }.boxed()), ..providers },
        Clock::new("2026-08-12T00:00:00Z").options(),
    );
    assert_eq!(failing.read(false).await, Err("relation missing".into()));
}

/// Node summary-fixture.mjs(같은 가짜 공급자)로 뽑은 JSON.stringify 결과와 바이트 단위로 같다.
#[tokio::test]
async fn summary_json_matches_node_bytes() {
    let round = Arc::new(AtomicUsize::new(0));
    let failing = |round: &Arc<AtomicUsize>| {
        let round = Arc::clone(round);
        move || round.load(Ordering::SeqCst) == 1
    };
    let (codex_failing, claude_failing, antigravity_failing) = (failing(&round), failing(&round), failing(&round));
    let providers = UsageProviders {
        codex: counted(&reads(), move |_| {
            if codex_failing() {
                return Err("  codex 실패  ".into());
            }
            Ok(Some(CodexLimits {
                five_hour: window(Some(70), Some("2026-08-23T03:00:00.000Z")),
                weekly: window(Some(60), None),
                plan: Some("Pro 20x".into()),
                subscription_expires_at: Some("2026-08-31T14:54:17.000Z".into()),
            }))
        }),
        claude: counted(&reads(), move |_| {
            if claude_failing() {
                return Err("   ".into());
            }
            Ok(Some(ClaudeLimits {
                five_hour: window(Some(48), None),
                weekly: None,
                model_weekly_limits: vec![model_limit("Fable", "Fable", Some(70), Some("2026-08-25T00:00:00.000Z"))],
                plan: Some("Max 5x".into()),
            }))
        }),
        antigravity: counted(&reads(), move |_| {
            if antigravity_failing() {
                return Ok(None);
            }
            Ok(Some(AntigravityLimits {
                five_hour: None,
                weekly: window(Some(49), Some("2026-10-24T05:44:32.000Z")),
                plan: Some("Google AI Pro".into()),
                groups: vec![QuotaGroup {
                    id: "argon".into(),
                    name: "Gemini 4 Argon".into(),
                    buckets: vec![quota_bucket_value(
                        "gemini-argon-wtu-weekly",
                        "weekly",
                        Some(49),
                        Some("2026-10-24T05:44:32.000Z"),
                    )],
                }],
                image_reset_at: Some("2026-08-23T00:30:00.000Z".into()),
            }))
        }),
        activity: Arc::new(|_| {
            async {
                Ok(UsageActivity {
                    codex: ProviderActivity {
                        today_cost_usd: 1.25,
                        recent_tokens: 1234.0,
                        last_30_days_cost_usd: 9.5,
                        last_30_days_tokens: 9876.0,
                        cost_estimate_supported: true,
                    },
                    claude: ProviderActivity { last_30_days_cost_usd: 0.1, ..ProviderActivity::default() },
                    antigravity: ProviderActivity::default(),
                })
            }
            .boxed()
        }),
    };
    let clock = Clock::new("2026-08-23T00:00:00Z");
    let reader = UsageSummaryReader::new(providers, clock.options());
    let rendered = |value: Value| String::from_utf8(crate::js::to_js_json(&value).unwrap()).unwrap();

    let activity = concat!(
        r#""codexActivity":{"todayCostUSD":1.25,"recentTokens":1234,"last30DaysCostUSD":9.5,"last30DaysTokens":9876,"costEstimateSupported":true},"#,
        r#""claudeActivity":{"todayCostUSD":0,"recentTokens":0,"last30DaysCostUSD":0.1,"last30DaysTokens":0,"costEstimateSupported":true},"#,
        r#""antigravityActivity":{"todayCostUSD":0,"recentTokens":0,"last30DaysCostUSD":0,"last30DaysTokens":0,"costEstimateSupported":true}}"#,
    );
    let limits = concat!(
        r#"{"codexFiveHour":70,"codexFiveHourResetAt":"2026-08-23T03:00:00.000Z","codexWeekly":60,"codexWeeklyResetAt":null,"#,
        r#""claudeFiveHour":48,"claudeFiveHourResetAt":null,"claudeWeekly":null,"claudeWeeklyResetAt":null,"#,
        r#""claudeModelWeeklyLimits":[{"id":"Fable","name":"Fable","remaining":70,"resetAt":"2026-08-25T00:00:00.000Z"}],"#,
        r#""antigravityFiveHour":null,"antigravityFiveHourResetAt":null,"antigravityWeekly":49,"#,
        r#""antigravityWeeklyResetAt":"2026-10-24T05:44:32.000Z","#,
        r#""antigravityQuotaGroups":[{"id":"argon","name":"Gemini 4 Argon","buckets":[{"id":"gemini-argon-wtu-weekly","#,
        r#""name":"gemini-argon-wtu-weekly","window":"weekly","remaining":49,"resetAt":"2026-10-24T05:44:32.000Z"}]}],"#,
        r#""antigravityImageResetAt":"2026-08-23T00:30:00.000Z","codexPlan":"Pro 20x","claudePlan":"Max 5x","#,
        r#""antigravityPlan":"Google AI Pro","codexSubscriptionExpiresAt":"2026-08-31T14:54:17.000Z","#,
    );
    let healthy = format!(
        "{limits}{}{activity}",
        concat!(
            r#""codexLimitError":null,"claudeLimitError":null,"antigravityLimitError":null,"#,
            r#""codexLimitStale":false,"claudeLimitStale":false,"antigravityLimitStale":false,"#,
            r#""fetchedAt":"2026-08-23T00:00:00.000Z","#,
        )
    );
    assert_eq!(rendered(reader.read(false).await.unwrap()), healthy);

    round.store(1, Ordering::SeqCst);
    clock.set("2026-08-23T00:10:00Z");
    let stale = format!(
        "{limits}{}{activity}",
        concat!(
            r#""codexLimitError":"codex 실패","claudeLimitError":"Claude Code 한도를 확인할 수 없습니다.","#,
            r#""antigravityLimitError":null,"codexLimitStale":true,"claudeLimitStale":true,"antigravityLimitStale":true,"#,
            r#""fetchedAt":"2026-08-23T00:10:00.000Z","#,
        )
    );
    assert_eq!(rendered(reader.read(true).await.unwrap()), stale);
}
