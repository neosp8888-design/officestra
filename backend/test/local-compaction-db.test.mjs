import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {randomUUID} from 'node:crypto';

test('local full-context migration changes local employees only and preserves cloud restore value', {skip:process.env.OFFICESTRA_LOCAL_DB_TEST!=='1'},async()=>{
  const {pool}=await import('../src/db.mjs');const client=await pool.connect();
  const schema='compact_test_'+randomUUID().replaceAll('-','');
  try{
    await client.query('BEGIN');await client.query(`CREATE SCHEMA "${schema}"`);
    await client.query(`SET LOCAL search_path TO "${schema}", public`);
    await client.query('CREATE TABLE characters(id text PRIMARY KEY, config jsonb, auto_compact_percent smallint CHECK(auto_compact_percent BETWEEN 20 AND 95), updated_at timestamptz)');
    await client.query('INSERT INTO characters VALUES ($1,$2,30,now()),($3,$4,65,now()),($5,$6,50,now())',['local-codex',{localProfileId:'c',localPreviousSettings:{model:'cloud'}},'local-claude',{localProfileId:'a',localPreviousSettings:{autoCompactPercent:40}},'cloud',{}]);
    const sql=await readFile(new URL('../../database/migrations/051_local_compaction_full_context.sql',import.meta.url),'utf8');
    await client.query(sql);await client.query(sql);
    const rows=(await client.query('SELECT id,config,auto_compact_percent FROM characters ORDER BY id')).rows;
    assert.equal(rows[0].auto_compact_percent,50);
    assert.equal(rows[1].auto_compact_percent,100);assert.equal(rows[1].config.localPreviousSettings.autoCompactPercent,40);
    assert.equal(rows[2].auto_compact_percent,100);assert.equal(rows[2].config.localPreviousSettings.autoCompactPercent,30);
  }finally{await client.query('ROLLBACK');client.release();await pool.end();}
});
