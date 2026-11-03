// Slack 소켓 모드 연결. @slack/socket-mode 3.0.0처럼 apps.connections.open → wss 연결 → hello 대기, envelope ack, ping·pong 감시, 끊기면 재연결한다.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::web::{SlackError, WebClient};
use crate::js::{js_truthy, now_milliseconds, number_to_string};

/// 재연결을 멈추는 apps.connections.open 오류 코드(UnrecoverableSocketModeStartError).
const UNRECOVERABLE: [&str; 5] = ["not_authed", "invalid_auth", "account_inactive", "user_removed_from_team", "team_disabled"];

/// 소켓으로 받은 메시지 하나. Bolt의 slack_event 인자와 같다.
#[derive(Clone, Debug)]
pub struct Envelope {
    pub kind: Option<Value>,
    pub envelope_id: Option<Value>,
    pub body: Option<Value>,
    pub retry_num: Option<Value>,
    pub retry_reason: Option<Value>,
}

/// ack를 보낼 때 그 순간 열린 연결을 쓴다. Node send()가 this.websocket을 보는 것과 같다.
#[derive(Clone)]
pub struct Acker(Arc<Shared>);

impl Acker {
    /// `send(envelope_id, {})`. 열린 연결이 없으면 Node처럼 실패한다.
    pub fn ack(&self, envelope_id: Option<&Value>) -> Result<(), String> {
        let mut message = serde_json::Map::new();
        if let Some(id) = envelope_id {
            message.insert("envelope_id".into(), id.clone());
        }
        message.insert("payload".into(), json!({}));
        let writer = self.0.writer.lock().unwrap().clone();
        let Some(writer) = writer else {
            eprintln!("[ERROR]  socket-mode:SocketModeClient Failed to send a message as the client has no active connection");
            return Err("Failed to send a message as the client has no active connection".into());
        };
        writer
            .send(Message::Text(Value::Object(message).to_string().into()))
            .map_err(|_| "Failed to send a WebSocket message".to_string())
    }
}

struct Shared {
    writer: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    shutting_down: AtomicBool,
}

/// 시간 설정. 기본값은 Node SocketModeClient·SlackWebSocket과 같다.
#[derive(Clone, Copy, Debug)]
pub struct SocketTimings {
    pub client_ping_timeout: Duration,
    pub server_ping_timeout: Duration,
}

impl Default for SocketTimings {
    fn default() -> Self {
        Self { client_ping_timeout: Duration::from_millis(5000), server_ping_timeout: Duration::from_millis(30000) }
    }
}

pub type EnvelopeHandler = Arc<dyn Fn(Envelope, Acker) + Send + Sync>;

