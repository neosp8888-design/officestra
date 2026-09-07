// Markdown 본문이 문단과 표 전체를 하나의 선택 범위로 제공하는지 검증한다.

import AppKit
import XCTest
@testable import OfficeGame

@MainActor
final class SelectableMarkdownTextViewTests: XCTestCase {
    nonisolated func testInlineCodeAttributeKeyIsAvailableOutsideMainActor() {
        XCTAssertEqual(
            NSAttributedString.Key.conversationInlineCodeBackground.rawValue,
            "conversationInlineCodeBackground"
        )
    }

    func testInlineCodeAtParagraphEndHasOnlyGlyphWidth() {
        let rendered = SelectableMarkdownAttributedRenderer.render(
            source: "수정: `SelectableMarkdownTextView.swift`, `SelectableMarkdownTextViewTests.swift`",
            fontSize: 14, fallbackDirectory: nil, isDark: false
        )
        let storage = NSTextStorage(attributedString: rendered)
        let manager = ConversationInlineCodeLayoutManager()
        let container = NSTextContainer(size: NSSize(width: 1200, height: 1000))
        storage.addLayoutManager(manager)
        manager.addTextContainer(container)
        manager.ensureLayout(for: container)
        var count = 0
        storage.enumerateAttribute(.conversationInlineCodeBackground, in: NSRange(location: 0, length: storage.length)) { value, range, _ in
            guard value != nil else { return }
            for rect in manager.codeBackgroundRects(for: range, in: container) {
                count += 1
                XCTAssertGreaterThan(rect.width, 100)
                XCTAssertLessThan(rect.width, 400, "Trailing line space must not become code background")
            }
        }
        XCTAssertEqual(count, 2)
    }

    func testInlineCodeUsesRoundedPaintWithoutChangingCopyText() {
        for dark in [false, true] {
            let rendered = SelectableMarkdownAttributedRenderer.render(
                source: "명령 `/model`, `/compact` 입니다.",
                fontSize: 14, fallbackDirectory: nil, isDark: dark
            )
            XCTAssertTrue(rendered.string.contains("명령 /model, /compact 입니다."))
            let range = (rendered.string as NSString).range(of: "/model")
            XCTAssertNotNil(rendered.attribute(.conversationInlineCodeBackground, at: range.location, effectiveRange: nil))
            XCTAssertNil(rendered.attribute(.backgroundColor, at: range.location, effectiveRange: nil))
            XCTAssertNil(rendered.attribute(.conversationInlineCodeBackground, at: 0, effectiveRange: nil))
        }
    }

    func testChangedFileMarkdownListBecomesCompactPresentation() {
        let source = """
        새 통로를 적용했습니다.

        변경 파일:

        - [structured-turn-result.mjs](</Users/neo/office/backend/src/structured-turn-result.mjs:26>)

        - [officestra-result](</Users/neo/office/backend/src/officestra-result:1>)

        커밋·푸시는 요청이 없어 하지 않았습니다.
        """

        let presentation = ConversationChangedFileListPresentation(
            source: source
        )

        XCTAssertEqual(
            presentation.leadingMarkdown,
            "새 통로를 적용했습니다."
        )
        XCTAssertEqual(
            presentation.files,
            [
                ConversationChangedFileReference(
                    id: 0,
                    title: "structured-turn-result.mjs",
                    path: "/Users/neo/office/backend/src/structured-turn-result.mjs"
                ),
                ConversationChangedFileReference(
                    id: 1,
                    title: "officestra-result",
                    path: "/Users/neo/office/backend/src/officestra-result"
                ),
            ]
        )
        XCTAssertEqual(
            presentation.trailingMarkdown,
            "커밋·푸시는 요청이 없어 하지 않았습니다."
        )
    }

    func testChangedFileHeadingWithoutListKeepsOriginalMarkdown() {
        let source = """
        변경 파일:

        현재 변경된 파일은 없습니다.
        """
        let presentation = ConversationChangedFileListPresentation(
            source: source
        )

        XCTAssertEqual(presentation.leadingMarkdown, source)
        XCTAssertTrue(presentation.files.isEmpty)
        XCTAssertTrue(presentation.trailingMarkdown.isEmpty)
    }

