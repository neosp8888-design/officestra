// 수동 시제품과 앱 버튼이 같은 응답을 동시에 읽지 않도록 프로세스 간 소유권을 잡는다.
import { mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { voiceHomeDirectory } from './voice-paths.mjs';

export function acquireFollowLock(home = voiceHomeDirectory()) {
  mkdirSync(home, { recursive: true });
  const directory = join(home, 'active-follow.lock');
  const ownerFile = join(directory, 'pid');
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      mkdirSync(directory);
      writeFileSync(ownerFile, `${process.pid}\n`, { flag: 'wx' });
      return () => {
        try {
          if (Number(readFileSync(ownerFile, 'utf8')) === process.pid) rmSync(directory, { recursive: true, force: true });
        } catch { /* 이미 정리됐거나 소유권이 바뀌었다. */ }
      };
    } catch (error) {
      if (error.code !== 'EEXIST') throw error;
    }
    let owner;
    try { owner = Number(readFileSync(ownerFile, 'utf8')); } catch { owner = null; }
    if (Number.isSafeInteger(owner) && owner > 0) {
      try {
        process.kill(owner, 0);
        throw new Error(`음성 따라 읽기가 이미 실행 중입니다 (PID ${owner}).`);
      } catch (error) {
        if (error.code !== 'ESRCH') throw error;
      }
    } else {
      // 방금 잠금을 만든 프로세스가 아직 PID를 기록하는 동안은 지우지 않는다.
      try {
        if (Date.now() - statSync(directory).mtimeMs < 10_000) {
          throw new Error('음성 따라 읽기가 이미 시작 중입니다.');
        }
      } catch (error) {
        if (error.code !== 'ENOENT') throw error;
      }
    }
    rmSync(directory, { recursive: true, force: true });
  }
  throw new Error('음성 따라 읽기 잠금을 얻지 못했습니다.');
}
