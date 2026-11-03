// 음성 따라 읽기가 직원의 최신 답변을 받아 오는 구독. Node voice/feed-subscription.mjs처럼 연결·재연결 때 한 번 맞추고, 이후에는 웹소켓 변경 알림 때만 조회한다.
// 대기 타이머로 DB를 조회하지 않는다. 실패하면 제한된 간격으로만 다시 시도한다.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use crate::js::{js_truthy, parse_js_json};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// onTurn·onError 자리. 구독 쪽은 보내기만 하고 받는 쪽이 순서대로 처리한다.
#[derive(Clone, Debug, PartialEq)]
pub enum FeedEvent {
    Turn(Value),
    Error(String),
}

#[derive(Clone, Debug)]
pub struct FeedOptions {
    pub backend: String,
    pub character_id: String,
    pub since: String,
    pub batch: Duration,
    pub retry: Duration,
    pub max_retry: Duration,
}

impl FeedOptions {
    /// Node 기본값 batchMs 100, retryMs 1000, maxRetryMs 30000.
    pub fn new(backend: &str, character_id: &str, since: &str) -> Self {
        Self {
            backend: backend.into(),
            character_id: character_id.into(),
            since: since.into(),
            batch: Duration::from_millis(100),
            retry: Duration::from_millis(1_000),
            max_retry: Duration::from_millis(30_000),
        }
    }
}

/// 구독 핸들. close나 버리기는 남은 타이머·요청·소켓을 모두 멈춘다.
pub struct FeedSubscription {
    task: JoinHandle<()>,
}

impl FeedSubscription {
    pub fn close(&self) {
        self.task.abort();
    }
}

impl Drop for FeedSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// subscribeVoiceFeed. 백엔드 주소가 URL이 아니면 Node처럼 바로 실패한다.
pub fn subscribe_voice_feed(options: FeedOptions, events: UnboundedSender<FeedEvent>) -> Result<FeedSubscription, String> {
    let invalid = |_| "Invalid URL".to_string();
    let base = Url::parse(&options.backend).map_err(invalid)?;
    let mut endpoint = base.join("/api/voice-feed").map_err(invalid)?;
    endpoint
        .query_pairs_mut()
        .append_pair("characterId", &options.character_id)
        .append_pair("since", &options.since);
    let mut socket_url = base.join("/ws").map_err(invalid)?;
    let scheme = if socket_url.scheme() == "https" { "wss" } else { "ws" };
    // URL protocol 설정처럼 바꿀 수 없는 조합이면 그대로 둔다.
    let _ = socket_url.set_scheme(scheme);
    let client = crate::http_client_builder().no_proxy().build().map_err(|error| error.to_string())?;
    let task = tokio::spawn(run(options, client, endpoint, socket_url, events));
    Ok(FeedSubscription { task })
}

type Request = Pin<Box<dyn Future<Output = Result<Option<Value>, String>> + Send>>;
type Connect = Pin<Box<dyn Future<Output = Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String>> + Send>>;

enum Socket {
    Connecting(Connect),
    Open(Box<WebSocketStream<MaybeTlsStream<TcpStream>>>),
    Waiting(Instant),
}

enum SocketStep {
    Opened(WebSocketStream<MaybeTlsStream<TcpStream>>),
    ConnectFailed(String),
    Message(Message),
    Closed(Option<String>),
    Reconnect,
}

/// refresh 타이머와 진행 중 요청. 요청 중에는 새 타이머를 걸지 않고 끝난 뒤 한 번 더 조회한다.
struct Refresh {
    dirty: bool,
    at: Option<Instant>,
    requesting: bool,
}

impl Refresh {
    fn schedule(&mut self, delay: Duration) {
        self.dirty = true;
        if self.requesting || self.at.is_some() {
            return;
        }
        self.at = Some(Instant::now() + delay);
    }
}