    func testEnglishChangedFileListDoesNotTreatWebLinkAsFinderPath() {
        let presentation = ConversationChangedFileListPresentation(
            source: """
            Done.

            ### Changed files
            - [README](README.md)
            - [Release notes](https://example.com/release)
            """
        )

        XCTAssertEqual(presentation.files.count, 2)
        XCTAssertEqual(presentation.files[0].path, "README.md")
        XCTAssertNil(presentation.files[1].path)
    }

    func testInlineChangedFileSummaryBecomesCompactPresentation() {
        let source = """
        깔끔하게 처리했습니다.

        과거 답변에도 적용됩니다.
        수정 파일: ConversationMarkdownView.swift · AgentActivityLogView.swift · ClaudeTranscriptView.swift
        """

        let presentation = ConversationChangedFileListPresentation(
            source: source
        )

        XCTAssertEqual(
            presentation.leadingMarkdown,
            """
            깔끔하게 처리했습니다.

            과거 답변에도 적용됩니다.
            """
        )
        XCTAssertEqual(
            presentation.files,
            [
                ConversationChangedFileReference(
                    id: 0,
                    title: "ConversationMarkdownView.swift",
                    path: "ConversationMarkdownView.swift"
                ),
                ConversationChangedFileReference(
                    id: 1,
                    title: "AgentActivityLogView.swift",
                    path: "AgentActivityLogView.swift"
                ),
                ConversationChangedFileReference(
                    id: 2,
                    title: "ClaudeTranscriptView.swift",
                    path: "ClaudeTranscriptView.swift"
                ),
            ]
        )
        XCTAssertTrue(presentation.trailingMarkdown.isEmpty)
    }

    func testInlineChangedFileSummaryWithNoFilesStaysUnmodified() {
        let source = "수정 파일: 없음"
        let presentation = ConversationChangedFileListPresentation(
            source: source
        )

        XCTAssertEqual(presentation.leadingMarkdown, source)
        XCTAssertTrue(presentation.files.isEmpty)
    }

    func testNaturalKoreanFileReportWithLocalLinksBecomesCompactPresentation() {
        let source = """
        이제 제가 쓴 `수정 파일: A · B · C` 형식도 접힌 카드로 표시됩니다. 과거 답변에도 적용됩니다.

        Swift 테스트 490개 전부 통과했고 앱 재빌드·서명도 완료했습니다. 앱을 완전히 종료 후 다시 열면 확인됩니다. 4317 백엔드는 재시작하지 않았습니다.

        이번 보완 파일은 [ConversationMarkdownView.swift](/Users/neo/office/Sources/OfficeGame/ConversationMarkdownView.swift)와 [SelectableMarkdownTextViewTests.swift](/Users/neo/office/Tests/OfficeCoreTests/SelectableMarkdownTextViewTests.swift)입니다.
        """

        let presentation = ConversationChangedFileListPresentation(
            source: source
        )

        XCTAssertEqual(
            presentation.leadingMarkdown,
            """
            이제 제가 쓴 `수정 파일: A · B · C` 형식도 접힌 카드로 표시됩니다. 과거 답변에도 적용됩니다.

            Swift 테스트 490개 전부 통과했고 앱 재빌드·서명도 완료했습니다. 앱을 완전히 종료 후 다시 열면 확인됩니다. 4317 백엔드는 재시작하지 않았습니다.
            """
        )
        XCTAssertEqual(
            presentation.files,
            [
                ConversationChangedFileReference(
                    id: 0,
                    title: "ConversationMarkdownView.swift",
                    path: "/Users/neo/office/Sources/OfficeGame/ConversationMarkdownView.swift"
                ),
                ConversationChangedFileReference(
                    id: 1,
                    title: "SelectableMarkdownTextViewTests.swift",
                    path: "/Users/neo/office/Tests/OfficeCoreTests/SelectableMarkdownTextViewTests.swift"
                ),
            ]
        )
        XCTAssertTrue(presentation.trailingMarkdown.isEmpty)
    }

    func testOrdinaryLocalFileLinkWithoutReportCueStaysUnmodified() {
        let source = "자세한 내용은 [README](README.md)를 확인하세요."
        let presentation = ConversationChangedFileListPresentation(
            source: source
        )

        XCTAssertEqual(presentation.leadingMarkdown, source)
        XCTAssertTrue(presentation.files.isEmpty)
    }

