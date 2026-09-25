// 직원 응답이 스트리밍되는 동안 완성된 문장부터 맥에서 음성으로 합성해 바로 재생하고, 첫 음성까지의 지연을 기록한다.
import { spawn } from 'node:child_process';
import { existsSync, rmSync } from 'node:fs';
import { appendFile, mkdir, mkdtemp, readFile, rm } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
import { PARAGRAPH_BREAK, createSentenceStream, parseDirections } from './speakable.mjs';
import { voiceHomeDirectory, voicePythonPath } from './voice-paths.mjs';
import { acquireFollowLock } from './follow-lock.mjs';
import { subscribeVoiceFeed } from './feed-subscription.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const args = parseArgs(process.argv.slice(2));
const backend = args.backend ?? `http://127.0.0.1:${process.env.OFFICE_BACKEND_PORT ?? 4317}`;
const characterId = args.follow ?? 'right-woman';
const localVoices = join(voiceHomeDirectory(), 'voices.json');
const voicesPath = existsSync(localVoices) ? localVoices : join(HERE, 'voices.json');
const voices = JSON.parse(await readFile(voicesPath, 'utf8'));
const logDir = join(voiceHomeDirectory(), 'logs');
// 수동 시제품과 앱 제어기가 동시에 실행돼도 s1.wav 같은 파일명이 충돌하지 않는다.
const audioDir = await mkdtemp(join(tmpdir(), 'officestra-local-voice-'));
process.on('exit', () => rmSync(audioDir, { recursive: true, force: true }));
await mkdir(logDir, { recursive: true });
let releaseFollowLock = null;
try { if (!args.replay) releaseFollowLock = acquireFollowLock(); }
catch (error) { console.error(error.message); process.exit(3); }
if (releaseFollowLock) process.on('exit', releaseFollowLock);

function parseArgs(list) {
  const out = {};
  for (let i = 0; i < list.length; i += 1) {
    const key = list[i].replace(/^--/, '');
    const next = list[i + 1];
    if (next && !next.startsWith('--')) { out[key] = next; i += 1; } else out[key] = true;
  }
  return out;
}

let stopping = false;
let current = null;
let feedSubscription = null;
const now = () => performance.now();
const log = message => console.log(`[${new Date().toTimeString().slice(0, 8)}] ${message}`);

/** 상주 합성 작업자. 문장 하나씩 보내고 답을 기다린다. */
async function startWorker(model) {
  const python = voicePythonPath();
  if (!existsSync(python)) {
    log(`음성 파이썬 환경이 없습니다: ${python}`);
    process.exit(2);
  }
  const child = spawn(python, [join(HERE, 'tts_worker.py')], {
    stdio: ['pipe', 'pipe', 'ignore'],
    env: model ? { ...process.env, LOCAL_VOICE_MODEL: model } : process.env,
  });
  const waiting = new Map();
  let onReady;
  const ready = new Promise(resolve => { onReady = resolve; });
  createInterface({ input: child.stdout }).on('line', line => {
    let message;
    try { message = JSON.parse(line); } catch { return; }
    if (message.ready) onReady(message);
    else waiting.get(message.id)?.(message);
  });
  child.on('exit', code => {
    if (stopping) return;
    log(`합성 작업자가 예기치 않게 종료됨 (코드 ${code})`);
    process.exit(1);
  });
  let counter = 0;
  return {
    ready,
    child,
    synthesize(text, settings) {
      const id = `s${++counter}`;
      const out = join(audioDir, `${process.pid}-${id}.wav`);
      return new Promise(resolve => {
        waiting.set(id, message => {
          waiting.delete(id);
          resolve(message);
        });
        child.stdin.write(`${JSON.stringify({ id, text, ...settings, out })}\n`);
      });
    },
  };
}

const profile = voices[characterId] ?? voices.default;

function voiceFor(sentence, style) {
  const rate = Number(args.rate ?? profile.rate ?? 1);
  // 기준 음성을 복제하는 직원은 말투가 기준 음성에 담겨 있으므로, 괄호 지침의 말투에 맞는 기준 음성을 고른다.
  const ref = profile.styles?.[style] ?? profile;
  if (ref.refAudio) return { refAudio: join(dirname(voicesPath), ref.refAudio), refText: ref.refText, rate };
  const tone = sentence.endsWith('!') ? voices.emotion?.exclaim : sentence.endsWith('?') ? voices.emotion?.question : null;
  return { voice: profile.voice, instruct: [profile.instruct, tone].filter(Boolean).join(', '), rate };
}

