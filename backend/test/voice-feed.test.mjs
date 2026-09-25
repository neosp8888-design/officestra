import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { fork } from 'node:child_process';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';
import { subscribeVoiceFeed } from '../src/voice/feed-subscription.mjs';

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(predicate) {
  for (let i = 0; i < 200; i++) {
    if (predicate()) return;
    await sleep(10);
  }
  assert.fail('condition did not become true');
}

async function fixture(t) {
  const state = { requests: 0, connections: 0, delay: 0, failure: false,
    turn: { id: 'a', characterId: 'right-woman', startedAt: new Date().toISOString(),
      response: '첫 문장.', status: 'running' } };
  const server = createServer(async (req, res) => {
    state.requests++;
    const url = new URL(req.url, 'http://localhost');
    assert.equal(url.pathname, '/api/voice-feed');
    assert.equal(url.searchParams.get('characterId'), 'right-woman');
    assert.ok(url.searchParams.get('since'));
    const body = JSON.stringify({ turn: state.turn });
    await sleep(state.delay);
    res.writeHead(state.failure ? 503 : 200, { 'Content-Type': 'application/json' });
    res.end(body);
  });
  const wss = new WebSocketServer({ server, path: '/ws' });
  wss.on('connection', () => state.connections++);
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const updates = [];
  const errors = [];
  const subscription = subscribeVoiceFeed({
    backend: `http://127.0.0.1:${server.address().port}`, characterId: 'right-woman',
    since: new Date().toISOString(), onTurn: turn => updates.push(turn),
    onError: error => errors.push(error), batchMs: 20, retryMs: 30, maxRetryMs: 120,
  });
  t.after(async () => {
    subscription.close();
    for (const client of wss.clients) client.terminate();
    await new Promise(resolve => wss.close(resolve));
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  });
  const notify = (characterId = 'right-woman') => {
    for (const c of wss.clients) c.send(JSON.stringify({ type: 'feed.changed', characterId }));
  };
  await until(() => updates.length === 1);
  return { state, updates, errors, notify, wss, subscription };
}

test('음성 대기와 다른 직원 알림은 추가 조회 0회', async t => {
  const { state, notify, updates } = await fixture(t);
  for (let i = 0; i < 100; i++) notify('boss');
  await sleep(1200); // 이전 대기 폴링 주기보다 길게 관찰
  assert.equal(state.requests, 1, '연결 시 최초 동기화 이외의 조회 없음');
  assert.equal(updates.length, 1);
});

test('알림 폭주를 합치고 동일 텍스트 재전달 없이 최종 상태는 전달', async t => {
  const { state, notify, updates } = await fixture(t);
  for (let i = 0; i < 100; i++) notify();
  await until(() => state.requests === 2);
  await sleep(60);
  assert.equal(updates.length, 1);
  assert.equal(state.requests, 2);
  state.turn = { ...state.turn, response: '첫 문장. 마지막 문장', status: 'completed' };
  notify();
  await until(() => updates.length === 2);
  assert.equal(updates[1].status, 'completed');
  assert.equal(updates[1].response, state.turn.response);
});

test('조회 도중 도착한 변경을 놓치지 않고 새 턴으로 전환', async t => {
  const { state, notify, updates } = await fixture(t);
  state.delay = 100;
  notify();
  await until(() => state.requests === 2);
  state.turn = { ...state.turn, id: 'b', response: '새 답변.' };
  notify();
  await until(() => updates.some(turn => turn.id === 'b'));
  assert.equal(state.requests, 3);
});

test('재연결 시 1회 동기화하되 이미 받은 응답은 다시 읽지 않음', async t => {
  const { state, updates, wss } = await fixture(t);
  for (const socket of wss.clients) socket.terminate();
  await until(() => state.connections === 2 && state.requests === 2);
  await sleep(80);
  assert.equal(updates.length, 1);
  assert.equal(state.requests, 2);
});

test('조회 오류는 재시도로 복구하고 종료 후 재연결/조회하지 않음', async t => {
  const { state, notify, errors, updates, subscription } = await fixture(t);
  state.failure = true;
  notify();
  await until(() => errors.length > 0);
  state.failure = false;
  state.turn = { ...state.turn, response: '복구된 답변.' };
  await until(() => updates.length === 2);
  subscription.close();
  const requests = state.requests;
  await sleep(200);
  assert.equal(state.requests, requests);
});

test('실제 따라읽기 프로세스는 알림으로 문장을 마무리하고 재연결 후 재낭독하지 않음', async t => {
  const home = await mkdtemp(join(tmpdir(), 'office-voice-events-'));
  let turn = null;
  let requests = 0;
  const server = createServer((req, res) => {
    assert.match(req.url, /^\/api\/voice-feed\?/);
    requests++;
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ turn }));
  });
  const wss = new WebSocketServer({ server, path: '/ws' });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const child = fork(fileURLToPath(new URL('../src/voice/voice-follow.mjs', import.meta.url)), [
    '--follow', 'right-woman', '--dry-run', '--backend', `http://127.0.0.1:${server.address().port}`,
  ], { stdio: ['ignore', 'ignore', 'ignore', 'ipc'], execArgv: [], env: { ...process.env, OFFICE_VOICE_DIR: home } });
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) {
      const exited = once(child, 'exit'); child.kill('SIGTERM'); await exited;
    }
    for (const c of wss.clients) c.terminate();
    await new Promise(resolve => wss.close(resolve));
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    await rm(home, { recursive: true, force: true });
  });
  await until(() => requests === 1);
  await sleep(1100);
  assert.equal(requests, 1);
  turn = { id: 'stream', characterId: 'right-woman', startedAt: new Date().toISOString(),
    status: 'running', response: '첫 문장입니다. ' };
  const notify = () => { for (const c of wss.clients) c.send(JSON.stringify({ type: 'feed.changed', characterId: 'right-woman' })); };
  notify();
  await until(() => requests === 2);
  await sleep(100);
  turn = { ...turn, status: 'completed', response: '첫 문장입니다. 마지막 문장입니다' };
  notify();
  let records = [];
  for (let i = 0; i < 100; i++) {
    try { records = (await readFile(join(home, 'logs/latency.jsonl'), 'utf8')).trim().split('\n').map(JSON.parse); } catch { /* writing */ }
    if (records.length) break;
    await sleep(20);
  }
  assert.equal(records.length, 1);
  assert.equal(records[0].sentences, 2, '완료 알림에서 마침표 없는 마지막 문장도 읽음');
  const beforeReconnect = requests;
  for (const c of wss.clients) c.terminate();
  await until(() => requests === beforeReconnect + 1);
  await sleep(200);
  const final = (await readFile(join(home, 'logs/latency.jsonl'), 'utf8')).trim().split('\n');
  assert.equal(final.length, 1);
});
