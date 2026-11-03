// Slack Web API 호출. @slack/web-api 8.0.0 WebClient처럼 form 본문·Bearer 토큰·재시도·429 대기·ok:false 오류를 다룬다.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::js::{js_string, js_truthy, number_to_string, parse_int, parse_js_json, to_js_json};

/// p-retry(retry 모듈) 설정. 기본 minTimeout은 1초다.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    pub retries: u32,
    pub factor: f64,
    pub randomize: bool,
}

/// WebClient 기본값 tenRetriesInAboutThirtyMinutes. Bolt의 app.client가 쓴다.
pub const TEN_RETRIES: RetryPolicy = RetryPolicy { retries: 10, factor: 1.96821, randomize: true };
/// SocketModeClient가 apps.connections.open용으로 넣는 값. Bolt와 설정 객체를 함께 써서 팀별 클라이언트도 이 값을 받는다.
pub const SOCKET_RETRIES: RetryPolicy = RetryPolicy { retries: 100, factor: 1.3, randomize: false };

/// WebClient가 던지는 오류. message()가 Node Error.message와 같다.
#[derive(Clone, Debug, PartialEq)]
pub enum SlackError {
    /// WebAPIPlatformError. 응답 본문을 그대로 지닌다.
    Platform(Value),
    /// WebAPIRequestError. 원래 오류 문구를 지닌다.
    Request(String),
    /// WebAPIHTTPError.
    Http(u16),
    /// 일반 Error·TypeError 문구.
    Plain(String),
}

impl SlackError {
    pub fn message(&self) -> String {
        match self {
            Self::Platform(result) => {
                let error = result.get("error").map_or_else(|| "undefined".to_string(), js_string);
                format!("An API error occurred: {error}")
            }
            Self::Request(original) => format!("A request error occurred: {original}"),
            Self::Http(status) => format!("An HTTP protocol error occurred: statusCode = {status}"),
            Self::Plain(message) => message.clone(),
        }
    }

    /// 응답의 error 코드. 플랫폼 오류가 아니면 None이다.
    pub fn platform_code(&self) -> Option<&str> {
        match self {
            Self::Platform(result) => result.get("error").and_then(Value::as_str),
            _ => None,
        }
    }
}

/// 한 번의 시도 결과. abort이면 남은 재시도를 하지 않는다.
struct Attempt {
    error: SlackError,
    abort: bool,
}

struct ClientInner {
    http: reqwest::Client,
    api_url: String,
    authorization: Option<String>,
    team_id: Option<String>,
    retry: RetryPolicy,
    min_timeout: Duration,
    /// p-queue pause(). 429 대기 동안 같은 클라이언트의 새 요청을 멈춘다.
    paused_until: Mutex<Option<Instant>>,
}

/// WebClient 하나. 복제해도 같은 대기 상태를 나눈다.
#[derive(Clone)]
pub struct WebClient(Arc<ClientInner>);

impl WebClient {
    /// slackApiUrl은 끝에 /가 없으면 붙인다. 토큰이 비면 Authorization을 보내지 않는다.
    pub fn new(
        http: reqwest::Client,
        api_url: &str,
        token: &str,
        team_id: Option<String>,
        retry: RetryPolicy,
        min_timeout: Duration,
    ) -> Self {
        let mut api_url = api_url.to_string();
        if !api_url.ends_with('/') {
            api_url.push('/');
        }
        let authorization = (!token.is_empty()).then(|| format!("Bearer {token}"));
        Self(Arc::new(ClientInner { http, api_url, authorization, team_id, retry, min_timeout, paused_until: Mutex::new(None) }))
    }

    pub fn team_id(&self) -> Option<&str> {
        self.0.team_id.as_deref()
    }

    pub fn retry(&self) -> RetryPolicy {
        self.0.retry
    }

    /// `client.apiCall(method, options)`. options는 객체이고 null 값은 보내지 않는다.
    pub async fn api_call(&self, method: &str, options: Value) -> Result<Value, SlackError> {
        let mut body = Map::new();
        body.insert("team_id".into(), self.0.team_id.clone().map_or(Value::Null, Value::String));
        let mut authorization = self.0.authorization.clone();
        if let Value::Object(fields) = options {
            if let Some(token) = fields.get("token").filter(|token| js_truthy(Some(token))) {
                authorization = Some(format!("Bearer {}", js_string(token)));
            }
            body.extend(fields);
        }
        let url = format!("{}{method}", self.0.api_url);
        let text = self.make_request(&url, &form_body(&body), authorization.as_deref()).await?;
        let result = parse_js_json(&text).unwrap_or_else(|_| json!({ "ok": false, "error": text }));
        if result.is_null() {
            return Err(SlackError::Plain("Cannot read properties of null (reading 'response_metadata')".into()));
        }
        if let Some(warnings) = result.pointer("/response_metadata/warnings").and_then(Value::as_array) {
            for warning in warnings {
                eprintln!("[WARN]  web-api:WebClient {}", js_string(warning));
            }
        }
        if !js_truthy(result.get("ok")) {
            return Err(SlackError::Platform(result));
        }
        Ok(result)
    }