    func testSelectionCrossesParagraphsAndTableCells() throws {
        let source = """
        첫 문단입니다.

        둘째 문단입니다.

        | 항목 | 결과 |
        | --- | --- |
        | 문단 선택 | 통과 |
        | 표 선택 | 통과 |

        마지막 문단입니다.
        """
        let documentView = SelectableMarkdownDocumentView(fontSize: 12)
        documentView.apply(
            source: source,
            fallbackDirectory: nil,
            isDark: false
        )
        documentView.frame = NSRect(x: 0, y: 0, width: 560, height: 500)
        _ = documentView.heightThatFits(width: 560)
        documentView.layoutSubtreeIfNeeded()

        let rendered = documentView.textView.string as NSString
        let start = rendered.range(of: "첫 문단입니다.").location
        let endRange = rendered.range(of: "마지막 문단입니다.")
        XCTAssertNotEqual(start, NSNotFound)
        XCTAssertNotEqual(endRange.location, NSNotFound)

        let selectedRange = NSRange(
            location: start,
            length: NSMaxRange(endRange) - start
        )
        documentView.textView.setSelectedRange(selectedRange)

        let copied = rendered.substring(with: selectedRange)
        XCTAssertTrue(copied.contains("첫 문단입니다."))
        XCTAssertTrue(copied.contains("둘째 문단입니다."))
        XCTAssertTrue(copied.contains("항목"))
        XCTAssertTrue(copied.contains("문단 선택"))
        XCTAssertTrue(copied.contains("표 선택"))
        XCTAssertTrue(copied.contains("마지막 문단입니다."))
    }

    func testRepeatedHeightRequestsReuseTextKitLayoutUntilInputChanges() {
        let documentView = SelectableMarkdownDocumentView(fontSize: 12)
        documentView.apply(
            source: String(
                repeating: "긴 Markdown 문단입니다. **선택 가능**해야 합니다.\n\n",
                count: 40
            ),
            fallbackDirectory: nil,
            isDark: false
        )

        let firstHeight = documentView.heightThatFits(width: 520)
        let firstMeasurementCount = documentView.textLayoutMeasurementCount
        XCTAssertGreaterThan(firstHeight, 0)
        XCTAssertEqual(firstMeasurementCount, 1)

        documentView.frame = NSRect(
            x: 0,
            y: 0,
            width: 520,
            height: firstHeight
        )
        for _ in 0..<50 {
            XCTAssertEqual(
                documentView.heightThatFits(width: 520),
                firstHeight
            )
            documentView.layout()
        }
        XCTAssertEqual(
            documentView.textLayoutMeasurementCount,
            firstMeasurementCount,
            "같은 내용과 폭의 SwiftUI 재배치는 TextKit 전체 측정을 반복하면 안 됩니다."
        )

        _ = documentView.heightThatFits(width: 360)
        XCTAssertEqual(
            documentView.textLayoutMeasurementCount,
            firstMeasurementCount + 1,
            "폭이 달라지면 새 레이아웃 높이를 한 번 측정해야 합니다."
        )

        documentView.apply(
            source: "교체된 본문\n\n| 항목 | 상태 |\n| --- | --- |\n| 캐시 | 갱신 |",
            fallbackDirectory: nil,
            isDark: false
        )
        let changedSourceMeasurementCount =
            documentView.textLayoutMeasurementCount
        XCTAssertEqual(
            changedSourceMeasurementCount,
            firstMeasurementCount + 2,
            "본문이 달라지면 현재 폭으로 새 높이를 측정해야 합니다."
        )

        _ = documentView.heightThatFits(width: 360)
        XCTAssertEqual(
            documentView.textLayoutMeasurementCount,
            changedSourceMeasurementCount,
            "변경된 본문도 첫 측정 뒤 같은 폭에서는 캐시를 재사용해야 합니다."
        )
    }

