// 앱의 음성 지원 토글에 맞춰 직원 응답 따라 읽기(voice/voice-follow.mjs) 자식 프로세스를 켜고 끈다.
import { fork } from 'node:child_process';
import { closeSync, mkdirSync, openSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { voiceHomeDirectory } from './voice/voice-paths.mjs';

const FOLLOW_SCRIPT = fileURLToPath(new URL('./voice/voice-follow.mjs', import.meta.url));
// voice-follow.mjs가 파이썬 환경을 찾지 못하면 이 코드로 끝난다.
const MISSING_PYTHON_EXIT = 2;
const FOLLOW_BUSY_EXIT = 3;

function defaultSpawn(characterId) {
  const logDir = join(voiceHomeDirectory(), 'logs');
  mkdirSync(logDir, { recursive: true });
  const logFd = openSync(join(logDir, 'follow.log'), 'a');
  try {
    return fork(FOLLOW_SCRIPT, ['--follow', characterId], { stdio: ['ignore', logFd, logFd, 'ipc'] });
  } finally {
    closeSync(logFd);
  }
}

export class VoiceFollowControl {
  constructor({ spawnFollower = defaultSpawn } = {}) {
    this.spawnFollower = spawnFollower;
    this.child = null;
    this.state = { characterId: null, state: 'stopped', error: null };
  }

  status() {
    return { ...this.state };
  }

  set({ enabled, characterId }) {
    if (typeof enabled !== 'boolean') throw new Error('enabled must be a boolean');
    if (!enabled) {
      this.stop();
      return this.status();
    }
    if (typeof characterId !== 'string' || !/^[\w-]+$/.test(characterId)) throw new Error('characterId is required');
    if (this.child && this.state.characterId === characterId) return this.status();
    this.stop();
    const child = this.spawnFollower(characterId);
    this.child = child;
    this.state = { characterId, state: 'starting', error: null };
    child.on('message', message => {
      if (this.child === child && message?.ready) this.state = { ...this.state, state: 'running' };
    });
    // 처리하지 않은 'error' 이벤트는 백엔드 전체를 끝내므로 실패 상태로만 남긴다.
    child.on('error', error => {
      if (this.child !== child) return;
      this.child = null;
      this.state = { characterId, state: 'failed', error: error.message };
    });
    child.on('exit', code => {
      if (this.child !== child) return;
      this.child = null;
      const error = code === MISSING_PYTHON_EXIT
        ? `음성 파이썬 환경이 없습니다 (${join(voiceHomeDirectory(), '.venv')}).`
        : code === FOLLOW_BUSY_EXIT
          ? '음성 따라 읽기가 이미 다른 프로세스에서 실행 중입니다. 기존 실행을 종료한 뒤 다시 켜 주세요.'
        : `음성 따라 읽기가 종료되었습니다 (코드 ${code}). 기록은 ${join(voiceHomeDirectory(), 'logs', 'follow.log')}에 있습니다.`;
      this.state = { characterId, state: 'failed', error };
    });
    return this.status();
  }

  stop() {
    const child = this.child;
    this.child = null;
    this.state = { characterId: null, state: 'stopped', error: null };
    child?.kill('SIGTERM');
  }
}
