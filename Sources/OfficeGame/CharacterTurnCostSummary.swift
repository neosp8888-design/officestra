import OfficeCore
import SwiftUI

struct CharacterTurnCostSummary: Decodable, Equatable, Sendable {
    let characterId: String
    let pricedTurnCount: Int
    let totalCostUsd: Double
    let timedCostUsd: Double?
    let totalDurationSeconds: Double?
    let likedCount: Int?
    let dislikedCount: Int?

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

    var averageTurnDurationMinutes: Double? {
        guard let costPerTurn = averageCostUsd,
              let costPerMinute = averageCostPerMinuteUsd,
              costPerTurn.isFinite,
              costPerTurn >= 0,
              costPerMinute.isFinite,
              costPerMinute > 0 else {
            return nil
        }
        let duration = costPerTurn / costPerMinute
        return duration.isFinite && duration >= 0 ? duration : nil
    }
}

struct CharacterTurnCostSnapshot: Decodable, Sendable {
    let version: Int64
    let characters: [CharacterTurnCostSummary]
}

@MainActor
final class CharacterTurnCostStore: ObservableObject {
    struct Average: Equatable {
        let costUsd: Double?
        let costPerMinuteUsd: Double?
        let turnCount: Int
        let likedCount: Int
        let dislikedCount: Int
        let evaluationScore: Int?
    }

    @Published private(set) var averages: [String: Average] = [:]
    private var version: Int64 = -1

    func apply(_ snapshot: CharacterTurnCostSnapshot?) {
        guard let snapshot, snapshot.version > version else { return }
        version = snapshot.version
        let medianDuration = Self.median(
            snapshot.characters.compactMap(\.averageTurnDurationMinutes)
        )
        var next: [String: Average] = [:]
        for summary in snapshot.characters {
            let likedCount = max(0, summary.likedCount ?? 0)
            let dislikedCount = max(0, summary.dislikedCount ?? 0)
            next[summary.characterId] = Average(
                costUsd: summary.averageCostUsd,
                costPerMinuteUsd: summary.averageCostPerMinuteUsd,
                turnCount: summary.pricedTurnCount,
                likedCount: likedCount,
                dislikedCount: dislikedCount,
                evaluationScore: Self.evaluationScore(
                    summary: summary,
                    likedCount: likedCount,
                    dislikedCount: dislikedCount,
                    medianDuration: medianDuration
                )
            )
        }
        if next != averages { averages = next }
    }

    private static func median(_ values: [Double]) -> Double? {
        let sorted = values
            .filter { $0.isFinite && $0 >= 0 }
            .sorted()
        guard !sorted.isEmpty else { return nil }
        let middle = sorted.count / 2
        if sorted.count.isMultiple(of: 2) {
            return (sorted[middle - 1] + sorted[middle]) / 2
        }
        return sorted[middle]
    }

    private static func evaluationScore(
        summary: CharacterTurnCostSummary,
        likedCount: Int,
        dislikedCount: Int,
        medianDuration: Double?
    ) -> Int? {
        guard summary.pricedTurnCount > 0 else { return nil }
        let satisfaction = Double(likedCount + 2)
            / Double(likedCount + dislikedCount + 4) * 70
        let persistence: Double
        if let duration = summary.averageTurnDurationMinutes,
           let medianDuration,
           medianDuration > 0 {
            persistence = 30 * duration / (duration + medianDuration)
        } else {
            persistence = 15
        }
        return Int(min(100, max(0, satisfaction + persistence)).rounded())
    }
}

// 대화 카드와 독립된 작은 뷰라 비용·평가 갱신이 카드 전체를 다시 그리지 않는다.
// 대화 스크롤/스트리밍에서는 집계하지 않고 서버가 준 전체 기간 요약만 표시한다.
struct CharacterTurnCostFooter: View {
    @ObservedObject var store: CharacterTurnCostStore
    @ObservedObject var selection: CharacterSelectionStore

    var body: some View {
        let average = selection.selectedCharacterID.flatMap {
            store.averages[$0.rawValue]
        }
        HStack(spacing: 7) {
            Text(average.map {
                OfficeLocalization.format(
                    "1턴당 평균 비용(추정) %@ · 분당 %@ · 전체 %d턴",
                    $0.costUsd.map { String(format: "$%.4f", $0) } ?? "—",
                    $0.costPerMinuteUsd.map { String(format: "$%.4f", $0) } ?? "—",
                    $0.turnCount
                )
            } ?? OfficeLocalization.string("1턴당 평균 비용(추정) — · 분당 —"))
            .lineLimit(1)
            .minimumScaleFactor(0.75)
            .help(OfficeLocalization.string("전체 기간 · 비용이 기록된 종료 턴의 소요 시간 합계 기준"))

            if let average {
                Label("\(average.likedCount)", systemImage: "heart.fill")
                    .foregroundStyle(Color.red.opacity(0.8))
                    .fixedSize()
                    .help(OfficeLocalization.format("좋아요 %d건", average.likedCount))
                    .accessibilityLabel(OfficeLocalization.format("좋아요 %d건", average.likedCount))

                Label("\(average.dislikedCount)", systemImage: "hand.thumbsdown.fill")
                    .fixedSize()
                    .help(OfficeLocalization.format("싫어요 %d건", average.dislikedCount))
                    .accessibilityLabel(OfficeLocalization.format("싫어요 %d건", average.dislikedCount))

                if let score = average.evaluationScore {
                    Label("\(score)", systemImage: "star.fill")
                        .foregroundStyle(Color.yellow.opacity(0.85))
                        .fixedSize()
                        .help(OfficeLocalization.format("종합 평가 %d점", score))
                        .accessibilityLabel(OfficeLocalization.format("종합 평가 %d점", score))
                }
            }
        }
        .font(.caption.monospacedDigit())
        .foregroundStyle(.secondary)
        .frame(maxWidth: .infinity, alignment: .trailing)
        .padding(.horizontal, 20)
        .padding(.vertical, 6)
        .accessibilityIdentifier("characterAverageTurnCost")
    }
}
