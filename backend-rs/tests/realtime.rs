// /ws 웹소켓 통합 테스트. 실제 TCP 서버를 띄워 Node ws 8.21과 같은 연결 수락·ready·방송·끊기를 확인한다.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use officestra_backend::db;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::realtime::Realtime;
use officestra_backend::usage_report::TurnCostSummaryCache;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const READY: &str = r#"{"type":"ready","compactingCharacterIds":[]}"#;
const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

async fn serve() -> (SocketAddr, Arc<Realtime>) {
    let realtime = Arc::new(Realtime::default());
    let state = AppState {
        pool: db::connect_lazy("postgres://office:x@127.0.0.1:1/none", Duration::from_secs(1)).unwrap(),
        runtime: Arc::new(Runtime::new(None, None)),
        release_id: None,
        cost_summary: Arc::default(),
        realtime: Arc::clone(&realtime),
        embedding: LocalEmbeddingService::default(),
        model_catalog: None,
        cli_updates: Arc::default(),
        usage_summary: None,
        roleplay: None,
        voice_follow: Arc::default(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    (address, realtime)
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(tokio::time::Instant::now() < deadline, "5초 안에 조건이 맞아야 한다");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 원시 HTTP 요청 하나를 보내고 머리말(소문자 이름)과 본문, 응답 뒤 연결이 닫혔는지를 돌려준다.
struct RawResponse {
    status: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    closed: bool,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

async fn read_more(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> usize {
    let mut chunk = [0u8; 4096];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk)).await.expect("응답이 와야 한다").unwrap();
    buffer.extend_from_slice(&chunk[..read]);
    read
}

/// body_length가 None이면 Content-Length만큼 읽는다. 101 응답은 이어지는 웹소켓 프레임 길이를 넘긴다.
async fn raw(address: SocketAddr, request: &str, body_length: Option<usize>, check_closed: bool) -> RawResponse {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buffer = Vec::new();
    let head_end = loop {
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        assert!(read_more(&mut stream, &mut buffer).await > 0, "머리말 전에 연결이 닫혔다");
    };
    let head = String::from_utf8(buffer[..head_end].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().to_string();
    let headers: Vec<(String, String)> = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.to_ascii_lowercase(), value.trim().to_string())
        })
        .collect();
    let length = body_length.unwrap_or_else(|| {
        headers.iter().find(|(name, _)| name == "content-length").map_or(0, |(_, value)| value.parse().unwrap())
    });
    while buffer.len() < head_end + 4 + length {
        assert!(read_more(&mut stream, &mut buffer).await > 0, "본문 전에 연결이 닫혔다");
    }
    let body = buffer[head_end + 4..head_end + 4 + length].to_vec();
    let closed = check_closed && read_more(&mut stream, &mut buffer).await == 0;
    RawResponse { status, headers, body, closed }
}

fn handshake(method: &str, extra: &str) -> String {
    format!("{method} /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n{extra}\r\n")
}

#[tokio::test]
async fn ready_then_broadcasts_reach_every_client_in_order() {
    let (address, realtime) = serve().await;
    let url = format!("ws://{address}/ws");
    let (mut first, _) = connect_async(url.as_str()).await.unwrap();
    let (mut second, _) = connect_async(url.as_str()).await.unwrap();
    for client in [&mut first, &mut second] {
        assert_eq!(client.next().await.unwrap().unwrap(), Message::text(READY));
    }
    // ready를 받았으면 방송 목록에도 들어가 있다.
    assert_eq!(realtime.connection_count(), 2);

    realtime.broadcast(&json!({ "type": "model-catalog.changed", "provider": "codex" }));
    realtime.broadcast(&json!({ "type": "feed.changed", "turnId": "t1", "costChanged": true, "ratio": 1.0, "big": 1e21 }));
    for client in [&mut first, &mut second] {
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            Message::text(r#"{"type":"model-catalog.changed","provider":"codex"}"#)
        );
        // JSON.stringify처럼 키 순서를 지키고 숫자는 JS 표기다.
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            Message::text(r#"{"type":"feed.changed","turnId":"t1","costChanged":true,"ratio":1,"big":1e+21}"#)
        );
    }
}

#[tokio::test]
async fn closed_and_dropped_clients_leave_the_broadcast_list() {
    let (address, realtime) = serve().await;
    let url = format!("ws://{address}/ws");
    let (mut closing, _) = connect_async(url.as_str()).await.unwrap();
    let (mut dropping, _) = connect_async(url.as_str()).await.unwrap();
    let (mut staying, _) = connect_async(url.as_str()).await.unwrap();
    for client in [&mut closing, &mut dropping, &mut staying] {
        assert_eq!(client.next().await.unwrap().unwrap(), Message::text(READY));
    }
    assert_eq!(realtime.connection_count(), 3);

    // 닫는 프레임을 보내면 서버도 닫는 프레임으로 답한다.
    closing.close(None).await.unwrap();
    let mut answered = false;
    while let Some(Ok(message)) = closing.next().await {
        answered |= matches!(message, Message::Close(_));
    }
    assert!(answered, "서버가 닫는 프레임으로 답해야 한다");
    wait_until(|| realtime.connection_count() == 2).await;

    // 닫는 프레임 없이 TCP만 끊겨도 목록에서 빠진다(Node error·close 처리).
    drop(dropping);
    wait_until(|| realtime.connection_count() == 1).await;

    realtime.broadcast(&json!({ "type": "model-catalog.changed", "provider": "claude" }));
    assert_eq!(
        staying.next().await.unwrap().unwrap(),
        Message::text(r#"{"type":"model-catalog.changed","provider":"claude"}"#)
    );
}

#[tokio::test]
async fn terminate_cuts_connections_without_close_frame() {
    let (address, realtime) = serve().await;
    let url = format!("ws://{address}/ws");
    let (mut client, _) = connect_async(url.as_str()).await.unwrap();
    assert_eq!(client.next().await.unwrap().unwrap(), Message::text(READY));

    // 테스트 런타임은 한 스레드라 아래 방송과 terminate 사이에 연결 작업이 끼지 않는다. 쌓인 방송도 나가지 않아야 한다.
    for index in 0..100 {
        realtime.broadcast(&json!({ "type": "feed.changed", "turnId": index }));
    }
    realtime.terminate();
    let next = tokio::time::timeout(Duration::from_secs(5), client.next()).await.expect("연결이 끊겨야 한다");
    assert!(matches!(next, None | Some(Err(_))), "쌓인 방송 없이, 닫는 프레임 없이 끊겨야 한다: {next:?}");
    assert_eq!(realtime.connection_count(), 0);

    // 종료 중에 들어온 연결도 ready 없이 바로 끊는다.
    let (mut late, _) = connect_async(url.as_str()).await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), late.next()).await.expect("연결이 끊겨야 한다");
    assert!(matches!(next, None | Some(Err(_))), "ready 없이 끊겨야 한다: {next:?}");
    assert_eq!(realtime.connection_count(), 0);
}