async fn run(
    options: FeedOptions,
    client: reqwest::Client,
    endpoint: Url,
    socket_url: Url,
    events: UnboundedSender<FeedEvent>,
) {
    let mut refresh = Refresh { dirty: false, at: None, requesting: false };
    let mut request: Option<Request> = None;
    let mut previous: Option<String> = None;
    let mut reconnect_delay = options.retry;
    let mut refresh_delay = options.retry;
    let mut socket = Socket::Connecting(connect(socket_url.clone()));
    loop {
        tokio::select! {
            () = wait_until(refresh.at) => {
                refresh.at = None;
                refresh.dirty = false;
                refresh.requesting = true;
                request = Some(fetch_turn(client.clone(), endpoint.clone()));
            }
            result = finish(&mut request) => {
                request = None;
                refresh.requesting = false;
                match result {
                    Ok(turn) => {
                        refresh_delay = options.retry;
                        if let Some(turn) = turn.filter(|turn| {
                            turn.get("characterId").and_then(Value::as_str) == Some(options.character_id.as_str())
                        }) {
                            let key = json!([turn.get("id"), turn.get("status"), turn.get("response")]).to_string();
                            if previous.as_deref() != Some(key.as_str()) {
                                let _ = events.send(FeedEvent::Turn(turn));
                                previous = Some(key);
                            }
                        }
                        if refresh.dirty {
                            refresh.schedule(options.batch);
                        }
                    }
                    Err(message) => {
                        let _ = events.send(FeedEvent::Error(message));
                        refresh.schedule(refresh_delay);
                        refresh_delay = options.max_retry.min(refresh_delay * 2);
                    }
                }
            }
            step = socket_step(&mut socket) => {
                let closed = match step {
                    SocketStep::Opened(stream) => {
                        socket = Socket::Open(Box::new(stream));
                        reconnect_delay = options.retry;
                        refresh.schedule(Duration::ZERO);
                        false
                    }
                    SocketStep::Message(message) => {
                        if changed_for(&message, &options.character_id) {
                            refresh.schedule(options.batch);
                        }
                        false
                    }
                    SocketStep::Reconnect => {
                        socket = Socket::Connecting(connect(socket_url.clone()));
                        false
                    }
                    SocketStep::ConnectFailed(message) | SocketStep::Closed(Some(message)) => {
                        let _ = events.send(FeedEvent::Error(message));
                        true
                    }
                    SocketStep::Closed(None) => true,
                };
                if closed {
                    socket = Socket::Waiting(Instant::now() + reconnect_delay);
                    reconnect_delay = options.max_retry.min(reconnect_delay * 2);
                }
            }
        }
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn finish(request: &mut Option<Request>) -> Result<Option<Value>, String> {
    match request {
        Some(request) => request.as_mut().await,
        None => std::future::pending().await,
    }
}

/// 이 직원의 feed.changed 알림인가. 직원 ID가 없는 알림은 모두에게 온 것으로 본다.
fn changed_for(message: &Message, character_id: &str) -> bool {
    let text = match message {
        Message::Text(text) => text.as_str().to_string(),
        Message::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => return false,
    };
    let Ok(Value::Object(event)) = parse_js_json(&text) else { return false };
    if event.get("type").and_then(Value::as_str) != Some("feed.changed") {
        return false;
    }
    let target = event.get("characterId");
    !js_truthy(target) || target.and_then(Value::as_str) == Some(character_id)
}

/// GET /api/voice-feed. 응답 본문까지 10초 안에 끝나야 한다. 오류 문구는 Node fetch와 같게 둔다.
fn fetch_turn(client: reqwest::Client, endpoint: Url) -> Request {
    Box::pin(async move {
        let attempt = async {
            let response = client.get(endpoint).send().await.map_err(|_| "fetch failed".to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("음성 응답 조회 HTTP {}", status.as_u16()));
            }
            let bytes = response.bytes().await.map_err(|_| "terminated".to_string())?;
            let text = String::from_utf8_lossy(&bytes);
            let parsed = parse_js_json(text.strip_prefix('\u{feff}').unwrap_or(&text)).map_err(|error| error.to_string())?;
            match parsed {
                Value::Null => Err("Cannot destructure property 'turn' of '(intermediate value)' as it is null.".into()),
                Value::Object(mut fields) => Ok(fields.shift_remove("turn").filter(|turn| js_truthy(Some(turn)))),
                _ => Ok(None),
            }
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, attempt).await {
            Ok(result) => result,
            Err(_) => Err("The operation was aborted due to timeout".into()),
        }
    })
}

fn connect(url: Url) -> Connect {
    Box::pin(async move {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::connect_async(url.as_str())).await {
            Err(_) => Err("Opening handshake has timed out".into()),
            Ok(Err(error)) => Err(connect_error_message(&error, &url)),
            Ok(Ok((stream, _))) => Ok(stream),
        }
    })
}

async fn socket_step(socket: &mut Socket) -> SocketStep {
    match socket {
        Socket::Connecting(connect) => match connect.as_mut().await {
            Ok(stream) => SocketStep::Opened(stream),
            Err(message) => SocketStep::ConnectFailed(message),
        },
        Socket::Open(stream) => match stream.next().await {
            None | Some(Ok(Message::Close(_))) => SocketStep::Closed(None),
            Some(Ok(message)) => SocketStep::Message(message),
            Some(Err(error)) => SocketStep::Closed(open_error_message(&error)),
        },
        Socket::Waiting(deadline) => {
            sleep_until(*deadline).await;
            SocketStep::Reconnect
        }
    }
}

/// 연결 단계 실패. ws 라이브러리처럼 'connect ECONNREFUSED 호스트:포트', 'Unexpected server response: 404'로 쓴다.
fn connect_error_message(error: &tungstenite::Error, url: &Url) -> String {
    match error {
        tungstenite::Error::Http(response) => format!("Unexpected server response: {}", response.status().as_u16()),
        tungstenite::Error::Io(io) => {
            let code = match io.raw_os_error() {
                Some(libc::ECONNREFUSED) => "ECONNREFUSED",
                Some(libc::ETIMEDOUT) => "ETIMEDOUT",
                Some(libc::EHOSTUNREACH) => "EHOSTUNREACH",
                Some(libc::ENETUNREACH) => "ENETUNREACH",
                Some(libc::ECONNRESET) => "ECONNRESET",
                Some(libc::EADDRNOTAVAIL) => "EADDRNOTAVAIL",
                _ => return io.to_string(),
            };
            let host = url.host_str().unwrap_or_default();
            let port = url.port_or_known_default().unwrap_or_default();
            format!("connect {code} {host}:{port}")
        }
        other => other.to_string(),
    }
}

/// 연결된 뒤의 소켓 오류. ws는 끊김(소켓 오류·닫는 절차 없는 종료)을 오류 없이 'close'로만 알린다.
fn open_error_message(error: &tungstenite::Error) -> Option<String> {
    match error {
        tungstenite::Error::ConnectionClosed
        | tungstenite::Error::AlreadyClosed
        | tungstenite::Error::Io(_)
        | tungstenite::Error::Protocol(tungstenite::error::ProtocolError::ResetWithoutClosingHandshake) => None,
        other => Some(other.to_string()),
    }
}
