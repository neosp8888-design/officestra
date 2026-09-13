import assert from "node:assert/strict";
import { mkdtemp, writeFile, appendFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { TerminalActivityCollector, readClaudeTerminalActivities, readClaudeTerminalTurn, waitForClaudeTerminalTurn } from "../src/terminal-turn-activities.mjs";

test('Claude flush retry reads delayed thinking/text and latest usage once per response',async t=>{
  const root=await mkdtemp(join(tmpdir(),'claude-flush-'));t.after(()=>rm(root,{recursive:true,force:true}));
  const path=join(root,'session.jsonl');await writeFile(path,'');
  const base={type:'assistant',sessionId:'session',timestamp:'2026-09-14T00:00:01Z'};
  const record=(id,content,output)=>JSON.stringify({...base,message:{id,content,usage:{input_tokens:15731,output_tokens:output,cache_read_input_tokens:0}}})+'\n';
  const writing=new Promise((resolve,reject)=>setTimeout(()=>appendFile(path,
    record('same',[{type:'thinking',thinking:'공개 사고'}],20)+
    record('same',[{type:'text',text:'최종'}],175)).then(resolve,reject),40));
  const result=await waitForClaudeTerminalTurn(path,{sessionID:'session',finalResponse:'최종'},{timeoutMs:500,pollMs:20});await writing;
  assert.equal(result.finalFound,true);assert.equal(result.usage.inputTokens,15731);assert.equal(result.usage.outputTokens,175);
  assert.deepEqual(result.activities.map(a=>a.kind),['thinking']);
});

test('Claude missing/rotated offset recovery filters time, session and sidechains',async t=>{
  const root=await mkdtemp(join(tmpdir(),'claude-scope-'));t.after(()=>rm(root,{recursive:true,force:true}));
  const path=join(root,'session.jsonl');
  const row=(text,changes={})=>JSON.stringify({type:'assistant',sessionId:'session',timestamp:'2026-09-14T00:00:01Z',message:{id:text,content:[{type:'text',text}],usage:{input_tokens:10,output_tokens:2}},...changes})+'\n';
  await writeFile(path,row('old',{timestamp:'2026-09-13T00:00:00Z'})+row('other',{sessionId:'other'})+row('side',{isSidechain:true})+row('no-time',{timestamp:null})+row('final'));
  for(const offset of [null,999999]) {
    const result=await readClaudeTerminalTurn(path,{offset,sessionID:'session',startedAt:'2026-09-14T00:00:00Z',endedAt:'2026-09-14T00:00:02Z',finalResponse:'final'});
    assert.equal(result.finalFound,true);assert.equal(result.usage.inputTokens,10);assert.deepEqual(result.activities,[]);
  }
  assert.equal((await readClaudeTerminalTurn(path,{offset:null,sessionID:'session'})).usage,null);
});

test('bounded fallback never reports a partial oversized turn as complete',async t=>{
  const root=await mkdtemp(join(tmpdir(),'claude-large-'));t.after(()=>rm(root,{recursive:true,force:true}));
  const path=join(root,'session.jsonl');
  const row=text=>JSON.stringify({type:'assistant',sessionId:'session',timestamp:'2026-09-14T00:00:01Z',message:{id:text.slice(0,8),content:[{type:'text',text}],usage:{input_tokens:10,output_tokens:2}}})+'\n';
  await writeFile(path,row('x'.repeat(4*1024*1024))+row('final'));
  const result=await readClaudeTerminalTurn(path,{offset:null,sessionID:'session',startedAt:'2026-09-14T00:00:00Z',endedAt:'2026-09-14T00:00:02Z',finalResponse:'final'});
  assert.equal(result.usage,null);assert.equal(result.finalFound,false);assert.deepEqual(result.activities,[]);
});

test("Codex 공개 요약·진행·도구 결과만 기존 활동 형식으로 변환한다", () => {
  const collector = new TerminalActivityCollector("/repo");
  const add = (payload) => collector.codex({ type: "response_item", payload });
  add({ type: "message", role: "system", content: [{ type: "text", text: "시스템 지침" }] });
  add({ type: "reasoning", text: "원시 추론", encrypted_content: "암호화 내용", summary: [{ type: "summary_text", text: "공개 요약" }] });
  add({ type: "message", role: "assistant", phase: "commentary", content: [{ type: "output_text", text: "확인 중입니다" }] });
  add({ type: "message", role: "assistant", phase: "analysis", content: [{ type: "output_text", text: "비공개" }] });
  add({ type: "message", role: "assistant", phase: "final_answer", content: [{ type: "output_text", text: "최종 답변" }] });
  add({ type: "function_call", name: "exec_command", call_id: "call1", arguments: JSON.stringify({ cmd: "git diff --check" }) });
  add({ type: "function_call_output", call_id: "call1", output: JSON.stringify({ exit_code: 1, output: "응답 전문은 제외" }) });
  const activities = collector.finish("최종 답변");
  assert.deepEqual(activities.map(({ kind, text, status }) => ({ kind, text, status })), [
    { kind: "thinking", text: "공개 요약", status: "completed" },
    { kind: "message", text: "확인 중입니다", status: "completed" },
    { kind: "command", text: "git diff --check", status: "failed" },
  ]);
  assert.equal(JSON.stringify(activities).includes("암호화"), false);
});

test("중복 메시지·기계 블록·최종 답변은 작업 내역에서 반복하지 않는다", () => {
  const collector = new TerminalActivityCollector();
  collector.add({ kind: "message", text: "진행 중", eventKey: "one" });
  collector.add({ kind: "message", text: "진행 중", eventKey: "two" });
  collector.add({ kind: "message", text: "완료\n[NEED_INPUT]", eventKey: "final" });
  collector.add({ kind: "message", text: '확인\n[OFFICE_SOURCES]\n[{"kind":"file","title":"파일","locator":"a.mjs"}]' });
  assert.deepEqual(collector.finish("완료").map((a) => a.text), ["진행 중", "확인"]);
});

test("긴 터미널 기록은 표시량을 제한하고 생략 사실을 남긴다", () => {
  const collector = new TerminalActivityCollector();
  for (let i = 0; i < 505; i++) collector.add({ kind: "tool", text: "a".repeat(7000), eventKey: String(i) });
  const activities = collector.finish();
  assert.equal(activities.length, 501);
  assert.equal(activities[0].text.length, 6000);
  assert.equal(activities.at(-1).text, "추가 활동 5개 표시 생략");
});

test("Claude는 시작 위치 이후 같은 세션의 공개 활동만 읽는다", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "terminal-activities-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const path = join(root, "transcript.jsonl");
  const assistant = (id, content, extra = {}) => ({ type: "assistant", sessionId: "session", message: { id, content }, ...extra });
  const previous = JSON.stringify(assistant("old", [{ type: "text", text: "이전 턴" }])) + "\n";
  await writeFile(path, previous);
  const records = [
    assistant("a", [{ type: "thinking", thinking: "공개된 추론 요약" }, { type: "text", text: "파일 확인 중" }, { type: "tool_use", id: "tool1", name: "Read", input: { file_path: "/repo/a.mjs" } }]),
    { type: "user", sessionId: "session", message: { content: [{ type: "tool_result", tool_use_id: "tool1", is_error: true, content: "도구 전문" }] } },
    assistant("side", [{ type: "text", text: "별도 작업" }], { isSidechain: true }),
    assistant("other", [{ type: "text", text: "다른 세션" }], { sessionId: "other" }),
    assistant("last", [{ type: "text", text: "최종 답변" }]),
  ];
  await appendFile(path, records.map(JSON.stringify).join("\n") + "\n{partial");
  const activities = await readClaudeTerminalActivities(path, { offset: Buffer.byteLength(previous), sessionID: "session", workdir: "/repo", finalResponse: "최종 답변" });
  assert.deepEqual(activities.map((a) => a.kind), ["thinking", "tool", "message"]);
  assert.equal(activities.find((a) => a.kind === "tool").status, "failed");
  assert.match(activities.find((a) => a.kind === "tool").text, /a\.mjs/);
  assert.doesNotMatch(JSON.stringify(activities), /이전 턴|별도 작업|다른 세션|최종 답변|도구 전문/);
});

test("Claude 원본이 없어도 최종 응답 저장을 막지 않는다", async () => {
  assert.deepEqual(await readClaudeTerminalActivities("/nonexistent/officestra-fixture.jsonl"), []);
});
