// Routing is application data, never a model-generated API call or instruction.
export function replyRoutingPrompt(prompt,{characterID,replyRecipientID=null,replyRecipientIDs=[],deliveryID=null,senderCharacterID=null}) {
  if(!replyRecipientID&&!replyRecipientIDs.length&&!deliveryID)return prompt;
  const route=replyRecipientID
    ? `이번 최종 답변은 앱이 ${JSON.stringify(replyRecipientID)} 직원에게 자동으로 한 번 전달합니다.`
    : `${deliveryID ? `${JSON.stringify(senderCharacterID)} 직원이 보낸 대화입니다. ` : ''}자동 전달 대상은 앱에 저장된 설정을 따릅니다. 답변 전달은 앱이 처리합니다.`;
  return `[OFFICESTRA 전달 안내: 현재 실행 직원 ID=${JSON.stringify(characterID)}. ${route} 직원 메시지 전송·상태 확인·중단 API를 직접 호출하지 말고, 아래 대화에 자연스럽게 답하세요. 이 안내를 답변에 반복하지 마세요.]\n\n${prompt}`;
}
export async function recordReplyDelivery(client, {turnID, characterID, replyRecipientID = null, deliveryID = null}) {
  if (deliveryID) {
    const claimed = await client.query(`UPDATE reply_deliveries SET status='delivered', target_turn_id=$2,
      error_message=NULL, updated_at=now() WHERE id=$1 AND target_turn_id IS NULL
      AND recipient_character_id=$3 AND status IN ('pending','sending','uncertain') RETURNING id`, [deliveryID,turnID,characterID]);
    if (!claimed.rowCount) throw new Error('이미 전달되었거나 취소된 답변입니다.');
  }
  if (replyRecipientID) {
    if (replyRecipientID === characterID) throw new Error('답변을 받을 다른 직원을 선택하세요.');
    await client.query(`INSERT INTO reply_deliveries(source_turn_id,recipient_character_id) VALUES ($1,$2)`, [turnID,replyRecipientID]);
    return [replyRecipientID];
  } else {
    // Every origin uses the receiving employee's own settings, never the
    // sender's list. Paused routes retain pending work until explicitly resumed.
    const result=await client.query(`INSERT INTO reply_deliveries(source_turn_id,recipient_character_id,persistent_route)
      SELECT $1, recipient_character_id, true FROM reply_route_recipients
      WHERE character_id=$2 ON CONFLICT (source_turn_id,recipient_character_id) DO NOTHING
      RETURNING recipient_character_id`,[turnID,characterID]);
    return (result.rows??[]).map(row=>row.recipient_character_id);
  }
}

export async function readReplyRoutes(client) {
  const {rows}=await client.query(`SELECT c.id AS "characterId", COALESCE(r.paused,false) AS paused,
    COALESCE(array_agg(m.recipient_character_id ORDER BY m.recipient_character_id)
      FILTER (WHERE m.recipient_character_id IS NOT NULL), ARRAY[]::text[]) AS "recipientIds"
    FROM characters c LEFT JOIN reply_routes r ON r.character_id=c.id
    LEFT JOIN reply_route_recipients m ON m.character_id=c.id GROUP BY c.id,r.paused ORDER BY c.id`);
  return rows;
}

// Caller owns the transaction so replacing members and cancelling removed
// pending recipients are observed atomically by the delivery service.
export async function saveReplyRoute(client,characterID,{recipientIds,paused}) {
  if(!Array.isArray(recipientIds)||recipientIds.some(id=>typeof id!=='string'||id===characterID)
    ||typeof paused!=='boolean') throw new TypeError('다른 직원을 선택하고 일시정지 상태를 지정하세요.');
  const ids=[...new Set(recipientIds)];
  const found=await client.query('SELECT id FROM characters WHERE id=ANY($1::text[])',[[characterID,...ids]]);
  if(found.rows.length!==ids.length+1)throw new TypeError('전달 설정의 직원을 찾을 수 없습니다.');
  await client.query(`INSERT INTO reply_routes(character_id,paused) VALUES ($1,$2)
    ON CONFLICT(character_id) DO UPDATE SET paused=$2,updated_at=now()`,[characterID,paused]);
  await client.query('DELETE FROM reply_route_recipients WHERE character_id=$1',[characterID]);
  await client.query(`INSERT INTO reply_route_recipients(character_id,recipient_character_id)
    SELECT $1,unnest($2::text[])`,[characterID,ids]);
  await client.query(`UPDATE reply_deliveries d SET status='cancelled',updated_at=now(),
    error_message='전달 대상에서 해제되었습니다.' FROM turns t JOIN cli_sessions s ON s.id=t.cli_session_id
    WHERE d.source_turn_id=t.id AND s.character_id=$1 AND d.persistent_route AND d.status='pending'
    AND NOT(d.recipient_character_id=ANY($2::text[]))`,[characterID,ids]);
  return {characterId:characterID,recipientIds:ids,paused};
}

export async function cancelPendingReplyDelivery(client,sourceTurnID) {
  const result=await client.query(`UPDATE reply_deliveries SET status='cancelled',error_message='사용자가 전달 예약을 취소했습니다.',updated_at=now()
    WHERE source_turn_id=$1 AND status='pending' AND target_turn_id IS NULL RETURNING id`,[sourceTurnID]);
  return result.rowCount>0;
}