    func testTableUsesNativeTextTableBlocks() {
        let rendered = SelectableMarkdownAttributedRenderer.render(
            source: """
            | A | B |
            | --- | --- |
            | 1 | 2 |
            """,
            fontSize: 12,
            fallbackDirectory: nil,
            isDark: false
        )

        var foundTableBlock = false
        rendered.enumerateAttribute(
            .paragraphStyle,
            in: NSRange(location: 0, length: rendered.length)
        ) { value, _, stop in
            guard
                let paragraphStyle = value as? NSParagraphStyle,
                !paragraphStyle.textBlocks.isEmpty
            else {
                return
            }
            foundTableBlock = true
            stop.pointee = true
        }
        XCTAssertTrue(foundTableBlock)
    }

    func testEmptyTableCellsPreserveEveryColumnAndRow() {
        let rendered = SelectableMarkdownAttributedRenderer.render(
            source: """
            | 항목 | | 비용 |
            | --- | ---: | ---: |
            | 일반 입력 | 118,672 | $1.186720 |
            | 합계 | | **$1.210208** |
            | | 가운데 | |
            | | | |
            | 마지막 행 |
            | | | |
            """,
            fontSize: 12,
            fallbackDirectory: nil,
            isDark: false
        )
        var columnsByRow: [Int: Set<Int>] = [:]
        var tables = Set<ObjectIdentifier>()
        rendered.enumerateAttribute(
            .paragraphStyle,
            in: NSRange(location: 0, length: rendered.length)
        ) { value, _, _ in
            guard let style = value as? NSParagraphStyle,
                  let block = style.textBlocks.first as? NSTextTableBlock
            else { return }
            columnsByRow[block.startingRow, default: []].insert(block.startingColumn)
            tables.insert(ObjectIdentifier(block.table))
            XCTAssertEqual(block.table.numberOfColumns, 3)
            XCTAssertEqual(block.columnSpan, 1)
        }
        XCTAssertEqual(columnsByRow.count, 7)
        for row in 0..<7 {
            XCTAssertEqual(columnsByRow[row], Set([0, 1, 2]), "row \(row)")
        }
        XCTAssertEqual(tables.count, 1)
        XCTAssertFalse(rendered.string.contains("\u{2060}"))
        XCTAssertTrue(rendered.string.contains("합계\n\n$1.210208\n"))
    }

    func testCostTableCopiesAcrossEmptyCellWithoutPlaceholder() throws {
        let source = """
        계산 결과입니다.

        | 항목 | 토큰 | 환산 비용 |
        |---|---:|---:|
        | 일반 입력 | 118,672 | $1.186720 |
        | 캐시 입력 | 12,288 | $0.012288 |
        | 출력·추론 포함 | 224 | $0.011200 |
        | 합계 | | **$1.210208** |

        기존 본문과 [링크](https://example.com)는 그대로입니다.
        """
        for isDark in [false, true] {
            let documentView = SelectableMarkdownDocumentView(fontSize: 15)
            documentView.appearance = NSAppearance(named: isDark ? .darkAqua : .aqua)
            documentView.apply(source: source, fallbackDirectory: nil, isDark: isDark)
            let height = documentView.heightThatFits(width: 960)
            documentView.frame = NSRect(x: 0, y: 0, width: 960, height: height)
            documentView.layoutSubtreeIfNeeded()

            let textView = documentView.textView
            textView.setSelectedRange(NSRange(location: 0, length: textView.string.utf16.count))
            let pasteboard = NSPasteboard.withUniqueName()
            defer { pasteboard.releaseGlobally() }
            XCTAssertTrue(textView.writeSelection(to: pasteboard, types: textView.writablePasteboardTypes))
            let copied = try XCTUnwrap(pasteboard.string(forType: .string))
            XCTAssertEqual(copied, textView.string)
            XCTAssertTrue(copied.contains("계산 결과입니다."))
            XCTAssertTrue(copied.contains("합계\n\n$1.210208"))
            XCTAssertTrue(copied.contains("기존 본문과 링크는 그대로입니다."))
            XCTAssertFalse(copied.contains("\u{2060}"))

            // 필요할 때 실제 TextKit 렌더링 결과를 눈으로 확인할 수 있다.
            if let directory = ProcessInfo.processInfo.environment["OFFICESTRA_TABLE_SNAPSHOT_DIRECTORY"] {
                textView.setSelectedRange(NSRange(location: 0, length: 0))
                documentView.wantsLayer = true
                documentView.layer?.backgroundColor = (isDark ? NSColor.black : .white).cgColor
                let bitmap = try XCTUnwrap(documentView.bitmapImageRepForCachingDisplay(in: documentView.bounds))
                documentView.cacheDisplay(in: documentView.bounds, to: bitmap)
                let png = try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
                try png.write(to: URL(fileURLWithPath: directory)
                    .appendingPathComponent(isDark ? "table-dark.png" : "table-light.png"))
            }
        }
    }