    /// p-retry로 감싼 makeRequest. 마지막 오류를 그대로 돌려준다.
    async fn make_request(&self, url: &str, body: &str, authorization: Option<&str>) -> Result<String, SlackError> {
        let timeouts = retry_timeouts(self.0.retry, self.0.min_timeout);
        let mut attempt = 0;
        loop {
            self.wait_while_paused().await;
            match self.attempt(url, body, authorization).await {
                Ok(text) => return Ok(text),
                Err(failed) if failed.abort || attempt >= timeouts.len() => return Err(failed.error),
                Err(_) => {
                    tokio::time::sleep(timeouts[attempt]).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn wait_while_paused(&self) {
        loop {
            let until = *self.0.paused_until.lock().unwrap();
            match until {
                Some(until) if until > Instant::now() => tokio::time::sleep_until(until).await,
                _ => return,
            }
        }
    }

    async fn attempt(&self, url: &str, body: &str, authorization: Option<&str>) -> Result<String, Attempt> {
        let retry = |error| Attempt { error, abort: false };
        let mut request = self
            .0
            .http
            .post(url)
            .header("User-Agent", "officestra-backend-rs")
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.to_string());
        if let Some(authorization) = authorization {
            request = request.header("Authorization", authorization);
        }
        let Ok(response) = request.send().await else {
            eprintln!("[WARN]  web-api:WebClient http request failed fetch failed");
            return Err(retry(SlackError::Request("fetch failed".into())));
        };
        let status = response.status().as_u16();
        // fetch redirect:'error'는 3xx를 받으면 TypeError("fetch failed")로 끝난다.
        if (300..400).contains(&status) {
            return Err(retry(SlackError::Request("fetch failed".into())));
        }
        if status == 429 {
            let header = response.headers().get("retry-after").map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
            let seconds = header.as_deref().map(parse_int).filter(|seconds| !seconds.is_nan());
            let Some(seconds) = seconds else {
                let shown = header.unwrap_or_else(|| "null".into());
                return Err(Attempt {
                    error: SlackError::Plain(format!(
                        "Retry header did not contain a valid timeout (url: {url}, retry-after header: {shown})"
                    )),
                    abort: true,
                });
            };
            eprintln!("[INFO]  web-api:WebClient API Call failed due to rate limiting. Will retry in {} seconds.", number_to_string(seconds));
            let wait = Duration::from_secs_f64((seconds * 1000.0).max(0.0) / 1000.0);
            *self.0.paused_until.lock().unwrap() = Some(Instant::now() + wait);
            tokio::time::sleep(wait).await;
            *self.0.paused_until.lock().unwrap() = None;
            return Err(retry(SlackError::Plain(format!(
                "A rate limit was exceeded (url: {url}, retry-after: {})",
                number_to_string(seconds)
            ))));
        }
        if status != 200 {
            let _ = response.bytes().await;
            return Err(retry(SlackError::Http(status)));
        }
        match response.bytes().await {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(_) => Err(Attempt { error: SlackError::Plain("terminated".into()), abort: true }),
        }
    }
}

/// retry.timeouts(). 시도마다 round(무작위(1~2) × max(minTimeout, 1) × factor^i)를 구해 오름차순으로 둔다.
pub fn retry_timeouts(policy: RetryPolicy, min_timeout: Duration) -> Vec<Duration> {
    let minimum = (min_timeout.as_secs_f64() * 1000.0).max(1.0);
    let mut timeouts: Vec<f64> = (0..policy.retries)
        .map(|attempt| {
            let random = if policy.randomize { random_unit() + 1.0 } else { 1.0 };
            (random * minimum * policy.factor.powi(attempt as i32)).round()
        })
        .collect();
    timeouts.sort_by(f64::total_cmp);
    timeouts.into_iter().map(|milliseconds| Duration::from_secs_f64(milliseconds.min(1e15) / 1000.0)).collect()
}

/// Math.random()처럼 [0, 1) 값. ring의 시스템 난수를 쓴다.
fn random_unit() -> f64 {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 8];
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

/// serializeBody + querystring.stringify. null은 빼고, 문자열·숫자·참거짓이 아니면 JSON 문자열로 보낸다.
pub fn form_body(body: &Map<String, Value>) -> String {
    let mut pairs = Vec::new();
    for (key, value) in body {
        let text = match value {
            Value::Null => continue,
            Value::String(text) => text.clone(),
            Value::Number(_) | Value::Bool(_) => js_string(value),
            other => String::from_utf8(to_js_json(other).unwrap_or_default()).unwrap_or_default(),
        };
        pairs.push(format!("{}={}", query_escape(key), query_escape(&text)));
    }
    pairs.join("&")
}

/// Node querystring.escape. 영숫자와 `-_.!~*'()`만 그대로 두고 나머지는 UTF-8 %XX로 바꾼다(공백은 %20).
pub fn query_escape(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}
