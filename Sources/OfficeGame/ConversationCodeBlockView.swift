// 이 파일은 Markdown 코드 블록을 에디터형 구문 강조 카드로 표시한다.

import AppKit
import Foundation
import MarkdownUI
import OfficeCore
import SwiftUI

struct ConversationCodeBlockView: View {
    let configuration: CodeBlockConfiguration
    let fontSize: CGFloat

    @State private var didCopy = false

    var body: some View {
        VStack(spacing: 0) {
            header

            Divider()
                .overlay(Color.white.opacity(0.08))

            ScrollView(.horizontal) {
                HStack(alignment: .top, spacing: 12) {
                    Text(lineNumbers)
                        .font(
                            .system(
                                size: fontSize,
                                design: .monospaced
                            )
                        )
                        .foregroundStyle(Color.white.opacity(0.27))
                        .multilineTextAlignment(.trailing)
                        .lineSpacing(3)
                        .accessibilityIdentifier(
                            "conversationCodeBlockLineNumbers"
                        )

                    Rectangle()
                        .fill(Color.white.opacity(0.08))
                        .frame(width: 1)

                    configuration.label
                        .fixedSize(
                            horizontal: false,
                            vertical: true
                        )
                        .relativeLineSpacing(.em(0.225))
                        .markdownTextStyle {
                            FontFamily(.system(.monospaced))
                            FontSize(fontSize)
                            ForegroundColor(
                                ConversationCodePalette.plain
                            )
                        }
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 13)
            }
            .background(ConversationCodePalette.editorBackground)
        }
        .background(ConversationCodePalette.editorBackground)
        .clipShape(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(Color.white.opacity(0.10))
        }
        .markdownMargin(top: 2, bottom: 16)
    }

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: "chevron.left.forwardslash.chevron.right")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(ConversationCodePalette.accent)

            Text(displayLanguage)
                .font(
                    .system(
                        size: 11,
                        weight: .semibold,
                        design: .rounded
                    )
                )
                .foregroundStyle(.secondary)

            Spacer(minLength: 12)

            Button(action: copyCode) {
                Label(
                    didCopy ? OfficeLocalization.string("복사됨") : OfficeLocalization.string("복사"),
                    systemImage: didCopy
                        ? "checkmark"
                        : "doc.on.doc"
                )
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(
                    didCopy
                        ? ConversationCodePalette.success
                        : Color.secondary
                )
                .contentTransition(.symbolEffect(.replace))
            }
            .officeGameTool()
            .help(OfficeLocalization.string("코드 복사"))
            .accessibilityLabel(OfficeLocalization.string("코드 복사"))
            .accessibilityIdentifier("conversationCodeBlockCopyButton")
        }
        .padding(.horizontal, 13)
        .frame(height: 34)
        .officeGameSurface(cornerRadius: 0)
    }

    private var displayLanguage: String {
        ConversationCodeBlockPresentation.displayLanguage(
            for: configuration.language
        )
    }

    private var lineNumbers: String {
        ConversationCodeBlockPresentation.lineNumbers(
            for: configuration.content
        )
    }

    private func copyCode() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(
            configuration.content,
            forType: .string
        )

        withAnimation(.easeOut(duration: 0.16)) {
            didCopy = true
        }

        Task { @MainActor in
            try? await Task.sleep(for: .seconds(1.4))
            withAnimation(.easeOut(duration: 0.16)) {
                didCopy = false
            }
        }
    }
}

enum ConversationCodeBlockPresentation {
    static func displayLanguage(for language: String?) -> String {
        let languageName = language?
            .split(whereSeparator: \.isWhitespace)
            .first
            .map(String.init)
        return (languageName ?? "code").uppercased()
    }

    static func lineNumbers(for content: String) -> String {
        let count = max(
            1,
            content.split(
                separator: "\n",
                omittingEmptySubsequences: false
            ).count
        )
        return (1...count)
            .map(String.init)
            .joined(separator: "\n")
    }
}

struct ConversationCodeSyntaxHighlighter: CodeSyntaxHighlighter {
    func highlightCode(_ code: String, language: String?) -> Text {
        let tokens = ConversationCodeTokenizer.tokens(
            in: code,
            language: language
        )

        return tokens.reduce(Text("")) { result, token in
            result + Text(token.text)
                .foregroundColor(token.kind.color)
        }
    }
}

private enum ConversationCodePalette {
    static let editorBackground = Color(
        red: 0.118,
        green: 0.118,
        blue: 0.118
    )
    static let headerBackground = Color(
        red: 0.145,
        green: 0.145,
        blue: 0.153
    )
    static let plain = Color(
        red: 0.831,
        green: 0.831,
        blue: 0.831
    )
    static let accent = Color(
        red: 0.302,
        green: 0.557,
        blue: 0.973
    )
    static let success = Color(
        red: 0.353,
        green: 0.749,
        blue: 0.471
    )
}

private struct ConversationCodeToken {
    enum Kind {
        case plain
        case comment
        case string
        case annotation
        case keyword
        case number
        case type
        case function