    func testEmptyCellPreparationPreservesCodeAndEscapedPipes() {
        let indentedCode = "    | A | B |\n    | --- | --- |\n    | | |"
        XCTAssertEqual(
            SelectableMarkdownSegmenter.preservingEmptyTableCells(
                in: indentedCode, placeholder: "EMPTY"
            ),
            indentedCode
        )
        let source = """
        ```text
        | A | B |
        | --- | --- |
        | | |
        ```

        | A | B | C |
        | --- | --- | --- |
        | left\\|right | | `code` |

        본문의 원래 기호 \u{2060}도 보존합니다.
        """
        let prepared = SelectableMarkdownSegmenter.preservingEmptyTableCells(
            in: source, placeholder: "EMPTY"
        )
        XCTAssertTrue(prepared.contains("| | |\n```"))
        XCTAssertTrue(prepared.contains("| left\\|right | EMPTY | `code` |"))
        let rendered = SelectableMarkdownAttributedRenderer.render(
            source: source, fontSize: 12, fallbackDirectory: nil, isDark: false
        )
        XCTAssertTrue(rendered.string.contains("left|right\n\ncode"))
        XCTAssertTrue(rendered.string.contains("본문의 원래 기호 \u{2060}도 보존합니다."))
    }

    func testListMarkerBeforeLinkIsNotPartOfTheLink() {
        let rendered = SelectableMarkdownAttributedRenderer.render(
            source: "- [공식 스펙](https://example.com/spec)\n- 일반 항목",
            fontSize: 12,
            fallbackDirectory: nil,
            isDark: false
        )

        let text = rendered.string as NSString
        let markerRange = text.range(of: "•\t")
        XCTAssertNotEqual(markerRange.location, NSNotFound)
        let markerAttributes = rendered.attributes(
            at: markerRange.location,
            effectiveRange: nil
        )
        XCTAssertNil(markerAttributes[.link])
        XCTAssertNil(markerAttributes[.underlineStyle])

        let linkRange = text.range(of: "공식 스펙")
        let linkAttributes = rendered.attributes(
            at: linkRange.location,
            effectiveRange: nil
        )
        XCTAssertNotNil(linkAttributes[.link])
        XCTAssertEqual(
            linkAttributes[.underlineStyle] as? Int,
            NSUnderlineStyle.single.rawValue
        )
    }

    func testNestedListItemsKeepTheirOwnMarkers() {
        // 구성 요소는 안쪽부터 바깥쪽 순서다. 바깥 "3." 항목을 잡으면 안쪽
        // 항목이 이미 글머리표를 붙인 항목으로 보여 글머리표가 빠진다.
        let rendered = SelectableMarkdownAttributedRenderer.render(
            source: "3. 바깥 항목\n   - 안쪽 첫째\n   - 안쪽 둘째",
            fontSize: 12,
            fallbackDirectory: nil,
            isDark: false
        )

        XCTAssertEqual(
            rendered.string,
            "3.\t바깥 항목\n•\t안쪽 첫째\n•\t안쪽 둘째\n"
        )
    }

    @MainActor
    func testTextViewKeepsLayoutWidthAfterSourceChangesWithoutLayoutPass() {
        // 본문이 바뀌어도 layout()이 다시 돌기 전에 NSTextView가 스스로 그 순간의
        // 사용 폭으로 프레임을 줄이면 가장 긴 줄의 끝이 잘린 채 남는다.
        let view = SelectableMarkdownDocumentView(fontSize: 14, minimumLayoutWidth: nil)
        view.apply(source: "- 짧은 항목", fallbackDirectory: nil, isDark: true)
        let width: CGFloat = 900
        view.frame = NSRect(x: 0, y: 0, width: width, height: view.heightThatFits(width: width))
        view.layout()
        view.apply(
            source: "- 앱 내 우측 메인 대화 피드에서 스크롤을 올려 상단 10건 추가 로딩 동작 확인",
            fallbackDirectory: nil,
            isDark: true
        )

        let textView = view.subviews.compactMap { $0 as? NSTextView }.first
        XCTAssertEqual(textView?.frame.width, width)
    }

