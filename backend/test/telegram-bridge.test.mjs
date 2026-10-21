import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createTelegramBridge, splitTelegramText, telegramClient, backendClient, readTelegramConfiguration } from '../src/telegram-bridge.mjs';

const employees = [{ id: 'boss', name: '백부장' }, { id: 'left-man', name: '클대리' }];
function fixture(state = { allowedChats: { 123: '123' }, offset: 0 }) {
  const calls = [], saved = [], jobs = [], roleplayPosts = [];
  let next = 100, available = true, jobError = null, postGate = null;
  let turn = { status: 'running', response: '' }, telegramError = null;
  let roleplay = { profile: { enabled: false }, messages: [], generating: false }, roleplayError = null;
  const backend = async (path, body) => {
    calls.push({ path, body });
    if (path === '/api/characters') return { characters: employees };
    if (path.startsWith('/api/roleplay/profiles/')) {
      if (roleplayError) throw roleplayError;
      if (body) {
        roleplayPosts.push({ path, body });
        return path.endsWith('/messages') ? { accepted: true, chatId: roleplay.profile.currentChatId } : {};
      }
      return roleplay;
    }
    if (path.startsWith('/api/agent-jobs?')) return { employees: [{ canReceive: available }] };
    if (path === '/api/agent-jobs' && !body) return { employees: employees.map(c => ({ ...c, canReceive: available })) };
    if (path === '/api/agent-jobs') {
      jobs.push(body);
      if (postGate) await postGate;
      if (jobError) throw jobError;
      return { turnId: 'turn-' + jobs.length, conversationId: body.conversationId ?? 'conversation-' + jobs.length };
    }
    if (path.startsWith('/api/live-feed/')) return { turn };
    throw new Error(path);
  };
  const telegram = async (method, payload) => {
    calls.push({ method, payload });
    if (telegramError && method === 'sendMessage') { const error = telegramError; telegramError = null; throw error; }
    return { message_id: next++ };
  };
  const bridge = createTelegramBridge({ state, persist: s => saved.push(structuredClone(s)), telegram, backend,
    logger: { warn() {}, log() {} } });
  return { state, bridge, calls, saved, jobs, roleplayPosts, backend, telegram,
    set roleplay(v) { roleplay = v; }, set roleplayError(v) { roleplayError = v; },
    set available(v) { available = v; }, set turn(v) { turn = v; },
    set jobError(v) { jobError = v; }, set postGate(v) { postGate = v; }, set telegramError(v) { telegramError = v; } };
}
function message(id, text, options = {}) {
  return { update_id: id, message: { message_id: id, chat: { id: 123, type: 'private' }, from: { id: 123 }, text, ...options } };
}

test('owner allowlist is mandatory; strangers, groups and missing senders cannot launch work', async () => {
  assert.throws(() => createTelegramBridge({ state: {} }), /allowlist/);
  const f = fixture();
  await f.bridge.receive(message(1, 'hello', { from: { id: 999 } }));
  await f.bridge.receive(message(2, 'hello', { chat: { id: 123, type: 'group' } }));
  await f.bridge.receive(message(3, 'hello', { from: undefined }));
  await f.bridge.receive(message(4, 'hello', { chat: { id: 999, type: 'private' }, from: undefined }));
  assert.equal(f.calls.length, 0);
  assert.equal(f.state.offset, 5);
});

test('staff menu uses real employee IDs; callbacks select employee and answer spinner', async () => {
  const f = fixture();
  await f.bridge.receive(message(1, '/office'));
  const menu = f.calls.find(c => c.method === 'sendMessage').payload;
  assert.equal(menu.reply_markup.inline_keyboard[1][0].callback_data, 'staff:left-man');
  await f.bridge.receive({ update_id: 2, callback_query: { id: 'cb', from: { id: 123 }, data: 'staff:left-man', message: message(0, '').message } });
  assert.equal(f.state.chats[123].selected, 'left-man');
  assert.ok(f.calls.some(c => c.method === 'answerCallbackQuery'));
  assert.equal(f.jobs.length, 0);
});

