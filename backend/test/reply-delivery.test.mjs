import test from 'node:test';
import assert from 'node:assert/strict';
import {ReplyDeliveryService,recordReplyDelivery,replyRoutingPrompt,cancelPendingReplyDelivery} from '../src/reply-delivery.mjs';

function fixture(overrides={}) {
  const row={id:'delivery-1',source_turn_id:'source-1',recipient_character_id:'right-woman',sender_id:'left-woman',
    source_status:'completed',needs_input:false,response:'오늘은 여기까지 이야기하자.',conversation_id:'conversation-1',
    status:'pending',target_turn_id:null,...overrides};
  const starts=[],events=[];let releases=0,unlocks=0;
  const client={release(){releases++;},async query(sql,args=[]){
    if(sql.includes('pg_try_advisory_lock'))return {rows:[{locked:true}]};
    if(sql.includes('pg_advisory_unlock')){unlocks++;return {rows:[]};}
    if(sql.includes("SET status='uncertain'")){if(row.status==='sending'&&!row.target_turn_id)row.status='uncertain';return {rowCount:1};}
    if(sql.includes('선택된 자동 전달 태그가 없어'))return {rows:[],rowCount:0};
    if(sql.includes("SET status='cancelled'")){if(row.status!=='pending'||row.target_turn_id)return {rowCount:0};row.status='cancelled';return {rowCount:1};}
    if(sql.startsWith('SELECT DISTINCT ON'))return {rows:row.status==='pending'?[structuredClone(row)]:[]};
    if(sql.startsWith('INSERT INTO reply_deliveries'))return {rows:[],rowCount:0};
    if(sql.includes("SET status='delivered'")){
      if(row.target_turn_id||!['pending','sending','uncertain'].includes(row.status)||args[2]!==row.recipient_character_id)return {rowCount:0,rows:[]};
      row.status='delivered';row.target_turn_id=args[1];return {rowCount:1,rows:[{id:row.id}]};
    }
    if(sql.includes('SET status=$2')){
      if(row.target_turn_id||!['pending','sending'].includes(row.status))return {rowCount:0};
      row.status=args[1];row.error_message=args[2];return {rowCount:1};
    }
    throw Error('Unexpected query: '+sql);
  }};
  const runtime={draining:false,async messageAvailability(){return [{characterId:'left-woman',canReceive:true},{characterId:'right-woman',canReceive:true}];},
    async start(options){starts.push(options);await recordReplyDelivery(client,{turnID:'target-1',characterID:options.characterID,deliveryID:options.deliveryID});return {turnId:'target-1'};}};
  const service=new ReplyDeliveryService({pool:{connect:async()=>client},runtime,broadcast:e=>events.push(e)});
  return {row,starts,client,runtime,service,events,counts:()=>({releases,unlocks})};
}

