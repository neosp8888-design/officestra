import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {randomUUID} from 'node:crypto';
import {pool} from '../src/db.mjs';
import {readReplyRoutes,saveReplyRoute,recordReplyDelivery,ReplyDeliveryService} from '../src/reply-delivery.mjs';

// Opt-in SQL integration: all objects live in an isolated, rolled-back schema.
// No production rows, routes, messages, or real agents are touched.
test('persistent routing SQL integration', {skip:process.env.OFFICESTRA_TEST_REPLY_POSTGRES!=='1'}, async t=>{
  const client=await pool.connect();
  try {
    await client.query('BEGIN');
    const schema='test_reply_'+randomUUID().replaceAll('-','');
    await client.query(`CREATE SCHEMA ${schema}`);
    await client.query(`SET LOCAL search_path TO ${schema}`);
    await client.query(`CREATE TABLE characters(id text PRIMARY KEY,name text NOT NULL);
      CREATE TABLE cli_sessions(id uuid PRIMARY KEY,character_id text REFERENCES characters(id),conversation_id uuid,ended_at timestamptz);
      CREATE TABLE turns(id uuid PRIMARY KEY,cli_session_id uuid REFERENCES cli_sessions(id),status text,needs_input boolean DEFAULT false);
      CREATE TABLE messages(id bigserial PRIMARY KEY,turn_id uuid REFERENCES turns(id),role text,text text,received_at timestamptz DEFAULT now());
      CREATE TABLE active_cli_sessions(character_id text PRIMARY KEY,cli_session_id uuid REFERENCES cli_sessions(id));`);
    for(const file of ['053_reply_deliveries.sql','054_persistent_reply_routes.sql','054_persistent_reply_routes.sql']) {
      await client.query(await readFile(new URL('../../database/migrations/'+file,import.meta.url),'utf8'));
    }
    const createTurn=async(character,status='completed',text='자연스러운 답변')=>{
      const session=randomUUID(),turn=randomUUID();
      await client.query('INSERT INTO cli_sessions(id,character_id,conversation_id) VALUES($1,$2,$3)',[session,character,randomUUID()]);
      await client.query('INSERT INTO turns(id,cli_session_id,status) VALUES($1,$2,$3)',[turn,session,status]);
      await client.query("INSERT INTO messages(turn_id,role,text) VALUES($1,'assistant',$2)",[turn,text]);
      return turn;
    };
    const reset=async()=>{
      await client.query('TRUNCATE characters,cli_sessions,turns,messages,active_cli_sessions,reply_routes,reply_route_recipients,reply_deliveries CASCADE');
      await client.query("INSERT INTO characters VALUES ('a','A'),('b','B'),('c','C')");
    };
    const setting=(character,ids,paused=false)=>saveReplyRoute(client,character,{recipientIds:ids,paused});
    const queued=async turn=>(await client.query('SELECT * FROM reply_deliveries WHERE source_turn_id=$1 ORDER BY recipient_character_id',[turn])).rows;
    const sent=[];const busy=new Set();
    const service=new ReplyDeliveryService({pool:{connect:async()=>({
      release(){},query(sql,args){
        if(sql.includes('pg_try_advisory_lock'))return {rows:[{locked:true}]};
        if(sql.includes('pg_advisory_unlock'))return {rows:[]};
        return client.query(sql,args);
      },
    })},runtime:{async messageAvailability(){return ['a','b','c'].map(characterId=>({characterId,canReceive:!busy.has(characterId)}));},
      async start(options){
        const turn=await createTurn(options.characterID,'running');
        await recordReplyDelivery(client,{turnID:turn,characterID:options.characterID,deliveryID:options.deliveryID});
        sent.push({...options,turn});return {turnId:turn};
      },
    }});

    await t.test('multiple recipients persist on fresh reads, self and unknown recipients are rejected',async()=>{
      await reset();await setting('a',['b','c','b']);
      assert.deepEqual((await readReplyRoutes(client)).find(r=>r.characterId==='a'),{characterId:'a',recipientIds:['b','c'],paused:false});
      await assert.rejects(setting('a',['a']),TypeError);
      await assert.rejects(setting('a',['missing']),TypeError);
      const turn=await createTurn('a');
      assert.deepEqual((await recordReplyDelivery(client,{turnID:turn,characterID:'a'})).sort(),['b','c']);
      await recordReplyDelivery(client,{turnID:turn,characterID:'a'});
      assert.equal((await queued(turn)).length,2,'repeated completion must not duplicate recipient deliveries');
    });
    await t.test('an API reply recipient cannot create or override saved tags',async()=>{
      await reset();sent.length=0;
      const untagged=await createTurn('a');
      assert.deepEqual(await recordReplyDelivery(client,{turnID:untagged,characterID:'a',replyRecipientID:'b'}),[]);
      assert.deepEqual(await queued(untagged),[]);
      await setting('a',['c']);const tagged=await createTurn('a');
      assert.deepEqual(await recordReplyDelivery(client,{turnID:tagged,characterID:'a',replyRecipientID:'b'}),['c']);
      await service.tick();assert.deepEqual(sent.map(s=>s.characterID),['c']);
      assert.deepEqual(await queued(sent[0].turn),[],'receiving without tags does not create another reply');
    });
    await t.test('legacy one-off reservations need current tags and obey pause and removal',async()=>{
      await reset();sent.length=0;
      const turn=await createTurn('a');
      await client.query('INSERT INTO reply_deliveries(source_turn_id,recipient_character_id) VALUES($1,$2)',[turn,'b']);
      await service.tick();assert.equal(sent.length,0);assert.equal((await queued(turn))[0].status,'cancelled');
      await setting('a',['b'],true);
      const paused=await createTurn('a');
      await client.query('INSERT INTO reply_deliveries(source_turn_id,recipient_character_id) VALUES($1,$2)',[paused,'b']);
      await service.tick();assert.equal(sent.length,0);assert.equal((await queued(paused))[0].status,'pending');
      await setting('a',['b'],false);await service.tick();assert.equal(sent.length,1);
      const removed=await createTurn('a');
      await client.query('INSERT INTO reply_deliveries(source_turn_id,recipient_character_id) VALUES($1,$2)',[removed,'b']);
      await setting('a',[]);assert.equal((await queued(removed))[0].status,'cancelled');
    });
    await t.test('removing tags between polling and claiming prevents even legacy delivery',async()=>{
      await reset();sent.length=0;await setting('a',['b']);
      const turn=await createTurn('a');
      await client.query('INSERT INTO reply_deliveries(source_turn_id,recipient_character_id) VALUES($1,$2)',[turn,'b']);
      const available=service.runtime.messageAvailability;
      service.runtime.messageAvailability=async()=>{await setting('a',[]);return available();};
      try {await service.tick();assert.equal(sent.length,0);assert.equal((await queued(turn))[0].status,'cancelled');}
      finally {service.runtime.messageAvailability=available;}
    });
    await t.test('a busy recipient does not block another; receiving turns use their own saved recipients',async()=>{
      await reset();sent.length=0;await setting('a',['b','c']);await setting('b',['a']);
      const turn=await createTurn('a');await recordReplyDelivery(client,{turnID:turn,characterID:'a'});
      busy.add('b');await service.tick();assert.deepEqual(sent.map(s=>s.characterID),['c']);
      busy.clear();await service.tick();assert.deepEqual(sent.map(s=>s.characterID),['c','b']);
      const b=sent.find(s=>s.characterID==='b');
      assert.deepEqual((await queued(b.turn)).map(r=>r.recipient_character_id),['a']);
      assert.equal((await queued(sent[0].turn)).length,0);
      await service.tick();assert.equal(sent.length,2,'running replies must not be forwarded');
      await client.query("UPDATE turns SET status='completed' WHERE id=$1",[b.turn]);
      await service.tick();assert.equal(sent.at(-1).characterID,'a');
      assert.equal(sent.at(-1).senderCharacterID,'b');
      await service.tick();assert.equal(sent.length,3,'acknowledged deliveries must not replay');
    });
    await t.test('pause survives reload and resume releases pending replies without clearing recipients',async()=>{
      await reset();sent.length=0;await setting('a',['b','c'],true);
      const turn=await createTurn('a');await recordReplyDelivery(client,{turnID:turn,characterID:'a'});
      await service.tick();assert.equal(sent.length,0);assert.equal((await queued(turn)).length,2);
      assert.equal((await readReplyRoutes(client)).find(r=>r.characterId==='a').paused,true);
      await setting('a',['b','c'],false);await service.tick();assert.equal(sent.length,2);
    });
    await t.test('removing a recipient cancels only that pending delivery',async()=>{
      await reset();sent.length=0;await setting('a',['b','c']);
      const turn=await createTurn('a');await recordReplyDelivery(client,{turnID:turn,characterID:'a'});
      await setting('a',['b']);await service.tick();assert.deepEqual(sent.map(s=>s.characterID),['b']);
      assert.deepEqual((await queued(turn)).map(r=>r.status),['delivered','cancelled']);
    });
    await t.test('pause between polling and claiming prevents delivery',async()=>{
      await reset();sent.length=0;await setting('a',['b']);
      const turn=await createTurn('a');await recordReplyDelivery(client,{turnID:turn,characterID:'a'});
      const available=service.runtime.messageAvailability;
      service.runtime.messageAvailability=async()=>{await setting('a',['b'],true);return available();};
      try {await service.tick();assert.equal(sent.length,0);assert.equal((await queued(turn))[0].status,'pending');}
      finally {service.runtime.messageAvailability=available;}
    });
    await t.test('more than 30 blocked deliveries do not starve another recipient',async()=>{
      await reset();sent.length=0;await setting('a',['b']);
      for(let i=0;i<31;i++) {const turn=await createTurn('a');await recordReplyDelivery(client,{turnID:turn,characterID:'a'});}
      await setting('a',['b','c']);const turn=await createTurn('a');await recordReplyDelivery(client,{turnID:turn,characterID:'a'});
      busy.add('b');try {await service.tick();assert.deepEqual(sent.map(s=>s.characterID),['c']);}finally{busy.clear();}
    });
  } finally {
    try {await client.query('ROLLBACK');} finally {client.release();await pool.end();}
  }
});
