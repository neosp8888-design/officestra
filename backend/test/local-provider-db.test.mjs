import test from 'node:test';
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import pg from 'pg';

test('PostgreSQL: GUI/terminal tokens retained, local reported prices audited/excluded including late writes', {skip:process.env.OFFICESTRA_LOCAL_DB_TEST!=='1'}, async()=>{
  const pool=new pg.Pool({connectionString:process.env.DATABASE_URL??'postgres://office:office-local@127.0.0.1:54329/office'});
  const client=await pool.connect();const schema='local_test_'+randomUUID().replaceAll('-','');
  try{
    await client.query('BEGIN');
    await client.query(`CREATE SCHEMA "${schema}"`);
    await client.query(`SET LOCAL search_path TO "${schema}", public`);
    await client.query('CREATE TABLE turns(id uuid PRIMARY KEY, origin text); CREATE TABLE usage_records(turn_id uuid PRIMARY KEY REFERENCES turns(id),cost_usd numeric,input_tokens bigint,output_tokens bigint,cached_input_tokens bigint)');
    for(const file of ['038_local_provider.sql','039_local_cost_guard.sql'])await client.query(await readFile(new URL(`../../database/migrations/${file}`,import.meta.url),'utf8'));
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
