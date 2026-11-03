// JS RegExp(exec·test·matchAll·replace)와 String.prototype.replace 규칙. 리스 정규식 스크립트·CBS가 Node와 같은 결과를 내도록 UTF-16 단위로 다룬다.

use regress::{Flags, Regex};

/// RegExpBuiltinExec 결과. captures[0]은 전체 일치이고, 참여하지 않은 그룹은 None(undefined)이다.
#[derive(Debug, Clone, PartialEq)]
pub struct JsMatch {
    /// 일치 시작(UTF-16 단위).
    pub index: usize,
    pub captures: Vec<Option<String>>,
    /// 이름 그룹이 하나도 없으면 None(JS groups가 undefined)이다.
    pub groups: Option<Vec<(String, Option<String>)>>,
}

impl JsMatch {
    pub fn matched(&self) -> &str {
        self.captures[0].as_deref().unwrap_or("")
    }

    /// `matched.groups?.[name]`. 그룹이 없거나 참여하지 않았으면 None이다.
    pub fn group(&self, name: &str) -> Option<&str> {
        self.groups.as_ref()?.iter().find(|(group, _)| group == name)?.1.as_deref()
    }
}

/// 상태(lastIndex)를 지니는 JS 정규식 객체.
pub struct JsRegExp {
    regex: Regex,
    global: bool,
    sticky: bool,
    full_unicode: bool,
    pub last_index: usize,
}

/// 내부 일치. 범위는 UTF-16 단위다.
struct RawMatch {
    start: usize,
    end: usize,
    captures: Vec<Option<(usize, usize)>>,
    names: Vec<(String, Option<(usize, usize)>)>,
}

impl JsRegExp {
    /// `new RegExp(pattern, flags)`. 모르는 플래그·겹친 플래그·u와 v 함께 쓰기는 SyntaxError다.
    pub fn new(pattern: &str, flags: &str) -> Result<Self, String> {
        let invalid = || format!("Invalid flags supplied to RegExp constructor '{flags}'");
        let mut seen = String::new();
        for flag in flags.chars() {
            if !"dgimsuvy".contains(flag) || seen.contains(flag) {
                return Err(invalid());
            }
            seen.push(flag);
        }
        let unicode = seen.contains('u');
        let unicode_sets = seen.contains('v');
        if unicode && unicode_sets {
            return Err(invalid());
        }
        let options = Flags {
            icase: seen.contains('i'),
            multiline: seen.contains('m'),
            dot_all: seen.contains('s'),
            // v는 u의 의미를 포함한다.
            unicode: unicode || unicode_sets,
            unicode_sets,
            ..Flags::default()
        };
        // u·v가 없으면 JS처럼 패턴도 UTF-16 단위로 읽는다(보충 평면 글자는 대리 문자 둘).
        let parsed = if unicode || unicode_sets {
            Regex::from_unicode(pattern.chars().map(u32::from), options)
        } else {
            Regex::from_unicode(pattern.encode_utf16().map(u32::from), options)
        };
        let regex = parsed.map_err(|error| format!("Invalid regular expression: /{pattern}/{flags}: {error}"))?;
        Ok(Self { regex, global: seen.contains('g'), sticky: seen.contains('y'), full_unicode: unicode || unicode_sets, last_index: 0 })
    }

    pub fn global(&self) -> bool {
        self.global
    }

    fn search(&self, text: &[u16], start: usize) -> Option<RawMatch> {
        let found = if self.full_unicode {
            self.regex.find_from_utf16(text, start).next()
        } else {
            self.regex.find_from_ucs2(text, start).next()
        }?;
        let names: Vec<(String, Option<(usize, usize)>)> = found
            .named_groups()
            .map(|(name, range)| (name.to_string(), range.map(|range| (range.start, range.end))))
            .collect();
        Some(RawMatch {
            start: found.range.start,
            end: found.range.end,
            captures: found.captures.iter().map(|range| range.as_ref().map(|range| (range.start, range.end))).collect(),
            names,
        })
    }

    /// RegExpBuiltinExec를 lastIndex 값으로 돌린다. (결과, 새 lastIndex)다.
    fn exec_at(&self, text: &[u16], last_index: usize) -> (Option<RawMatch>, usize) {
        let tracks = self.global || self.sticky;
        let start = if tracks { last_index } else { 0 };
        if start > text.len() {
            return (None, if tracks { 0 } else { last_index });
        }
        let found = self.search(text, start).filter(|found| !self.sticky || found.start == start);
        match found {
            None => (None, if tracks { 0 } else { last_index }),
            Some(found) => {
                let next = if tracks { found.end } else { last_index };
                (Some(found), next)
            }
        }
    }

