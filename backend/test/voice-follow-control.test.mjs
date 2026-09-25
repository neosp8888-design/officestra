// 음성 지원 토글이 따라 읽기 자식 프로세스를 켜고 끄며 상태를 알맞게 보고하는지 확인한다.
import assert from 'node:assert/strict';
import { fork } from 'node:child_process';
import { EventEmitter, once } from 'node:events';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { VoiceFollowControl } from '../src/voice-follow-control.mjs';

const FOLLOW_SCRIPT = fileURLToPath(new URL('../src/voice/voice-follow.mjs', import.meta.url));

function fakeSpawner() {
  const children = [];
  const spawnFollower = characterId => {
    const child = new EventEmitter();
    child.characterId = characterId;
    child.killed = null;
    child.kill = signal => { child.killed = signal; };
    children.push(child);
    return child;
  };
  return { children, spawnFollower };
}

test('켜면 준비 신호를 받을 때까지 starting, 받은 뒤 running이 된다', () => {
  const { children, spawnFollower } = fakeSpawner();
  const control = new VoiceFollowControl({ spawnFollower });
  assert.deepEqual(control.status(), { characterId: null, state: 'stopped', error: null });
  assert.equal(control.set({ enabled: true, characterId: 'right-woman' }).state, 'starting');
  children[0].emit('message', { ready: true });
  assert.deepEqual(control.status(), { characterId: 'right-woman', state: 'running', error: null });
  // 같은 직원으로 다시 켜도 새로 띄우지 않는다.
  control.set({ enabled: true, characterId: 'right-woman' });
  assert.equal(children.length, 1);
});

test('끄면 자식을 멈추고, 그 뒤의 종료는 실패로 보지 않는다', () => {
  const { children, spawnFollower } = fakeSpawner();
  const control = new VoiceFollowControl({ spawnFollower });
  control.set({ enabled: true, characterId: 'right-woman' });
  assert.equal(control.set({ enabled: false }).state, 'stopped');
  assert.equal(children[0].killed, 'SIGTERM');
  children[0].emit('exit', null);
  assert.equal(control.status().state, 'stopped');
});

test('다른 직원으로 켜면 이전 따라 읽기를 멈추고 새로 띄운다', () => {
  const { children, spawnFollower } = fakeSpawner();
  const control = new VoiceFollowControl({ spawnFollower });
  control.set({ enabled: true, characterId: 'right-woman' });
  control.set({ enabled: true, characterId: 'left-man' });
  assert.equal(children[0].killed, 'SIGTERM');
  assert.equal(children[1].characterId, 'left-man');
  children[0].emit('exit', null);
  assert.equal(control.status().characterId, 'left-man');
});

test('예기치 않은 종료와 실행 오류는 실패 상태와 이유로 남긴다', () => {
  const { children, spawnFollower } = fakeSpawner();
  const control = new VoiceFollowControl({ spawnFollower });
  control.set({ enabled: true, characterId: 'right-woman' });
  children[0].emit('exit', 2);
  assert.equal(control.status().state, 'failed');
  assert.match(control.status().error, /파이썬 환경이 없습니다/);
  control.set({ enabled: true, characterId: 'right-woman' });
  children[1].emit('error', new Error('spawn failed'));
  assert.deepEqual(control.status(), { characterId: 'right-woman', state: 'failed', error: 'spawn failed' });
});

test('잘못된 요청은 거부한다', () => {
  const control = new VoiceFollowControl({ spawnFollower: fakeSpawner().spawnFollower });
  assert.throws(() => control.set({ enabled: 'yes' }), /enabled/);
  assert.throws(() => control.set({ enabled: true, characterId: '../x' }), /characterId/);
});

test('실제 자식 IPC는 모델 적재 없이 준비 신호와 종료를 전달한다', async () => {
  const home = mkdtempSync(join(tmpdir(), 'officestra-voice-control-test-'));
  const control = new VoiceFollowControl({
    spawnFollower: characterId => fork(FOLLOW_SCRIPT, [
      '--follow', characterId, '--dry-run', '--backend', 'http://127.0.0.1:9',
    ], { stdio: ['ignore', 'ignore', 'ignore', 'ipc'], execArgv: [], env: { ...process.env, OFFICE_VOICE_DIR: home } }),
  });
  control.set({ enabled: true, characterId: 'right-woman' });
  const child = control.child;
  try {
    for (let attempt = 0; attempt < 50 && control.status().state === 'starting'; attempt += 1) {
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    assert.equal(control.status().state, 'running');
    const exited = once(child, 'exit');
    control.set({ enabled: false });
    await exited;
    assert.equal(control.status().state, 'stopped');
  } finally {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    rmSync(home, { recursive: true, force: true });
  }
});

test('수동 시제품과 앱 제어기는 같은 응답을 동시에 읽지 못한다', async () => {
  const home = mkdtempSync(join(tmpdir(), 'officestra-voice-singleton-test-'));
  const manualScript = fileURLToPath(new URL('../../work/local-voice/voice-follow.mjs', import.meta.url));
  const start = script => fork(script, ['--dry-run', '--backend', 'http://127.0.0.1:9'], {
    stdio: ['ignore', 'ignore', 'pipe', 'ipc'],
    execArgv: [],
    env: { ...process.env, OFFICE_VOICE_DIR: home },
  });
  const first = start(FOLLOW_SCRIPT);
  let second;
  let third;
  try {
    await Promise.race([once(first, 'message'), once(first, 'exit').then(() => { throw new Error('첫 따라 읽기 실행 실패'); })]);
    second = start(manualScript);
    const [code] = await once(second, 'exit');
    assert.equal(code, 3);
    const stopped = once(first, 'exit');
    first.kill('SIGTERM');
    await stopped;
    third = start(manualScript);
    let owner = null;
    for (let attempt = 0; attempt < 20; attempt += 1) {
      try { owner = Number(readFileSync(join(home, 'active-follow.lock', 'pid'), 'utf8')); } catch { /* 시작 중 */ }
      if (owner === third.pid) break;
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    assert.equal(owner, third.pid);
  } finally {
    for (const child of [first, second, third]) {
      if (child && child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    }
    rmSync(home, { recursive: true, force: true });
  }
});