test('app delivers the exact final response with the actual sender once, without asking the model to call an API',async()=>{
  const f=fixture();await f.service.tick();await f.service.tick();
  assert.equal(f.starts.length,1);assert.equal(f.row.status,'delivered');assert.equal(f.row.target_turn_id,'target-1');
  assert.deepEqual(f.starts[0],{characterID:'right-woman',prompt:f.row.response,conversationID:'conversation-1',senderCharacterID:'left-woman',deliveryID:'delivery-1'});
  assert.equal(f.starts[0].replyRecipientID,undefined,'the receiving turn resolves its own saved route, not the sender route');
  assert.deepEqual(f.counts(),{releases:2,unlocks:2});
});
test('busy recipient or source cleanup leaves durable delivery pending until available',async()=>{
  for(const busy of ['left-woman','right-woman']){
    const f=fixture(),available=f.runtime.messageAvailability;
    f.runtime.messageAvailability=async()=> (await available()).map(s=>({...s,canReceive:s.characterId!==busy}));
    await f.service.tick();assert.equal(f.starts.length,0);assert.equal(f.row.status,'pending');
    f.runtime.messageAvailability=available;await f.service.tick();assert.equal(f.starts.length,1);
  }
});
test('cancelled, failed, empty and user-confirmation answers are never injected into another employee',async()=>{
  for(const values of [{source_status:'interrupted'},{source_status:'failed'},{needs_input:true},{response:'  '}]){
    const f=fixture(values);await f.service.tick();assert.equal(f.starts.length,0);assert.equal(f.row.status,'cancelled');
  }
});
test('restart preserves unknown send outcomes and never duplicates acknowledged delivery',async()=>{
  const stale=fixture({status:'sending'});await stale.service.tick();assert.equal(stale.starts.length,0);assert.equal(stale.row.status,'uncertain');
  await recordReplyDelivery(stale.client,{turnID:'late-target',characterID:'right-woman',deliveryID:stale.row.id});assert.equal(stale.row.status,'delivered');
  const f=fixture(),start=f.runtime.start;f.runtime.start=async options=>{await start(options);throw Error('runner failed after committed acceptance');};
  await f.service.tick();await f.service.tick();assert.equal(f.starts.length,1);assert.equal(f.row.status,'delivered');
});
test('busy race retries, other dispatch errors remain visible and do not loop',async()=>{
  class AgentBusyError extends Error {}
  const f=fixture();f.runtime.start=async()=>{throw new AgentBusyError('busy');};
  await f.service.tick();assert.equal(f.row.status,'pending');
  f.runtime.start=async()=>{throw Error('CLI missing');};await f.service.tick();assert.equal(f.row.status,'failed');assert.equal(f.row.error_message,'CLI missing');
});
test('concurrent ticks share one dispatch and always release the lease',async()=>{
  const f=fixture();await Promise.all([f.service.tick(),f.service.tick(),f.service.tick()]);
  assert.equal(f.starts.length,1);assert.deepEqual(f.counts(),{releases:1,unlocks:1});
});
test('user can cancel pending delivery without stopping either employee; claimed terminal timeout is never retried',async()=>{
  const f=fixture();assert.equal(await cancelPendingReplyDelivery(f.client,f.row.source_turn_id),true);
  await f.service.tick();assert.equal(f.starts.length,0);assert.equal(f.row.status,'cancelled');
  const g=fixture();class AgentBusyError extends Error {}
  g.runtime.start=async()=>{throw Object.assign(new AgentBusyError('unknown PTY receipt'),{deliveryMayHaveStarted:true});};
  await g.service.tick();assert.equal(g.row.status,'uncertain');assert.equal(await cancelPendingReplyDelivery(g.client,g.row.source_turn_id),false);
  await g.service.tick();assert.equal(g.row.status,'uncertain');
});
test('routing is snapshotted transactionally and duplicate or wrong-recipient acknowledgements fail',async()=>{
  const calls=[];const client={query:async(sql,args)=>{calls.push({sql,args});return {rowCount:1};}};
  assert.deepEqual(await recordReplyDelivery(client,{turnID:'source',characterID:'left-woman',replyRecipientID:'right-woman'}),[]);
  assert.deepEqual(calls[0].args,['source','left-woman']);
  assert.match(calls[0].sql,/FROM reply_route_recipients/);
  const f=fixture();await assert.rejects(recordReplyDelivery(f.client,{turnID:'t',characterID:'left-woman',deliveryID:f.row.id}),/이미 전달/);
  await f.service.tick();await assert.rejects(recordReplyDelivery(f.client,{turnID:'duplicate',characterID:'right-woman',deliveryID:f.row.id}),/이미 전달/);
});
test('routing guidance is execution-only and tells the model to respond rather than perform delivery',()=>{
  const prompt='안녕';assert.equal(replyRoutingPrompt(prompt,{characterID:'left-woman'}),prompt);
  for(const legacy of [{replyRecipientID:'right-woman'},{deliveryID:'incoming',senderCharacterID:'boss'},
    {replyRecipientID:'right-woman',deliveryID:'incoming',senderCharacterID:'boss'}]) {
    assert.equal(replyRoutingPrompt(prompt,{characterID:'left-woman',...legacy}),prompt);
  }
  const routed=replyRoutingPrompt(prompt,{characterID:'left-woman',replyRecipientIDs:['right-woman','boss']});
  assert.match(routed,/선택된 자동 전달 태그=\["right-woman","boss"\]/);assert.match(routed,/중복 전송하지 말고/);assert.ok(routed.endsWith(prompt));
});