    fn exec_raw(&mut self, text: &[u16]) -> Option<RawMatch> {
        let (found, next) = self.exec_at(text, self.last_index);
        self.last_index = next;
        found
    }

    /// `regex.exec(text)`.
    pub fn exec(&mut self, text: &str) -> Option<JsMatch> {
        let units: Vec<u16> = text.encode_utf16().collect();
        self.exec_raw(&units).map(|found| owned(&units, &found))
    }

    /// `regex.test(text)`.
    pub fn test(&mut self, text: &str) -> bool {
        let units: Vec<u16> = text.encode_utf16().collect();
        self.exec_raw(&units).is_some()
    }

    /// `[...text.matchAll(regex)]`. g가 없으면 TypeError다. 원래 객체의 lastIndex는 바꾸지 않는다.
    pub fn match_all(&self, text: &str) -> Result<Vec<JsMatch>, String> {
        if !self.global {
            return Err("String.prototype.matchAll called with a non-global RegExp argument".into());
        }
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut last_index = self.last_index;
        let mut matches = Vec::new();
        loop {
            let (found, next) = self.exec_at(&units, last_index);
            last_index = next;
            let Some(found) = found else { break };
            if found.start == found.end {
                last_index = advance_string_index(&units, last_index, self.full_unicode);
            }
            matches.push(owned(&units, &found));
        }
        Ok(matches)
    }

    /// `text.replace(regex, replacement)`. 치환 문자열의 `$&`·`$1`·`$<name>`·`$$`를 JS 규칙대로 푼다.
    pub fn replace(&mut self, text: &str, replacement: &str) -> String {
        let template: Vec<u16> = replacement.encode_utf16().collect();
        self.replace_units(text, |units, found| {
            let captures: Vec<Option<&[u16]>> =
                found.captures.iter().map(|span| span.map(|(start, end)| &units[start..end])).collect();
            let names: Vec<(&str, Option<&[u16]>)> = found
                .names
                .iter()
                .map(|(name, span)| (name.as_str(), span.map(|(start, end)| &units[start..end])))
                .collect();
            let groups = (!names.is_empty()).then_some(names.as_slice());
            get_substitution(&units[found.start..found.end], units, found.start, &captures, groups, &template)
        })
    }

    /// `text.replace(regex, (matched, ...) => ...)`. 함수 결과를 그대로 넣는다.
    pub fn replace_with(&mut self, text: &str, mut replacer: impl FnMut(&JsMatch) -> String) -> String {
        self.replace_units(text, |units, found| replacer(&owned(units, found)).encode_utf16().collect())
    }

    /// RegExp.prototype[Symbol.replace]의 일치 모으기와 이어 붙이기.
    fn replace_units(&mut self, text: &str, mut replacement: impl FnMut(&[u16], &RawMatch) -> Vec<u16>) -> String {
        let units: Vec<u16> = text.encode_utf16().collect();
        if self.global {
            self.last_index = 0;
        }
        let mut results = Vec::new();
        while let Some(found) = self.exec_raw(&units) {
            let empty = found.start == found.end;
            results.push(found);
            if !self.global {
                break;
            }
            if empty {
                self.last_index = advance_string_index(&units, self.last_index, self.full_unicode);
            }
        }
        let mut output = Vec::with_capacity(units.len());
        let mut next = 0;
        for found in &results {
            let position = found.start.min(units.len());
            let replaced = replacement(&units, found);
            if position >= next {
                output.extend_from_slice(&units[next..position]);
                output.extend_from_slice(&replaced);
                next = position + (found.end - found.start);
            }
        }
        if next < units.len() {
            output.extend_from_slice(&units[next..]);
        }
        String::from_utf16_lossy(&output)
    }
}

fn owned(units: &[u16], found: &RawMatch) -> JsMatch {
    let text = |span: Option<(usize, usize)>| span.map(|(start, end)| String::from_utf16_lossy(&units[start..end]));
    let mut captures = vec![Some(String::from_utf16_lossy(&units[found.start..found.end]))];
    captures.extend(found.captures.iter().map(|span| text(*span)));
    let groups = (!found.names.is_empty())
        .then(|| found.names.iter().map(|(name, span)| (name.clone(), text(*span))).collect());
    JsMatch { index: found.start, captures, groups }
}