    func testSingleNewlinesRemainVisibleAndSelectable() {
        let rendered = SelectableMarkdownAttributedRenderer.render(
            source: "첫째 줄\n둘째 줄\n셋째 줄",
            fontSize: 12,
            fallbackDirectory: nil,
            isDark: false
        )

        XCTAssertEqual(rendered.string, "첫째 줄\n둘째 줄\n셋째 줄\n")
    }

    func testNarrowTableKeepsReadableWidthForHorizontalScrolling() {
        let documentView = SelectableMarkdownDocumentView(
            fontSize: 12,
            minimumLayoutWidth: 330
        )
        documentView.apply(
            source: """
            | 긴 항목 이름 | 현재 상태 | 비고 |
            | --- | --- | --- |
            | 문단 선택 | 통과 | 연속 범위 |
            """,
            fallbackDirectory: nil,
            isDark: false
        )
        let width = CGFloat(170)
        let height = documentView.heightThatFits(width: width)
        documentView.frame = NSRect(
            x: 0,
            y: 0,
            width: width,
            height: height
        )
        documentView.layout()

        XCTAssertGreaterThan(
            documentView.textView.frame.width,
            width,
            "좁은 보관함에서도 표 열이 한 글자 폭으로 찌그러지면 안 됩니다."
        )
    }

    func testSegmenterKeepsParagraphsAndTablesInOneSelectionDocument() {
        let segments = SelectableMarkdownSegmenter.split(
            """
            첫 문단입니다.

            둘째 문단입니다.

            | 항목 | 상태 | 비고 |
            | --- | --- | --- |
            | 문단 | 통과 | 연속 선택 |

            마지막 문단입니다.
            """
        )

        XCTAssertEqual(segments.count, 1)
        XCTAssertEqual(segments[0].kind, .table(columnCount: 3))
        XCTAssertEqual(segments[0].minimumLayoutWidth, 330)
        XCTAssertTrue(segments[0].source.contains("첫 문단"))
        XCTAssertTrue(segments[0].source.contains("둘째 문단"))
        XCTAssertTrue(segments[0].source.contains("| 항목 | 상태 | 비고 |"))
        XCTAssertTrue(segments[0].source.contains("마지막 문단"))
    }

    func testSegmenterExtractsFencedCodeBlockFromSelectableProse() {
        let segments = SelectableMarkdownSegmenter.split(
            """
            첫 문단입니다.

            둘째 문단도 같은 선택 문서입니다.

            ```swift
            let answer = 42
            print(answer)
            ```

            | 항목 | 상태 | 비고 |
            | --- | --- | --- |
            | 카드 | 연결 | 완료 |

            마지막 문단입니다.
            """
        )

        XCTAssertEqual(segments.count, 3)
        XCTAssertEqual(segments[0].kind, .prose)
        XCTAssertTrue(segments[0].source.contains("첫 문단"))
        XCTAssertTrue(segments[0].source.contains("둘째 문단"))
        XCTAssertEqual(segments[1].kind, .codeBlock)
        XCTAssertEqual(
            segments[1].source,
            """
            ```swift
            let answer = 42
            print(answer)
            ```
            """
        )
        XCTAssertEqual(segments[2].kind, .table(columnCount: 3))
        XCTAssertTrue(segments[2].source.contains("| 항목 | 상태 | 비고 |"))
        XCTAssertTrue(segments[2].source.contains("마지막 문단"))
    }

    func testSegmenterTreatsUnclosedFenceAsCodeBlockThroughEnd() {
        let segments = SelectableMarkdownSegmenter.split(
            """
            설명입니다.

            ~~~json
            {"enabled": true}
            """
        )

        XCTAssertEqual(segments.count, 2)
        XCTAssertEqual(segments[0].kind, .prose)
        XCTAssertEqual(segments[1].kind, .codeBlock)
        XCTAssertEqual(
            segments[1].source,
            """
            ~~~json
            {"enabled": true}
            """
        )
    }

