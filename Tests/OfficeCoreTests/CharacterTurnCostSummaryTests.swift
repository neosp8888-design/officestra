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

        XCTAssertEqual(
            summary("boss", count: 2, cost: 3, timedCost: 2, duration: 120).averageCostPerMinuteUsd,
            1
        )
        XCTAssertNil(summary("boss", count: 1, cost: 1).averageCostPerMinuteUsd)
        XCTAssertNil(summary("boss", count: 1, cost: 1, timedCost: 1, duration: 0).averageCostPerMinuteUsd)
        XCTAssertNil(summary("boss", count: 1, cost: 1, timedCost: -1, duration: 60).averageCostPerMinuteUsd)
    }

    func testOldBackendResponseStillDecodes() throws {
        let data = Data(#"{"turns":[],"costSummary":{"version":1,"characters":[{"characterId":"boss","pricedTurnCount":2,"totalCostUsd":3}]}}"#.utf8)
        let response = try JSONDecoder().decode(LiveFeedResponse.self, from: data)
        XCTAssertEqual(response.costSummary?.characters.first?.averageCostUsd, 1.5)
        XCTAssertNil(response.costSummary?.characters.first?.averageCostPerMinuteUsd)
        XCTAssertNil(response.costSummary?.characters.first?.likedCount)
        XCTAssertNil(response.costSummary?.characters.first?.dislikedCount)
        XCTAssertNil(response.costSummary?.characters.first?.finishedTurnCount)
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
        XCTAssertNil(store.averages["boss"]?.costPerMinuteUsd)
        XCTAssertEqual(store.averages["boss"]?.likedCount, 0)
        XCTAssertEqual(store.averages["boss"]?.dislikedCount, 0)
        XCTAssertEqual(store.averages["boss"]?.evaluationScore, 75)
        XCTAssertEqual(store.averages["boss"]?.evaluationIsProvisional, true)
        XCTAssertEqual(store.averages["left-man"]?.costUsd, 1)
        store.apply(CharacterTurnCostSnapshot(version: 4, characters: [
            summary(
                "boss",
                count: 3,
                cost: 12,
                timedCost: 8,
                duration: 120,
                liked: 7,
                disliked: 2
            )
        ]))
        XCTAssertEqual(publications, 2)
        XCTAssertEqual(store.averages["boss"]?.costUsd, 4)
        XCTAssertEqual(store.averages["boss"]?.costPerMinuteUsd, 4)
        XCTAssertEqual(store.averages["boss"]?.likedCount, 7)
        XCTAssertEqual(store.averages["boss"]?.dislikedCount, 2)
        XCTAssertEqual(store.averages["boss"]?.evaluationScore, 81)
        XCTAssertNil(store.averages["left-man"])
        withExtendedLifetime(subscription) {}
    }

    @MainActor
    func testEqualPerMinuteCostDoesNotRewardLongerTurns() {
        let store = CharacterTurnCostStore()
        store.apply(CharacterTurnCostSnapshot(version: 1, characters: [
            summary(
                "patient",
                count: 2,
                cost: 20,
                timedCost: 20,
                duration: 1200,
                liked: 8,
                disliked: 0
            ),
            summary(
                "brief",
                count: 2,
                cost: 2,
                timedCost: 2,
                duration: 120,
                liked: 8,
                disliked: 0
            ),
        ]))

        XCTAssertEqual(store.averages["patient"]?.evaluationScore, 85)
        XCTAssertEqual(store.averages["brief"]?.evaluationScore, 85)
    }

    @MainActor
    func testFailureAndInterruptionUseAllFinishedTurnsEvenWithoutCost() {
        let store = CharacterTurnCostStore()
        store.apply(CharacterTurnCostSnapshot(version: 1, characters: [
            summary("healthy", count: 1, cost: 1, timedCost: 1, duration: 60,
                    liked: 10, finished: 10),
            summary("failed", count: 1, cost: 1, timedCost: 1, duration: 60,
                    liked: 10, finished: 10, failed: 5),
            summary("interrupted", count: 1, cost: 1, timedCost: 1, duration: 60,
                    liked: 10, finished: 10, interrupted: 5),
            summary("unpricedFailure", count: 0, cost: 0, finished: 10, failed: 10),
        ]))
        XCTAssertEqual(store.averages["healthy"]?.evaluationScore, 86)
        XCTAssertEqual(store.averages["failed"]?.evaluationScore, 71)
        XCTAssertEqual(store.averages["interrupted"]?.evaluationScore, 76)
        XCTAssertEqual(store.averages["unpricedFailure"]?.evaluationScore, 45)
        XCTAssertEqual(store.averages["unpricedFailure"]?.turnCount, 10)
        XCTAssertEqual(store.averages["failed"]?.failedRate, 0.5)
        XCTAssertEqual(store.averages["healthy"]?.evaluationIsProvisional, false)
    }

    @MainActor
    func testZeroPriceAndMissingPriceAreDifferentAndNoTurnsHaveNoScore() {
        let store = CharacterTurnCostStore()
        store.apply(CharacterTurnCostSnapshot(version: 1, characters: [
            summary("free", count: 1, cost: 0, timedCost: 0, duration: 60),
            summary("free2", count: 1, cost: 0, timedCost: 0, duration: 60),
            summary("paid", count: 1, cost: 1, timedCost: 1, duration: 60),
            summary("unknown", count: 1, cost: 1),
            summary("empty", count: 0, cost: 0),
        ]))
        XCTAssertEqual(store.averages["free"]?.evaluationScore, 85)
        XCTAssertEqual(store.averages["paid"]?.evaluationScore, 65)
        XCTAssertEqual(store.averages["unknown"]?.evaluationScore, 75)
        XCTAssertNil(store.averages["empty"]?.evaluationScore)
    }

    @MainActor
    func testOldBackendAndInvalidStatusCountsDoNotInventScore() throws {
        let old = try JSONDecoder().decode(CharacterTurnCostSnapshot.self, from: Data(
            #"{"version":1,"characters":[{"characterId":"old","pricedTurnCount":2,"totalCostUsd":3}]}"#.utf8
        ))
        let store = CharacterTurnCostStore()
        store.apply(old)
        XCTAssertNil(store.averages["old"]?.evaluationScore)
        store.apply(CharacterTurnCostSnapshot(version: 2, characters: [
            summary("bad", count: 1, cost: 1, finished: 2, failed: 2, interrupted: 1)
        ]))
        XCTAssertNil(store.averages["bad"]?.evaluationScore)
    }

    private func summary(
        _ id: String,
        count: Int,
        cost: Double,
        timedCost: Double? = nil,
        duration: Double? = nil,
        liked: Int? = nil,
        disliked: Int? = nil,
        finished: Int? = nil,
        failed: Int = 0,
        interrupted: Int = 0
    ) -> CharacterTurnCostSummary {
        CharacterTurnCostSummary(
            characterId: id,
            pricedTurnCount: count,
            totalCostUsd: cost,
            timedCostUsd: timedCost,
            totalDurationSeconds: duration,
            likedCount: liked,
            dislikedCount: disliked,
            finishedTurnCount: finished ?? count,
            failedTurnCount: failed,
            interruptedTurnCount: interrupted
        )
    }

    func testFooterFormattingInKoreanAndEnglish() {
        let oneTurn = OfficeLocalization.format(
            "1턴당 평균 비용(추정) %@ · 분당 %@ · 전체 %d턴",
            arguments: ["$0.1234", "$0.0567", 1],
            languages: ["en"]
        )
        XCTAssertEqual(oneTurn, "Average cost (est.) per turn $0.1234 · per minute $0.0567 · 1 turn total")

        let multipleTurns = OfficeLocalization.format(
            "1턴당 평균 비용(추정) %@ · 분당 %@ · 전체 %d턴",
            arguments: ["$0.1234", "$0.0567", 4],
            languages: ["en"]
        )
        XCTAssertEqual(multipleTurns, "Average cost (est.) per turn $0.1234 · per minute $0.0567 · 4 turns total")

        let korean = OfficeLocalization.format(
            "1턴당 평균 비용(추정) %@ · 분당 %@ · 전체 %d턴",
            arguments: ["$0.1234", "$0.0567", 1],
            languages: ["ko"]
        )
        XCTAssertEqual(korean, "1턴당 평균 비용(추정) $0.1234 · 분당 $0.0567 · 전체 1턴")

        let placeholderEn = OfficeLocalization.string(
            "1턴당 평균 비용(추정) — · 분당 —",
            languages: ["en"]
        )
        XCTAssertEqual(placeholderEn, "Average cost (est.) per turn — · per minute —")

        let tooltipEn = OfficeLocalization.string(
            "전체 기간 · 비용이 기록된 종료 턴의 소요 시간 합계 기준",
            languages: ["en"]
        )
        XCTAssertEqual(tooltipEn, "All time · Total elapsed time of finished turns with recorded costs")
    }
}
