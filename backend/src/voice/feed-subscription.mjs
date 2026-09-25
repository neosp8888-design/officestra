import { WebSocket } from 'ws';

// 연결/재연결 때 한 번 동기화하고, 이후에는 변경 알림만 처리한다.
// 대기 타이머로 DB를 조회하지 않는다. 실패 시에만 제한된 빈도로 재시도한다.
export function subscribeVoiceFeed({ backend, characterId, since, onTurn,
  onError = () => {}, batchMs = 100, retryMs = 1000, maxRetryMs = 30000 }) {
  let closed = false;
  let socket;
  let reconnectTimer;
  let refreshTimer;
  let request;
  let dirty = false;
  let previous = null;
  let reconnectDelay = retryMs;
  let refreshDelay = retryMs;
  const endpoint = new URL('/api/voice-feed', backend);
  endpoint.searchParams.set('characterId', characterId);
  endpoint.searchParams.set('since', since);
  const socketURL = new URL('/ws', backend);
  socketURL.protocol = socketURL.protocol === 'https:' ? 'wss:' : 'ws:';

  function schedule(delay = batchMs) {
    if (closed) return;
    dirty = true;
    if (request || refreshTimer) return;
    refreshTimer = setTimeout(refresh, delay);
  }

  async function refresh() {
    refreshTimer = null;
    if (closed) return;
    dirty = false;
    request = new AbortController();
    let failed = false;
    try {
      const response = await fetch(endpoint, {
        signal: AbortSignal.any([request.signal, AbortSignal.timeout(10000)]),
      });
      if (!response.ok) throw new Error(`음성 응답 조회 HTTP ${response.status}`);
      const { turn } = await response.json();
      if (closed) return;
      refreshDelay = retryMs;
      if (turn && turn.characterId === characterId) {
        const key = JSON.stringify([turn.id, turn.status, turn.response]);
        if (key !== previous) {
          onTurn(turn);
          previous = key;
        }
      }
    } catch (error) {
      if (!closed) { failed = true; onError(error); }
    } finally {
      request = null;
      if (!closed && failed) {
        schedule(refreshDelay);
        refreshDelay = Math.min(maxRetryMs, refreshDelay * 2);
      } else if (dirty) schedule();
    }
  }

  function connect() {
    if (closed) return;
    socket = new WebSocket(socketURL, { handshakeTimeout: 10000 });
    socket.on('open', () => {
      reconnectDelay = retryMs;
      schedule(0);
    });
    socket.on('message', data => {
      let event;
      try { event = JSON.parse(data.toString()); } catch { return; }
      if (event.type !== 'feed.changed') return;
      if (event.characterId && event.characterId !== characterId) return;
      schedule();
    });
    socket.on('error', error => { if (!closed) onError(error); });
    socket.on('close', () => {
      if (closed) return;
      reconnectTimer = setTimeout(connect, reconnectDelay);
      reconnectDelay = Math.min(maxRetryMs, reconnectDelay * 2);
    });
  }

  connect();
  return {
    close() {
      closed = true;
      clearTimeout(reconnectTimer);
      clearTimeout(refreshTimer);
      request?.abort();
      socket?.terminate();
    },
  };
}
