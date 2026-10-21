// Telegram 개인 대화를 기존 4317 직원 실행 API에 연결한다. 토큰은 디스크 밖으로 기록하지 않는다.
import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

const TERMINAL = new Set(['completed', 'failed', 'interrupted']);

export function readTelegramConfiguration(file = join(homedir(), '.config/officestra/telegram.env')) {
  const values = {};
  for (const line of readFileSync(file, 'utf8').split(/\r?\n/)) {
    const match = line.match(/^\s*(?:export\s+)?([A-Z_][A-Z_0-9]*)\s*=\s*(.*?)\s*$/);
    if (match) values[match[1]] = match[2].replace(/^(['"])(.*)\1$/, '$2');
  }
  if (!values.TELEGRAM_BOT_TOKEN) throw new Error('Telegram token is missing');
  return { token: values.TELEGRAM_BOT_TOKEN };
}

export function telegramClient(token, fetcher = fetch) {
  return async (method, body = {}) => {
    let response;
    try {
      response = await fetcher(`https://api.telegram.org/bot${token}/${method}`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body), signal: AbortSignal.timeout(method === 'getUpdates' ? 35_000 : 20_000),
      });
    } catch {
      // fetch 오류에는 요청 URL(봇 토큰)이 들어갈 수 있다.
      throw Object.assign(new Error(`Telegram ${method} connection failed`), { uncertain: true });
    }
    const payload = await response.json().catch(() => ({}));
    if (!response.ok || !payload.ok) {
      throw Object.assign(new Error(`Telegram ${method} failed (${response.status})`), {
        status: response.status, retryAfter: payload.parameters?.retry_after,
      });
    }
    return payload.result;
  };
}

export function backendClient(base = 'http://127.0.0.1:4317', fetcher = fetch) {
  const url = new URL(base);
  if (url.protocol !== 'http:' || !['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname)) {
    throw new Error('Telegram bridge requires a loopback backend');
  }
  return async (path, body) => {
    let response;
    try {
      response = await fetcher(new URL(path, base), {
        method: body ? 'POST' : 'GET', headers: body ? { 'content-type': 'application/json' } : {},
        body: body ? JSON.stringify(body) : undefined, signal: AbortSignal.timeout(120_000),
      });
    } catch { throw Object.assign(new Error('OFFICESTRA backend connection failed'), { uncertain: Boolean(body) }); }
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw Object.assign(new Error(`OFFICESTRA request failed (${response.status})`), { status: response.status });
    return data;
  };
}

export function splitTelegramText(text, limit = 3800) {
  const points = Array.from(String(text));
  const parts = [];
  let part = '', units = 0;
  for (const point of points) {
    if (units + point.length > limit) { parts.push(part); part = ''; units = 0; }
    part += point; units += point.length;
  }
  if (part) parts.push(part);
  return parts;
}

export function createTelegramBridge({ state, persist, telegram, backend, logger = console }) {
  state.chats ??= {};
  state.pending ??= {};
  state.offset ??= 0;
  if (!Object.keys(state.allowedChats ?? {}).length) throw new Error('Explicit Telegram owner allowlist is required');
  let checking = false;
  const submitting = new Set();

  function chatState(chatID) {
    return state.chats[chatID] ??= { selected: 'boss', conversations: {}, routes: {} };
  }
  function remember(chatID, messageID, characterID) {
    if (!messageID) return;
    const routes = chatState(chatID).routes;
    routes[messageID] = characterID;
    for (const id of Object.keys(routes).slice(0, Math.max(0, Object.keys(routes).length - 500))) delete routes[id];
    persist(state);
  }
  async function send(chatID, text, characterID, extra = {}) {
    const message = await telegram('sendMessage', { chat_id: chatID, text, ...extra });
    if (characterID) remember(chatID, message.message_id, characterID);
    return message;
  }
  async function characters() { return (await backend('/api/characters')).characters ?? []; }
  async function roleplay(characterID) {
    try { return await backend(`/api/roleplay/profiles/${encodeURIComponent(characterID)}`); }
    catch (error) { if (error.status === 404) return null; throw error; }
  }
  async function menu(chatID) {
    const list = await characters();
    const modes = await Promise.all(list.map(c => roleplay(c.id)));
    const selected = chatState(chatID).selected;
    await send(chatID, '대화할 직원을 선택하세요. 선택 후 메시지를 보내면 직원의 답변이 여기로 옵니다.\n\n/office 직원 선택 · /status 현재 상태 · /new 선택한 직원과 새 대화\n직원 이름: 내용 형식이나 직원 답장에 답글을 달아도 됩니다.', null, {
      reply_markup: { inline_keyboard: list.map((c, i) => [{ text: `${c.id === selected ? '✓ ' : ''}${c.name}${modes[i]?.profile?.enabled ? ' · 롤플레이' : ''}`, callback_data: `staff:${c.id}` }]) },
    });
  }
  async function receive(update) {
    if (!Number.isSafeInteger(update.update_id) || update.update_id < state.offset) return;
    // 먼저 저장한다. 전송 결과가 불명확해도 같은 사용자 요청을 자동 재실행하지 않는다.
    state.offset = update.update_id + 1;
    persist(state);
    const callback = update.callback_query;
    const message = callback?.message ?? update.message;
    const chat = message?.chat;
    const from = callback?.from ?? message?.from;
    if (chat?.type !== 'private' || !from?.id || from.is_bot ||
        !Object.hasOwn(state.allowedChats, String(chat.id)) ||
        String(state.allowedChats?.[String(chat?.id)]) !== String(from?.id)) return;
    const chatID = String(chat.id);
    const local = chatState(chatID);
    if (callback) {
      await telegram('answerCallbackQuery', { callback_query_id: callback.id });
      const id = String(callback.data ?? '').replace(/^staff:/, '');
      const character = (await characters()).find(c => c.id === id);
      if (!character) { await menu(chatID); return; }
      local.selected = id; persist(state);
      const activeRoleplay = (await roleplay(id))?.profile?.enabled;
      await send(chatID, `${character.name} 선택 완료. ${activeRoleplay ? '앱에서 진행 중인 롤플레이 대화에 이어집니다.' : '이제 메시지를 보내세요.'}`, id);
      return;
    }
    const text = message.text?.trim();
    if (!text) { await send(chatID, '현재는 텍스트 대화를 지원합니다. 내용을 글로 보내주세요.'); return; }
    const command = text.split(/\s/)[0].split('@')[0];
    if (['/start', '/office', '/staff', '/help'].includes(command)) { await menu(chatID); return; }
    if (command === '/new') {
      const active = await roleplay(local.selected);
      if (active?.profile?.enabled) {
        if (active.generating) { await send(chatID, '롤플레이 답변을 만드는 중입니다. 끝난 후 새 대화를 시작해 주세요.', local.selected); return; }
        await backend(`/api/roleplay/profiles/${encodeURIComponent(local.selected)}/chats`, {});
        await send(chatID, '새 롤플레이 대화를 시작했습니다. 이전 대화 기록과 설정은 보존됩니다.', local.selected);
        return;
      }
      delete local.conversations[local.selected]; persist(state);
      await send(chatID, '선택한 직원과 다음 메시지부터 새 대화를 시작합니다. 기존 기록은 보존됩니다.', local.selected);
      return;
    }
    if (command === '/status') {
      const employees = (await backend('/api/agent-jobs')).employees ?? [];
      const lines = await Promise.all(employees.map(async c => {
        const mode = await roleplay(c.characterId ?? c.id);
        return `${c.name}: ${mode?.profile?.enabled ? '롤플레이 · ' + (mode.generating ? '답변 생성 중' : '대화 가능') : c.canReceive ? '대화 가능' : '작업 중 (' + c.state + ')'}`;
      }));
      await send(chatID, lines.join('\n') || '직원이 없습니다.');
      return;
    }
    if (text.startsWith('/')) { await menu(chatID); return; }
    const list = await characters();
    let target = local.routes[message.reply_to_message?.message_id] ?? local.selected;
    let prompt = text;
    for (const c of list) {
      for (const prefix of [`${c.name}:`, `${c.name}：`, `@${c.name} `, `@${c.id} `]) {
        if (text.startsWith(prefix)) { target = c.id; prompt = text.slice(prefix.length).trim(); }
      }
    }
    const character = list.find(c => c.id === target);
    if (!character || !prompt) { await menu(chatID); return; }
    const active = await roleplay(target);
    const isRoleplay = active?.profile?.enabled === true;
    if (isRoleplay && active.generating) {
      await send(chatID, `${character.name}의 롤플레이 답변을 만드는 중입니다. 완료 후 이어 보내주세요.`, target);
      return;
    }
    const availability = isRoleplay ? null : await backend(`/api/agent-jobs?characterId=${encodeURIComponent(target)}`);
    if (!isRoleplay && !availability.employees?.[0]?.canReceive) {
      await send(chatID, `${character.name}은 현재 작업 중입니다. 끝난 후 다시 보내거나 /office에서 다른 직원을 선택하세요.`, target);
      return;
    }
    local.selected = target; persist(state);
    const status = await send(chatID, `⏳ ${character.name}${isRoleplay ? ' 롤플레이에' : '에게'} 전달합니다.`, target, {
      reply_parameters: { message_id: message.message_id },
    });
    const key = String(update.update_id);
    const pending = state.pending[key] = { chatID, characterID: target, name: character.name,
      statusMessageID: status.message_id, stage: 'launching', nextPart: 0,
      ...(isRoleplay ? { mode: 'roleplay', afterMessageID: active.messages?.at(-1)?.id ?? null } : {}) };
    persist(state);
    submitting.add(key);
    try {
      if (isRoleplay) {
        const accepted = await backend(`/api/roleplay/profiles/${encodeURIComponent(target)}/messages`, { content: prompt });
        if (!accepted.accepted || !accepted.chatId) throw new Error('Invalid roleplay response');
        pending.roleplayChatID = accepted.chatId; pending.stage = 'running';
        persist(state);
        return;
      }
      const job = await backend('/api/agent-jobs', {
        characterId: target, senderCharacterId: 'boss', requestSource: 'remote', prompt,
        ...(local.conversations[target] ? { conversationId: local.conversations[target] } : {}),
      });
      if (!job.turnId || !job.conversationId) throw new Error('Invalid job response');
      pending.turnID = job.turnId; pending.stage = 'running';
      local.conversations[target] = job.conversationId; persist(state);
    } catch (error) {
      pending.stage = 'uncertain'; persist(state);
      await send(chatID, error.status === 409 ? `${character.name}이 다른 작업을 시작해 접수하지 못했습니다. 잠시 후 다시 보내주세요.` :
        `${character.name} 전달을 확인하지 못했습니다. /status와 앱의 대화 기록을 확인해 주세요. 같은 요청을 자동으로 다시 실행하지 않습니다.`, target);
      logger.warn('Telegram employee submission did not complete', error.status ?? 'connection');
      delete state.pending[key]; persist(state);
    } finally { submitting.delete(key); }
  }

  async function deliver(key, pending) {
    if (pending.stage === 'launching' || pending.stage === 'uncertain') {
      await send(pending.chatID, `${pending.name}에게 전달하던 중 연결이 끊겼습니다. 앱 기록을 확인해 주세요. 같은 요청은 자동 재실행하지 않았습니다.`, pending.characterID);
      delete state.pending[key]; persist(state); return;
    }
    let turn;
    if (pending.mode === 'roleplay') {
      const current = await roleplay(pending.characterID);
      if (current?.profile?.currentChatId !== pending.roleplayChatID) {
        turn = { status: 'interrupted', response: '앱에서 롤플레이 대화가 변경되어 이전 답변을 여기로 가져오지 않았습니다. 앱에서 이전 대화를 확인해 주세요.' };
      } else if (current.generating) return;
      else if (current.lastError) turn = { status: 'failed', response: '롤플레이 답변 생성에 실패했습니다. 앱의 롤플레이 대화에 표시된 오류를 확인해 주세요.' };
      else {
        const history = current.messages ?? [];
        const anchor = pending.afterMessageID ? history.findIndex(m => m.id === pending.afterMessageID) : -1;
        const added = history.slice(anchor + 1);
        const users = added.filter(m => m.role === 'user');
        if ((pending.afterMessageID && anchor < 0) || users.length > 1) {
          turn = { status: 'interrupted', response: '앱과 텔레그램의 동시 입력으로 답변 대상을 확정할 수 없습니다. 앱에서 롤플레이 대화를 확인해 주세요.' };
        } else {
          const userIndex = added.findIndex(m => m.role === 'user');
          const reply = userIndex < 0 ? null : added.slice(userIndex + 1).find(m => m.role === 'char');
          if (!reply) return;
          turn = { status: 'completed', response: reply.content };
        }
      }
    } else ({ turn } = await backend(`/api/live-feed/${encodeURIComponent(pending.turnID)}`));
    if (!turn) return;
    if (!TERMINAL.has(turn.status)) return;
    const failure = /SSH|4090|로컬 연결 실패/i.test(turn.errorMessage ?? '')
      ? '직원의 로컬 모델(4090) 연결에 실패했습니다. /office에서 다른 직원을 선택하거나 앱에서 로컬 모델 연결을 확인해 주세요.'
      : `작업 ${turn.status === 'failed' ? '실패' : '중단'} — 앱에서 상세 내용을 확인해 주세요.`;
    pending.parts ??= splitTelegramText(`[${pending.name}]\n${turn.response?.trim() ||
      (turn.status === 'completed' ? '완료했습니다. 응답 본문은 앱에서 확인해 주세요.' : failure)}`);
    while (pending.nextPart < pending.parts.length) {
      const part = pending.parts[pending.nextPart++];
      // 네트워크 응답이 불명확할 때 중복 답장을 자동 전송하지 않도록 먼저 예약한다.
      persist(state);
      try { await send(pending.chatID, part, pending.characterID); }
      catch (error) {
        if (!error.uncertain) pending.nextPart--;
        else pending.uncertainDelivery = true;
        if (error.retryAfter) pending.retryAt = Date.now() + error.retryAfter * 1000;
        if ([400, 403].includes(error.status)) pending.stage = 'delivery-failed';
        persist(state); throw error;
      }
    }
    const label = pending.uncertainDelivery ? '답장 일부의 전송 확인 불가 · 앱에서 확인' :
      turn.needsInput ? '답변 필요 · 메시지를 이어 보내세요' : turn.status === 'completed' ? '완료' : turn.status === 'failed' ? '실행 실패' : '중단';
    const icon = pending.uncertainDelivery ? '⚠️' : turn.status === 'failed' ? '❌' : turn.status === 'interrupted' ? '⏹️' : turn.needsInput ? '❓' : '✅';
    await telegram('editMessageText', { chat_id: pending.chatID, message_id: pending.statusMessageID,
      text: `${icon} ${pending.name}: ${label}` }).catch(() => {});
    delete state.pending[key]; persist(state);
    logger.log(`Telegram ${pending.mode === 'roleplay' ? 'roleplay' : 'turn'} delivered: ${pending.turnID ?? pending.roleplayChatID}`);
  }
  async function tick() {
    if (checking) return;
    checking = true;
    try {
      for (const [key, pending] of Object.entries(state.pending)) {
        if (submitting.has(key) || pending.stage === 'delivery-failed' || pending.retryAt > Date.now()) continue;
        try { await deliver(key, pending); }
        catch (error) { logger.warn('Telegram delivery pending', error.status ?? 'connection'); }
      }
    } finally { checking = false; }
  }
  return { receive, tick, menu };
}
