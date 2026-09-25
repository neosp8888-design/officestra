// 음성 전용 응답 조회와 변경 알림을 흉내 내는 시험용 서버다.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
const { WebSocketServer, WebSocket } = createRequire(new URL('../backend/package.json', import.meta.url))('ws');

const [file = new URL('../backend/test/fixtures/voice-sample-reply.md', import.meta.url).pathname, port = '47999', cps = '30', delayMs = '1500'] = process.argv.slice(2);
const text = await readFile(file, 'utf8');
const startedAt = Date.now() + Number(delayMs);

const server = createServer((request, response) => {
  const elapsed = (Date.now() - startedAt) / 1000;
  const turns = [];
  if (elapsed >= 0) {
    // 백엔드처럼 250ms 단위로 합쳐진 누적 원문을 돌려준다.
    const shown = Math.min(text.length, Math.floor(Math.floor(elapsed * 4) / 4 * Number(cps)));
    turns.push({
      id: 'mock-turn', characterId: 'right-woman', status: shown >= text.length ? 'completed' : 'running',
      startedAt: new Date(startedAt).toISOString(), response: text.slice(0, shown),
    });
  }
  response.setHeader('content-type', 'application/json');
  response.end(JSON.stringify(request.url.startsWith('/api/voice-feed') ? { turn: turns[0] ?? null } : { turns }));
});
const wss = new WebSocketServer({ server, path: '/ws' });
const timer = setInterval(() => {
  if (Date.now() < startedAt) return;
  for (const socket of wss.clients) {
    if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify({
      type: 'feed.changed', characterId: 'right-woman', turnId: 'mock-turn',
    }));
  }
  if ((Date.now() - startedAt) / 1000 * Number(cps) >= text.length) clearInterval(timer);
}, 250);
server.listen(Number(port), '127.0.0.1');
