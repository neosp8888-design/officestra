// 이 파일은 화이트보드의 반응형 제공자 열 배치를 검증한다.

import XCTest
@testable import OfficeGame

final class UsageBoardLayoutTests: XCTestCase {
    func testResetCountdownCountsLocallyFromFetchedSnapshot() throws {
        let sourceRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appending(path: "Sources/OfficeGame", directoryHint: .isDirectory)
        let source = try String(
            contentsOf: sourceRoot.appending(path: "OfficeDashboardPanels.swift"),
            encoding: .utf8
        )
        let start = try XCTUnwrap(
            source.range(of: "private struct UsageResetCountdown")?.lowerBound
        )
        let end = try XCTUnwrap(
            source.range(
                of: "private struct UsageMeter",
                range: start..<source.endIndex
            )?.lowerBound
        )
        let countdown = String(source[start..<end])

        XCTAssertTrue(countdown.contains("VStack(spacing: 0)"))
        XCTAssertTrue(countdown.contains("relativeTo: fetchedAt"))
        XCTAssertTrue(countdown.contains("Task.sleep(for: .seconds(60))"))
        XCTAssertTrue(countdown.contains("remainingMinutes = current - 1"))
        XCTAssertFalse(countdown.contains("TimelineView"))
    }

    func testUsageCardPresentsEstimatedAPICostAsCompactSummary() throws {
        let sourceRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appending(path: "Sources/OfficeGame", directoryHint: .isDirectory)
        let source = try String(
            contentsOf: sourceRoot.appending(path: "OfficeDashboardPanels.swift"),
            encoding: .utf8
        )
        let start = try XCTUnwrap(
            source.range(of: "private struct UsageActivitySummary")?.lowerBound
        )
        let end = try XCTUnwrap(
            source.range(
                of: "private struct UsageMeter",
                range: start..<source.endIndex
            )?.lowerBound
        )
        let summary = String(source[start..<end])

        XCTAssertTrue(summary.contains("\"API 요금 추정\""))
        XCTAssertTrue(summary.contains("오늘 비용"))
        XCTAssertTrue(summary.contains("30일 비용"))
        XCTAssertTrue(summary.contains("size: 8.5"))
        XCTAssertEqual(
            summary.components(separatedBy: ".fontWeight(.bold)").count - 1,
            2
        )
        XCTAssertFalse(summary.contains("오늘 토큰"))
        XCTAssertFalse(summary.contains("30일 토큰"))
    }

    func testUsageCardPresentsSubscriptionExpirationAsCompactSummary() throws {
        let sourceRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appending(path: "Sources/OfficeGame", directoryHint: .isDirectory)
        let source = try String(
            contentsOf: sourceRoot.appending(path: "OfficeDashboardPanels.swift"),
            encoding: .utf8
        )
        let start = try XCTUnwrap(
            source.range(of: "private struct UsageSubscriptionSummary")?
                .lowerBound
        )
        let end = try XCTUnwrap(
            source.range(
                of: "private struct UsageActivitySummary",
                range: start..<source.endIndex
            )?.lowerBound
        )
        let summary = String(source[start..<end])

        XCTAssertTrue(summary.contains("\"상품 만료\""))
        XCTAssertTrue(summary.contains("calendar.badge.clock"))
        XCTAssertTrue(summary.contains("size: 8.5"))
        XCTAssertTrue(summary.contains("formatter.locale = OfficeLocalization.locale"))
        XCTAssertTrue(summary.contains("setLocalizedDateFormatFromTemplate(\"MdHm\")"))
        XCTAssertFalse(summary.contains("Locale(identifier: \"ko_KR\")"))
        XCTAssertEqual(
            summary.components(separatedBy: ".fontWeight(.bold)").count - 1,
            1
        )
        XCTAssertTrue(
            source.contains(
                "subscriptionExpiresAt: snapshot.codexSubscriptionExpiresAt"
            )
        )
        XCTAssertEqual(
            source.components(
                separatedBy: "subscriptionExpiresAt: snapshot."
            ).count - 1,
            1
        )
        XCTAssertTrue(source.contains("if let subscriptionExpiresAt"))
        let activityPosition = try XCTUnwrap(
            source.range(of: "UsageActivitySummary(")?.lowerBound
        )
        let expirationPosition = try XCTUnwrap(
            source.range(
                of: "if let subscriptionExpiresAt",
                range: activityPosition..<source.endIndex
            )?.lowerBound
        )
        XCTAssertLessThan(activityPosition, expirationPosition)
    }