test('dispatch persists receipt before POST, preserves multiline prompt and resumes same employee conversation', async () => {
  const f = fixture();
  await f.bridge.receive(message(1, '클대리: 첫 줄\n둘째 줄'));
  assert.deepEqual(f.jobs[0], { characterId: 'left-man', senderCharacterId: 'boss', requestSource: 'remote', prompt: '첫 줄\n둘째 줄' });
  assert.ok(f.saved.some(s => s.offset === 2 && Object.values(s.pending)[0]?.stage === 'launching'));
  await f.bridge.receive(message(1, 'duplicate'));
  assert.equal(f.jobs.length, 1);
  await f.bridge.receive(message(2, '이어서'));
  assert.equal(f.jobs[1].conversationId, 'conversation-1');
  assert.equal(f.jobs[1].characterId, 'left-man');
});

test('reply to an earlier employee response routes to its employee even after selection changes', async () => {
  const f = fixture();
  await f.bridge.receive(message(1, '클대리: 질문'));
  f.state.chats[123].selected = 'boss';
  await f.bridge.receive(message(2, '추가 질문', { reply_to_message: { message_id: 100 } }));
  assert.equal(f.jobs[1].characterId, 'left-man');
});

test('busy preflight and authoritative POST 409 never cancel or retry employee work', async () => {
  const f = fixture(); f.available = false;
  await f.bridge.receive(message(1, '질문'));
  assert.equal(f.jobs.length, 0);
  f.available = true; f.jobError = Object.assign(new Error('busy'), { status: 409 });
  await f.bridge.receive(message(2, '질문'));
  await f.bridge.receive(message(2, '질문'));
  assert.equal(f.jobs.length, 1);
  assert.deepEqual(f.state.pending, {});
  assert.ok(f.calls.some(c => c.payload?.text?.includes('접수하지 못했습니다')));
});

test('completed response is relayed in chunks and not redelivered on next tick', async () => {
  const f = fixture(); await f.bridge.receive(message(1, '질문'));
  await f.bridge.tick();
  assert.equal(Object.keys(f.state.pending).length, 1);
  const response = '한😀'.repeat(3000);
  f.turn = { status: 'completed', response, needsInput: true };
  await f.bridge.tick();
  const answers = f.calls.filter(c => c.method === 'sendMessage').slice(1);
  assert.ok(answers.length >= 3);
  assert.equal(answers.map(c => c.payload.text).join(''), '[백부장]\n' + response);
  const count = f.calls.length;
  await f.bridge.tick(); assert.equal(f.calls.length, count);
  assert.ok(f.calls.some(c => c.method === 'editMessageText' && c.payload.text.includes('답변 필요')));
  assert.deepEqual(f.state.pending, {});
});

test('restart restores pending response delivery without relaunching work', async () => {
  const f = fixture(); await f.bridge.receive(message(1, '질문'));
  const restored = fixture(structuredClone(f.state));
  restored.turn = { status: 'completed', response: '응답' };
  await restored.bridge.tick();
  assert.equal(restored.jobs.length, 0);
  assert.ok(restored.calls.some(c => c.payload?.text === '[백부장]\n응답'));
});

test('ambiguous Telegram send is not duplicated and final status reports the uncertainty', async () => {
  const f = fixture(); await f.bridge.receive(message(1, '질문'));
  f.turn = { status: 'completed', response: '확인' };
  f.telegramError = Object.assign(new Error('timeout'), { uncertain: true });
  await f.bridge.tick();
  assert.equal(Object.values(f.state.pending)[0].uncertainDelivery, true);
  await f.bridge.tick();
  assert.equal(f.calls.filter(c => c.payload?.text === '[백부장]\n확인').length, 1);
  assert.ok(f.calls.some(c => c.method === 'editMessageText' && c.payload.text.includes('전송 확인 불가')));
});

test('Telegram rate limit delays delivery retry; blocked chat does not retry indefinitely', async () => {
  const f = fixture(); await f.bridge.receive(message(1, '질문'));
  f.turn = { status: 'completed', response: '확인' };
  f.telegramError = Object.assign(new Error('rate limit'), { status: 429, retryAfter: 30 });
  await f.bridge.tick(); const count = f.calls.length;
  await f.bridge.tick(); assert.equal(f.calls.length, count);
  const pending = Object.values(f.state.pending)[0]; pending.retryAt = 0;
  f.telegramError = Object.assign(new Error('blocked'), { status: 403 });
  await f.bridge.tick();
  assert.equal(pending.stage, 'delivery-failed');
  const blocked = f.calls.length; await f.bridge.tick(); assert.equal(f.calls.length, blocked);
});

