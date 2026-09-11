import AppKit
import SwiftUI
import XCTest
@testable import OfficeGame

final class CharacterEvaluationReportTests: XCTestCase {
    private func sample() -> CharacterTurnCostSummary {
        CharacterTurnCostSummary(characterId: "boss", pricedTurnCount: 8, totalCostUsd: 16,
                                 timedCostUsd: 12, totalDurationSeconds: 720,
                                 likedCount: 8, dislikedCount: 0,
                                 finishedTurnCount: 10, failedTurnCount: 1, interruptedTurnCount: 2)
    }

    @MainActor
    func testReportAndFooterShareExactlyTheSameBreakdown() {
        let store = CharacterTurnCostStore()
        store.apply(CharacterTurnCostSnapshot(version: 1, characters: [sample()]))
        let report = store.averages["boss"]!.report
        XCTAssertEqual(report.satisfactionScore, 25)
        XCTAssertEqual(report.costScore, 10)
        XCTAssertEqual(report.failureScore, 27)
        XCTAssertEqual(report.interruptionScore, 16)
        XCTAssertEqual(report.totalScore, 78)
        XCTAssertEqual(report.totalScore, store.averages["boss"]?.evaluationScore)
        XCTAssertEqual(report.completedCount, 7)
        XCTAssertEqual(report.feedbackCount, 8)
        XCTAssertTrue(report.isProvisional)

        var updated = sample()
        updated.failedTurnCount = 2
        store.apply(CharacterTurnCostSnapshot(version: 2, characters: [updated]))
        XCTAssertEqual(store.averages["boss"]?.report.failureScore, 24)
        XCTAssertEqual(store.averages["boss"]?.report.totalScore, 75)
    }

    func testMissingCountsAreNotReportedAsZeroFailures() {
        var summary = sample()
        summary.finishedTurnCount = nil
        summary.failedTurnCount = nil
        summary.interruptedTurnCount = nil
        let report = CharacterEvaluationReport(summary: summary, medianCost: 1)
        XCTAssertNil(report.completedCount)
        XCTAssertNil(report.failureScore)
        XCTAssertNil(report.interruptionScore)
        XCTAssertNil(report.totalScore)
    }

    func testEveryStaticReportLabelHasEnglishTranslation() throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
        let source = try String(contentsOf: root.appendingPathComponent("Sources/OfficeGame/CharacterEvaluationReport.swift"))
        let regex = try NSRegularExpression(pattern: #"(?:tr|fact|scoreRow)\("([^"]+)""#)
        let ns = source as NSString
        for match in regex.matches(in: source, range: NSRange(location: 0, length: ns.length)) {
            let key = ns.substring(with: match.range(at: 1))
            XCTAssertNotEqual(OfficeLocalization.string(key, languages: ["en"]), key, key)
        }
    }

    @MainActor
    func testReportNativeLayoutAndOptionalPreview() throws {
        _ = NSApplication.shared
        let store = CharacterTurnCostStore()
        store.apply(CharacterTurnCostSnapshot(version: 1, characters: [sample()]))
        let host = NSHostingView(rootView: CharacterEvaluationReportSheet(
            store: store, characterID: "boss", name: "Preview"
        ))
        host.frame = NSRect(x: 0, y: 0, width: 820, height: 700)
        let window = NSWindow(contentRect: host.frame, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = host
        defer { window.contentView = nil }
        host.layoutSubtreeIfNeeded()
        XCTAssertEqual(host.fittingSize.width, 820, accuracy: 1)
        XCTAssertEqual(host.fittingSize.height, 700, accuracy: 1)
        // Opt-in offscreen preview using synthetic data; never capture the user's desktop.
        if let path = ProcessInfo.processInfo.environment["OFFICESTRA_EVALUATION_PREVIEW_PATH"] {
            let bitmap = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
            host.cacheDisplay(in: host.bounds, to: bitmap)
            try XCTUnwrap(bitmap.representation(using: .png, properties: [:])).write(to: URL(fileURLWithPath: path))
        }
    }
}
