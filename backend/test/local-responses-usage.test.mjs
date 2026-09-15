import test from 'node:test';
import assert from 'node:assert/strict';
import { Readable } from 'node:stream';
import {
  RESPONSES_INCLUSIVE_PROFILE,
  extractResponsesUsage,
  normalizeResponsesUsage,
  createResponsesUsageSSETransform,
} from '../src/local-responses-usage.mjs';

const PROFILE = { profile: RESPONSES_INCLUSIVE_PROFILE };

function sseStream(frames) {
  return Readable.from([Buffer.concat(frames.map(f => Buffer.from(f)))]);
}

async function collect(stream) {
  const chunks = [];
  for await (const chunk of stream) chunks.push(chunk);
  return Buffer.concat(chunks).toString('utf8');
}

test('extractResponsesUsage: inclusive input split, reasoning kept in output', () => {
  const usage = {
    input_tokens: 55,
    output_tokens: 16,
    total_tokens: 71,
    input_tokens_details: { cached_tokens: 42 },
    output_tokens_details: { reasoning_tokens: 16 },
  };
  const record = extractResponsesUsage(usage, PROFILE);
  assert.equal(record.profile, RESPONSES_INCLUSIVE_PROFILE);
  assert.deepEqual(record.raw, usage);
  // Cached subset split out of the inclusive input count.
  assert.equal(record.normalized.input_tokens, 55 - 42);
  // Reasoning stays inside output: it is real generated output.
  assert.equal(record.normalized.output_tokens, 16);
  assert.equal(record.totalInputTokens, 55);
});

test('extractResponsesUsage: absent usage returns null (never fabricated)', () => {
  assert.equal(extractResponsesUsage(undefined, PROFILE), null);
  assert.equal(extractResponsesUsage(null, PROFILE), null);
  assert.equal(extractResponsesUsage({}, PROFILE), null);
});

test('extractResponsesUsage: rejects missing profile', () => {
  assert.throws(() => extractResponsesUsage({ input_tokens: 1, output_tokens: 1 }), /profile/);
});

test('extractResponsesUsage: rejects malformed counts', () => {
  const base = { input_tokens: 55, output_tokens: 16 };
  // Only one of the two core counts present is malformed (the other is required).
  assert.throws(() => extractResponsesUsage({ input_tokens: 55 }, PROFILE), /input\/output tokens required/);
  assert.throws(
    () => extractResponsesUsage({ ...base, input_tokens_details: { cached_tokens: 99 } }, PROFILE),
    /Inconsistent inclusive input\/cache/,
  );
  assert.throws(
    () => extractResponsesUsage({ ...base, output_tokens_details: { reasoning_tokens: 99 } }, PROFILE),
    /Inconsistent inclusive output\/reasoning/,
  );
  assert.throws(() => extractResponsesUsage({ ...base, input_tokens: -1 }, PROFILE), /Invalid input_tokens/);
});

test('extractResponsesUsage: rejects inconsistent total', () => {
  const usage = { input_tokens: 55, output_tokens: 16, total_tokens: 99 };
  assert.throws(() => extractResponsesUsage(usage, PROFILE), /Inconsistent Responses usage total/);
});

test('normalizeResponsesUsage: completed response yields audit record with id', () => {
  const response = {
    id: 'resp_123',
    status: 'completed',
    usage: { input_tokens: 55, output_tokens: 16, total_tokens: 71 },
  };
  const record = normalizeResponsesUsage(response, PROFILE);
  assert.equal(record.responseId, 'resp_123');
  assert.equal(record.totalInputTokens, 55);
});

test('normalizeResponsesUsage: failed or errored response is never audited', () => {
  assert.equal(normalizeResponsesUsage({ status: 'failed', usage: { input_tokens: 1, output_tokens: 1 } }, PROFILE), null);
  assert.equal(
    normalizeResponsesUsage({ status: 'completed', error: { message: 'boom' }, usage: { input_tokens: 1, output_tokens: 1 } }, PROFILE),
    null,
  );
});

test('normalizeResponsesUsage: unverified completion status throws', () => {
  assert.throws(
    () => normalizeResponsesUsage({ status: 'in_progress', usage: { input_tokens: 1, output_tokens: 1 } }, PROFILE),
    /Unverified Responses completion status/,
  );
});

