import { Transform } from 'node:stream';
import { StringDecoder } from 'node:string_decoder';

// Opt-in dialect established by LM Studio runtime stats vs Messages usage.
// Never enable for Anthropic/cloud providers, or infer from token ratios.
export const INCLUSIVE_INPUT_PROFILE = 'lmstudio-0.4.24+1-inclusive-input';

function tokens(value, name) {
  if (!Number.isSafeInteger(value) || value < 0) throw new Error(`Invalid ${name}`);
  return value;
}

export function createUsageNormalizer({ profile, onUsage = () => {} } = {}) {
  if (profile !== INCLUSIVE_INPUT_PROFILE) throw new Error('Explicit verified local usage profile required');
  let rawInput, cacheRead = 0, cacheWrite = 0, messageId;
  return (event) => {
    const start = event?.type === 'message_start';
    const complete = event?.type === 'message';
    if (!start && !complete && event?.type !== 'message_delta') return event;
    if (start || complete) {
      rawInput = undefined; cacheRead = 0; cacheWrite = 0;
      messageId = (start ? event.message : event)?.id;
    }
    const usage = start ? event.message?.usage : event.usage;
    if (!usage || typeof usage !== 'object' || Array.isArray(usage)) return event;
    const raw = { ...usage };
    if (usage.input_tokens !== undefined) rawInput = tokens(usage.input_tokens, 'input_tokens');
    if (usage.cache_read_input_tokens !== undefined) cacheRead = tokens(usage.cache_read_input_tokens, 'cache_read_input_tokens');
    if (usage.cache_creation_input_tokens !== undefined) cacheWrite = tokens(usage.cache_creation_input_tokens, 'cache_creation_input_tokens');
    // This observed dialect has no cache creation. Do not guess new semantics.
    if (cacheWrite !== 0) throw new Error('Unverified local cache-creation semantics');
    const changesInput = 'input_tokens' in usage || 'cache_read_input_tokens' in usage;
    if (!changesInput) return event;
    if (rawInput === undefined || cacheRead > rawInput) throw new Error('Inconsistent inclusive input/cache counts');
    const normalized = { ...usage, input_tokens: rawInput - cacheRead };
    onUsage({ profile, messageId, raw, normalized: { ...normalized }, totalInputTokens: rawInput });
    return start ? { ...event, message: { ...event.message, usage: normalized } } : { ...event, usage: normalized };
  };
}

// Changes usage metadata only. Requests, text, tools, and roles are untouched.
export function createUsageSSETransform(options) {
  const normalize = createUsageNormalizer(options);
  const decoder = new StringDecoder('utf8');
  let pending = '';
  function frame(input) {
    const data = input.split(/\r?\n/).filter(line => line.startsWith('data:'));
    if (!data.length) return input;
    const raw = data.map(line => line.slice(5).replace(/^ /, '')).join('\n');
    let event;
    try { event = JSON.parse(raw); } catch { return input; }
    const next = normalize(event);
    if (next === event) return input;
    const eol = input.includes('\r\n') ? '\r\n' : '\n';
    let emitted = false;
    return input.split(/\r?\n/).flatMap(line => {
      if (!line.startsWith('data:')) return [line];
      if (emitted) return [];
      emitted = true;
      return [`data: ${JSON.stringify(next)}`];
    }).join(eol);
  }
  function drain(stream, final = false) {
    let match;
    while ((match = /\r?\n\r?\n/.exec(pending))) {
      if (match.index > 8 * 1024 * 1024) throw new Error('Oversized SSE frame');
      const end = match.index + match[0].length;
      stream.push(frame(pending.slice(0, match.index)) + match[0]);
      pending = pending.slice(end);
    }
    if (pending.length > 8 * 1024 * 1024) throw new Error('Oversized SSE frame');
    if (final && pending) { stream.push(frame(pending)); pending = ''; }
  }
  return new Transform({
    transform(chunk, encoding, callback) {
      try { pending += decoder.write(chunk); drain(this); callback(); } catch (error) { callback(error); }
    },
    flush(callback) {
      try { pending += decoder.end(); drain(this, true); callback(); } catch (error) { callback(error); }
    },
  });
}