/// 시작한 소켓 모드 연결. stop()이나 drop이면 재연결을 멈춘다.
pub struct SocketMode {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl SocketMode {
    /// `client.start()`. 첫 hello를 받으면 돌아온다. 복구할 수 없는 연결 주소 오류만 실패다.
    pub async fn start(client: WebClient, timings: SocketTimings, handler: EnvelopeHandler) -> Result<Self, SlackError> {
        let shared = Arc::new(Shared { writer: Mutex::new(None), shutting_down: AtomicBool::new(false) });
        let (connected, started) = oneshot::channel();
        let task = tokio::spawn(run(client, timings, handler, Arc::clone(&shared), connected));
        match started.await {
            Ok(Ok(())) => Ok(Self { shared, task }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(SlackError::Plain("Socket Mode 연결이 끝났습니다.".into())),
        }
    }

    pub fn stop(&self) {
        self.shared.shutting_down.store(true, Ordering::SeqCst);
        self.task.abort();
    }
}

impl Drop for SocketMode {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn run(
    client: WebClient,
    timings: SocketTimings,
    handler: EnvelopeHandler,
    shared: Arc<Shared>,
    connected: oneshot::Sender<Result<(), SlackError>>,
) {
    let mut connected = Some(connected);
    let mut failures: u32 = 0;
    loop {
        let url = match retrieve_wss_url(&client, timings, &mut failures, &shared).await {
            Ok(url) => url,
            Err(error) => {
                match connected.take() {
                    Some(connected) => {
                        let _ = connected.send(Err(error));
                    }
                    // Node는 재연결 중 이 오류를 처리하지 않는다. 여기서는 기록하고 재연결을 멈춘다.
                    None => eprintln!("Slack 연결 오류 {}", error.message()),
                }
                return;
            }
        };
        if let Err(error) = connect_and_run(&url, timings, &handler, &shared, &mut connected).await {
            eprintln!("[ERROR]  socket-mode:SlackWebSocket WebSocket error occurred: {error}");
        }
        *shared.writer.lock().unwrap() = None;
        if shared.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        // delayReconnectAttempt: clientPingTimeout × 연속 실패 횟수만큼 기다린다.
        failures += 1;
        tokio::time::sleep(timings.client_ping_timeout * failures).await;
        if shared.shutting_down.load(Ordering::SeqCst) {
            return;
        }
    }
}

/// retrieveWSSURL. 복구 가능한 오류는 같은 대기 규칙으로 다시 묻는다.
async fn retrieve_wss_url(
    client: &WebClient,
    timings: SocketTimings,
    failures: &mut u32,
    shared: &Shared,
) -> Result<String, SlackError> {
    loop {
        let error = match client.api_call("apps.connections.open", json!({})).await {
            Ok(response) => match response.get("url").filter(|url| js_truthy(Some(url))) {
                Some(url) => {
                    *failures = 0;
                    return Ok(crate::js::js_string(url));
                }
                None => SlackError::Plain("apps.connections.open did not return a URL! (response: [object Object])".into()),
            },
            Err(error) => error,
        };
        eprintln!("[ERROR]  socket-mode:SocketModeClient Failed to retrieve a new WSS URL (error: {})", error.message());
        let recoverable = match &error {
            SlackError::Platform(_) => !error.platform_code().is_some_and(|code| UNRECOVERABLE.contains(&code)),
            SlackError::Request(_) | SlackError::Http(_) => false,
            SlackError::Plain(_) => true,
        };
        if !recoverable || shared.shutting_down.load(Ordering::SeqCst) {
            return Err(error);
        }
        *failures += 1;
        tokio::time::sleep(timings.client_ping_timeout * *failures).await;
    }
}

/// wss는 Mozilla 인증서 묶음(webpki-roots)으로 TLS를 연 뒤 핸드셰이크하고, ws는 그대로 연다(시험용 가짜 서버).
async fn connect_and_run(
    url: &str,
    timings: SocketTimings,
    handler: &EnvelopeHandler,
    shared: &Arc<Shared>,
    connected: &mut Option<oneshot::Sender<Result<(), SlackError>>>,
) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|error| error.to_string())?;
    if parsed.scheme() != "wss" {
        let (stream, _) = tokio_tungstenite::connect_async(url).await.map_err(|error| error.to_string())?;
        connection(stream, timings, handler, shared, connected).await;
        return Ok(());
    }
    let host = parsed.host_str().unwrap_or_default().to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await.map_err(|error| error.to_string())?;
    let server_name = rustls::pki_types::ServerName::try_from(host).map_err(|error| error.to_string())?;
    let tls = tls_connector()?.connect(server_name, tcp).await.map_err(|error| error.to_string())?;
    let (stream, _) = tokio_tungstenite::client_async(url, tls).await.map_err(|error| error.to_string())?;
    connection(stream, timings, handler, shared, connected).await;
    Ok(())
}

fn tls_connector() -> Result<tokio_rustls::TlsConnector, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

/// 연결 하나를 닫힐 때까지 돌린다. SlackWebSocket의 ping·pong 감시와 disconnect 규칙을 따른다.
async fn connection<S>(
    stream: tokio_tungstenite::WebSocketStream<S>,
    timings: SocketTimings,
    handler: &EnvelopeHandler,
    shared: &Arc<Shared>,
    connected: &mut Option<oneshot::Sender<Result<(), SlackError>>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut sink, mut source) = stream.split();
    let (writer, mut outgoing) = mpsc::unbounded_channel();
    // 열리면(readyState OPEN) hello 전이라도 ack를 보낼 수 있다.
    *shared.writer.lock().unwrap() = Some(writer);
    let ping_every = timings.client_ping_timeout / 3;
    let mut ping = tokio::time::interval_at(Instant::now() + ping_every, ping_every);
    let mut last_pong: Option<f64> = None;
    let mut ping_attempts = 0u32;
    let mut server_deadline: Option<Instant> = None;
    let mut closing = false;
    loop {
        tokio::select! {
            incoming = source.next() => {
                let message = match incoming {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => {
                        eprintln!("[ERROR]  socket-mode:SlackWebSocket WebSocket error occurred: {error}");
                        return;
                    }
                    None => return,
                };
                match message {
                    Message::Text(text) => {
                        let Ok(event) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                        match event.get("type").and_then(Value::as_str) {
                            Some("hello") => {
                                if let Some(connected) = connected.take() {
                                    let _ = connected.send(Ok(()));
                                }
                                continue;
                            }
                            Some("disconnect") => {
                                if closing {
                                    return;
                                }
                                closing = true;
                                let _ = sink.send(normal_close()).await;
                                continue;
                            }
                            _ => {}
                        }
                        let field = |key: &str| event.get(key).cloned();
                        let envelope = Envelope {
                            kind: field("type"),
                            envelope_id: field("envelope_id"),
                            body: field("payload"),
                            retry_num: field("retry_attempt"),
                            retry_reason: field("retry_reason"),
                        };
                        handler(envelope, Acker(Arc::clone(shared)));
                    }
                    Message::Ping(_) => server_deadline = Some(Instant::now() + timings.server_ping_timeout),
                    Message::Pong(_) => last_pong = Some(now_milliseconds()),
                    Message::Close(_) => return,
                    Message::Binary(_) | Message::Frame(_) => {}
                }
            }
            Some(message) = outgoing.recv() => {
                if sink.send(message).await.is_err() {
                    return;
                }
            }
            _ = ping.tick() => {
                if closing {
                    // 닫기 프레임을 보냈는데 답이 없으면 Node처럼 바로 끊는다.
                    return;
                }
                let now = now_milliseconds();
                let payload = format!("Ping from client ({})", number_to_string(now));
                if sink.send(Message::Ping(payload.into_bytes().into())).await.is_err() {
                    return;
                }
                if last_pong.is_none() {
                    ping_attempts += 1;
                } else {
                    ping_attempts = 0;
                }
                let invalid = match last_pong {
                    Some(pong) => now - pong > timings.client_ping_timeout.as_secs_f64() * 1000.0,
                    None => ping_attempts > 3,
                };
                if invalid {
                    eprintln!(
                        "[WARN]  socket-mode:SlackWebSocket A pong wasn't received from the server before the timeout of {}ms!",
                        timings.client_ping_timeout.as_millis()
                    );
                    closing = true;
                    let _ = sink.send(normal_close()).await;
                }
            }
            _ = async { tokio::time::sleep_until(server_deadline.unwrap()).await }, if server_deadline.is_some() => {
                eprintln!(
                    "[WARN]  socket-mode:SlackWebSocket A ping wasn't received from the server before the timeout of {}ms!",
                    timings.server_ping_timeout.as_millis()
                );
                server_deadline = None;
                if closing {
                    return;
                }
                closing = true;
                let _ = sink.send(normal_close()).await;
            }
        }
    }
}

fn normal_close() -> Message {
    Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() }))
}
