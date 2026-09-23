import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import { developerToolEnvironment, parseDeveloperToolEvent, recordDeveloperToolEvent } from '../src/developer-tool-events.mjs';
import { executionEnvironment } from '../src/agent-runtime.mjs';
import { terminalEnvironment } from '../src/terminal-sessions.mjs';

const event = (overrides = {}) => ({characterId:'test', sessionId:'cli-session', mode:'gui',
  activity:{eventKey:'developer-tool:graft:abc',status:'running',text:'Graft · 관련 코드 탐색 중'}, ...overrides});

test('new mutation route preserves both terminal inventory and terminal creation methods', () => {
  const server=readFileSync(new URL('../src/server.mjs',import.meta.url),'utf8');
  assert.match(server,/request.method === "POST" &&\s+url.pathname === "\/api\/developer-tool-events"/);
  assert.match(server,/request.method === "GET" &&\s+url.pathname === "\/api\/terminal-sessions"\s+\) \{\s+send\(response, 200/);
  assert.match(server,/request.method === "POST" &&\s+url.pathname === "\/api\/terminal-sessions"\s+\) \{\s+if \(!trustedJSONMutation/);
});

test('optional tool display is supplied to every backend and local profile in GUI and terminal', () => {
  for (const backend of ['codex','claude','antigravity']) {
    for (const localProfile of [null,{id:'local-model'}]) {
      const character={id:'test',backend,localProfile};
      const gui=executionEnvironment(character,{PATH:'/bin',OFFICE_BACKEND_PORT:'4317'},{workdir:'/tmp/office'});
      const terminal=terminalEnvironment(character,{baseEnvironment:{PATH:'/bin'},workdir:'/tmp/office',characterID:'test',port:4320});
      assert.equal(gui.OFFICESTRA_TOOL_MODE,'gui');
      assert.equal(terminal.OFFICESTRA_TOOL_MODE,'terminal');
      assert.equal(gui.OFFICESTRA_TOOL_CHARACTER_ID,'test');
      assert.match(terminal.OFFICESTRA_TOOL_EVENTS_URL,/127\.0\.0\.1:4320/);
    }
  }
  assert.match(developerToolEnvironment('test').OFFICESTRA_TOOL_EVENTS_URL,/developer-tool-events$/);
});

test('display events cannot supply activity kinds, permissions or unbounded content', () => {
  assert.equal(parseDeveloperToolEvent(event({activity:{...event().activity,kind:'command',permissionDecision:'allow'}})).activity.kind,'tool');
  for(const body of [null,{},event({mode:'unknown'}),event({sessionId:''}),
    event({activity:{...event().activity,eventKey:'other:key'}}),
    event({activity:{...event().activity,text:'x'.repeat(301)}})]) assert.equal(parseDeveloperToolEvent(body),null);
});

test('GUI progress and completion update one activity; stale session cannot write', async () => {
  const state={externalSessionID:'cli-session',activityRecords:new Map()};
  const calls=[];
  const runtime={running:new Map([['test',state]]),addActivity:async(s,a)=>{calls.push(a);s.activityRecords.set(a.eventKey,a);}};
  assert.equal((await recordDeveloperToolEvent(runtime,event())).accepted,true);
  assert.equal((await recordDeveloperToolEvent(runtime,event({activity:{...event().activity,status:'completed',text:'Graft · 관련 코드 3곳 · 200ms'}}))).accepted,true);
  assert.equal(state.activityRecords.size,1);
  assert.equal(state.activityRecords.values().next().value.status,'completed');
  assert.equal((await recordDeveloperToolEvent(runtime,event({sessionId:'old'}))).accepted,false);
  state.cancelRequested=true;
  assert.equal((await recordDeveloperToolEvent(runtime,event())).accepted,false);
  assert.equal(calls.length,2);
});

test('terminal progress attaches only to its running turn and blocks late completion writes', async () => {
  const terminal={externalSessionID:'cli-session',runningTurnID:'turn-1'};
  const states=[];
  const runtime={terminalSessionRegistry:{sessions:new Map([['test',terminal]])},addActivity:async(s)=>states.push(s)};
  const body=event({mode:'terminal'});
  assert.equal((await recordDeveloperToolEvent(runtime,body)).accepted,true);
  assert.equal(states[0].turnID,'turn-1');
  terminal.toolEventsClosing=true;
  assert.equal((await recordDeveloperToolEvent(runtime,body)).accepted,false);
  terminal.toolEventsClosing=false;
  terminal.runningTurnID='turn-2';
  assert.equal((await recordDeveloperToolEvent(runtime,body)).accepted,true);
  assert.notEqual(states[0],states[1]);
  terminal.runningTurnID=null;
  assert.equal((await recordDeveloperToolEvent(runtime,body)).accepted,false);
});

test('event limit still allows existing progress entries to finish', async () => {
  const state={externalSessionID:'cli-session'};
  const runtime={running:new Map([['test',state]]),addActivity:async()=>{}};
  for(let i=0;i<40;i++) assert.equal((await recordDeveloperToolEvent(runtime,event({activity:{...event().activity,eventKey:`developer-tool:graft:${i}`}}))).accepted,true);
  assert.equal((await recordDeveloperToolEvent(runtime,event())).reason,'event_limit');
  assert.equal((await recordDeveloperToolEvent(runtime,event({activity:{...event().activity,eventKey:'developer-tool:graft:0',status:'completed'}}))).accepted,true);
});
