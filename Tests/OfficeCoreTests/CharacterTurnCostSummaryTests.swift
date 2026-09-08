import Combine
import XCTest
@testable import OfficeGame

final class CharacterTurnCostSummaryTests: XCTestCase {
    func testAverageAndUnknownCosts() {
        XCTAssertEqual(summary("boss", count: 4, cost: 10).averageCostUsd, 2.5)
        XCTAssertEqual(summary("boss", count: 3, cost: 0).averageCostUsd, 0)
        XCTAssertNil(summary("boss", count: 0, cost: 0).averageCostUsd)
        XCTAssertNil(summary("boss", count: 1, cost: .infinity).averageCostUsd)
        XCTAssertNil(summary("boss", count: 1, cost: -1).averageCostUsd)
    }

    func testOldBackendResponseStillDecodes() throws {
        let data = Data(#"{"turns":[]}"#.utf8)
        let response = try JSONDecoder().decode(LiveFeedResponse.self, from: data)
        XCTAssertNil(response.costSummary)
    }

    @MainActor
    func testOnlyChangedAveragesPublishAndOlderSnapshotsCannotOverwrite() {
        let store = CharacterTurnCostStore()
        var publications = 0
        let subscription = store.$averages.dropFirst().sink { _ in publications += 1 }
        let entries = [summary("boss", count: 2, cost: 10), summary("left-man", count: 4, cost: 4)]
        store.apply(CharacterTurnCostSnapshot(version: 2, characters: entries))
        for _ in 0..<100 {
            store.apply(CharacterTurnCostSnapshot(version: 2, characters: entries))
        }
        store.apply(CharacterTurnCostSnapshot(version: 3, characters: entries))
        store.apply(CharacterTurnCostSnapshot(version: 1, characters: []))
        store.apply(nil)
        XCTAssertEqual(publications, 1)
        XCTAssertEqual(store.averages["boss"]?.costUsd, 5)
        XCTAssertEqual(store.averages["left-man"]?.costUsd, 1)
        store.apply(CharacterTurnCostSnapshot(version: 4, characters: [summary("boss", count: 3, cost: 12)]))
        XCTAssertEqual(publications, 2)
        XCTAssertEqual(store.averages["boss"]?.costUsd, 4)
        XCTAssertNil(store.averages["left-man"])
        withExtendedLifetime(subscription) {}
    }

    private func summary(_ id: String, count: Int, cost: Double) -> CharacterTurnCostSummary {
        CharacterTurnCostSummary(characterId: id, pricedTurnCount: count, totalCostUsd: cost)
    }

    func testFooterFormattingInKoreanAndEnglish() {
        let oneTurn = OfficeLocalization.format(
            "1턴당 평균 비용(추정) %@ · 전체 %d턴",
            arguments: ["$0.1234", 1],
            languages: ["en"]
        )
        XCTAssertEqual(oneTurn, "Average cost per turn (est.) $0.1234 · 1 turn total")

        let multipleTurns = OfficeLocalization.format(
            "1턴당 평균 비용(추정) %@ · 전체 %d턴",
            arguments: ["$0.1234", 4],
            languages: ["en"]
        )
        XCTAssertEqual(multipleTurns, "Average cost per turn (est.) $0.1234 · 4 turns total")

        let korean = OfficeLocalization.format(
            "1턴당 평균 비용(추정) %@ · 전체 %d턴",
            arguments: ["$0.1234", 1],
            languages: ["ko"]
        )
        XCTAssertEqual(korean, "1턴당 평균 비용(추정) $0.1234 · 전체 1턴")

        let placeholderEn = OfficeLocalization.string("1턴당 평균 비용(추정) —", languages: ["en"])
        XCTAssertEqual(placeholderEn, "Average cost per turn (est.) —")

        let tooltipEn = OfficeLocalization.string("전체 기간 · 비용이 기록된 종료 턴 기준", languages: ["en"])
        XCTAssertEqual(tooltipEn, "All time · Finished turns with recorded costs")
    }
}
