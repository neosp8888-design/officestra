// 터미널 원본에서 기존 GUI가 표시할 수 있는 공개 활동만 추출한다.
// 원시 추론/암호화 내용, 도구 응답 전문, 시스템 지침은 저장하지 않는다.
import { createReadStream, statSync } from "node:fs";
import { createInterface } from "node:readline";
import { decodeAgentResponse, parseAgentEvent, claudeMessageUsage, claudeSessionUsageKey } from "./agent-event-parser.mjs";
import { addUsage } from "./terminal-usage.mjs";

const MAX_ACTIVITIES = 500;
const MAX_TEXT = 6_000;
const KINDS = new Set(["message", "thinking", "command", "tool", "suggestion"]);
const clipped = (value) => {
  const text = String(value ?? "").trim();
  return text.length > MAX_TEXT ? `${text.slice(0, MAX_TEXT - 1)}…` : text;
};
const contentText = (content) => (Array.isArray(content) ? content : [])
  .filter((part) => ["text", "Text", "output_text", "summary_text"].includes(part?.type))
  .map((part) => part.text ?? "").join("\n").trim();

export class TerminalActivityCollector {
  constructor(workdir = null) {
    this.workdir = workdir;
    this.entries = new Map();
    this.omitted = 0;
  }

  add(activity) {
    if (!KINDS.has(activity?.kind)) return;
    const key = String(activity.eventKey ?? `${activity.kind}:${activity.text}`);
    const old = this.entries.get(key);
    const text = activity.preserveText && old ? old.text : clipped(activity.text);
    if (!text) return;
    if (!old && this.entries.size >= MAX_ACTIVITIES) {
      this.omitted += 1;
      return;
    }
    this.entries.set(key, {
      kind: activity.kind,
      text,
      eventKey: key,
      status: activity.status === "failed" ? "failed" : "completed",
    });
  }

  parsed(event) {
    for (const value of event?.activities ?? []) this.add(value);
    if (event?.activity) this.add(event.activity);
    if (event?.agentMessage) this.add({
      kind: "message", text: decodeAgentResponse(event.agentMessage).text,
      eventKey: event.agentMessageKey ? `message:${event.agentMessageKey}` : null,
    });
  }

  codex(record) {
    const item = record?.payload ?? {};
    if (record.type === "response_item") {
      if (item.type === "message" && item.role === "assistant" && item.phase === "commentary") {
        this.add({ kind: "message", text: contentText(item.content) });
      } else if (item.type === "reasoning") {
        // summary는 CLI 공개 요약이다. encrypted_content/text는 읽지 않는다.
        this.add({ kind: "thinking", text: contentText(item.summary) });
      } else if (["function_call", "custom_tool_call"].includes(item.type)) {
        let args;
        try { args = JSON.parse(item.arguments ?? "{}"); } catch {}
        const name = [item.namespace, item.name].filter(Boolean).join(".");
        const command = /(?:exec_command|shell_command|shell)$/.test(name)
          ? args?.cmd ?? args?.command : null;
        this.add({
          kind: command ? "command" : "tool",
          text: command ? String(command) : `도구 · ${name || "실행"}`,
          eventKey: `call:${item.call_id ?? item.id}`,
        });
      } else if (["function_call_output", "custom_tool_call_output"].includes(item.type)) {
        let output;
        try { output = typeof item.output === "string" ? JSON.parse(item.output) : item.output; } catch {}
        this.add({
          kind: this.entries.get(`call:${item.call_id}`)?.kind ?? "tool",
          text: "도구 완료", eventKey: `call:${item.call_id}`, preserveText: true,
          status: output?.isError === true || (Number.isInteger(output?.exit_code) && output.exit_code !== 0)
            ? "failed" : "completed",
        });
      }
      return;
    }
    if (record.type !== "event_msg") return;
    if (item.type === "item_completed") {
      const value = item.item ?? {};
      const type = String(value.type ?? "").replace(/[_-]/g, "").toLowerCase();
      const types = { commandexecution: "command_execution", filechange: "file_change", mcptoolcall: "mcp_tool_call", websearch: "web_search" };
      if (type === "agentmessage" && value.phase === "commentary") {
        this.add({ kind: "message", text: contentText(value.content) || value.text });
      } else if (type === "reasoning") {
        this.add({ kind: "thinking", text: contentText(value.summary) || value.text });
      } else if (types[type]) {
        this.parsed(parseAgentEvent(JSON.stringify({
          type: "item.completed", item: { ...value, type: types[type] },
        }), "codex", this.workdir));
      }
    } else if (item.type === "patch_apply_end") {
      const changes = Object.entries(item.changes ?? {}).map(([path, change]) => ({ path, kind: change.type ?? "update" }));
      this.parsed(parseAgentEvent(JSON.stringify({ type: "item.completed", item: {
        type: "file_change", id: item.call_id, changes,
        status: item.success === false ? "failed" : "completed",
      } }), "codex", this.workdir));
    }
  }

