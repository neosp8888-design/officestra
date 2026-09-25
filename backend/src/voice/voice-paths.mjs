// 음성 따라 읽기의 파이썬 환경과 기록 폴더 위치를 정한다. 앱 번들 밖(~/.officestra/voice)에 둔다.
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';

export function voiceHomeDirectory(environment = process.env) {
  const configured = String(environment.OFFICE_VOICE_DIR ?? '').trim();
  return configured || resolve(homedir(), '.officestra', 'voice');
}

export function voicePythonPath(environment = process.env) {
  const configured = String(environment.OFFICE_VOICE_PYTHON ?? '').trim();
  return configured || join(voiceHomeDirectory(environment), '.venv', 'bin', 'python');
}
