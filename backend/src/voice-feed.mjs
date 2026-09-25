// 음성에는 최신 답변 하나만 필요하다. 작업 내역·비용·출처를 조립하지 않는다.
export async function readVoiceTurn(pool, { characterId, since }) {
  const result = await pool.query(`
    SELECT t.id, s.character_id AS "characterId", t.status,
      t.started_at AS "startedAt",
      COALESCE((SELECT m.text FROM messages m
        WHERE m.turn_id=t.id AND m.role='assistant'
        ORDER BY m.received_at DESC LIMIT 1), '') AS response
    FROM turns t JOIN cli_sessions s ON s.id=t.cli_session_id
    WHERE s.character_id=$1 AND t.started_at >= $2::timestamptz
    ORDER BY t.started_at DESC, t.id DESC LIMIT 1
  `, [characterId, since]);
  return result.rows[0] ?? null;
}