#[tokio::test]
async fn first_offered_subprotocol_is_selected() {
    let (address, _) = serve().await;
    let mut request = format!("ws://{address}/ws").into_client_request().unwrap();
    request.headers_mut().insert("Sec-WebSocket-Protocol", "chat, superchat".parse().unwrap());
    let (mut client, response) = connect_async(request).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "chat");
    assert_eq!(client.next().await.unwrap().unwrap(), Message::text(READY));
}

#[tokio::test]
async fn raw_handshake_accepts_version_8_and_sends_ready_frame() {
    let (address, _) = serve().await;
    let request = handshake("GET", &format!("Sec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 8\r\n"));
    let response = raw(address, &request, Some(2 + READY.len()), false).await;
    assert_eq!(response.status, "HTTP/1.1 101 Switching Protocols");
    // RFC 6455 예시 키의 응답 값.
    assert_eq!(response.header("sec-websocket-accept"), Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
    assert_eq!(response.header("upgrade"), Some("websocket"));
    assert_eq!(response.header("sec-websocket-protocol"), None);
    assert_eq!(response.header("sec-websocket-extensions"), None, "ws 기본값처럼 압축을 쓰지 않는다");
    let mut frame = vec![0x81, READY.len() as u8];
    frame.extend_from_slice(READY.as_bytes());
    assert_eq!(response.body, frame);
}

#[tokio::test]
async fn raw_handshake_failures_follow_ws_and_close() {
    let (address, realtime) = serve().await;
    let cases = [
        (handshake("POST", &format!("Sec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 13\r\n")), "405 Method Not Allowed", "Invalid HTTP method", None),
        (
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: Upgrade\r\nUpgrade: h2c\r\n\r\n".to_string(),
            "400 Bad Request",
            "Invalid Upgrade header",
            None,
        ),
        (handshake("GET", "Sec-WebSocket-Version: 13\r\n"), "400 Bad Request", "Missing or invalid Sec-WebSocket-Key header", None),
        (
            handshake("GET", &format!("Sec-WebSocket-Key: {KEY}\r\n")),
            "400 Bad Request",
            "Missing or invalid Sec-WebSocket-Version header",
            Some("13, 8"),
        ),
        (
            handshake("GET", &format!("Sec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: chat,\r\n")),
            "400 Bad Request",
            "Invalid Sec-WebSocket-Protocol header",
            None,
        ),
    ];
    for (request, status, message, version) in cases {
        let response = raw(address, &request, None, true).await;
        assert_eq!(response.status, format!("HTTP/1.1 {status}"), "{message}");
        assert_eq!(response.header("connection"), Some("close"), "{message}");
        assert_eq!(response.header("content-type"), Some("text/html"), "{message}");
        assert_eq!(response.header("sec-websocket-version"), version, "{message}");
        assert_eq!(response.body, message.as_bytes());
        assert!(response.closed, "{message} 뒤 연결이 닫혀야 한다");
    }
    assert_eq!(realtime.connection_count(), 0);
}

#[tokio::test]
async fn non_upgrade_requests_to_ws_are_json_404() {
    let (address, _) = serve().await;
    for request in [
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        // Upgrade 헤더만 있고 Connection에 upgrade 토큰이 없으면 일반 요청이다.
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nUpgrade: websocket\r\n\r\n",
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: Upgrade\r\n\r\n",
    ] {
        let response = raw(address, request, None, false).await;
        assert_eq!(response.status, "HTTP/1.1 404 Not Found", "{request}");
        assert_eq!(response.header("content-type"), Some("application/json; charset=utf-8"));
        assert_eq!(String::from_utf8(response.body).unwrap(), r#"{"error":"경로를 찾을 수 없습니다."}"#);
    }
}

async fn read_summary(cache: &TurnCostSummaryCache, queries: &AtomicUsize) {
    cache
        .read_with(|| async {
            queries.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(Vec::new())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn cost_and_feedback_broadcasts_invalidate_the_summary_cache() {
    let cache = Arc::new(TurnCostSummaryCache::default());
    let realtime = Realtime::new(Arc::clone(&cache));
    let queries = AtomicUsize::new(0);
    read_summary(&cache, &queries).await;
    read_summary(&cache, &queries).await;
    assert_eq!(queries.load(Ordering::SeqCst), 1);

    realtime.broadcast(&json!({ "type": "feed.changed", "turnId": "t1" }));
    realtime.broadcast(&json!({ "type": "model-catalog.changed", "provider": "codex" }));
    read_summary(&cache, &queries).await;
    assert_eq!(queries.load(Ordering::SeqCst), 1, "비용·평가와 무관한 방송은 캐시를 지우지 않는다");

    realtime.broadcast(&json!({ "type": "feed.changed", "turnId": "t1", "costChanged": true }));
    read_summary(&cache, &queries).await;
    assert_eq!(queries.load(Ordering::SeqCst), 2);

    realtime.broadcast(&json!({ "type": "feed.changed", "turnId": "t1", "feedbackChanged": true }));
    read_summary(&cache, &queries).await;
    assert_eq!(queries.load(Ordering::SeqCst), 3);
}