    func testCodeBlockPresentationProvidesLanguageAndLineNumbers() {
        XCTAssertEqual(
            ConversationCodeBlockPresentation.displayLanguage(
                for: "swift linenums"
            ),
            "SWIFT"
        )
        XCTAssertEqual(
            ConversationCodeBlockPresentation.displayLanguage(for: nil),
            "CODE"
        )
        XCTAssertEqual(
            ConversationCodeBlockPresentation.lineNumbers(
                for: "let answer = 42\nprint(answer)\nreturn"
            ),
            "1\n2\n3"
        )
    }

    func testTableViewportDoesNotAddNestedScrollView() {
        let documentView = SelectableMarkdownDocumentView(
            fontSize: 12,
            minimumLayoutWidth: 440
        )
        documentView.apply(
            source: "문단\n\n| A | B | C | D |\n| --- | --- | --- | --- |\n| 1 | 2 | 3 | 4 |",
            fallbackDirectory: nil,
            isDark: false
        )
        let height = documentView.heightThatFits(width: 220)
        documentView.frame = NSRect(x: 0, y: 0, width: 220, height: height)
        documentView.layoutSubtreeIfNeeded()

        func descendants(of view: NSView) -> [NSView] {
            view.subviews.flatMap { [$0] + descendants(of: $0) }
        }

        XCTAssertFalse(
            descendants(of: documentView).contains { $0 is NSScrollView },
            "표의 가로 이동 영역이 직원별 세로 대화 스크롤과 중첩되면 안 됩니다."
        )
    }

    func testRemoteImagesAreRemovedBeforeHTMLImport() {
        let rendered = SelectableMarkdownAttributedRenderer.render(
            source: "앞 ![원격 이미지](https://example.com/tracker.png) 뒤",
            fontSize: 12,
            fallbackDirectory: nil,
            isDark: false
        )

        XCTAssertFalse(rendered.string.contains("tracker.png"))
        XCTAssertTrue(rendered.string.contains("원격 이미지"))
        var foundAttachment = false
        rendered.enumerateAttribute(
            .attachment,
            in: NSRange(location: 0, length: rendered.length)
        ) { value, _, stop in
            guard value != nil else {
                return
            }
            foundAttachment = true
            stop.pointee = true
        }
        XCTAssertFalse(foundAttachment)
    }

    func testLocalMarkdownLinkClickOpensResolvedFile() throws {
        let directory = FileManager.default.temporaryDirectory
            .appending(path: UUID().uuidString, directoryHint: .isDirectory)
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        defer {
            try? FileManager.default.removeItem(at: directory)
        }
        let fileURL = directory.appending(path: "episode 03.png")
        try Data([0]).write(to: fileURL)
        let documentView = SelectableMarkdownDocumentView(fontSize: 12)
        documentView.apply(
            source: "[결과 파일](<\(fileURL.path)>)",
            fallbackDirectory: nil,
            isDark: false
        )

        var openedURL: URL?
        documentView.linkOpener = { url in
            openedURL = url
            return true
        }
        let link = try XCTUnwrap(
            documentView.textView.textStorage?.attribute(
                .link,
                at: 0,
                effectiveRange: nil
            )
        )

        XCTAssertTrue(
            documentView.textView(
                documentView.textView,
                clickedOnLink: link,
                at: 0
            )
        )
        XCTAssertEqual(openedURL, fileURL.standardizedFileURL)
    }

    func testWebMarkdownLinkUsesExternalOpener() throws {
        let documentView = SelectableMarkdownDocumentView(fontSize: 12)
        documentView.apply(
            source: "[웹](https://example.com/result)",
            fallbackDirectory: nil,
            isDark: false
        )

        var openedURL: URL?
        documentView.linkOpener = { url in
            openedURL = url
            return true
        }
        let link = try XCTUnwrap(
            documentView.textView.textStorage?.attribute(
                .link,
                at: 0,
                effectiveRange: nil
            )
        )

        XCTAssertTrue(
            documentView.textView(
                documentView.textView,
                clickedOnLink: link,
                at: 0
            )
        )
        XCTAssertEqual(openedURL?.absoluteString, "https://example.com/result")
    }
}
