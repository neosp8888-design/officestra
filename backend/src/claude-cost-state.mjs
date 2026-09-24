import { closeSync, fstatSync, openSync, readSync } from 'node:fs';

// Claude resumes the persisted session counters, including modelUsage. Read
// the same last cost-state before spawning; no token-price reconstruction.
export function readClaudeCostState(path, sessionID) {
  if (!path || !sessionID) return null;
  let fd;
  try {
    fd = openSync(path, 'r');
    let offset = fstatSync(fd).size;
    let remainder = Buffer.alloc(0);
    while (offset > 0) {
      const size = Math.min(offset, 64 * 1024);
      offset -= size;
      const chunk = Buffer.alloc(size);
      const read = readSync(fd, chunk, 0, size, offset);
      const data = Buffer.concat([chunk.subarray(0, read), remainder]);
      let end = data.length;
      for (let i = data.length - 1; i >= 0; i--) {
        if (data[i] !== 10) continue;
        const state = parseState(data.subarray(i + 1, end), sessionID);
        if (state) return state;
        end = i;
      }
      remainder = data.subarray(0, end);
    }
    return parseState(remainder, sessionID);
  } catch {
    return null;
  } finally {
    if (fd !== undefined) closeSync(fd);
  }
}

function parseState(bytes, sessionID) {
  try {
    const value = JSON.parse(bytes.toString('utf8'));
    if (value.type !== 'cost-state' || value.sessionId !== sessionID ||
        typeof value.totalCostUSD !== 'number' || !Number.isFinite(value.totalCostUSD) ||
        value.totalCostUSD < 0 || !value.modelUsage || typeof value.modelUsage !== 'object' ||
        Array.isArray(value.modelUsage)) return null;
    return { totalCostUsd: value.totalCostUSD, modelUsage: value.modelUsage };
  } catch {
    return null;
  }
}
