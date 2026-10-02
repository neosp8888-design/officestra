// Publish existing public transcript events while the PTY turn is running.
// Share the developer-tool writer so sequence numbers and completion stay ordered.
import { closeSync, openSync, readSync, statSync, watch } from "node:fs";
import { basename, dirname } from "node:path";
import { StringDecoder } from "node:string_decoder";
import { parseAgentEvent } from "./agent-event-parser.mjs";

export async function publishTerminalProgress(state, runtime) {
  const turnID = state.runningTurnID;
  if (!turnID || state.closed || state.toolEventsClosing || !runtime.addActivity) return;
  const collector = state.activities;
  const revision = collector.revision;
  const published = state.publishedProgress;
  if (published?.turnID === turnID && published.collector === collector && published.revision === revision) return;
  const activities = collector.snapshot();
  if (!activities.length) return;
  if (state.toolActivityState?.turnID !== turnID) {
    state.toolActivityState = {
      turnID, character: { id: state.characterID }, sequence: 0,
      activityRecords: new Map(), activityWritePromise: null,
    };
  }
  const writer = state.toolActivityState;
  for (const activity of activities) {
    if (state.runningTurnID !== turnID || state.closed || state.toolEventsClosing) return;
    await runtime.addActivity(writer, activity);
  }
  state.publishedProgress = { turnID, collector, revision };
}

// Claude supplies the start offset through UserPromptSubmit. Watch just this
// transcript's directory during the active turn; no idle polling or extra CLI.
export class ClaudeTerminalProgressWatcher {
  constructor({ state, runtime }) {
    this.state = state;
    this.runtime = runtime;
    this.turnID = state.runningTurnID;
    this.source = { ...state.claudeTranscript };
    this.offset = this.source.offset;
    this.inode = this.source.inode;
    this.decoder = new StringDecoder("utf8");
    this.remainder = "";
    this.watcher = null;
    this.timer = null;
    this.stopped = false;
    this.pending = Promise.resolve();
  }

  start() {
    if (!this.source.path || !Number.isInteger(this.offset)) return;
    try {
      this.watcher = watch(dirname(this.source.path), (_event, file) => {
        if (file && String(file) !== basename(this.source.path)) return;
        clearTimeout(this.timer);
        this.timer = setTimeout(() => this.schedule(), 150);
        this.timer.unref?.();
      });
      this.watcher.on("error", () => this.watcher?.close());
      this.schedule();
    } catch { /* Final transcript import remains the fallback. */ }
  }

  schedule() {
    this.pending = this.pending.then(() => this.sweep()).catch(error => {
      console.warn("Claude 터미널 진행 기록 읽기 실패:", error.message);
    });
    return this.pending;
  }

  async sweep() {
    const state = this.state;
    if (this.stopped || state.closed || state.runningTurnID !== this.turnID || state.toolEventsClosing) return;
    let stat;
    try { stat = statSync(this.source.path); } catch { return; }
    // Compaction/replacement may prepend old turns. Leave bounded recovery to
    // the final importer instead of replaying historical records as live work.
    if ((this.inode !== undefined && stat.ino !== this.inode) || stat.size < this.offset) return;
    if (stat.size === this.offset) return;
    const length = Math.min(stat.size - this.offset, 1024 * 1024);
    const buffer = Buffer.alloc(length);
    let fd;
    let read;
    try {
      fd = openSync(this.source.path, "r");
      read = readSync(fd, buffer, 0, length, this.offset);
    } finally { if (fd !== undefined) closeSync(fd); }
    this.offset += read;
    const lines = (this.remainder + this.decoder.write(buffer.subarray(0, read))).split("\n");
    this.remainder = lines.pop() ?? "";
    for (const line of lines) {
      let record;
      try { record = JSON.parse(line); } catch { continue; }
      if (record.isSidechain || (record.sessionId && record.sessionId !== state.externalSessionID)) continue;
      const at = Date.parse(record.timestamp);
      if (Number.isFinite(at) && at < Date.parse(this.source.startedAt)) continue;
      if (!["assistant", "user"].includes(record.type)) continue;
      state.activities.parsed(parseAgentEvent(line, "claude", state.workdir));
    }
    await publishTerminalProgress(state, this.runtime);
    if (this.offset < stat.size && !this.stopped) {
      this.timer = setTimeout(() => this.schedule(), 0);
      this.timer.unref?.();
    }
  }

  async stop() {
    this.stopped = true;
    clearTimeout(this.timer);
    this.watcher?.close();
    this.watcher = null;
    await this.pending;
  }
}