    func testUsesSingleColumnBelowThreshold() {
        XCTAssertTrue(
            UsageBoardLayout.usesSingleColumn(
                for: UsageBoardLayout.singleColumnThreshold - 1
            )
        )
    }

    func testAntigravityTranscriptUsesWhiteboardBlueAccent() throws {
        let sourceRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appending(path: "Sources/OfficeGame", directoryHint: .isDirectory)
        let dashboard = try String(
            contentsOf: sourceRoot.appending(path: "OfficeDashboardPanels.swift"),
            encoding: .utf8
        )
        let transcript = try String(
            contentsOf: sourceRoot.appending(path: "ClaudeTranscriptView.swift"),
            encoding: .utf8
        )

        XCTAssertTrue(
            dashboard.contains(
                "tint: DashboardPalette.providerAccent(for: .antigravity)"
            )
        )
        XCTAssertTrue(
            dashboard.contains("backend: effectiveBackend")
        )
        XCTAssertTrue(
            dashboard.contains(
                "antigravityAccent = Color(red: 0.19, green: 0.49, blue: 0.88)"
            )
        )
        XCTAssertTrue(
            transcript.contains(
                "DashboardPalette.providerAccent(for: backend)"
            )
        )
    }

    func testAntigravityFiveHourLimitIsVisibleInBothWhiteboards() throws {
        let sourceRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appending(path: "Sources/OfficeGame", directoryHint: .isDirectory)
        let dashboard = try String(
            contentsOf: sourceRoot.appending(path: "OfficeDashboardPanels.swift"),
            encoding: .utf8
        )
        let whiteboard = try String(
            contentsOf: sourceRoot.appending(path: "WhiteboardUsageLayer.swift"),
            encoding: .utf8
        )

        let dashboardStart = try XCTUnwrap(
            dashboard.range(of: "name: \"Antigravity\"")?.lowerBound
        )
        let dashboardEnd = try XCTUnwrap(
            dashboard.range(
                of: "tint: DashboardPalette.providerAccent(for: .antigravity)",
                range: dashboardStart..<dashboard.endIndex
            )?.upperBound
        )
        let dashboardBlock = String(dashboard[dashboardStart..<dashboardEnd])

        let whiteboardStart = try XCTUnwrap(
            whiteboard.range(of: "provider: \"ANTIGRAVITY\"")?.lowerBound
        )
        let whiteboardEnd = try XCTUnwrap(
            whiteboard.range(
                of: "context: &context",
                range: whiteboardStart..<whiteboard.endIndex
            )?.upperBound
        )
        let whiteboardBlock = String(whiteboard[whiteboardStart..<whiteboardEnd])

        XCTAssertTrue(dashboardBlock.contains("showsFiveHour: true"))
        XCTAssertTrue(whiteboardBlock.contains("showsFiveHour: true"))
        XCTAssertTrue(
            whiteboard.contains(
                "provider: \"Antigravity\",\n                window: \"5시간\""
            )
        )
    }

    func testConversationFooterUsesSelectedProviderAccent() throws {
        let sourceRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appending(path: "Sources/OfficeGame", directoryHint: .isDirectory)
        let source = try String(
            contentsOf: sourceRoot.appending(path: "OfficeGameApp.swift"),
            encoding: .utf8
        )
        let compactionStart = try XCTUnwrap(
            source.range(
                of: "private struct ContextCompactionControls"
            )?.lowerBound
        )
        let compactionEnd = try XCTUnwrap(
            source.range(
                of: "private enum ContextCompactionAlert",
                range: compactionStart..<source.endIndex
            )?.lowerBound
        )
        let compaction = String(source[compactionStart..<compactionEnd])
        let profileStart = try XCTUnwrap(
            source.range(of: "OfficeLocalization.string(\"프로필\")")?
                .lowerBound
        )
        let profileEnd = try XCTUnwrap(
            source.range(
                of: ".accessibilityLabel(OfficeLocalization.string(\"직원 프로필\"))",
                range: profileStart..<source.endIndex
            )?.upperBound
        )
        let profile = String(source[profileStart..<profileEnd])

        XCTAssertTrue(
            compaction.contains(
                "DashboardPalette.providerAccent(for: character.backend)"
            )
        )
        XCTAssertTrue(compaction.contains(".tint(accent)"))
        XCTAssertGreaterThanOrEqual(
            compaction.components(separatedBy: ".foregroundStyle(accent)").count
                - 1,
            3
        )
        XCTAssertTrue(
            profile.contains(
                "DashboardPalette.providerAccent(\n                                for: character.backend"
            )
        )
    }

