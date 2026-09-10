import OfficeCore
import SwiftUI

struct CharacterTurnCostSummary: Decodable, Equatable, Sendable {
    let characterId: String
    let pricedTurnCount: Int
    let totalCostUsd: Double
    let timedCostUsd: Double?
    let totalDurationSeconds: Double?

    var averageCostUsd: Double? {
        guard pricedTurnCount > 0, totalCostUsd.isFinite, totalCostUsd >= 0 else {
            return nil
        }
        return totalCostUsd / Double(pricedTurnCount)
    }

    var averageCostPerMinuteUsd: Double? {
        guard let timedCostUsd,
              let totalDurationSeconds,
              timedCostUsd.isFinite,
              timedCostUsd >= 0,
              totalDurationSeconds.isFinite,
              totalDurationSeconds > 0 else {
            return nil
        }
        return timedCostUsd * 60 / totalDurationSeconds
    }
}

struct CharacterTurnCostSnapshot: Decodable, Sendable {
    let version: Int64
    let characters: [CharacterTurnCostSummary]
}

@MainActor
final class CharacterTurnCostStore: ObservableObject {
    struct Average: Equatable {
        let costUsd: Double
        let costPerMinuteUsd: Double?
        let turnCount: Int
    }

    @Published private(set) var averages: [String: Average] = [:]
    private var version: Int64 = -1

    func apply(_ snapshot: CharacterTurnCostSnapshot?) {
        guard let snapshot, snapshot.version > version else { return }
        version = snapshot.version
        var next: [String: Average] = [:]
        for summary in snapshot.characters {
            if let cost = summary.averageCostUsd {
                next[summary.characterId] = Average(
                    costUsd: cost,
                    costPerMinuteUsd: summary.averageCostPerMinuteUsd,
                    turnCount: summary.pricedTurnCount
                )
            }
        }
        if next != averages { averages = next }
    }
}

// 대화 카드와 독립된 작은 뷰라 비용 갱신이 카드 전체를 다시 그리지 않는다.
// 대화 스크롤/스트리밍에서는 집계하지 않고 서버가 준 전체 기간 평균만 표시한다.
struct CharacterTurnCostFooter: View {
    @ObservedObject var store: CharacterTurnCostStore
    @ObservedObject var selection: CharacterSelectionStore

    var body: some View {
        let average = selection.selectedCharacterID.flatMap {
            store.averages[$0.rawValue]
        }
        Text(average.map {
            OfficeLocalization.format(
                "1턴당 평균 비용(추정) %@ · 분당 %@ · 전체 %d턴",
                String(format: "$%.4f", $0.costUsd),
                $0.costPerMinuteUsd.map { String(format: "$%.4f", $0) } ?? "—",
                $0.turnCount
            )
        } ?? OfficeLocalization.string("1턴당 평균 비용(추정) — · 분당 —"))
        .font(.caption.monospacedDigit())
        .foregroundStyle(.secondary)
        .lineLimit(1)
        .minimumScaleFactor(0.75)
        .frame(maxWidth: .infinity, alignment: .trailing)
        .padding(.horizontal, 20)
        .padding(.vertical, 6)
        .help(OfficeLocalization.string("전체 기간 · 비용이 기록된 종료 턴의 소요 시간 합계 기준"))
        .accessibilityIdentifier("characterAverageTurnCost")
    }
}
