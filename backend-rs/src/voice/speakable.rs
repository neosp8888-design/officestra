// 스트리밍 중인 Markdown 응답에서 완성된 문장만 골라 읽을 수 있는 글로 바꾼다. Node voice/speakable.mjs를 옮기며 정규식은 JS 의미 그대로 쓴다.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::js::{js_length, js_trim};
use crate::roleplay::js_regex::JsRegExp;

const MIN_SENTENCE_CHARS: usize = 6;

/// 빈 줄(문단 끝) 표시. paragraph_breaks를 켜면 문장 사이에 끼워 내보낸다.
pub const PARAGRAPH_BREAK: &str = "\n";
pub const DIRECTION_PAUSE_MS: u64 = 700;

thread_local! {
    static REGEXES: RefCell<HashMap<(&'static str, &'static str), JsRegExp>> = RefCell::new(HashMap::new());
}

/// 고정 정규식을 한 번만 만들어 쓴다. 넘긴 함수 안에서 다시 부르지 않는다.
fn with_regex<R>(pattern: &'static str, flags: &'static str, run: impl FnOnce(&mut JsRegExp) -> R) -> R {
    REGEXES.with(|regexes| {
        let mut regexes = regexes.borrow_mut();
        let regex = regexes
            .entry((pattern, flags))
            .or_insert_with(|| JsRegExp::new(pattern, flags).expect("고정 정규식"));
        run(regex)
    })
}

fn replace(text: &str, pattern: &'static str, flags: &'static str, replacement: &str) -> String {
    with_regex(pattern, flags, |regex| regex.replace(text, replacement))
}

fn test(text: &str, pattern: &'static str) -> bool {
    with_regex(pattern, "", |regex| regex.test(text))
}

