#!/usr/bin/env node
// 독립 연결 프로세스. 4317과 앱을 재시작하지 않고 기존 API만 사용한다.
import { existsSync, readFileSync, writeFileSync, renameSync, mkdirSync, openSync, closeSync, unlinkSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { createTelegramBridge, readTelegramConfiguration, telegramClient, backendClient } from '../backend/src/telegram-bridge.mjs';

const directory = join(homedir(), '.config/officestra');
const statePath = join(directory, 'telegram-state.json');
const lockPath = join(directory, 'telegram-bridge.pid');
mkdirSync(directory, { recursive: true, mode: 0o700 });
let lock;
try { lock = openSync(lockPath, 'wx', 0o600); }
catch (error) {
  if (error.code !== 'EEXIST') throw error;
  const pid = Number(readFileSync(lockPath, 'utf8'));
  try { process.kill(pid, 0); console.error('Telegram bridge already running'); process.exit(1); }
  catch (check) { if (check.code !== 'ESRCH') throw check; }
  unlinkSync(lockPath); lock = openSync(lockPath, 'wx', 0o600);
}
writeFileSync(lock, String(process.pid)); closeSync(lock);
process.on('exit', () => { try { unlinkSync(lockPath); } catch {} });
let stopping = false;
for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => { stopping = true; process.exit(0); });
function persist(state) {
  const temporary = statePath + '.tmp';
  writeFileSync(temporary, JSON.stringify(state) + '\n', { mode: 0o600 });
  renameSync(temporary, statePath);
}

const telegram = telegramClient(readTelegramConfiguration().token);
const backend = backendClient();
if (process.argv.includes('--pair')) {
  // 설치 시에만 방금 검증한 개인 /start 대화를 명시적으로 등록한다. 런타임 자동 등록 금지.
  if (existsSync(statePath)) throw new Error('Telegram state already exists; owner pairing is never overwritten');
  const updates = await telegram('getUpdates', { timeout: 0, limit: 100 });
  const recent = updates.filter(u => u.message?.chat?.type === 'private' &&
    !u.message.from?.is_bot && u.message.text === '/start' && Date.now() / 1000 - u.message.date < 7200);
  const ids = new Set(recent.map(u => String(u.message.from.id)));
  if (ids.size !== 1) throw new Error('Exactly one recent verified owner /start is required');
  const message = recent.at(-1).message;
  persist({ offset: Math.max(0, ...updates.map(u => u.update_id + 1)),
    allowedChats: { [String(message.chat.id)]: String(message.from.id) }, chats: {}, pending: {} });
  console.log('Verified private Telegram owner paired');
  process.exit(0);
}
if (!existsSync(statePath)) throw new Error('Pair the verified owner before starting Telegram bridge');
const state = JSON.parse(readFileSync(statePath, 'utf8'));
const info = await telegram('getWebhookInfo');
if (info.url) throw new Error('Existing Telegram webhook detected; refusing to replace it');
const bridge = createTelegramBridge({ state, persist, telegram, backend });
if (process.argv.includes('--announce')) {
  await telegram('setMyCommands', { commands: [
    { command: 'office', description: '대화할 직원 선택' },
    { command: 'status', description: '직원별 현재 작업 상태' },
    { command: 'new', description: '선택한 직원과 새 대화' },
  ] });
  for (const id of Object.keys(state.allowedChats)) await bridge.menu(id);
  console.log('Telegram employee menu sent'); process.exit(0);
}
const timer = setInterval(() => { void bridge.tick(); }, 2000);
console.log('OFFICESTRA Telegram bridge started');
while (!stopping) {
  try {
    const updates = await telegram('getUpdates', { offset: state.offset, timeout: 25,
      allowed_updates: ['message', 'callback_query'] });
    for (const update of updates) {
      try { await bridge.receive(update); }
      catch (error) {
        console.error('Telegram update handling failed', error.status ?? 'connection');
        const message = update.callback_query?.message ?? update.message;
        const from = update.callback_query?.from ?? message?.from;
        if (message?.chat?.type === 'private' && from?.id &&
            String(state.allowedChats[String(message.chat.id)]) === String(from.id)) {
          await telegram('sendMessage', { chat_id: message.chat.id,
            text: '연결 문제로 요청 처리를 확인하지 못했습니다. /status와 앱 기록을 확인한 뒤 다시 보내주세요.' }).catch(() => {});
        }
      }
    }
  } catch (error) {
    console.error('Telegram polling failed', error.status ?? 'connection');
    if ([401, 409].includes(error.status)) { clearInterval(timer); process.exit(0); }
    await new Promise(resolve => setTimeout(resolve, Math.max(5000, (error.retryAfter ?? 0) * 1000)));
  }
}