test('SSE transform: passes bytes through unchanged and audits completed event', async () => {
  const frames = [
    'event: response.created\ndata: {"type":"response.created"}\n\n',
    'event: response.output_item.added\ndata: {"type":"response.output_item.added","item":{"id":"msg_1"}}\n\n',
    'event: response.completed\ndata: {"type":"response.completed","response":{"id":"resp_9","status":"completed","usage":{"input_tokens":55,"output_tokens":16,"total_tokens":71}}}\n\n',
  ];
  const seen = [];
  const out = sseStream(frames).pipe(createResponsesUsageSSETransform({ ...PROFILE, onUsage: r => seen.push(r) }));
  const text = await collect(out);
  // Pass-through: original bytes are emitted untouched.
  assert.equal(text, frames.join(''));
  assert.equal(seen.length, 1);
  assert.equal(seen[0].responseId, 'resp_9');
  assert.equal(seen[0].totalInputTokens, 55);
});

test('SSE transform: CRLF line endings pass through unchanged', async () => {
  const frame = 'event: response.completed\r\ndata: {"type":"response.completed","response":{"id":"r1","status":"completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}\r\n\r\n';
  const seen = [];
  const out = sseStream([frame]).pipe(createResponsesUsageSSETransform({ ...PROFILE, onUsage: r => seen.push(r) }));
  const text = await collect(out);
  assert.equal(text, frame);
  assert.equal(seen.length, 1);
});

test('SSE transform: multi-chunk split across frames still audits once', async () => {
  const full = 'event: response.completed\ndata: {"type":"response.completed","response":{"id":"r2","status":"completed","usage":{"input_tokens":30,"output_tokens":7,"total_tokens":37}}}\n\n';
  // Split mid-frame so the transform must buffer across chunks.
  const a = full.slice(0, 10);
  const b = full.slice(10, 40);
  const c = full.slice(40);
  const seen = [];
  const out = Readable.from([Buffer.from(a), Buffer.from(b), Buffer.from(c)])
    .pipe(createResponsesUsageSSETransform({ ...PROFILE, onUsage: r => seen.push(r) }));
  const text = await collect(out);
  assert.equal(text, full);
  assert.equal(seen.length, 1);
});

test('SSE transform: UTF-8 continuation byte split across chunks', async () => {
  // A multi-byte character (한글) inside a non-audited event must survive intact.
  const frame = 'event: response.output_item.added\ndata: {"type":"response.output_item.added","item":{"content":[{"text":"안녕"}]}}\n\n';
  const done = 'event: response.completed\ndata: {"type":"response.completed","response":{"id":"r5","status":"completed","usage":{"input_tokens":20,"output_tokens":4,"total_tokens":24}}}\n\n';
  const buf = Buffer.from(frame, 'utf8');
  // Find a split point inside a multi-byte sequence.
  let splitAt = -1;
  for (let i = 0; i < buf.length; i++) {
    if ((buf[i] & 0xc0) === 0x80) { splitAt = i; break; } // continuation byte
  }
  assert.ok(splitAt > 0, 'expected a multi-byte sequence to split');
  const full = frame + done;
  const seen = [];
  const out = Readable.from([buf.subarray(0, splitAt), buf.subarray(splitAt), Buffer.from(done)])
    .pipe(createResponsesUsageSSETransform({ ...PROFILE, onUsage: r => seen.push(r) }));
  const text = await collect(out);
  assert.equal(text, full);
  // The multi-byte character survived the mid-sequence split intact.
  assert.ok(text.includes('안녕'));
});

test('SSE transform: response.failed is not audited as success', async () => {
  const frames = [
    'event: response.created\ndata: {"type":"response.created"}\n\n',
    'event: response.failed\ndata: {"type":"response.failed","response":{"id":"r3","status":"failed"}}\n\n',
  ];
  const seen = [];
  const out = sseStream(frames).pipe(createResponsesUsageSSETransform({ ...PROFILE, onUsage: r => seen.push(r) }));
  await collect(out);
  assert.equal(seen.length, 0);
});

test('SSE transform: completed event with error field is not audited', async () => {
  const frame = 'event: response.completed\ndata: {"type":"response.completed","response":{"id":"r4","status":"completed","error":{"message":"x"},"usage":{"input_tokens":1,"output_tokens":1}}}\n\n';
  const seen = [];
  const out = sseStream([frame]).pipe(createResponsesUsageSSETransform({ ...PROFILE, onUsage: r => seen.push(r) }));
  await collect(out);
  assert.equal(seen.length, 0);
});

test('SSE transform: stream ending without completion errors (no fabricated usage)', async () => {
  const frames = [
    'event: response.created\ndata: {"type":"response.created"}\n\n',
    'event: response.in_progress\ndata: {"type":"response.in_progress"}\n\n',
  ];
  const seen = [];
  const out = sseStream(frames).pipe(createResponsesUsageSSETransform({ ...PROFILE, onUsage: r => seen.push(r) }));
  await assert.rejects(collect(out), /ended without completion/);
  assert.equal(seen.length, 0);
});

test('SSE transform: rejects missing profile', () => {
  assert.throws(() => createResponsesUsageSSETransform({ onUsage: () => {} }), /profile/);
});