/** 한 응답(턴)을 읽는 세션. 합성과 재생을 겹쳐서 다음 문장을 재생 중에 미리 만든다. */
function createSession(worker, label, startedAt) {
  const stream = createSentenceStream({ paragraphBreaks: true });
  const synthQueue = [];
  const playQueue = [];
  const marks = { label, startedAt };
  let cancelled = false;
  let synthesizing = false;
  let playing = null;
  let finished = false;
  let underruns = 0;
  let spokenSentences = 0;
  // 괄호 지침의 말투는 새 지침이 나오거나 문단이 끝날 때까지 이어진다. 쉼은 지침이 붙은 문장에만 쓴다.
  let mood = null;
  let pendingPauseMs = 0;
  let resolveDone;
  const done = new Promise(resolve => { resolveDone = resolve; });

  const mark = key => { if (marks[key] == null) marks[key] = Math.round(now() - startedAt); };

  async function pumpSynth() {
    if (synthesizing || cancelled) return;
    const raw = synthQueue.shift();
    if (raw == null) { maybeDone(); return; }
    if (raw === PARAGRAPH_BREAK) { mood = null; pumpSynth(); return; }
    const parsed = parseDirections(raw);
    if (parsed.style) mood = parsed.style;
    const direction = { style: mood, pauseMs: Math.max(parsed.pauseMs, pendingPauseMs) };
    // 지침만 있고 읽을 말이 없으면 쉼을 다음 문장에 넘긴다.
    if (!/[\p{L}\p{N}]/u.test(parsed.text)) { pendingPauseMs = direction.pauseMs; pumpSynth(); return; }
    pendingPauseMs = 0;
    const sentence = parsed.text;
    synthesizing = true;
    const result = args['dry-run'] ? { path: null, synthMs: 0, audioMs: 0 } : await worker.synthesize(sentence, voiceFor(sentence, direction.style));
    synthesizing = false;
    if (cancelled) { if (result.path) rm(result.path, { force: true }); return; }
    if (result.error) log(`합성 실패: ${result.error}`);
    else {
      mark('firstAudioReadyMs');
      const tags = [direction.style, direction.pauseMs && `쉼 ${direction.pauseMs}ms`].filter(Boolean).join(' · ');
      log(`합성 ${result.synthMs}ms → 음성 ${result.audioMs}ms${tags ? ` [${tags}]` : ''} · ${sentence}`);
      playQueue.push({ ...result, pauseMs: direction.pauseMs });
      pumpPlay();
    }
    pumpSynth();
  }

  function pumpPlay() {
    if (playing || cancelled) return;
    const item = playQueue.shift();
    if (!item) {
      // 재생할 게 없는데 아직 만들 문장이 남아 있으면 끊김으로 센다.
      if (spokenSentences > 0 && (synthesizing || synthQueue.length)) underruns += 1;
      maybeDone();
      return;
    }
    spokenSentences += 1;
    mark('firstPlayMs');
    if (args['dry-run'] || !item.path) { pumpPlay(); return; }
    const start = () => {
      if (args['no-play']) {
        // 소리는 내지 않되 음성 길이만큼 기다려 실제 재생과 같은 끊김을 잰다.
        playing = setTimeout(() => { playing = null; rm(item.path, { force: true }); pumpPlay(); }, item.audioMs);
        return;
      }
      playing = spawn('afplay', [item.path], { stdio: 'ignore' });
      playing.on('exit', () => { playing = null; rm(item.path, { force: true }); pumpPlay(); });
    };
    // "잠시 침묵" 같은 지침이 붙은 문장은 앞에서 쉬었다가 읽는다.
    if (item.pauseMs) playing = setTimeout(start, item.pauseMs);
    else start();
  }

  function maybeDone() {
    if (finished && !synthesizing && !synthQueue.length && !playQueue.length && !playing) {
      marks.totalMs = Math.round(now() - startedAt);
      marks.sentences = spokenSentences;
      marks.underruns = underruns;
      resolveDone(marks);
    }
  }

  function enqueue(sentences) {
    if (!sentences.length) return;
    if (sentences.some(sentence => sentence !== PARAGRAPH_BREAK)) mark('firstSentenceMs');
    synthQueue.push(...sentences);
    pumpSynth();
  }

  return {
    marks,
    done,
    update(text) {
      if (text) mark('firstTextMs');
      enqueue(stream.push(text));
    },
    finish() {
      enqueue(stream.finish());
      finished = true;
      maybeDone();
    },
    cancel() {
      cancelled = true;
      for (const item of playQueue) if (item.path) rm(item.path, { force: true });
      synthQueue.length = 0;
      playQueue.length = 0;
      if (playing?.kill) playing.kill();
      else clearTimeout(playing);
      marks.cancelled = true;
      resolveDone(marks);
    },
  };
}