test('monitor does not mistake an in-flight job submission for a crashed launch', async () => {
  const f = fixture(); let release;
  f.postGate = new Promise(resolve => { release = resolve; });
  const receive = f.bridge.receive(message(1, '질문'));
  while (!f.jobs.length) await new Promise(resolve => setImmediate(resolve));
  await f.bridge.tick();
  assert.equal(Object.keys(f.state.pending).length, 1);
  release(); await receive;
  assert.equal(Object.values(f.state.pending)[0].stage, 'running');
});

test('uncertain launch after process restart is reported without a second POST', async () => {
  const f = fixture({ allowedChats: { 123: '123' }, pending: { 1: {
    stage: 'launching', chatID: '123', characterID: 'boss', name: '백부장', nextPart: 0,
  } } });
  await f.bridge.tick();
  assert.equal(f.jobs.length, 0);
  assert.ok(f.calls.some(c => c.payload?.text?.includes('자동 재실행하지 않았습니다')));
});

test('failed/interrupted turns produce a visible result, and /new retains old records', async () => {
  const f = fixture(); await f.bridge.receive(message(1, '질문'));
  f.turn = { status: 'failed', errorMessage: '로컬 연결 실패: 4090 SSH operation failed' }; await f.bridge.tick();
  assert.ok(f.calls.some(c => c.payload?.text?.includes('실패')));
  assert.ok(f.calls.some(c => c.payload?.text?.includes('로컬 모델(4090)')));
  assert.ok(f.calls.some(c => c.method === 'editMessageText' && c.payload.text.startsWith('❌')));
  await f.bridge.receive(message(2, '/new'));
  assert.deepEqual(f.state.chats[123].conversations, {});
  assert.ok(f.calls.some(c => c.payload?.text?.includes('기존 기록은 보존')));
});

test('Telegram network errors redact tokens and are uncertain; HTTP 429 preserves retry_after', async () => {
  const client = telegramClient('SECRET', async () => { throw new Error('https://api.telegram.org/botSECRET/sendMessage'); });
  await assert.rejects(client('sendMessage'), e => !e.message.includes('SECRET') && e.uncertain);
  const throttled = telegramClient('SECRET', async () => ({ ok: false, status: 429, json: async () => ({ ok: false, parameters: { retry_after: 20 } }) }));
  await assert.rejects(throttled('sendMessage'), e => e.status === 429 && e.retryAfter === 20);
  assert.throws(() => backendClient('https://example.com'), /loopback/);
});

test('Unicode split respects UTF-16 limits without breaking emoji or dropping whitespace', () => {
  const source = 'a😀\n '.repeat(2000), parts = splitTelegramText(source);
  assert.equal(parts.join(''), source);
  assert.ok(parts.every(part => part.length <= 3800 && !/[\uD800-\uDBFF]$/.test(part)));
});

test('token file supports quoted/exported environment values without executing shell code', () => {
  const dir = mkdtempSync(join(tmpdir(), 'telegram-test-'));
  try {
    const path = join(dir, 'telegram.env');
    writeFileSync(path, 'export TELEGRAM_BOT_TOKEN="example-token"\n');
    assert.equal(readTelegramConfiguration(path).token, 'example-token');
  } finally { rmSync(dir, { recursive: true }); }
});

function roleplayState(messages = [], extra = {}) {
  return { profile: { enabled: true, currentChatId: 'existing-roleplay-chat', presetId: 'unchanged-preset' },
    messages, generating: false, lastError: null, ...extra };
}

test('enabled roleplay routes to current roleplay API without launching CLI or altering its chat/settings', async () => {
  const f = fixture(); f.available = false;
  f.roleplay = roleplayState([{ id: 'old-user', role: 'user' }, { id: 'old-reply', role: 'char', content: 'previous' }]);
  await f.bridge.receive(message(1, '클대리: 다음 대화'));
  assert.deepEqual(f.roleplayPosts, [{ path: '/api/roleplay/profiles/left-man/messages', body: { content: '다음 대화' } }]);
  assert.equal(f.jobs.length, 0);
  assert.equal(f.calls.filter(c => c.path?.startsWith('/api/agent-jobs')).length, 0);
  assert.deepEqual(f.state.chats[123].conversations, {});
  assert.equal(f.state.pending[1].roleplayChatID, 'existing-roleplay-chat');
  assert.equal(f.state.pending[1].afterMessageID, 'old-reply');
});

