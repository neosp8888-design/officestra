import { Transform } from 'node:stream';
import { StringDecoder } from 'node:string_decoder';

// Opt-in dialect established by live LM Studio 0.4.24+1 /v1/responses probes:
// input_tokens is INCLUSIVE of cached tokens (input_tokens_details.cached_tokens),
// output_tokens is INCLUSIVE of reasoning tokens (output_tokens_details.reasoning_tokens).
// Never enable for OpenAI/cloud providers, or infer semantics from token ratios.
export const RESPONSES_INCLUSIVE_PROFILE = 'lmstudio-0.4.24+1-responses-inclusive';
export const LLAMA_RESPONSES_PROFILE = 'llama.cpp-b10982-responses-inclusive';
export const verifiedResponsesProfile = value => [RESPONSES_INCLUSIVE_PROFILE,LLAMA_RESPONSES_PROFILE].includes(value);

function tokens(value, name) {
  if (!Number.isSafeInteger(value) || value < 0) throw new Error(`Invalid ${name}`);
  return value;
}

// Extracts a normalized audit record from an OpenAI Responses usage object.
// Returns null when the response carries no usage (policy: absent usage is not
// audited and never fabricated). Throws on malformed or inconsistent counts.
export function extractResponsesUsage(usage, { profile } = {}) {
  if (!verifiedResponsesProfile(profile)) throw new Error('Explicit verified local usage profile required');
  if (!usage || typeof usage !== 'object' || Array.isArray(usage)) return null;
  const raw = { ...usage };
  let input, output, total, cached = 0, reasoning = 0;
  if (raw.input_tokens !== undefined) input = tokens(raw.input_tokens, 'input_tokens');
  if (raw.output_tokens !== undefined) output = tokens(raw.output_tokens, 'output_tokens');
  if (raw.total_tokens !== undefined) total = tokens(raw.total_tokens, 'total_tokens');
  const detailsIn = raw.input_tokens_details;
  if (detailsIn && typeof detailsIn === 'object' && !Array.isArray(detailsIn)) {
    if (detailsIn.cached_tokens !== undefined) cached = tokens(detailsIn.cached_tokens, 'cached_tokens');
  }
  const detailsOut = raw.output_tokens_details;
  if (detailsOut && typeof detailsOut === 'object' && !Array.isArray(detailsOut)) {
    if (detailsOut.reasoning_tokens !== undefined) reasoning = tokens(detailsOut.reasoning_tokens, 'reasoning_tokens');
  }
  // Absent usage is a supported policy: no audit record, never fabricated.
  if (input === undefined && output === undefined && total === undefined) return null;
  if (input === undefined || output === undefined) throw new Error('Malformed Responses usage: input/output tokens required');
  if (cached > input) throw new Error('Inconsistent inclusive input/cache counts');
  if (reasoning > output) throw new Error('Inconsistent inclusive output/reasoning counts');
  const normalized = { ...usage };
  // Split the cached subset out of the inclusive input count. Reasoning stays
  // inside output_tokens: it is real generated output, not a double count.
  normalized.input_tokens = input - cached;
  if (total !== undefined && total !== input + output) throw new Error('Inconsistent Responses usage total');
  return { profile, raw, normalized: { ...normalized }, totalInputTokens: input };
}

// Normalizes a full non-streaming Responses response object. Returns the audit
// record (or null when no usage is present). Throws on malformed usage or an
// error/failed response that carries usage-like fields.
export function normalizeResponsesUsage(response, { profile } = {}) {
  if (!response || typeof response !== 'object' || Array.isArray(response)) throw new Error('Invalid Responses payload');
  if (!verifiedResponsesProfile(profile)) throw new Error('Explicit verified local usage profile required');
  const status = response.status;
  if (status === 'failed' || response.error) {
    // A failed or errored response must never be audited as a successful turn.
    return null;
  }
  if (status !== undefined && status !== 'completed') throw new Error(`Unverified Responses completion status: ${String(status)}`);
  const record = extractResponsesUsage(response.usage, { profile });
  if (!record) return null;
  record.responseId = typeof response.id === 'string' ? response.id : undefined;
  return record;
}

// Pass-through SSE transform. Emits the original bytes untouched and audits only
// the usage carried by a `response.completed` event. A `response.failed` event,
// an error field on completion, or a stream that ends without either is never
// audited as success (no fabricated usage). Bounded parse buffer; supports CRLF,
// multi-chunk frames, UTF-8 continuation bytes, and multi-line data fields.
export function createResponsesUsageSSETransform({ profile, onUsage = () => {}, onCompleted = () => {} } = {}) {
  if (!verifiedResponsesProfile(profile)) throw new Error('Explicit verified local usage profile required');
  const decoder = new StringDecoder('utf8');
  let pending = '';
  let sawCompletion = false;
  function frame(input) {
    // Parse completion before forwarding it: Codex may close HTTP immediately
    // after receiving this frame. That is not an in-flight generation cancel.
    const forward=()=>this.push(input);
    const dataLines = input.split(/\r?\n/).filter(line => line.startsWith('data:'));
    if (!dataLines.length) {forward();return;}
    const raw = dataLines.map(line => line.slice(5).replace(/^ /, '')).join('\n');
    let event;
    try { event = JSON.parse(raw); } catch {forward();return;}
    if (event?.type === 'response.failed') { sawCompletion = true;forward();return; }
    if (event?.type !== 'response.completed' || !event.response) {forward();return;}
    sawCompletion = true;
    const response = event.response;
    if (response.error || response.status === 'failed') {forward();return;}
    const record = normalizeResponsesUsage(response, { profile });
    if (record) onUsage(record);
    onCompleted();
    forward();
  }
  function drain(stream, final = false) {
    let match;
    while ((match = /\r?\n\r?\n/.exec(pending))) {
      if (match.index > 8 * 1024 * 1024) throw new Error('Oversized SSE frame');
      const end = match.index + match[0].length;
      frame.call(stream, pending.slice(0, match.index));
      stream.push(match[0]);
      pending = pending.slice(end);
    }
    if (pending.length > 8 * 1024 * 1024) throw new Error('Oversized SSE frame');
    if (final && pending) { frame.call(stream, pending); pending = ''; }
  }
  return new Transform({
    transform(chunk, encoding, callback) {
      try { pending += decoder.write(chunk); drain(this); callback(); } catch (error) { callback(error); }
    },
    flush(callback) {
      try {
        pending += decoder.end();
        drain(this, true);
        // A stream that never delivered response.completed or response.failed is
        // not a completed turn: no audit record, no fabricated usage.
        if (!sawCompletion) callback(new Error('Responses stream ended without completion'));
        else callback();
      } catch (error) { callback(error); }
    },
  });
}
