// 토크나이저 적재 실패를 준비 완료로 알리지 않고, 따라 읽기 실행을 중단한다.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const WORKER = fileURLToPath(new URL('../src/voice/tts_worker.py', import.meta.url));
const FOLLOWER = fileURLToPath(new URL('../src/voice/voice-follow.mjs', import.meta.url));
const SAMPLE = fileURLToPath(new URL('./fixtures/voice-sample-reply.md', import.meta.url));

test('토크나이저가 없는 모델은 준비 실패 JSON을 반환하고 종료한다', () => {
  const root = mkdtempSync(join(tmpdir(), 'officestra-voice-worker-test-'));
  try {
    mkdirSync(join(root, 'mlx_audio', 'tts'), { recursive: true });
    writeFileSync(join(root, 'numpy.py'), '');
    writeFileSync(join(root, 'mlx_audio', '__init__.py'), '');
    writeFileSync(join(root, 'mlx_audio', 'tts', '__init__.py'), '');
    writeFileSync(join(root, 'mlx_audio', 'tts', 'utils.py'), [
      'def load_model(_):',
      '    print("Warning: Could not load tokenizer: missing source")',
      '    return type("Model", (), {"sample_rate": 24000, "tokenizer": None})()',
      '',
    ].join('\n'));
    const result = spawnSync('python3', [WORKER], {
      cwd: root, env: { ...process.env, PYTHONPATH: root }, encoding: 'utf8', timeout: 10_000,
    });
    assert.equal(result.status, 4, result.stderr);
    assert.match(result.stderr, /Could not load tokenizer/);
    assert.deepEqual(JSON.parse(result.stdout.trim()), {
      ready: false, error: 'RuntimeError: 텍스트 토크나이저를 불러오지 못했습니다.',
    });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('작업자 초기화 오류가 오면 따라 읽기 실행도 실패로 끝난다', () => {
  const root = mkdtempSync(join(tmpdir(), 'officestra-voice-start-test-'));
  try {
    const fakePython = join(root, 'fake-python');
    writeFileSync(fakePython, '#!/bin/sh\nprintf \'{"ready":false,"error":"tokenizer unavailable"}\\n\'\nexit 4\n', { mode: 0o755 });
    const result = spawnSync(process.execPath, [FOLLOWER, '--replay', SAMPLE, '--no-play'], {
      cwd: root,
      env: { ...process.env, OFFICE_VOICE_DIR: root, OFFICE_VOICE_PYTHON: fakePython },
      encoding: 'utf8', timeout: 10_000,
    });
    assert.equal(result.status, 4, result.stderr);
    assert.match(result.stdout, /합성 초기화 실패: tokenizer unavailable/);
    assert.doesNotMatch(result.stdout, /합성 준비 완료/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
