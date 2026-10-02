import AppKit
import SwiftTerm
import XCTest

final class TerminalWideCharacterTests: XCTestCase {
    private final class Delegate: TerminalDelegate {
        func send(source: Terminal, data: ArraySlice<UInt8>) {}
    }

    private let delegate = Delegate()

    private func terminal(_ text: String, cols: Int = 40) -> Terminal {
        let terminal = Terminal(delegate: delegate)
        terminal.resize(cols: cols, rows: 8)
        terminal.feed(text: text)
        return terminal
    }

    private func assertBlank(
        _ cell: CharData, background: Attribute.Color = .defaultColor,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        XCTAssertTrue(cell.getCharacter() == " " || cell.getCharacter() == "\0", file: file, line: line)
        XCTAssertEqual(cell.width, 1, file: file, line: line)
        XCTAssertEqual(cell.attribute.bg, background, file: file, line: line)
        XCTAssertEqual(cell.attribute.style, .none, file: file, line: line)
        XCTAssertFalse(cell.hasPayload, file: file, line: line)
    }

    func testWideContinuationInheritsGlyphAttributes() {
        for prefix in ["", "\u{1b}[1;4;7;31;44m", "\u{1b}[48;2;20;40;60m"] {
            let row = terminal(prefix + "한글🙂").getLine(row: 0)!
            for column in stride(from: 0, to: 6, by: 2) {
                XCTAssertEqual(row[column].width, 2)
                XCTAssertEqual(row[column + 1].width, 0)
                XCTAssertEqual(row[column + 1].attribute, row[column].attribute)
            }
        }
    }

    func testSparseASCIIOverwriteRemovesWhiteBlocks() {
        let row = terminal("가나다라\rA\u{1b}[3GC\u{1b}[5GE\u{1b}[7GG").getLine(row: 0)!
        for column in [1, 3, 5, 7] { assertBlank(row[column]) }
        for (column, expected) in zip([0, 2, 4, 6], "ACEG") {
            XCTAssertEqual(row[column].getCharacter(), expected)
        }
    }

    func testASCIIOverwriteOfRightHalfClearsLeftHalf() {
        let row = terminal("가나다\u{1b}[2GXY").getLine(row: 0)!
        assertBlank(row[0])
        XCTAssertEqual(row[1].getCharacter(), "X")
        XCTAssertEqual(row[2].getCharacter(), "Y")
        assertBlank(row[3])
        XCTAssertEqual(row[4].getCharacter(), "다")
        XCTAssertEqual(row[5].width, 0)
    }

    func testNonASCIIWidthOneOverwriteClearsEitherHalf() {
        let row = terminal("가나\ré\u{1b}[4Gé").getLine(row: 0)!
        XCTAssertEqual(row[0].getCharacter(), "é")
        assertBlank(row[1])
        assertBlank(row[2])
        XCTAssertEqual(row[3].getCharacter(), "é")
    }

    func testOffsetWideOverwriteClearsBothOutsideFragments() {
        let row = terminal("가나다\u{1b}[2G한").getLine(row: 0)!
        assertBlank(row[0])
        XCTAssertEqual(row[1].getCharacter(), "한")
        XCTAssertEqual(row[2].width, 0)
        assertBlank(row[3])
        XCTAssertEqual(row[4].getCharacter(), "다")
    }

    func testErasedFragmentsUseCurrentBackgroundAndDropOldLinkAndStyle() {
        let t = terminal("\u{1b}]8;;https://example.com\u{1b}\\\u{1b}[1;7;41m가\u{1b}]8;;\u{1b}\\\u{1b}[0;44m\rA")
        let row = t.getLine(row: 0)!
        assertBlank(row[1], background: row[0].attribute.bg)
        XCTAssertNotEqual(row[0].attribute.bg, .defaultColor)
    }

    func testEndOfRowAndWrappingPreserveAdjacentText() {
        let t = terminal("AB가\u{1b}[3GX", cols: 4)
        assertBlank(t.getLine(row: 0)![3])
        t.feed(text: "YZ")
        XCTAssertEqual(t.getLine(row: 0)![3].getCharacter(), "Y")
        XCTAssertEqual(t.getLine(row: 1)![0].getCharacter(), "Z")
        t.feed(text: "\u{1b}[1;4H한")
        XCTAssertEqual(t.getLine(row: 1)![0].getCharacter(), "한")
        XCTAssertEqual(t.getLine(row: 1)![1].width, 0)
    }

    func testInsertModePreservesWholeGlyphAndClearsSplitGlyph() {
        let t = terminal("가나\r\u{1b}[4hX", cols: 8)
        XCTAssertEqual(t.getLine(row: 0)![1].getCharacter(), "가")
        XCTAssertEqual(t.getLine(row: 0)![2].width, 0)
        t.feed(text: "\u{1b}[3GY")
        assertBlank(t.getLine(row: 0)![1])
        XCTAssertEqual(t.getLine(row: 0)![2].getCharacter(), "Y")
        assertBlank(t.getLine(row: 0)![3])
        XCTAssertEqual(t.getLine(row: 0)![4].getCharacter(), "나")
    }

    func testChunkedUTF8AndEscapeSequencesMatchSingleFeed() {
        let stream = "가나다라\rA\u{1b}[3GC\u{1b}[5GE\u{1b}[7GG"
        let t = terminal("")
        for byte in stream.utf8 { t.feed(byteArray: [byte]) }
        let expected = terminal(stream).getLine(row: 0)!
        for column in 0..<8 {
            let actual = t.getLine(row: 0)![column]
            XCTAssertEqual(actual.getCharacter(), expected[column].getCharacter())
            XCTAssertEqual(actual.width, expected[column].width)
            XCTAssertEqual(actual.attribute, expected[column].attribute)
        }
    }

    @MainActor
    func testSparseOverwriteRendersLikeCleanTextInLightAndDarkTerminals() throws {
        _ = NSApplication.shared
        for (foreground, background) in [(NSColor.white, NSColor.black), (.black, .white)] {
            func render(_ text: String) throws -> Data {
                let view = TerminalView(frame: NSRect(x: 0, y: 0, width: 600, height: 160))
                view.font = NSFont.monospacedSystemFont(ofSize: 18, weight: .regular)
                view.nativeForegroundColor = foreground
                view.nativeBackgroundColor = background
                view.feed(text: "\u{1b}[?25l" + text)
                let bitmap = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: view.bounds))
                view.cacheDisplay(in: view.bounds, to: bitmap)
                return try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
            }
            XCTAssertEqual(
                try render("가나다라\rA\u{1b}[3GC\u{1b}[5GE\u{1b}[7GG"),
                try render("A C E G ")
            )
        }
    }
}