test('roleplay polling waits for new reply and only delivers messages after the accepted user input', async () => {
  const f = fixture();
  const old = [{ id: 'old', role: 'char', content: 'must not repeat' }];
  f.roleplay = roleplayState(old);
  await f.bridge.receive(message(1, '안녕'));
  f.roleplay = roleplayState([...old, { id: 'input', role: 'user', content: '안녕' }], { generating: true });
  await f.bridge.tick(); assert.ok(f.state.pending[1]);
  f.roleplay = roleplayState([...old, { id: 'input', role: 'user' }, { id: 'reply', role: 'char', content: '안녕하세요' }]);
  await f.bridge.tick();
  assert.ok(f.calls.some(c => c.payload?.text === '[백부장]\n안녕하세요'));
  assert.ok(!f.calls.some(c => c.payload?.text?.includes('must not repeat')));
  assert.deepEqual(f.state.pending, {});
});

test('roleplay busy state blocks only generation and does not touch employee CLI sessions', async () => {
  const f = fixture(); f.roleplay = roleplayState([], { generating: true });
  await f.bridge.receive(message(1, '대화'));
  await f.bridge.receive(message(2, '/new'));
  assert.equal(f.roleplayPosts.length, 0);
  assert.equal(f.jobs.length, 0);
  assert.ok(f.calls.some(c => c.payload?.text?.includes('완료 후')));
});

test('roleplay response is restored across bridge restart, without another generation', async () => {
  const f = fixture(); f.roleplay = roleplayState();
  await f.bridge.receive(message(1, '질문'));
  const restored = fixture(structuredClone(f.state));
  restored.roleplay = roleplayState([{ id: 'input', role: 'user' }, { id: 'reply', role: 'char', content: 'reply after restart' }]);
  await restored.bridge.tick();
  assert.equal(restored.jobs.length, 0);
  assert.equal(restored.roleplayPosts.length, 0);
  assert.ok(restored.calls.some(c => c.payload?.text?.includes('reply after restart')));
});

test('roleplay chat change and simultaneous user inputs never deliver an unrelated reply', async () => {
  for (const changedChat of [false, true]) {
    const f = fixture(); f.roleplay = roleplayState();
    await f.bridge.receive(message(1, '질문'));
    f.roleplay = roleplayState([{ id: 'input1', role: 'user' }, { id: 'input2', role: 'user' },
      { id: 'reply', role: 'char', content: 'unrelated reply' }],
    changedChat ? { profile: { enabled: true, currentChatId: 'different-chat' } } : {});
    await f.bridge.tick();
    assert.ok(!f.calls.some(c => c.payload?.text?.includes('unrelated reply')));
    assert.deepEqual(f.state.pending, {});
  }
});

test('roleplay generation failure is surfaced and never falls back to CLI', async () => {
  const f = fixture(); f.roleplay = roleplayState();
  await f.bridge.receive(message(1, '질문'));
  f.roleplay = roleplayState([{ id: 'input', role: 'user' }], { lastError: 'upstream failed' });
  await f.bridge.tick();
  assert.ok(f.calls.some(c => c.payload?.text?.includes('롤플레이 답변 생성에 실패')));
  assert.equal(f.jobs.length, 0);
});

test('roleplay profile errors fail closed; only a missing API permits normal employee routing', async () => {
  const f = fixture(); f.roleplayError = Object.assign(new Error('unavailable'), { status: 503 });
  await assert.rejects(f.bridge.receive(message(1, '질문')));
  assert.equal(f.jobs.length, 0);
  f.roleplayError = Object.assign(new Error('not found'), { status: 404 });
  await f.bridge.receive(message(2, '질문'));
  assert.equal(f.jobs.length, 1);
});

test('roleplay /new uses existing new-chat API only after explicit command; menu identifies its mode', async () => {
  const f = fixture(); f.roleplay = roleplayState();
  await f.bridge.receive(message(1, '/office'));
  const menu = f.calls.find(c => c.payload?.reply_markup)?.payload;
  assert.ok(menu.reply_markup.inline_keyboard[0][0].text.includes('롤플레이'));
  assert.equal(f.roleplayPosts.length, 0);
  await f.bridge.receive(message(2, '/new'));
  assert.deepEqual(f.roleplayPosts, [{ path: '/api/roleplay/profiles/boss/chats', body: {} }]);
  assert.equal(f.jobs.length, 0);
});
