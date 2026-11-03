// 웹소켓 /ws 연결과 방송. Node server.mjs의 sockets 집합·broadcast()·upgrade 처리와 ws 8.21의 연결 수락 규칙을 옮긴다.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use axum::extract::{FromRequestParts, Request};
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::http::header::{
    CONNECTION, CONTENT_TYPE, HeaderName, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION, UPGRADE,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use regex::Regex;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::js::{string_to_number, to_js_json};
use crate::usage_report::TurnCostSummaryCache;

/// ws 기본 maxPayload(100MiB). 받은 메시지·프레임이 이보다 크면 연결을 끊는다.
const MAX_PAYLOAD: usize = 100 * 1024 * 1024;

static KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[+/0-9A-Za-z]{22}==$").unwrap());

/// 열린 웹소켓 연결 목록과 방송. 연결마다 보낼 메시지 줄을 두고, Node처럼 상대가 느려도 버리지 않고 쌓는다.
pub struct Realtime {
    cost_summary: Arc<TurnCostSummaryCache>,
    clients: Mutex<HashMap<u64, mpsc::UnboundedSender<Utf8Bytes>>>,
    next_id: AtomicU64,
    terminated: AtomicBool,
}

impl Default for Realtime {
    fn default() -> Self {
        Self::new(Arc::default())
    }
}

impl Realtime {
    /// 방송마다 비용·평가 요약 캐시에 먼저 알린다(Node turnCostSummaryCache.observe).
    pub fn new(cost_summary: Arc<TurnCostSummaryCache>) -> Self {
        Self { cost_summary, clients: Mutex::default(), next_id: AtomicU64::new(0), terminated: AtomicBool::new(false) }
    }

    fn clients(&self) -> MutexGuard<'_, HashMap<u64, mpsc::UnboundedSender<Utf8Bytes>>> {
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Node broadcast(). JSON.stringify 한 번으로 만든 같은 본문을 열린 연결 모두에 보낸다.
    pub fn broadcast(&self, event: &Value) {
        self.cost_summary.observe(event);
        let payload = text(event);
        for sender in self.clients().values() {
            let _ = sender.send(payload.clone());
        }
    }

    /// 방송을 받는 열린 연결 수.
    pub fn connection_count(&self) -> usize {
        self.clients().len()
    }

    /// Node shutdown의 socket.terminate(). 닫는 프레임 없이 모든 연결을 끊고, 그 뒤 들어온 연결도 바로 끊는다.
    pub fn terminate(&self) {
        self.terminated.store(true, Ordering::SeqCst);
        self.clients().clear();
    }

    /// /ws 요청 처리. Upgrade 요청이 아니면 None이고 호출한 쪽이 일반 경로처럼 404를 낸다.
    /// compacting은 연결이 열린 순간 ready에 실을 압축 중 직원 ID다(Node runtime.compactingCharacterIDs()).
    pub async fn upgrade(
        self: &Arc<Self>,
        request: Request,
        compacting: impl FnOnce() -> Vec<String> + Send + 'static,
    ) -> Option<Response> {
        if !is_upgrade_request(request.headers()) {
            return None;
        }
        let protocol = match check_handshake(request.method(), request.headers()) {
            Ok(protocol) => protocol,
            Err(response) => return Some(response),
        };
        let (mut parts, _) = request.into_parts();
        // ws는 버전 13과 8, 여러 Connection 헤더를 같게 받는다. 검사는 위에서 끝냈으니 axum 추출기가 읽는 꼴로 맞춘다.
        parts.headers.insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
        parts.headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(upgrade) => upgrade,
            Err(rejection) => return Some(rejection.into_response()),
        };
        let realtime = Arc::clone(self);
        Some(
            upgrade
                .protocols(protocol)
                .max_message_size(MAX_PAYLOAD)
                .max_frame_size(MAX_PAYLOAD)
                .on_upgrade(move |socket| realtime.serve(socket, compacting))
                .into_response(),
        )
    }

    /// Node connection 처리. ready를 먼저 보내고 방송 목록에 넣는다. 받은 메시지는 버린다(message 처리기 없음, ping은 pong으로 답함).
    async fn serve(self: Arc<Self>, mut socket: WebSocket, compacting: impl FnOnce() -> Vec<String>) {
        if self.terminated.load(Ordering::SeqCst) {
            return;
        }
        let (sender, mut outgoing) = mpsc::unbounded_channel();
        let _ = sender.send(text(&json!({ "type": "ready", "compactingCharacterIds": compacting() })));
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.clients().insert(id, sender);
        if self.terminated.load(Ordering::SeqCst) {
            self.clients().remove(&id);
            return;
        }
        loop {
            tokio::select! {
                payload = outgoing.recv() => match payload {
                    // terminate() 뒤에는 쌓여 있던 방송도 보내지 않고 닫는 프레임 없이 끊는다(Node socket.terminate()).
                    Some(_) | None if self.terminated.load(Ordering::SeqCst) => return,
                    Some(payload) => {
                        if socket.send(Message::Text(payload)).await.is_err() {
                            break;
                        }
                    }
                    None => return,
                },
                incoming = socket.recv() => match incoming {
                    Some(Ok(Message::Close(_))) => {
                        // 닫는 중인 연결에는 방송하지 않는다(Node readyState OPEN 검사). 답장 닫는 프레임이 나가도록 끝까지 읽는다.
                        self.clients().remove(&id);
                        while let Some(Ok(_)) = socket.recv().await {}
                        return;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                },
            }
        }
        self.clients().remove(&id);
    }
}

fn text(event: &Value) -> Utf8Bytes {
    let bytes = to_js_json(event).unwrap_or_else(|_| b"{}".to_vec());
    Utf8Bytes::from(String::from_utf8(bytes).unwrap_or_default())
}

/// Node IncomingMessage처럼 같은 이름의 헤더를 ", "로 잇는다. 값은 latin1로 읽는다.
fn joined(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    let values: Vec<String> =
        headers.get_all(name).iter().map(|value| value.as_bytes().iter().map(|&byte| char::from(byte)).collect()).collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// llhttp가 Upgrade 요청으로 보는 조건: Upgrade 헤더가 있고 Connection 값에 upgrade 토큰이 있다.
fn is_upgrade_request(headers: &HeaderMap) -> bool {
    headers.contains_key(UPGRADE)
        && headers.get_all(CONNECTION).iter().any(|value| {
            value.as_bytes().split(|&byte| byte == b',').any(|token| token.trim_ascii().eq_ignore_ascii_case(b"upgrade"))
        })
}

/// ws handleUpgrade() 검사 순서. 통과하면 고를 하위 프로토콜(클라이언트가 처음 적은 것)을 돌려준다.
fn check_handshake(method: &Method, headers: &HeaderMap) -> Result<Option<String>, Response> {
    if method != Method::GET {
        return Err(abort_handshake(StatusCode::METHOD_NOT_ALLOWED, "Invalid HTTP method", None));
    }
    if !joined(headers, &UPGRADE).is_some_and(|upgrade| upgrade.eq_ignore_ascii_case("websocket")) {
        return Err(abort_handshake(StatusCode::BAD_REQUEST, "Invalid Upgrade header", None));
    }
    if !joined(headers, &SEC_WEBSOCKET_KEY).is_some_and(|key| KEY.is_match(&key)) {
        return Err(abort_handshake(StatusCode::BAD_REQUEST, "Missing or invalid Sec-WebSocket-Key header", None));
    }
    let version = joined(headers, &SEC_WEBSOCKET_VERSION).map_or(f64::NAN, |version| string_to_number(&version));
    if version != 13.0 && version != 8.0 {
        return Err(abort_handshake(
            StatusCode::BAD_REQUEST,
            "Missing or invalid Sec-WebSocket-Version header",
            Some((SEC_WEBSOCKET_VERSION, "13, 8")),
        ));
    }
    match joined(headers, &SEC_WEBSOCKET_PROTOCOL) {
        None => Ok(None),
        Some(header) => match parse_subprotocols(&header) {
            Some(protocols) => Ok(protocols.into_iter().next()),
            None => Err(abort_handshake(StatusCode::BAD_REQUEST, "Invalid Sec-WebSocket-Protocol header", None)),
        },
    }
}

/// ws abortHandshake(). text/html 본문에 사유를 적고 연결을 닫는다.
fn abort_handshake(status: StatusCode, message: &'static str, extra: Option<(HeaderName, &'static str)>) -> Response {
    let mut response = (status, [(CONNECTION, "close"), (CONTENT_TYPE, "text/html")], message).into_response();
    if let Some((name, value)) = extra {
        response.headers_mut().insert(name, HeaderValue::from_static(value));
    }
    response
}

/// ws subprotocol.parse(). RFC 7230 토큰을 쉼표로 나누고 앞뒤 공백·탭만 허용한다. 중복이나 빈 항목은 오류(None)다.
fn parse_subprotocols(header: &str) -> Option<Vec<String>> {
    let units: Vec<u16> = header.encode_utf16().collect();
    let token = |unit: u16| unit < 128 && TOKEN_CHARS[unit as usize] == 1;
    let mut protocols: Vec<String> = Vec::new();
    let add = |start: usize, end: usize, protocols: &mut Vec<String>| {
        let protocol = String::from_utf16_lossy(&units[start..end]);
        if protocols.contains(&protocol) {
            return false;
        }
        protocols.push(protocol);
        true
    };
    let (mut start, mut end): (Option<usize>, Option<usize>) = (None, None);
    for (index, &unit) in units.iter().enumerate() {
        if end.is_none() && token(unit) {
            start.get_or_insert(index);
        } else if index != 0 && (unit == 0x20 || unit == 0x09) {
            if end.is_none() && start.is_some() {
                end = Some(index);
            }
        } else if unit == 0x2c {
            let begin = start?;
            if !add(begin, end.unwrap_or(index), &mut protocols) {
                return None;
            }
            (start, end) = (None, None);
        } else {
            return None;
        }
    }
    let (Some(begin), None) = (start, end) else { return None };
    add(begin, units.len(), &mut protocols).then_some(protocols)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 기대값은 Node 24에서 ws 8.21 subprotocol.parse()로 직접 뽑았다.
    #[test]
    fn subprotocols_follow_ws_parser() {
        let parsed = |header: &str| parse_subprotocols(header);
        let list = |items: &[&str]| Some(items.iter().map(|item| item.to_string()).collect::<Vec<_>>());
        assert_eq!(parsed("chat"), list(&["chat"]));
        assert_eq!(parsed("chat, superchat"), list(&["chat", "superchat"]));
        assert_eq!(parsed("chat,superchat"), list(&["chat", "superchat"]));
        assert_eq!(parsed("chat ,superchat"), list(&["chat", "superchat"]));
        assert_eq!(parsed("chat\t,\tsuperchat"), list(&["chat", "superchat"]));
        assert_eq!(parsed("a, \tb"), list(&["a", "b"]));
        assert_eq!(parsed("x!#$%&*+-.^_`|~9"), list(&["x!#$%&*+-.^_`|~9"]));
        for invalid in [" chat", "chat ", "chat,,x", ",chat", "chat,", "a b", "chat, chat", "a,b,a", "a\"b", "a(b)", "", "é"] {
            assert_eq!(parsed(invalid), None, "{invalid:?}");
        }
    }

    fn headers(pairs: &[(&HeaderName, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append((*name).clone(), HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    async fn failure(method: Method, pairs: &[(&HeaderName, &str)]) -> (StatusCode, String, Option<String>) {
        let response = check_handshake(&method, &headers(pairs)).err().expect("거절되어야 한다");
        let status = response.status();
        let version = response.headers().get(SEC_WEBSOCKET_VERSION).map(|value| value.to_str().unwrap().to_string());
        assert_eq!(response.headers().get(CONNECTION).unwrap(), "close");
        assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "text/html");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap(), version)
    }

    #[tokio::test]
    async fn handshake_checks_follow_ws_order() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let invalid = |message: &str| (StatusCode::BAD_REQUEST, message.to_string(), None);
        assert_eq!(
            failure(Method::POST, &[(&UPGRADE, "nope")]).await,
            (StatusCode::METHOD_NOT_ALLOWED, "Invalid HTTP method".to_string(), None)
        );
        assert_eq!(failure(Method::GET, &[(&UPGRADE, "h2c"), (&SEC_WEBSOCKET_KEY, "x")]).await, invalid("Invalid Upgrade header"));
        // 같은 이름 헤더는 ", "로 이어져 "websocket, websocket"이 되므로 거절된다.
        assert_eq!(
            failure(Method::GET, &[(&UPGRADE, "websocket"), (&UPGRADE, "websocket")]).await,
            invalid("Invalid Upgrade header")
        );
        assert_eq!(
            failure(Method::GET, &[(&UPGRADE, "WebSocket"), (&SEC_WEBSOCKET_KEY, "short==")]).await,
            invalid("Missing or invalid Sec-WebSocket-Key header")
        );
        for version in ["13", "13.0", "0xd", "+13", "1.3e1", "8", "08", "0b1101"] {
            let pairs = [(&UPGRADE, "websocket"), (&SEC_WEBSOCKET_KEY, key), (&SEC_WEBSOCKET_VERSION, version)];
            assert_eq!(check_handshake(&Method::GET, &headers(&pairs)).ok(), Some(None), "{version}");
        }
        for version in ["", "12", "13, 13", "x"] {
            let pairs = [(&UPGRADE, "websocket"), (&SEC_WEBSOCKET_KEY, key), (&SEC_WEBSOCKET_VERSION, version)];
            let expected = (
                StatusCode::BAD_REQUEST,
                "Missing or invalid Sec-WebSocket-Version header".to_string(),
                Some("13, 8".to_string()),
            );
            assert_eq!(failure(Method::GET, &pairs).await, expected, "{version}");
        }
        let base = [(&UPGRADE, "websocket"), (&SEC_WEBSOCKET_KEY, key), (&SEC_WEBSOCKET_VERSION, "13")];
        let mut chosen = base.to_vec();
        chosen.push((&SEC_WEBSOCKET_PROTOCOL, "chat, superchat"));
        assert_eq!(check_handshake(&Method::GET, &headers(&chosen)).ok(), Some(Some("chat".to_string())));
        let mut broken = base.to_vec();
        broken.push((&SEC_WEBSOCKET_PROTOCOL, "chat,"));
        assert_eq!(failure(Method::GET, &broken).await, invalid("Invalid Sec-WebSocket-Protocol header"));
    }

    #[test]
    fn upgrade_requests_need_both_headers() {
        assert!(is_upgrade_request(&headers(&[(&UPGRADE, "websocket"), (&CONNECTION, "keep-alive, Upgrade")])));
        assert!(is_upgrade_request(&headers(&[(&UPGRADE, ""), (&CONNECTION, "keep-alive"), (&CONNECTION, "upgrade")])));
        assert!(!is_upgrade_request(&headers(&[(&UPGRADE, "websocket")])));
        assert!(!is_upgrade_request(&headers(&[(&CONNECTION, "upgrade")])));
        assert!(!is_upgrade_request(&headers(&[(&UPGRADE, "websocket"), (&CONNECTION, "upgraded")])));
    }
}

/// ws validation.js tokenChars. RFC 7230 tchar이면 1이다.
#[rustfmt::skip]
const TOKEN_CHARS: [u8; 128] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 1, 0, 1, 1, 1, 1, 1, 0, 0, 1, 1, 0, 1, 1, 0,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0,
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1, 0,
];