    func testKeepsTwoColumnsAtThreshold() {
        XCTAssertFalse(
            UsageBoardLayout.usesSingleColumn(
                for: UsageBoardLayout.singleColumnThreshold
            )
        )
    }

    func testResetTimeUsesRemainingHoursAndMinutes() throws {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = try XCTUnwrap(TimeZone(identifier: "Asia/Seoul"))
        let now = try XCTUnwrap(
            calendar.date(from: DateComponents(
                year: 2026,
                month: 8,
                day: 7,
                hour: 22
            ))
        )
        let today = try XCTUnwrap(
            calendar.date(from: DateComponents(
                year: 2026,
                month: 8,
                day: 7,
                hour: 23,
                minute: 30
            ))
        )
        let tomorrow = try XCTUnwrap(
            calendar.date(from: DateComponents(
                year: 2026,
                month: 8,
                day: 8,
                hour: 4
            ))
        )
        let later = try XCTUnwrap(
            calendar.date(from: DateComponents(
                year: 2026,
                month: 8,
                day: 13,
                hour: 9
            ))
        )

        XCTAssertEqual(
            usageResetRemainingText(today, relativeTo: now),
            "1시간 30분"
        )
        XCTAssertEqual(
            usageResetRemainingMinutes(today, relativeTo: now),
            90
        )
        XCTAssertEqual(
            usageResetRemainingText(tomorrow, relativeTo: now),
            "6시간 0분"
        )
        XCTAssertEqual(
            usageResetRemainingText(later, relativeTo: now),
            "5일 11시간 0분"
        )
        XCTAssertEqual(
            usageResetRemainingText(
                now.addingTimeInterval(1),
                relativeTo: now
            ),
            "1분"
        )
        XCTAssertNil(
            usageResetRemainingText(
                now.addingTimeInterval(-1),
                relativeTo: now
            )
        )
        XCTAssertEqual(
            usageResetRemainingText(minutes: 89),
            "1시간 29분"
        )
        XCTAssertNil(usageResetRemainingText(minutes: 0))
    }

    func testUsagePaceMarkerTracksRemainingShareOfWindow() throws {
        let sevenDays = 7 * 24 * 60
        let fourDays = 4 * 24 * 60

        XCTAssertEqual(
            try XCTUnwrap(usagePaceRemainingFraction(
                remainingMinutes: fourDays,
                windowMinutes: sevenDays
            )),
            4.0 / 7.0,
            accuracy: 0.000_001
        )
        let expected = try XCTUnwrap(usagePaceRemainingFraction(
            remainingMinutes: fourDays,
            windowMinutes: sevenDays
        ))
        XCTAssertEqual(
            usagePaceStatus(remainingPercent: 57, expectedFraction: expected),
            .comfortable
        )
        XCTAssertEqual(
            usagePaceStatus(remainingPercent: 56, expectedFraction: expected),
            .behind
        )
        XCTAssertNil(
            usagePaceStatus(remainingPercent: nil, expectedFraction: expected)
        )
        XCTAssertNil(
            usagePaceRemainingFraction(
                remainingMinutes: nil,
                windowMinutes: sevenDays
            )
        )
        XCTAssertNil(
            usagePaceRemainingFraction(
                remainingMinutes: 0,
                windowMinutes: sevenDays
            )
        )
    }

    func testUsageSummaryDecodesProviderResetTimestamp() throws {
        let data = Data(
            #"{"codexFiveHour":90,"codexFiveHourResetAt":"2026-08-07T19:00:00.000Z","codexWeekly":null,"codexWeeklyResetAt":null,"claudeFiveHour":null,"claudeFiveHourResetAt":null,"claudeWeekly":null,"claudeWeeklyResetAt":null,"codexPlan":"Pro","claudePlan":null,"codexSubscriptionExpiresAt":"2026-08-31T14:54:17.000Z","codexActivity":null,"claudeActivity":null,"codexLimitError":null,"claudeLimitError":null,"fetchedAt":"2026-08-07T18:00:00.000Z"}"#
                .utf8
        )
        let client = OfficeDatabaseClient(
            baseURL: URL(string: "http://127.0.0.1:4317")!
        )

        let snapshot = try client.decodeUsageSummary(data)

        XCTAssertEqual(snapshot.codexFiveHour, 90)
        XCTAssertEqual(snapshot.codexPlan, "Pro")
        XCTAssertEqual(
            snapshot.codexSubscriptionExpiresAt,
            ISO8601DateFormatter().date(from: "2026-08-31T14:54:17Z")
        )
        XCTAssertEqual(
            snapshot.codexFiveHourResetAt,
            ISO8601DateFormatter().date(from: "2026-08-07T19:00:00Z")
        )
    }
}
