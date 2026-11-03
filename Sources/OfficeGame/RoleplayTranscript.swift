// 롤플레이 답변을 서술·대사·속마음 조각으로 나누고 화자별 색을 정한다.
import SwiftUI

enum RoleplayTranscript {
    enum Segment: Equatable {
        case narration(String)
        /// `speaker`가 없으면 이름 없이 따옴표만 있는 대사다.
        case dialogue(speaker: Speaker?, text: String)
        case thought(speaker: Speaker?, text: String)

        var speaker: Speaker? {
            switch self {
            case .narration: nil
            case .dialogue(let speaker, _), .thought(let speaker, _): speaker
            }
        }
    }

    struct Speaker: Equatable {
        /// 소악마 "Gender_Emoji Name:" 형식의 앞 이모지. 없을 수 있다.
        let emoji: String?
        let name: String
    }

    // "헬레나: "…"" 또는 "헬레나: '…'" 줄. 앞 이모지는 따로 떼어 낸다. 이름에는 따옴표·콜론·문장부호가 들어가지 않는다.
    private static let cuePattern = #/^(?<name>[^:"“”'‘’.,!?…\n]{1,20}?)\s*:\s*(?<rest>["“'‘].*)$/#

    /// 이모지 한 글자(합자 포함)로 시작하면 그 글자와 나머지를 돌려준다. 숫자·#처럼 글자로도 쓰이는 것은 제외한다.
    private static func splitLeadingEmoji(_ line: String) -> (emoji: String?, rest: Substring) {
        guard let first = line.first, let scalar = first.unicodeScalars.first,
              scalar.properties.isEmojiPresentation
                || (scalar.properties.isEmoji && first.unicodeScalars.contains("\u{FE0F}"))
        else { return (nil, Substring(line)) }
        return (String(first), line.dropFirst().drop(while: \.isWhitespace))
    }

    /// 이름표가 하나도 없는 답변이면 이름 없는 대사·속마음에 `fallbackSpeaker`를 붙인다.
    /// 이름표가 하나라도 있으면 모델이 일부러 뺀 줄로 보고 그대로 둔다.
    static func segments(_ text: String, fallbackSpeaker: Speaker?) -> [Segment] {
        let parsed = segments(text)
        guard let fallbackSpeaker, !parsed.contains(where: { $0.speaker != nil }) else { return parsed }
        return parsed.map { segment in
            switch segment {
            case .dialogue(nil, let line): .dialogue(speaker: fallbackSpeaker, text: line)
            case .thought(nil, let line): .thought(speaker: fallbackSpeaker, text: line)
            default: segment
            }
        }
    }

    /// 답변마다 그 앞 답변들에서 이름표로 가장 많이 나온 화자를 돌려준다. 같은 횟수면 먼저 나온 화자,
    /// 앞에 이름표가 하나도 없으면 `defaultName`이다.
    static func fallbackSpeakers(for texts: [String], defaultName: String) -> [Speaker] {
        var counts: [String: (count: Int, order: Int, emoji: String?)] = [:]
        var result: [Speaker] = []
        for text in texts {
            let top = counts.max { lhs, rhs in
                lhs.value.count != rhs.value.count ? lhs.value.count < rhs.value.count : lhs.value.order > rhs.value.order
            }
            result.append(top.map { Speaker(emoji: $0.value.emoji, name: $0.key) } ?? Speaker(emoji: nil, name: defaultName))
            for case let speaker? in segments(text).map(\.speaker) {
                let current = counts[speaker.name] ?? (0, counts.count, speaker.emoji)
                counts[speaker.name] = (current.count + 1, current.order, current.emoji ?? speaker.emoji)
            }
        }
        return result
    }

    static func segments(_ text: String) -> [Segment] {
        var segments: [Segment] = []
        var narration: [String] = []
        func flushNarration() {
            let joined = narration.joined(separator: "\n").trimmingCharacters(in: .newlines)
            if !joined.trimmingCharacters(in: .whitespaces).isEmpty { segments.append(.narration(joined)) }
            narration.removeAll()
        }
        for line in text.components(separatedBy: "\n") {
            if let segment = quotedLine(line.trimmingCharacters(in: .whitespaces)) {
                flushNarration()
                segments.append(segment)
            } else {
                narration.append(line)
            }
        }
        flushNarration()
        return segments
    }

    private static func quotedLine(_ line: String) -> Segment? {
        var speaker: Speaker?
        var rest = line
        let (emoji, body) = splitLeadingEmoji(line)
        if let match = body.wholeMatch(of: cuePattern) {
            let name = String(match.output.name).trimmingCharacters(in: .whitespaces)
            guard !name.isEmpty else { return nil }
            speaker = Speaker(emoji: emoji, name: name)
            rest = String(match.output.rest)
        }
        guard let first = rest.first, let last = rest.last, rest.count >= 2 else { return nil }
        if "\"“".contains(first), "\"”".contains(last) { return .dialogue(speaker: speaker, text: rest) }
        if "'‘".contains(first), "'’".contains(last) { return .thought(speaker: speaker, text: rest) }
        return nil
    }

    /// 같은 이름은 언제나 같은 색. Swift `hashValue`는 실행마다 달라 쓰지 않는다.
    static func colorIndex(for name: String, count: Int) -> Int {
        let sum = name.unicodeScalars.reduce(0) { ($0 &* 31 &+ Int($1.value)) & 0x7fff_ffff }
        return sum % count
    }

    /// 사용자 보라·원격 자홍·직원 연두와 겹치지 않는 화자색. 라이트에서는 글자 대비를 위해 짙게 쓴다.
    private static let speakerColors: [(light: Color, dark: Color)] = [
        (Color(red: 0.72, green: 0.16, blue: 0.36), Color(red: 1.00, green: 0.55, blue: 0.68)),
        (Color(red: 0.55, green: 0.40, blue: 0.00), Color(red: 0.98, green: 0.80, blue: 0.35)),
        (Color(red: 0.05, green: 0.45, blue: 0.40), Color(red: 0.45, green: 0.90, blue: 0.80)),
        (Color(red: 0.14, green: 0.33, blue: 0.70), Color(red: 0.55, green: 0.72, blue: 1.00)),
        (Color(red: 0.62, green: 0.28, blue: 0.10), Color(red: 1.00, green: 0.66, blue: 0.48)),
        (Color(red: 0.40, green: 0.36, blue: 0.62), Color(red: 0.76, green: 0.72, blue: 1.00)),
    ]

    static func speakerColor(for name: String?, isDark: Bool) -> Color {
        guard let name else { return isDark ? Color(white: 0.78) : Color(white: 0.38) }
        let pair = speakerColors[colorIndex(for: name, count: speakerColors.count)]
        return isDark ? pair.dark : pair.light
    }
}