/// AdvanceStringIndex. 유니코드 모드면 대리 문자 쌍을 한 번에 넘는다.
fn advance_string_index(units: &[u16], index: usize, unicode: bool) -> usize {
    if !unicode || index + 1 >= units.len() {
        return index + 1;
    }
    let high = (0xD800..0xDC00).contains(&units[index]);
    let low = (0xDC00..0xE000).contains(&units[index + 1]);
    if high && low { index + 2 } else { index + 1 }
}

/// GetSubstitution. captures는 1번 그룹부터, groups는 이름 그룹(없으면 None)이다.
fn get_substitution(
    matched: &[u16],
    text: &[u16],
    position: usize,
    captures: &[Option<&[u16]>],
    groups: Option<&[(&str, Option<&[u16]>)]>,
    template: &[u16],
) -> Vec<u16> {
    const DOLLAR: u16 = b'$' as u16;
    let digit = |unit: u16| (b'0' as u16..=b'9' as u16).contains(&unit).then(|| usize::from(unit - b'0' as u16));
    let mut output = Vec::with_capacity(template.len());
    let mut index = 0;
    while index < template.len() {
        let unit = template[index];
        if unit != DOLLAR || index + 1 >= template.len() {
            output.push(unit);
            index += 1;
            continue;
        }
        let next = template[index + 1];
        if next == DOLLAR {
            output.push(DOLLAR);
            index += 2;
        } else if next == b'&' as u16 {
            output.extend_from_slice(matched);
            index += 2;
        } else if next == b'`' as u16 {
            output.extend_from_slice(&text[..position.min(text.len())]);
            index += 2;
        } else if next == b'\'' as u16 {
            let tail = (position + matched.len()).min(text.len());
            output.extend_from_slice(&text[tail..]);
            index += 2;
        } else if let Some(first) = digit(next) {
            let second = template.get(index + 2).copied().and_then(digit);
            let (mut number, mut width) = match second {
                Some(second) => (first * 10 + second, 2),
                None => (first, 1),
            };
            if width == 2 && number > captures.len() {
                number = first;
                width = 1;
            }
            if (1..=captures.len()).contains(&number) {
                if let Some(capture) = captures[number - 1] {
                    output.extend_from_slice(capture);
                }
            } else {
                output.extend_from_slice(&template[index..index + 1 + width]);
            }
            index += 1 + width;
        } else if next == b'<' as u16 {
            let close = template[index + 2..].iter().position(|unit| *unit == b'>' as u16);
            match (groups, close) {
                (Some(groups), Some(close)) => {
                    let name = String::from_utf16_lossy(&template[index + 2..index + 2 + close]);
                    if let Some((_, Some(capture))) = groups.iter().find(|(group, _)| *group == name) {
                        output.extend_from_slice(capture);
                    }
                    index += 3 + close;
                }
                _ => {
                    output.extend_from_slice(&template[index..index + 2]);
                    index += 2;
                }
            }
        } else {
            output.push(DOLLAR);
            index += 1;
        }
    }
    output
}

/// StringIndexOf. 빈 검색어는 from 위치에서 찾은 것으로 본다.
fn index_of(text: &[u16], search: &[u16], from: usize) -> Option<usize> {
    if search.is_empty() {
        return (from <= text.len()).then_some(from);
    }
    if search.len() > text.len() {
        return None;
    }
    (from..=text.len() - search.len()).find(|start| &text[*start..*start + search.len()] == search)
}

/// `text.replace(search, replacement)`(문자열 검색). 첫 일치만 바꾸고 `$&` 같은 치환 기호를 JS 규칙대로 푼다.
pub fn replace_first(text: &str, search: &str, replacement: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let search: Vec<u16> = search.encode_utf16().collect();
    let Some(position) = index_of(&units, &search, 0) else {
        return text.to_string();
    };
    let template: Vec<u16> = replacement.encode_utf16().collect();
    let mut output = units[..position].to_vec();
    output.extend(get_substitution(&search, &units, position, &[], None, &template));
    output.extend_from_slice(&units[position + search.len()..]);
    String::from_utf16_lossy(&output)
}

