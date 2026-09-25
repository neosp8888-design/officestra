// 스트리밍 중인 Markdown 응답에서 완성된 문장만 골라 읽을 수 있는 글로 바꾼다.

const TERMINATOR = /[.!?。…](?=\s)|[.!?。…]$/g;
const MIN_SENTENCE_CHARS = 6;

/** Markdown 한 조각을 소리 내어 읽을 글로 정리한다. 줄 머리 기호는 줄 시작일 때만 뗀다. */
export function speakableText(fragment, atLineStart = true) {
  let text = fragment;
  if (atLineStart) {
    text = text
      .replace(/^\s{0,3}#{1,6}\s+/, '')
      .replace(/^\s*>\s?/, '')
      .replace(/^\s*(?:[-*+]|\d+[.)])\s+(?:\[[ xX]\]\s+)?/, '');
  }
  return text
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/\[([^\]]+)\]\([^)]*\)/g, '$1')
    .replace(/https?:\/\/\S+/g, '링크')
    .replace(/`([^`]*)`/g, '$1')
    // 경로는 마지막 파일 이름만 읽는다.
    .replace(/[~\w.@-]*(?:\/[\w.@-]+)+\/?/g, path => {
      const segments = path.replace(/\/$/, '').split('/');
      const last = segments.at(-1);
      // "그리고/또는" 같은 짧은 표현은 두고, 슬래시가 둘 이상이거나 확장자가 있으면 경로로 본다.
      return segments.length > 2 || /\.[A-Za-z0-9]{1,8}$/.test(last) ? last : path;
    })
    .replace(/(\*\*|__|~~)(.+?)\1/g, '$2')
    .replace(/(^|\s)[*_](\S(?:.*?\S)?)[*_](?=\s|$|[.,!?])/g, '$1$2')
    .replace(/[\u{1F300}-\u{1FAFF}\u{2600}-\u{27BF}\u{FE0F}]/gu, '')
    .replace(/\s+/g, ' ')
    .trim();
}

// 앞에 있을수록 먼저 고른다. 감정을 속삭임보다 앞에 두어 "(울먹이며 작게)"는 슬픔으로 읽는다.
// voices.json에 기준 음성이 없는 말투(playful 등)는 기본 목소리로 읽는다.
const DIRECTION_STYLES = [
  ['sad', /슬프|슬픔|슬퍼|울먹|울며|눈물|훌쩍|흐느|서글|침울|우울|쓸쓸/],
  ['pain', /아파|아픈|아픔|고통|신음|끙끙|괴로|통증|찡그/],
  ['angry', /화가|화난|화내|분노|버럭|짜증|언성|격분|노려|이를 악/],
  ['jealous', /질투|샘나|시샘|삐진|삐쳐|토라|서운|뾰로통|새침|못마땅/],
  ['excited', /신나|신난|흥분|들뜬|들떠|환호|설레|기뻐|기쁜|벅차|감격/],
  ['whisper', /속삭|귓속|귓가|나지막|작은 목소리|목소리를 낮|조용히|몰래|은밀/],
  ['playful', /장난|놀리|짓궂|킥킥|키득|깔깔|웃으며|웃음|애교|윙크|능청/],
];
const DIRECTION_PAUSE = /침묵|한숨|머뭇|망설|뜸을|숨을 고르|잠시/;
export const DIRECTION_PAUSE_MS = 700;

/**
 * 문장 속 괄호(연기 지침)를 읽지 않게 떼어 내고, 지침에서 말투와 앞쪽 쉼을 뽑는다.
 * 문장이 괄호 안에서 끊겨 한쪽 괄호만 남아도 그 부분을 지침으로 본다.
 */
export function parseDirections(sentence) {
  const notes = [];
  const take = (_, note) => { notes.push(note); return ' '; };
  const text = sentence
    .replace(/[(（]([^()（）]*)[)）]/g, take)
    .replace(/[(（]([^()（）]*)$/, take)
    .replace(/^([^()（）]*)[)）]/, take)
    .replace(/\s+([.,!?…])/g, '$1')
    .replace(/\s+/g, ' ')
    .trim();
  const joined = notes.join(' ');
  const style = DIRECTION_STYLES.find(([, pattern]) => pattern.test(joined))?.[0] ?? null;
  return { text, style, pauseMs: DIRECTION_PAUSE.test(joined) ? DIRECTION_PAUSE_MS : 0 };
}

/** 한 줄 안의 문장을 끊는다. 너무 짧은 조각은 다음 문장에 붙인다. */
export function splitSentences(line) {
  const parts = [];
  let start = 0;
  for (const match of line.matchAll(TERMINATOR)) {
    const end = match.index + match[0].length;
    parts.push(line.slice(start, end));
    start = end;
  }
  if (start < line.length) parts.push(line.slice(start));
  const merged = [];
  for (const part of parts.map(p => p.trim()).filter(Boolean)) {
    if (merged.length && merged.at(-1).length < MIN_SENTENCE_CHARS) merged[merged.length - 1] += ` ${part}`;
    else merged.push(part);
  }
  return merged;
}

/** 빈 줄(문단 끝) 표시. paragraphBreaks를 켜면 문장 사이에 끼워 내보낸다. */
export const PARAGRAPH_BREAK = '\n';

/**
 * 누적 응답 원문을 계속 받아 새로 완성된 문장만 돌려준다.
 * 코드 블록·표·수평선은 읽지 않고, 줄이 끝나지 않은 부분은 문장 부호 뒤까지만 내보낸다.
 */
export function createSentenceStream({ paragraphBreaks = false } = {}) {
  let source = '';
  let consumed = 0;
  let atLineStart = true;
  let inFence = false;
  let skipLine = false;

  function emitFrom(raw, lineStart, out) {
    const cleaned = speakableText(raw, lineStart);
    if (cleaned) out.push(...splitSentences(cleaned));
  }

  function consumeLine(raw, out) {
    const trimmed = raw.trim();
    if (atLineStart && /^(```|~~~)/.test(trimmed)) {
      inFence = !inFence;
    } else if (paragraphBreaks && !inFence && atLineStart && !trimmed) {
      out.push(PARAGRAPH_BREAK);
    } else if (!inFence && !skipLine && !(atLineStart && (/^\|/.test(trimmed) || /^([-*_])\1{2,}$/.test(trimmed)))) {
      emitFrom(raw, atLineStart, out);
    }
    atLineStart = true;
    skipLine = false;
  }

  function drain(final) {
    const out = [];
    for (;;) {
      const rest = source.slice(consumed);
      const newline = rest.indexOf('\n');
      if (newline >= 0) {
        consumeLine(rest.slice(0, newline), out);
        consumed += newline + 1;
        continue;
      }
      if (!rest) break;
      if (final) {
        consumeLine(rest, out);
        consumed = source.length;
        break;
      }
      // 줄 머리가 코드 펜스·표가 될지 아직 모르면 줄이 끝날 때까지 기다린다.
      if (inFence || skipLine || (atLineStart && /^\s*[`|~]/.test(rest)) || (atLineStart && rest.trim().length < 3)) break;
      let cut = -1;
      for (const match of rest.matchAll(/[.!?。…](?=\s)/g)) cut = match.index + 1;
      if (cut < 0) break;
      emitFrom(rest.slice(0, cut), atLineStart, out);
      consumed += cut;
      atLineStart = false;
    }
    return out;
  }

  return {
    /** 지금까지의 전체 응답 원문을 넣는다. 앞부분이 바뀌면 처음부터 다시 읽지 않고 이어지는 부분만 본다. */
    push(fullText) {
      if (!fullText.startsWith(source)) {
        // 응답이 다시 쓰였으면 이미 읽은 길이까지는 건너뛴다.
        consumed = Math.min(consumed, fullText.length);
      }
      source = fullText;
      return drain(false);
    },
    /** 응답이 끝났을 때 남은 조각을 모두 내보낸다. */
    finish() {
      return drain(true);
    },
  };
}
