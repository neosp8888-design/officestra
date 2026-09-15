import test from 'node:test';
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import pg from 'pg';
import {LOCAL_TURN_EFFORT_SQL} from '../src/local-provider-service.mjs';

test('PostgreSQL: GUI/terminal tokens retained, local reported prices audited/excluded including late writes', {skip:process.env.OFFICESTRA_LOCAL_DB_TEST!=='1'}, async()=>{
  const pool=new pg.Pool({connectionString:process.env.DATABASE_URL??'postgres://office:office-local@127.0.0.1:54329/office'});
  const client=await pool.connect();const schema='local_test_'+randomUUID().replaceAll('-','');
  try{
    await client.query('BEGIN');
    for(const [kind,runtime,stored,selected,expected] of [
      ['local','llama-cpp-b10982','default','medium','medium'],
      ['local','llama-cpp-b10982','default','low','low'],
      ['local','llama-cpp-b10982','default','xhigh','xhigh'],
      ['local','llama-cpp-b10982','default','default','xhigh'],
      ['local','llama-cpp-b10982','high','medium','high'],
      ['local','lmstudio','default','on','default'],
      ['cloud','llama-cpp-b10982','default','medium','default'],
      ['local',null,'default',null,'default'],
    ]){
      const result=await client.query(`SELECT ${LOCAL_TURN_EFFORT_SQL} AS effort FROM (SELECT $1::text provider_kind,$2::text effort,$3::jsonb provider_snapshot) t`,[kind,stored,{profile:{runtime,reasoning:selected}}]);
      assert.equal(result.rows[0].effort,expected);
    }
    await client.query(`CREATE SCHEMA "${schema}"`);
    await client.query(`SET LOCAL search_path TO "${schema}", public`);
    await client.query('CREATE TABLE turns(id uuid PRIMARY KEY, origin text); CREATE TABLE usage_records(turn_id uuid PRIMARY KEY REFERENCES turns(id),cost_usd numeric,input_tokens bigint,output_tokens bigint,cached_input_tokens bigint)');
    for(const file of ['038_local_provider.sql','039_local_cost_guard.sql'])await client.query(await readFile(new URL(`../../database/migrations/${file}`,import.meta.url),'utf8'));
    const legacy={profile:{id:'local-4090-qwen38',backend:'claude',model:'officestra-qwen38-27b',contextWindow:65536},host:{address:'192.168.0.10',modelKey:'qwen3.8-27b'}};
    await client.query('INSERT INTO local_agent_profiles(id,definition,enabled) VALUES($1,$2,true)',['local-4090-qwen38',legacy]);
    const migration=await readFile(new URL('../../database/migrations/044_local_llama_codex.sql',import.meta.url),'utf8');
    await client.query(migration);await client.query(migration);
    const profiles=(await client.query('SELECT id,definition FROM local_agent_profiles ORDER BY id')).rows;
    assert.equal(profiles.length,2);
    assert.deepEqual(profiles[0].definition,legacy);
    assert.deepEqual(profiles[1].definition.host,legacy.host);
    assert.equal(profiles[1].definition.profile.backend,'codex');
    assert.equal(profiles[1].definition.profile.runtime,'llama-cpp-b10982');
    assert.equal(profiles[1].definition.profile.contextWindow,65536);
    assert.equal(profiles[1].definition.profile.kvCacheQuantization,'q8_0');
    for(const origin of ['gui','terminal']) {
      const id=randomUUID();await client.query("INSERT INTO turns(id,origin,provider_kind,provider_snapshot) VALUES($1,$2,'local',$3::jsonb)",[id,origin,JSON.stringify({profile:{id:'local-test',contextWindow:32768}})]);
      await client.query('INSERT INTO usage_records(turn_id,cost_usd,input_tokens,output_tokens,cached_input_tokens) VALUES($1,0.0372725,3300,12,100)',[id]);
      await client.query('UPDATE usage_records SET cost_usd=7.5 WHERE turn_id=$1',[id]);
      const {rows:[row]}=await client.query('SELECT * FROM usage_records WHERE turn_id=$1',[id]);
      assert.equal(row.cost_usd,null);assert.equal(row.cost_basis,'local-not-applicable');assert.equal(row.reported_cost_audit.lastReportedCostUsd,7.5);assert.equal(row.input_tokens,'3300');assert.equal(row.cached_input_tokens,'100');
    }
    const id=randomUUID();await client.query("INSERT INTO turns(id,origin) VALUES($1,'gui')",[id]);await client.query('INSERT INTO usage_records(turn_id,cost_usd) VALUES($1,1.23)',[id]);
    const {rows:[cloud]}=await client.query('SELECT cost_usd FROM usage_records WHERE turn_id=$1',[id]);assert.equal(cloud.cost_usd,'1.23');
  }finally{await client.query('ROLLBACK');client.release();await pool.end();}
});
