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
    var finishedTurnCount: Int? = nil
    var failedTurnCount: Int? = nil
    var interruptedTurnCount: Int? = nil

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
        let costUsd: Double?
        let costPerMinuteUsd: Double?
        let turnCount: Int
        let likedCount: Int
        let dislikedCount: Int
        let evaluationScore: Int?
        let evaluationIsProvisional: Bool
        let failedRate: Double?
        let interruptedRate: Double?
        let report: CharacterEvaluationReport
    }

    @Published private(set) var averages: [String: Average] = [:]
    private var version: Int64 = -1

    func apply(_ snapshot: CharacterTurnCostSnapshot?) {
        guard let snapshot, snapshot.version > version else { return }
        version = snapshot.version
        let medianCost = Self.median(
            snapshot.characters.compactMap(\.averageCostPerMinuteUsd)
        )
        var next: [String: Average] = [:]
        for summary in snapshot.characters {
            let likedCount = max(0, summary.likedCount ?? 0)
            let dislikedCount = max(0, summary.dislikedCount ?? 0)
            let report = CharacterEvaluationReport(summary: summary, medianCost: medianCost)
            next[summary.characterId] = Average(
                costUsd: summary.averageCostUsd,
                costPerMinuteUsd: summary.averageCostPerMinuteUsd,
                turnCount: summary.finishedTurnCount ?? summary.pricedTurnCount,
                likedCount: likedCount,
                dislikedCount: dislikedCount,
                evaluationScore: report.totalScore,
                evaluationIsProvisional: report.isProvisional,
                failedRate: report.failedRate,
                interruptedRate: report.interruptedRate,
                report: report
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

}

// 대화 카드와 독립된 작은 뷰라 비용·평가 갱신이 카드 전체를 다시 그리지 않는다.
// 대화 스크롤/스트리밍에서는 집계하지 않고 서버가 준 전체 기간 요약만 표시한다.
struct CharacterTurnCostFooter: View {
    @ObservedObject var store: CharacterTurnCostStore
    @ObservedObject var selection: CharacterSelectionStore
    let characterName: (OfficeCharacter) -> String
    @State private var reportTarget: ReportTarget?

    private struct ReportTarget: Identifiable {
        let id: String
        let name: String
    }

    var body: some View {
        let average = selection.selectedCharacterID.flatMap {
            store.averages[$0.rawValue]
        }
        Button {
            if let id = selection.selectedCharacterID {
                reportTarget = ReportTarget(id: id.rawValue, name: characterName(id))
            }
        } label: {
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
            .help(OfficeLocalization.string("전체 종료 턴 집계 · 비용 평균은 비용이 기록된 턴 기준"))

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
                    Label("\(score)\(average.evaluationIsProvisional ? "*" : "")", systemImage: "star.fill")
                        .foregroundStyle(Color.yellow.opacity(0.85))
                        .fixedSize()
                        .help(OfficeLocalization.format(
                            "종합 평가 %d점 · 만족 30 / 분당 비용 20 / 실패율 30 / 중단율 20 · 실패 %.1f%% / 중단 %.1f%% · * 평가 10건 미만 또는 비용 미확인",
                            score, (average.failedRate ?? 0) * 100, (average.interruptedRate ?? 0) * 100
                        ))
                        .accessibilityLabel(OfficeLocalization.format("종합 평가 %d점", score))
                }
            }
        }
        .font(.caption.monospacedDigit())
        .foregroundStyle(.secondary)
        .frame(maxWidth: .infinity, alignment: .trailing)
        .padding(.horizontal, 20)
        .padding(.vertical, 6)
        .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(selection.selectedCharacterID == nil)
        .accessibilityLabel(OfficeLocalization.string("직원 평가 상세 열기"))
        .accessibilityIdentifier("characterAverageTurnCost")
        .sheet(item: $reportTarget) { target in
            CharacterEvaluationReportSheet(store: store, characterID: target.id, name: target.name)
        }
    }
}