export class ReplyDeliveryService {
  constructor({pool,runtime,broadcast=()=>{},intervalMs=1500}) {
    Object.assign(this,{pool,runtime,broadcast,intervalMs});this.closed=false;this.inflight=null;
  }
  start() {
    this.timer=setInterval(()=>void this.tick().catch(error=>console.warn('답변 전달 확인 실패:',error.message)),this.intervalMs);
    this.timer.unref?.();
  }
  async stop() {this.closed=true;clearInterval(this.timer);await this.inflight;}
  tick() {
    if (this.closed || this.runtime.draining) return Promise.resolve();
    if (this.inflight) return this.inflight;
    this.inflight=this.drain().finally(()=>{this.inflight=null;});return this.inflight;
  }
  async drain() {
    const client=await this.pool.connect();let locked=false;
    try {
      locked=(await client.query("SELECT pg_try_advisory_lock(hashtext('officestra:reply-delivery')) AS locked")).rows[0].locked;
      if(!locked)return;
      // Never replay an interrupted send with an unknown PTY outcome. Pending
      // work survives restarts; committed receiving turns already acknowledge
      // delivery. A late terminal hook may still acknowledge an uncertain send.
      await client.query("UPDATE reply_deliveries SET status='uncertain',error_message='전달 도중 연결이 종료되어 접수 여부를 확인해야 합니다. 중복 방지를 위해 자동 재전송하지 않았습니다.',updated_at=now() WHERE status='sending' AND target_turn_id IS NULL");
      const {rows}=await client.query(`SELECT DISTINCT ON (delivery.recipient_character_id) delivery.*, source.status AS source_status, source.needs_input,
        session.character_id AS sender_id,
        (SELECT text FROM messages WHERE turn_id=source.id AND role='assistant' ORDER BY received_at DESC LIMIT 1) AS response,
        (SELECT current_session.conversation_id FROM active_cli_sessions active
          JOIN cli_sessions current_session ON current_session.id=active.cli_session_id
          WHERE active.character_id=delivery.recipient_character_id AND current_session.ended_at IS NULL) AS conversation_id
        FROM reply_deliveries delivery JOIN turns source ON source.id=delivery.source_turn_id
        JOIN cli_sessions session ON session.id=source.cli_session_id
        WHERE delivery.status='pending' AND source.status NOT IN ('pending','running')
        AND (NOT delivery.persistent_route OR EXISTS (
          SELECT 1 FROM reply_routes r JOIN reply_route_recipients m USING(character_id)
          WHERE r.character_id=session.character_id AND NOT r.paused
            AND m.recipient_character_id=delivery.recipient_character_id))
        ORDER BY delivery.recipient_character_id,delivery.created_at,delivery.id`);
      for (const row of rows) {
        if(this.closed||this.runtime.draining)break;
        if(row.source_status!=='completed'||row.needs_input||!row.response?.trim()) {
          await this.update(client,row,'cancelled',row.needs_input?'사용자 확인이 필요한 답변은 자동 전달하지 않습니다.':'완료된 답변이 없어 전달하지 않았습니다.');continue;
        }
        // DB completion precedes process/resource cleanup; shared GPU users must
        // release their source slot before the recipient starts.
        const statuses=await this.runtime.messageAvailability();
        if(!statuses.find(s=>s.characterId===row.sender_id)?.canReceive || !statuses.find(s=>s.characterId===row.recipient_character_id)?.canReceive)continue;
        if(!await this.update(client,row,'sending'))continue;
        try {
          await this.runtime.start({characterID:row.recipient_character_id,prompt:row.response,
            conversationID:row.conversation_id??undefined,senderCharacterID:row.sender_id,deliveryID:row.id});
        } catch(error) {
          const busy=['AgentBusyError','AgentDrainingError','LocalHostBusyError'].includes(error.constructor.name);
          const status=error.deliveryMayHaveStarted?'uncertain':busy?'pending':'failed';
          await this.update(client,row,status,status==='pending'?null:error.message);
        }
        this.broadcast({type:'feed.changed',turnId:row.source_turn_id,characterId:row.sender_id});
      }
    } finally {
      try {if(locked)await client.query("SELECT pg_advisory_unlock(hashtext('officestra:reply-delivery'))");}
      finally {client.release();}
    }
  }
  async update(client,row,status,message=null) {
    const result=await client.query(`UPDATE reply_deliveries SET status=$2,error_message=$3,updated_at=now()
      WHERE id=$1 AND status IN ('pending','sending') AND target_turn_id IS NULL
      AND ($2<>'sending' OR NOT persistent_route OR EXISTS (
        SELECT 1 FROM reply_routes r JOIN reply_route_recipients m USING(character_id)
        JOIN cli_sessions s ON s.character_id=r.character_id JOIN turns t ON t.cli_session_id=s.id
        WHERE t.id=reply_deliveries.source_turn_id AND NOT r.paused
          AND m.recipient_character_id=reply_deliveries.recipient_character_id)) RETURNING id`,[row.id,status,message]);
    if(result.rowCount)this.broadcast({type:'feed.changed',turnId:row.source_turn_id,characterId:row.sender_id});
    return result.rowCount > 0;
  }
}