  finish(finalResponse = "") {
    const finalText = decodeAgentResponse(finalResponse).text;
    const signatures = new Set();
    const result = [];
    for (const entry of this.entries.values()) {
      const text = entry.kind === "message" ? decodeAgentResponse(entry.text).text : entry.text;
      if (!text || (entry.kind === "message" && text === finalText)) continue;
      // 같은 공개 메시지가 여러 CLI 기록 형식에 나타나도 한 번만 표시한다.
      const signature = [entry.kind, text, entry.status].join("\n");
      if (["message", "thinking"].includes(entry.kind) && signatures.has(signature)) continue;
      signatures.add(signature);
      result.push({ ...entry, text, eventKey: `terminal:${entry.eventKey}` });
    }
    if (this.omitted) result.push({ kind: "tool", text: `추가 활동 ${this.omitted}개 표시 생략`, status: "completed", eventKey: "terminal:omitted" });
    return result;
  }
}

// UserPromptSubmit 시점의 바이트 위치부터 읽어 같은 세션의 과거 턴을 섞지 않는다.
export async function readClaudeTerminalActivities(path, { offset = 0, sessionID, workdir, finalResponse } = {}) {
  return (await readClaudeTerminalTurn(path,{offset,sessionID,workdir,finalResponse})).activities;
}

export async function readClaudeTerminalTurn(path, {offset=0,sessionID,workdir,finalResponse,startedAt,endedAt,requireTimestamps=false,inode}={}) {
  const collector = new TerminalActivityCollector(workdir);
  const requests=new Map();let finalFound=false, needsEarlierBoundary=false, earlierBoundary=false;
  if (!path) return {activities:[],usage:null,finalFound:false};
  let stream;
  try {
    const stat=statSync(path);
    if(offset===null||offset>stat.size||(inode!==undefined&&inode!==stat.ino)) {
      // Missing start path or compaction replaced the file. Bound the recovery
      // scan and require matching session plus timestamps; never import history.
      if(!startedAt||!endedAt||!sessionID)return {activities:[],usage:null,finalFound:false};
      offset=Math.max(0,stat.size-4*1024*1024);requireTimestamps=true;
      needsEarlierBoundary=offset>0;
    }
    stream = createReadStream(path, { start: offset, encoding: "utf8" });
    for await (const line of createInterface({ input: stream, crlfDelay: Infinity })) {
      let record;
      try { record = JSON.parse(line); } catch { continue; }
      if (record.isSidechain || (record.sessionId && record.sessionId !== sessionID)) continue;
      const at=Date.parse(record.timestamp);
      if(record.sessionId===sessionID&&Number.isFinite(at)&&startedAt&&at<Date.parse(startedAt))earlierBoundary=true;
      if(requireTimestamps&&(!Number.isFinite(at)||record.sessionId!==sessionID))continue;
      if(Number.isFinite(at)&&((startedAt&&at<Date.parse(startedAt))||(endedAt&&at>Date.parse(endedAt))))continue;
      if (!["assistant", "user"].includes(record.type)) continue;
      collector.parsed(parseAgentEvent(line, "claude", workdir));
      if(record.type==='assistant') {
        if(contentText(record.message?.content)===String(finalResponse??'').trim()&&String(finalResponse??'').trim())finalFound=true;
        const usage=claudeMessageUsage(record),key=claudeSessionUsageKey(record);
        // Streaming snapshots for one response share message.id. Keep the latest,
        // not the first partial count, and never sum thought/text duplicates.
        if(usage&&key)requests.set(key,{...(requests.get(key)??{}),...Object.fromEntries(Object.entries(usage).filter(([,value])=>value!==null&&value!==undefined))});
      }
    }
  } catch (error) {
    // 부가 기록을 읽지 못해도 원래 최종 답변 저장은 중단하지 않는다.
    // The caller emits one warning after its bounded flush retry, not per poll.
  } finally {
    stream?.destroy();
  }
  let usage=null;for(const value of requests.values())usage=addUsage(usage,value);
  if(needsEarlierBoundary&&!earlierBoundary)return {activities:[],usage:null,finalFound:false};
  return {activities:collector.finish(finalResponse),usage,finalFound};
}

export async function waitForClaudeTerminalTurn(path,options,{timeoutMs=1200,pollMs=100}={}) {
  const deadline=Date.now()+timeoutMs;let result;
  do {
    result=await readClaudeTerminalTurn(path,options);
    if(result.finalFound&&result.usage)return result;
    if(Date.now()>=deadline||!path)break;
    await new Promise(resolve=>setTimeout(resolve,Math.min(pollMs,Math.max(0,deadline-Date.now()))));
  }while(true);
  return result;
}
