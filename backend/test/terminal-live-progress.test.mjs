import assert from "node:assert/strict";
import test from "node:test";
import { mkdtemp, writeFile, appendFile, rm, stat } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { setTimeout as delay } from "node:timers/promises";
import { TerminalActivityCollector } from "../src/terminal-turn-activities.mjs";
import { ClaudeTerminalProgressWatcher, publishTerminalProgress } from "../src/terminal-live-progress.mjs";
import { AgentRuntime } from "../src/agent-runtime.mjs";

function harness() {
  const writes = [], events = [];
  const runtime = new AgentRuntime({
    pool: { async query(sql, values) { writes.push({sql, values}); return { rows: [], rowCount: 1 }; } },
    withTransaction: async f => f(runtime.pool), workdir: "/repo", broadcast: event => events.push(event),
  });
  runtime.touchTurn = async () => {};
  const state = { characterID: "boss", runningTurnID: "turn-1", externalSessionID: "session-1", activities: new TerminalActivityCollector("/repo") };
  return { runtime, state, writes, events };
}

test("live snapshot retains running then failure and hides command credentials", () => {
  const c = new TerminalActivityCollector();
  c.codex({ type: "response_item", payload: { type: "function_call", call_id: "c1", name: "exec_command", arguments: JSON.stringify({ cmd: "curl -H 'Authorization: Bearer secret123' https://example.test" }) } });
  assert.equal(c.snapshot()[0].status, "running");
  assert.equal(c.snapshot()[0].text.includes("secret123"), false);
  c.codex({ type: "response_item", payload: { type: "function_call_output", call_id: "c1", output: JSON.stringify({exit_code: 1}) } });
  assert.equal(c.snapshot().length, 1);
  assert.equal(c.snapshot()[0].status, "failed");
  assert.equal(c.finish()[0].eventKey, c.snapshot()[0].eventKey);
});

test("progress shares the developer-tool writer, updates in place and skips unchanged sweeps", async () => {
  const h = harness();
  h.state.toolActivityState = { turnID:"turn-1", character:{id:"boss"}, sequence:1, activityRecords:new Map(), activityWritePromise:null };
  h.state.activities.add({ kind:"command",text:"swift test",eventKey:"test",status:"running" });
  await publishTerminalProgress(h.state,h.runtime);
  const first = h.writes.find(w => w.sql.includes("INSERT INTO turn_activities"));
  assert.deepEqual(first.values.slice(0,6),["turn-1",2,"command","swift test","terminal:test","running"]);
  const count = h.writes.length;
  await publishTerminalProgress(h.state,h.runtime);
  assert.equal(h.writes.length,count);
  h.state.activities.add({ kind:"command",text:"swift test",eventKey:"test",status:"completed" });
  await publishTerminalProgress(h.state,h.runtime);
  assert.equal(h.writes.filter(w => w.sql.includes("INSERT INTO turn_activities")).length,1);
  assert.equal(h.writes.find(w => w.sql.includes("UPDATE turn_activities")).values[4],"completed");
  assert.equal(h.events.at(-1).type,"feed.changed");
});

test("a long final answer stays bounded live and is excluded from the final activity import", () => {
  const c = new TerminalActivityCollector();
  const response = "긴 답변".repeat(2000);
  c.add({ kind: "message", text: response, eventKey: "final" });
  assert.equal(c.snapshot()[0].text.length, 6000);
  assert.deepEqual(c.finish(response), []);
});

test("progress obeys the completion lock and starts a fresh writer per turn", async () => {
  const h = harness();
  h.state.activities.add({kind:"message",text:"진행",eventKey:"m"});
  h.state.toolEventsClosing = true;
  await publishTerminalProgress(h.state,h.runtime);
  assert.equal(h.writes.length,0);
  h.state.toolEventsClosing = false;
  await publishTerminalProgress(h.state,h.runtime);
  h.state.runningTurnID = "turn-2";
  h.state.activities = new TerminalActivityCollector();
  h.state.activities.add({kind:"message",text:"다음 턴",eventKey:"n"});
  await publishTerminalProgress(h.state,h.runtime);
  assert.equal(h.state.toolActivityState.turnID,"turn-2");
  assert.equal(h.state.toolActivityState.sequence,1);
});

async function claudeFixture(t) {
  const h = harness();
  const dir = await mkdtemp(join(tmpdir(),"terminal-live-"));
  t.after(() => rm(dir,{recursive:true,force:true}));
  const path = join(dir,"session.jsonl");
  await writeFile(path,JSON.stringify({type:"assistant",message:{content:[{type:"text",text:"과거 답변"}]}})+"\n");
  const s = await stat(path);
  h.state.claudeTranscript = {path,offset:s.size,inode:s.ino,startedAt:"2026-10-02T00:00:00Z"};
  h.state.workdir="/repo";
  const watcher = new ClaudeTerminalProgressWatcher(h);
  t.after(() => watcher.stop());
  return {...h,path,watcher};
}
const row = (text,extra={}) => ({type:"assistant",sessionId:"session-1",timestamp:"2026-10-02T01:00:00Z",message:{id:text,content:[{type:"text",text}]},...extra});

test("Claude imports new same-session public events and waits for complete UTF-8 lines", async t => {
  const h = await claudeFixture(t);
  const text = [row("정상 진행"),row("다른 세션",{sessionId:"other"}),row("다른 작업자",{isSidechain:true}),row("예전 시각",{timestamp:"2026-10-01T01:00:00Z"})].map(r=>JSON.stringify(r)+"\n").join("");
  const bytes=Buffer.from(text), split=bytes.indexOf(Buffer.from("정상"))+1;
  await appendFile(h.path,bytes.subarray(0,split));
  await h.watcher.sweep();
  assert.equal(h.writes.length,0);
  await appendFile(h.path,bytes.subarray(split));
  await h.watcher.sweep();
  assert.deepEqual(h.state.activities.snapshot().map(a=>a.text),["정상 진행"]);
  assert.ok(h.writes.some(w=>w.sql.includes("INSERT INTO turn_activities")));
});

test("Claude file events publish before Stop and dispose the watch without idle polling", async t => {
  const h = await claudeFixture(t);
  h.watcher.start();
  await appendFile(h.path,JSON.stringify(row("작업 중"))+"\n");
  const deadline=Date.now()+2000;
  while(!h.writes.length&&Date.now()<deadline)await delay(20);
  assert.ok(h.writes.length>0,"publish must happen without a Stop hook");
  await h.watcher.stop();
  assert.equal(h.watcher.watcher,null);
  const count=h.writes.length;
  await appendFile(h.path,JSON.stringify(row("종료 이후"))+"\n");
  await h.watcher.sweep();
  assert.equal(h.writes.length,count);
});

test("Claude stops live reads after turn change or file truncation", async t => {
  const h=await claudeFixture(t);
  await writeFile(h.path,"\n");
  await h.watcher.sweep();
  assert.equal(h.writes.length,0);
  h.state.runningTurnID="turn-2";
  await appendFile(h.path,JSON.stringify(row("섞으면 안 됨"))+"\n");
  await h.watcher.sweep();
  assert.equal(h.writes.length,0);
});