async function record(marks) {
  const line = { at: new Date().toISOString(), characterId, mode: args.replay ? 'replay' : 'follow', ...marks };
  delete line.startedAt;
  await appendFile(join(logDir, 'latency.jsonl'), `${JSON.stringify(line)}\n`);
  log(`기록: 첫 글자 ${marks.firstTextMs ?? '-'}ms · 첫 문장 ${marks.firstSentenceMs ?? '-'}ms · 첫 음성 준비 ${marks.firstAudioReadyMs ?? '-'}ms · 재생 시작 ${marks.firstPlayMs ?? '-'}ms · 문장 ${marks.sentences ?? '-'}개 · 끊김 ${marks.underruns ?? '-'}회`);
}

/** 파일을 초당 글자 수만큼 흘려 LLM 스트리밍을 흉내 낸다. 4090 없이 파이프라인을 확인할 때 쓴다. */
async function replay(worker) {
  const text = await readFile(args.replay, 'utf8');
  const cps = Number(args.cps ?? 30);
  const session = createSession(worker, args.replay, now());
  const started = now();
  for (let shown = 0; shown < text.length;) {
    await new Promise(resolve => setTimeout(resolve, 50));
    shown = Math.min(text.length, Math.floor(((now() - started) / 1000) * cps));
    session.update(text.slice(0, shown));
  }
  session.finish();
  await record(await session.done);
}

/** 변경 알림으로 대상 직원의 답변만 읽는다. 음성 대기 중 정기 조회 없음. */
async function follow(worker) {
  const launchedAt = Date.now();
  log(`${characterId}의 새 응답을 기다립니다.`);
  feedSubscription = subscribeVoiceFeed({
    backend, characterId, since: new Date(launchedAt - 2000).toISOString(),
    onError: error => log(`음성 변경 알림/조회 실패: ${error.message}`),
    onTurn: turn => {
      if (Date.parse(turn.startedAt) < launchedAt - 2000) return;
      if (turn.id !== current?.id) {
        // 지연된 이전 턴이나 삭제로 되돌아간 턴을 다시 읽지 않는다.
        if (current && Date.parse(turn.startedAt) < current.startedAt) return;
        current?.session.cancel();
        log(`새 턴 ${turn.id} (${turn.status})`);
        const session = createSession(worker, turn.id, now());
        current = { id: turn.id, startedAt: Date.parse(turn.startedAt), session };
        session.done.then(record);
      }
      if (current.finished) return;
      current.session.update(turn.response ?? '');
      if (!['pending', 'running'].includes(turn.status)) {
        current.finished = true;
        current.session.finish();
      }
    },
  });
}

const worker = args['dry-run'] ? null : await startWorker(profile.model);
function stop() {
  stopping = true;
  feedSubscription?.close();
  current?.session.cancel();
  worker?.child.kill();
  process.exit(0);
}
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, stop);
// 백엔드가 띄운 경우 백엔드가 끝나면(연결이 끊기면) 함께 멈춘다.
if (process.send) process.on('disconnect', stop);
if (worker) {
  const ready = await worker.ready;
  const warmup = await worker.synthesize('준비됐습니다.', voiceFor('준비됐습니다.'));
  if (warmup.error) log(`예열 실패: ${warmup.error}`);
  else rm(warmup.path, { force: true });
  log(`합성 준비 완료 · ${ready.model} · 적재 ${ready.loadMs}ms · 예열 ${warmup.synthMs ?? '-'}ms`);
}
process.send?.({ ready: true });
if (args.replay) {
  await replay(worker);
  stopping = true;
  worker?.child.kill();
  process.exit(0);
} else {
  await follow(worker);
}