        var color: Color {
            switch self {
            case .plain:
                ConversationCodePalette.plain
            case .comment:
                Color(red: 0.416, green: 0.600, blue: 0.333)
            case .string:
                Color(red: 0.808, green: 0.569, blue: 0.471)
            case .annotation:
                Color(red: 0.863, green: 0.792, blue: 0.459)
            case .keyword:
                Color(red: 0.773, green: 0.525, blue: 0.753)
            case .number:
                Color(red: 0.710, green: 0.808, blue: 0.659)
            case .type:
                Color(red: 0.306, green: 0.788, blue: 0.690)
            case .function:
                Color(red: 0.863, green: 0.792, blue: 0.459)
            }
        }
    }

    let text: String
    let kind: Kind
}

private enum ConversationCodeTokenizer {
    private static let cStyleExpression = makeExpression(
        comments: #"/\*[\s\S]*?\*/|//[^\n]*"#,
        keywords: [
            "abstract", "as", "async", "await", "break", "case",
            "catch", "class", "const", "continue", "default", "defer",
            "do", "else", "enum", "extends", "false", "final",
            "finally", "for", "func", "function", "guard", "if",
            "implements", "import", "in", "instanceof", "interface",
            "let", "new", "nil", "null", "override", "package",
            "private", "protected", "protocol", "public", "repeat",
            "return", "self", "static", "struct", "super", "switch",
            "this", "throw", "throws", "true", "try", "typealias",
            "var", "void", "while", "yield"
        ]
    )

    private static let hashCommentExpression = makeExpression(
        comments: #"#[^\n]*"#,
        keywords: [
            "and", "as", "async", "await", "break", "case", "class",
            "continue", "def", "do", "elif", "else", "except", "false",
            "finally", "for", "from", "global", "if", "import", "in",
            "is", "lambda", "let", "match", "nil", "none", "not",
            "or", "pass", "raise", "return", "then", "true", "try",
            "unless", "until", "while", "with", "yield"
        ]
    )

    private static let sqlExpression = makeExpression(
        comments: #"/\*[\s\S]*?\*/|--[^\n]*"#,
        keywords: [
            "all", "alter", "and", "as", "asc", "begin", "between",
            "by", "case", "create", "delete", "desc", "distinct",
            "drop", "else", "end", "exists", "from", "group", "having",
            "in", "index", "inner", "insert", "into", "is", "join",
            "left", "like", "limit", "not", "null", "offset", "on",
            "or", "order", "outer", "primary", "references", "right",
            "select", "set", "table", "then", "union", "unique",
            "update", "values", "when", "where"
        ]
    )

    static func tokens(
        in source: String,
        language: String?
    ) -> [ConversationCodeToken] {
        let expression = expression(for: language)
        let fullRange = NSRange(source.startIndex..., in: source)
        let matches = expression.matches(
            in: source,
            range: fullRange
        )

        var tokens: [ConversationCodeToken] = []
        var cursor = source.startIndex

        for match in matches {
            guard let matchRange = Range(match.range, in: source) else {
                continue
            }

            if cursor < matchRange.lowerBound {
                tokens.append(
                    .init(
                        text: String(source[cursor..<matchRange.lowerBound]),
                        kind: .plain
                    )
                )
            }

            tokens.append(
                .init(
                    text: String(source[matchRange]),
                    kind: kind(for: match)
                )
            )
            cursor = matchRange.upperBound
        }

        if cursor < source.endIndex {
            tokens.append(
                .init(
                    text: String(source[cursor...]),
                    kind: .plain
                )
            )
        }

        return tokens.isEmpty
            ? [.init(text: source, kind: .plain)]
            : tokens
    }

    private static func expression(
        for language: String?
    ) -> NSRegularExpression {
        let language = language?
            .split(whereSeparator: \.isWhitespace)
            .first?
            .lowercased()

        switch language {
        case "py", "python", "rb", "ruby", "sh", "shell", "bash",
             "zsh", "yaml", "yml", "toml":
            return hashCommentExpression
        case "sql", "postgres", "postgresql":
            return sqlExpression
        default:
            return cStyleExpression
        }
    }

    private static func kind(
        for match: NSTextCheckingResult
    ) -> ConversationCodeToken.Kind {
        let groups: [
            (String, ConversationCodeToken.Kind)
        ] = [
            ("comment", .comment),
            ("string", .string),
            ("annotation", .annotation),
            ("keyword", .keyword),
            ("number", .number),
            ("type", .type),
            ("function", .function)
        ]

        for (name, kind) in groups
        where match.range(withName: name).location != NSNotFound {
            return kind
        }

        return .plain
    }

    private static func makeExpression(
        comments: String,
        keywords: [String]
    ) -> NSRegularExpression {
        let keywordAlternation = keywords
            .map(NSRegularExpression.escapedPattern)
            .joined(separator: "|")

        let pattern = [
            "(?<comment>\(comments))",
            #"(?<string>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')"#,
            #"(?<annotation>@[A-Za-z_][A-Za-z0-9_]*)"#,
            #"(?<keyword>\b(?i:"# + keywordAlternation + #")\b)"#,
            #"(?<number>\b(?:0[xX][0-9A-Fa-f]+|\d+(?:\.\d+)?)\b)"#,
            #"(?<type>\b[A-Z][A-Za-z0-9_]*\b)"#,
            #"(?<function>\b[A-Za-z_][A-Za-z0-9_]*(?=\s*\())"#
        ].joined(separator: "|")

        return try! NSRegularExpression(
            pattern: pattern,
            options: []
        )
    }
}
