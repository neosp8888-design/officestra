// 이 파일은 화이트보드 사용 현황 상세의 API 계약과 직원·모델별 평가율 집계를 검증한다.

import Foundation
import XCTest
@testable import OfficeGame

final class UsageReportTests: XCTestCase {
    private let client = OfficeDatabaseClient(
        baseURL: URL(string: "http://127.0.0.1:4317")!
    )

    func testReportRequestCarriesBackendGranularityAndTimeZone() throws {
        let url = client.usageReportURL(
            backend: .claude,
            granularity: .month,
            timeZone: TimeZone(identifier: "Asia/Seoul")!
        )
        let components = try XCTUnwrap(
            URLComponents(url: url, resolvingAgainstBaseURL: false)
        )

        XCTAssertEqual(components.path, "/api/usage-report")
        XCTAssertEqual(
            Dictionary(
                uniqueKeysWithValues: components.queryItems?.map {
                    ($0.name, $0.value ?? "")
                } ?? []
            ),
            ["backend": "claude", "granularity": "month", "tz": "Asia/Seoul"]
        )
    }

    func testCharacterModelRatesPutMostRatedFirstAndKeepUnratedDistinct() {
        let rows = [
            row(period: "2026-09-05", character: "left-woman", model: "claude-opus-5", turns: 10, liked: 3, disliked: 1),
            row(period: "2026-09-06", character: "left-woman", model: "claude-opus-5", turns: 5, liked: 1, disliked: 1),
            row(period: "2026-09-06", character: "left-man", model: "claude-sonnet-5", turns: 40, liked: 0, disliked: 0),
            row(period: "2026-09-06", character: "left-man", model: "claude-opus-5", turns: 2, liked: 0, disliked: 2),
        ]

        let summaries = UsageReportAggregation.byCharacterAndModel(rows)

        XCTAssertEqual(
            summaries.map { "\($0.primary)/\($0.secondary)" },
            [
                "left-woman/claude-opus-5",
                "left-man/claude-opus-5",
                "left-man/claude-sonnet-5",
            ]
        )
        XCTAssertEqual(summaries[0].turns, 15)
        XCTAssertEqual(summaries[0].liked, 4)
        XCTAssertEqual(summaries[0].disliked, 2)
        XCTAssertEqual(summaries[0].likeRate.map { ($0 * 100).rounded() }, 67)
        XCTAssertEqual(summaries[1].likeRate, 0)
        XCTAssertEqual(summaries[1].dislikeRate, 1)
        // 평가가 한 건도 없으면 0%가 아니라 비율 없음이다.
        XCTAssertNil(summaries[2].likeRate)
        XCTAssertEqual(UsageReportAggregation.percentText(summaries[2].likeRate), "–")
        XCTAssertEqual(UsageReportAggregation.percentText(summaries[0].likeRate), "67%")
    }

    func testPeriodsStayChronologicalAndTotalsSumEverything() {
        let rows = [
            row(period: "2026-09-06", character: "boss", model: "gpt-5.6-sol", turns: 1, liked: 1, disliked: 0, cost: 2),
            row(period: "2026-09-04", character: "boss", model: "gpt-5.6-sol", turns: 3, liked: 0, disliked: 1, cost: 1.5),
            row(period: "2026-09-04", character: "left-man", model: "gpt-5.6-terra", turns: 2, liked: 1, disliked: 0, cost: 0.5),
        ]

        let periods = UsageReportAggregation.byPeriod(rows)
        XCTAssertEqual(periods.map(\.primary), ["2026-09-04", "2026-09-06"])
        XCTAssertEqual(periods[0].turns, 5)
        XCTAssertEqual(periods[0].costUSD, 2)

        let total = UsageReportAggregation.total(rows)
        XCTAssertEqual(total.turns, 6)
        XCTAssertEqual(total.costUSD, 4)
        XCTAssertEqual(total.liked, 2)
        XCTAssertEqual(total.disliked, 1)
        XCTAssertEqual(total.tokens, 6 * 300)

        XCTAssertEqual(
            UsageReportAggregation.byModel(rows).map(\.primary),
            ["gpt-5.6-sol", "gpt-5.6-terra"]
        )
    }

    func testPeriodsStayChronologicalAcrossYearBoundary() {
        // 라벨에 연도가 들어 있어 문자열 순서가 곧 시간 순서다.
        let days = [
            row(period: "2027-01-02", character: "boss", model: "m", turns: 1, liked: 0, disliked: 0),
            row(period: "2026-12-31", character: "boss", model: "m", turns: 1, liked: 0, disliked: 0),
            row(period: "2027-01-01", character: "boss", model: "m", turns: 1, liked: 0, disliked: 0),
        ]
        XCTAssertEqual(
            UsageReportAggregation.byPeriod(days).map(\.primary),
            ["2026-12-31", "2027-01-01", "2027-01-02"]
        )

        let months = [
            row(period: "2027-01", character: "boss", model: "m", turns: 1, liked: 0, disliked: 0),
            row(period: "2026-02", character: "boss", model: "m", turns: 1, liked: 0, disliked: 0),
            row(period: "2026-12", character: "boss", model: "m", turns: 1, liked: 0, disliked: 0),
        ]
        XCTAssertEqual(
            UsageReportAggregation.byPeriod(months).map(\.primary),
            ["2026-02", "2026-12", "2027-01"]
        )
    }

    func testReportDecodesBackendRows() throws {
        for timestamp in ["2026-09-06T09:00:00.000Z", "2026-09-06T09:00:00Z"] {
            let json = """
            {"backend":"codex","granularity":"day","timeZone":"Asia/Seoul",
             "generatedAt":"\(timestamp)",
             "rows":[{"period":"2026-09-06","characterId":"boss","model":"gpt-5.6-sol",
                      "effort":"max","turns":3,"costUSD":1.25,"inputTokens":1000,
                      "cachedInputTokens":200,"outputTokens":50,"liked":2,"disliked":0}]}
            """
            let report = try client.decodeUsageReport(Data(json.utf8))

            XCTAssertEqual(report.rows.count, 1)
            XCTAssertEqual(report.rows[0].characterId, "boss")
            XCTAssertEqual(report.rows[0].liked, 2)
            XCTAssertEqual(
                report.generatedAt,
                ISO8601DateFormatter().date(from: "2026-09-06T09:00:00Z")
            )
        }
    }

    func testReportRejectsInvalidBackendDate() {
        let json = """
        {"backend":"codex","granularity":"day","timeZone":"Asia/Seoul",
         "generatedAt":"not-a-date","rows":[]}
        """
        XCTAssertThrowsError(try client.decodeUsageReport(Data(json.utf8)))
    }

    private func row(
        period: String,
        character: String,
        model: String,
        turns: Int,
        liked: Int,
        disliked: Int,
        cost: Double = 0
    ) -> UsageReportRow {
        UsageReportRow(
            period: period,
            characterId: character,
            model: model,
            effort: "high",
            turns: turns,
            costUSD: cost,
            inputTokens: Int64(turns) * 100,
            cachedInputTokens: Int64(turns) * 100,
            outputTokens: Int64(turns) * 100,
            liked: liked,
            disliked: disliked
        )
    }
}
