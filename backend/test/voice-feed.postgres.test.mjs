import test from 'node:test';
import assert from 'node:assert/strict';
import { pool } from '../src/db.mjs';
import { readVoiceTurn } from '../src/voice-feed.mjs';

test('음성 조회는 선택 직원의 새 턴과 최신 assistant 응답만 반환', {
  skip: process.env.OFFICESTRA_TEST_VOICE_POSTGRES !== '1',
}, async () => {
  const client = await pool.connect();
  try {
    await client.query('BEGIN');
    // 테스트 전용 임시 테이블로 실제 테이블을 가린다. 실사용 행 변경 없음.
    await client.query(`CREATE TEMP TABLE cli_sessions(id text, character_id text) ON COMMIT DROP;
      CREATE TEMP TABLE turns(id text, cli_session_id text, status text, started_at timestamptz) ON COMMIT DROP;
      CREATE TEMP TABLE messages(turn_id text, role text, text text, received_at timestamptz) ON COMMIT DROP;
      INSERT INTO cli_sessions VALUES ('sa','a'),('sb','b');
      INSERT INTO turns VALUES ('old','sa','completed','2026-01-01'),
        ('new','sa','running','2026-01-03'),('other','sb','running','2026-01-04');
      INSERT INTO messages VALUES ('new','assistant','이전','2026-01-03'),
        ('new','assistant','최신 답변','2026-01-04'),('new','user','사용자 내용','2026-01-05');`);
    const turn = await readVoiceTurn(client, { characterId: 'a', since: '2026-01-02' });
    assert.equal(turn.id, 'new');
    assert.equal(turn.response, '최신 답변');
    assert.equal(turn.characterId, 'a');
    assert.deepEqual(Object.keys(turn).sort(), ['characterId', 'id', 'response', 'startedAt', 'status']);
    assert.equal(await readVoiceTurn(client, { characterId: 'a', since: '2026-01-06' }), null);
  } finally {
    await client.query('ROLLBACK');
    client.release();
    await pool.end();
  }
});