/// 경로는 마지막 파일 이름만 읽는다. "그리고/또는" 같은 짧은 표현은 두고, 슬래시가 둘 이상이거나 확장자가 있으면 경로로 본다.
fn path_name(path: &str) -> String {
    let segments: Vec<&str> = path.strip_suffix('/').unwrap_or(path).split('/').collect();
    let last = segments.last().copied().unwrap_or_default();
    // /\.[A-Za-z0-9]{1,8}$/
    let extension = last
        .rsplit_once('.')
        .is_some_and(|(_, extension)| (1..=8).contains(&extension.len()) && extension.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    if segments.len() > 2 || extension { last.to_string() } else { path.to_string() }
}

/// Markdown 한 조각을 소리 내어 읽을 글로 정리한다. 줄 머리 기호는 줄 시작일 때만 뗀다.
pub fn speakable_text(fragment: &str, at_line_start: bool) -> String {
    let mut text = fragment.to_string();
    if at_line_start {
        text = replace(&text, r"^\s{0,3}#{1,6}\s+", "", "");
        text = replace(&text, r"^\s*>\s?", "", "");
        text = replace(&text, r"^\s*(?:[-*+]|\d+[.)])\s+(?:\[[ xX]\]\s+)?", "", "");
    }
    text = replace(&text, r"!\[([^\]]*)\]\([^)]*\)", "g", "$1");
    text = replace(&text, r"\[([^\]]+)\]\([^)]*\)", "g", "$1");
    text = replace(&text, r"https?:\/\/\S+", "g", "링크");
    text = replace(&text, r"`([^`]*)`", "g", "$1");
    text = with_regex(r"[~\w.@-]*(?:\/[\w.@-]+)+\/?", "g", |regex| regex.replace_with(&text, |found| path_name(found.matched())));
    text = replace(&text, r"(\*\*|__|~~)(.+?)\1", "g", "$2");
    text = replace(&text, r"(^|\s)[*_](\S(?:.*?\S)?)[*_](?=\s|$|[.,!?])", "g", "$1$2");
    text = replace(&text, r"[\u{1F300}-\u{1FAFF}\u{2600}-\u{27BF}\u{FE0F}]", "gu", "");
    text = replace(&text, r"\s+", "g", " ");
    js_trim(&text).to_string()
}

/// 앞에 있을수록 먼저 고른다. 감정을 속삭임보다 앞에 두어 "(울먹이며 작게)"는 슬픔으로 읽는다.
/// voices.json에 기준 음성이 없는 말투(playful 등)는 기본 목소리로 읽는다.
const DIRECTION_STYLES: [(&str, &str); 7] = [
    ("sad", "슬프|슬픔|슬퍼|울먹|울며|눈물|훌쩍|흐느|서글|침울|우울|쓸쓸"),
    ("pain", "아파|아픈|아픔|고통|신음|끙끙|괴로|통증|찡그"),
    ("angry", "화가|화난|화내|분노|버럭|짜증|언성|격분|노려|이를 악"),
    ("jealous", "질투|샘나|시샘|삐진|삐쳐|토라|서운|뾰로통|새침|못마땅"),
    ("excited", "신나|신난|흥분|들뜬|들떠|환호|설레|기뻐|기쁜|벅차|감격"),
    ("whisper", "속삭|귓속|귓가|나지막|작은 목소리|목소리를 낮|조용히|몰래|은밀"),
    ("playful", "장난|놀리|짓궂|킥킥|키득|깔깔|웃으며|웃음|애교|윙크|능청"),
];
const DIRECTION_PAUSE: &str = "침묵|한숨|머뭇|망설|뜸을|숨을 고르|잠시";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directions {
    pub text: String,
    pub style: Option<&'static str>,
    pub pause_ms: u64,
}

/// 문장 속 괄호(연기 지침)를 읽지 않게 떼어 내고, 지침에서 말투와 앞쪽 쉼을 뽑는다.
/// 문장이 괄호 안에서 끊겨 한쪽 괄호만 남아도 그 부분을 지침으로 본다.
pub fn parse_directions(sentence: &str) -> Directions {
    let mut notes: Vec<String> = Vec::new();
    let mut take = |pattern: &'static str, flags: &'static str, text: &str| {
        with_regex(pattern, flags, |regex| {
            regex.replace_with(text, |found| {
                notes.push(found.captures.get(1).cloned().flatten().unwrap_or_default());
                " ".to_string()
            })
        })
    };
    let text = take(r"[(（]([^()（）]*)[)）]", "g", sentence);
    let text = take(r"[(（]([^()（）]*)$", "", &text);
    let text = take(r"^([^()（）]*)[)）]", "", &text);
    let text = replace(&text, r"\s+([.,!?…])", "g", "$1");
    let text = replace(&text, r"\s+", "g", " ");
    let joined = notes.join(" ");
    let style = DIRECTION_STYLES.iter().find(|(_, pattern)| test(&joined, *pattern)).map(|(style, _)| *style);
    Directions {
        text: js_trim(&text).to_string(),
        style,
        pause_ms: if test(&joined, DIRECTION_PAUSE) { DIRECTION_PAUSE_MS } else { 0 },
    }
}

fn utf16_slice(units: &[u16], start: usize, end: usize) -> String {
    String::from_utf16_lossy(&units[start.min(units.len())..end.min(units.len())])
}

/// 한 줄 안의 문장을 끊는다. 너무 짧은 조각은 다음 문장에 붙인다.
pub fn split_sentences(line: &str) -> Vec<String> {
    let units: Vec<u16> = line.encode_utf16().collect();
    let matches = with_regex(r"[.!?。…](?=\s)|[.!?。…]$", "g", |regex| regex.match_all(line)).unwrap_or_default();
    let mut parts = Vec::new();
    let mut start = 0;
    for found in matches {
        let end = found.index + js_length(found.matched());
        parts.push(utf16_slice(&units, start, end));
        start = end;
    }
    if start < units.len() {
        parts.push(utf16_slice(&units, start, units.len()));
    }
    let mut merged: Vec<String> = Vec::new();
    for part in parts.iter().map(|part| js_trim(part)).filter(|part| !part.is_empty()) {
        match merged.last_mut() {
            Some(last) if js_length(last) < MIN_SENTENCE_CHARS => {
                last.push(' ');
                last.push_str(part);
            }
            _ => merged.push(part.to_string()),
        }
    }
    merged
}

/// 누적 응답 원문을 계속 받아 새로 완성된 문장만 돌려준다.
/// 코드 블록·표·수평선은 읽지 않고, 줄이 끝나지 않은 부분은 문장 부호 뒤까지만 내보낸다.
#[derive(Debug, Default)]
pub struct SentenceStream {
    paragraph_breaks: bool,
    /// 지금까지 받은 원문(UTF-16 단위). Node 문자열 길이·자르기와 같게 센다.
    source: Vec<u16>,
    consumed: usize,
    at_line_start: bool,
    in_fence: bool,
    skip_line: bool,
}

impl SentenceStream {
    pub fn new(paragraph_breaks: bool) -> Self {
        Self { paragraph_breaks, at_line_start: true, ..Self::default() }
    }

    fn emit_from(raw: &str, line_start: bool, out: &mut Vec<String>) {
        let cleaned = speakable_text(raw, line_start);
        if !cleaned.is_empty() {
            out.extend(split_sentences(&cleaned));
        }
    }

    fn consume_line(&mut self, raw: &str, out: &mut Vec<String>) {
        let trimmed = js_trim(raw);
        if self.at_line_start && test(trimmed, r"^(```|~~~)") {
            self.in_fence = !self.in_fence;
        } else if self.paragraph_breaks && !self.in_fence && self.at_line_start && trimmed.is_empty() {
            out.push(PARAGRAPH_BREAK.to_string());
        } else if !self.in_fence
            && !self.skip_line
            && !(self.at_line_start && (trimmed.starts_with('|') || test(trimmed, r"^([-*_])\1{2,}$")))
        {
            Self::emit_from(raw, self.at_line_start, out);
        }
        self.at_line_start = true;
        self.skip_line = false;
    }

    fn drain(&mut self, last: bool) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let rest = self.source[self.consumed.min(self.source.len())..].to_vec();
            if let Some(newline) = rest.iter().position(|unit| *unit == u16::from(b'\n')) {
                self.consume_line(&String::from_utf16_lossy(&rest[..newline]), &mut out);
                self.consumed += newline + 1;
                continue;
            }
            if rest.is_empty() {
                break;
            }
            let text = String::from_utf16_lossy(&rest);
            if last {
                self.consume_line(&text, &mut out);
                self.consumed = self.source.len();
                break;
            }
            // 줄 머리가 코드 펜스·표가 될지 아직 모르면 줄이 끝날 때까지 기다린다.
            if self.in_fence
                || self.skip_line
                || (self.at_line_start && test(&text, r"^\s*[`|~]"))
                || (self.at_line_start && js_length(js_trim(&text)) < 3)
            {
                break;
            }
            let cut = with_regex(r"[.!?。…](?=\s)", "g", |regex| regex.match_all(&text))
                .unwrap_or_default()
                .last()
                .map(|found| found.index + 1);
            let Some(cut) = cut else { break };
            Self::emit_from(&utf16_slice(&rest, 0, cut), self.at_line_start, &mut out);
            self.consumed += cut;
            self.at_line_start = false;
        }
        out
    }

    /// 지금까지의 전체 응답 원문을 넣는다. 앞부분이 바뀌면 처음부터 다시 읽지 않고 이어지는 부분만 본다.
    pub fn push(&mut self, full_text: &str) -> Vec<String> {
        let units: Vec<u16> = full_text.encode_utf16().collect();
        if !units.starts_with(&self.source) {
            // 응답이 다시 쓰였으면 이미 읽은 길이까지는 건너뛴다.
            self.consumed = self.consumed.min(units.len());
        }
        self.source = units;
        self.drain(false)
    }

    /// 응답이 끝났을 때 남은 조각을 모두 내보낸다.
    pub fn finish(&mut self) -> Vec<String> {
        self.drain(true)
    }
}
