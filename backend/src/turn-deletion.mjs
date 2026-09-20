// 이 파일은 보관함에서 선택한 종료 턴과 그 턴에서 파생된 기록을 한 트랜잭션으로 삭제한다.

export class TurnDeletionError extends Error {
  constructor(message, statusCode = 400) {
    super(message);
    this.name = "TurnDeletionError";
    this.statusCode = statusCode;
  }
}

export async function deleteArchivedTurn(pool, turnID) {
  const normalizedTurnID = String(turnID ?? "").trim();
  if (!normalizedTurnID) {
    throw new TurnDeletionError("삭제할 대화를 찾을 수 없습니다.");
  }

  const client = await pool.connect();
  try {
    await client.query("BEGIN");
    const selected = await client.query(
      `
        SELECT
          turn.id,
          turn.status,
          turn.cli_session_id AS "sessionId",
          session.character_id AS "characterId"
        FROM turns AS turn
        JOIN cli_sessions AS session
          ON session.id = turn.cli_session_id
        WHERE turn.id::text = $1
        FOR UPDATE OF turn, session
      `,
      [normalizedTurnID],
    );
    if (selected.rows.length === 0) {
      await client.query("COMMIT");
      return null;
    }

    const turn = selected.rows[0];
    if (["pending", "running"].includes(turn.status)) {
      throw new TurnDeletionError(
        "진행 중인 대화는 삭제할 수 없습니다.",
        409,
      );
    }

    const running = await client.query(
      `
        SELECT 1
        FROM turns
        WHERE cli_session_id = $1
          AND status IN ('pending', 'running')
        LIMIT 1
      `,
      [turn.sessionId],
    );
    if (running.rows.length > 0) {
      throw new TurnDeletionError(
        "같은 세션에서 대화가 진행 중이라 삭제할 수 없습니다.",
        409,
      );
    }

    const records = await client.query(
      `
        SELECT id
        FROM work_records
        WHERE source_turn_id = $1
        FOR UPDATE
      `,
      [turn.id],
    );
    const recordIDs = records.rows.map(({ id }) => id);

    const activeSession = await client.query(
      `
        DELETE FROM active_cli_sessions
        WHERE cli_session_id = $1
      `,
      [turn.sessionId],
    );
    if (activeSession.rowCount > 0) {
      await client.query(
        `
          UPDATE cli_sessions
          SET ended_at = COALESCE(ended_at, now())
          WHERE id = $1
        `,
        [turn.sessionId],
      );
    }

    await client.query(
      `
        DELETE FROM turn_response_sources
        WHERE work_record_id = ANY($1::uuid[])
      `,
      [recordIDs],
    );
    await client.query(
      `
        DELETE FROM wiki_proposals
        WHERE author_turn_id = $1
          OR target_record_id = ANY($2::uuid[])
      `,
      [turn.id, recordIDs],
    );
    await client.query(
      `
        DELETE FROM work_record_events
        WHERE source_turn_id = $1
          OR record_id = ANY($2::uuid[])
      `,
      [turn.id, recordIDs],
    );
    await client.query(
      `
        DELETE FROM work_record_links
        WHERE source_record_id = ANY($1::uuid[])
          OR target_record_id = ANY($1::uuid[])
      `,
      [recordIDs],
    );
    const deletedRecords = await client.query(
      `
        DELETE FROM work_records
        WHERE id = ANY($1::uuid[])
      `,
      [recordIDs],
    );
    const deletedTurn = await client.query(
      "DELETE FROM turns WHERE id = $1",
      [turn.id],
    );
    if (deletedTurn.rowCount !== 1) {
      throw new Error("대화 삭제 결과를 확인하지 못했습니다.");
    }

    await client.query("COMMIT");
    return {
      id: String(turn.id),
      characterId: turn.characterId,
      deletedWorkRecordCount: deletedRecords.rowCount,
      sessionReset: activeSession.rowCount > 0,
    };
  } catch (error) {
    await client.query("ROLLBACK").catch(() => {});
    throw error;
  } finally {
    client.release();
  }
}