/// `text.replaceAll(search, replacement)`(문자열 검색). 빈 검색어는 UTF-16 단위 사이마다 넣는다.
pub fn replace_all(text: &str, search: &str, replacement: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let search: Vec<u16> = search.encode_utf16().collect();
    let template: Vec<u16> = replacement.encode_utf16().collect();
    let advance = search.len().max(1);
    let mut positions = Vec::new();
    let mut from = index_of(&units, &search, 0);
    while let Some(position) = from {
        positions.push(position);
        from = index_of(&units, &search, position + advance);
    }
    if positions.is_empty() {
        return text.to_string();
    }
    let mut output = Vec::with_capacity(units.len());
    let mut end_of_last = 0;
    for position in positions {
        output.extend_from_slice(&units[end_of_last..position]);
        output.extend(get_substitution(&search, &units, position, &[], None, &template));
        end_of_last = position + search.len();
    }
    if end_of_last < units.len() {
        output.extend_from_slice(&units[end_of_last..]);
    }
    String::from_utf16_lossy(&output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replaced(pattern: &str, flags: &str, text: &str, replacement: &str) -> String {
        JsRegExp::new(pattern, flags).unwrap().replace(text, replacement)
    }

    // 기대값은 Node 24의 같은 식 결과다.
    #[test]
    fn 정규식_치환은_js_치환_기호와_빈_일치_규칙을_따른다() {
        assert_eq!(replaced("(\\d+)", "g", "a1 a22", "[$1]"), "a[1] a[22]");
        assert_eq!(replaced("(\\d+)", "", "a1 a22", "[$1]"), "a[1] a22");
        assert_eq!(replaced("b", "g", "abc", "[$`|$'|$&|$$|$1|$<x>]"), "a[a|c|b|$|$1|$<x>]c");
        assert_eq!(replaced("(?<x>b)(c)?", "g", "abd", "[$<x>|$<y>|$2|$10|$01|$0]"), "a[b|||b0|b|$0]d");
        assert_eq!(replaced("x*", "g", "abc", "-"), "-a-b-c-");
        assert_eq!(replaced("", "gu", "😀a", "-"), "-😀-a-");
        assert_eq!(replaced("(?<=a)b", "g", "ab cb", "X"), "aX cb");
        assert_eq!(replaced("^b", "gm", "a\nb\nb", "X"), "a\nX\nX");
        assert_eq!(replaced(".+", "g", "a\u{2028}b", "X"), "X\u{2028}X");
        assert_eq!(replaced("A", "gi", "aA", "x"), "xx");
        assert_eq!(replaced("a", "y", "ba", "x"), "ba");
        assert_eq!(replaced("a", "gy", "aab", "x"), "xxb");
    }

    #[test]
    fn exec와_matchall은_lastindex를_js처럼_다룬다() {
        let mut sticky = JsRegExp::new("a", "y").unwrap();
        assert!(sticky.exec("ab").is_some());
        assert_eq!(sticky.last_index, 1);
        assert!(sticky.exec("ab").is_none());
        assert_eq!(sticky.last_index, 0);
        let all = JsRegExp::new("(a)|(b)", "g").unwrap().match_all("ab").unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].captures, vec![Some("b".into()), None, Some("b".into())]);
        assert!(JsRegExp::new("a", "").unwrap().match_all("a").is_err());
        let named = JsRegExp::new("(?<word>\\w+)", "").unwrap().exec("hi").unwrap();
        assert_eq!(named.group("word"), Some("hi"));
        assert!(JsRegExp::new("a", "uv").is_err());
        assert!(JsRegExp::new("(", "g").is_err());
    }

    #[test]
    fn 비유니코드_모드는_utf16_단위로_찾는다() {
        assert_eq!(replaced(".", "g", "😀", "x"), "xx");
        assert_eq!(replaced(".", "gu", "😀", "x"), "x");
        assert_eq!(replaced("😀", "g", "a😀", "x"), "ax");
    }

    #[test]
    fn 문자열_치환도_치환_기호를_푼다() {
        assert_eq!(replace_first("<{{slot}}>", "{{slot}}", "$&·$$·$`"), "<{{slot}}·$·<>");
        assert_eq!(replace_first("abc", "z", "$&"), "abc");
        assert_eq!(replace_all("a-b-c", "-", "+"), "a+b+c");
        assert_eq!(replace_all("abc", "", "-"), "-a-b-c-");
        assert_eq!(replace_all("aaa", "aa", "b"), "ba");
        assert_eq!(replace_all("x1", "1", "$'$`"), "xx");
    }
}
