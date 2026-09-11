import SwiftUI

/// The footer and report share this calculation. No extra DB query or AI call.
struct CharacterEvaluationReport: Equatable {
    let summary: CharacterTurnCostSummary
    let medianCost: Double?

    var likes: Int { max(0, summary.likedCount ?? 0) }
    var dislikes: Int { max(0, summary.dislikedCount ?? 0) }
    var feedbackCount: Int { likes + dislikes }
    var isProvisional: Bool { feedbackCount < 10 || summary.averageCostPerMinuteUsd == nil }
    var satisfactionScore: Double { Double(likes + 2) / Double(feedbackCount + 4) * 30 }
    var costScore: Double {
        guard let cost = summary.averageCostPerMinuteUsd, let medianCost else { return 10 }
        return medianCost > 0 ? 20 / (1 + cost / medianCost) : (cost == 0 ? 20 : 0)
    }
    var failedRate: Double? { rate(summary.failedTurnCount) }
    var interruptedRate: Double? { rate(summary.interruptedTurnCount) }
    var failureScore: Double? { failedRate.map { 30 * (1 - $0) } }
    var interruptionScore: Double? { interruptedRate.map { 20 * (1 - $0) } }
    var completedCount: Int? {
        guard let total = summary.finishedTurnCount, let failed = summary.failedTurnCount,
              let interrupted = summary.interruptedTurnCount,
              total >= 0, failed >= 0, interrupted >= 0, failed + interrupted <= total else { return nil }
        return total - failed - interrupted
    }
    var totalScore: Int? {
        guard completedCount != nil, let failureScore, let interruptionScore else { return nil }
        return Int(min(100, max(0, satisfactionScore + costScore + failureScore + interruptionScore)).rounded())
    }
    private func rate(_ count: Int?) -> Double? {
        guard let count, let total = summary.finishedTurnCount,
              total > 0, count >= 0, count <= total else { return nil }
        return Double(count) / Double(total)
    }
}

struct CharacterEvaluationReportSheet: View {
    @ObservedObject var store: CharacterTurnCostStore
    let characterID: String
    let name: String
    @Environment(\.dismiss) private var dismiss
    private let tint = Color.teal
    private func tr(_ key: String) -> String { OfficeLocalization.string(key) }
    private func number(_ value: Double?) -> String { value.map { String(format: "%.2f", $0) } ?? "—" }
    private func money(_ value: Double?) -> String { value.map { String(format: "$%.6f", $0) } ?? "—" }
    private func count(_ value: Int?) -> String { value.map(String.init) ?? "—" }
    private func percent(_ value: Double?) -> String { value.map { String(format: "%.1f%%", $0 * 100) } ?? "—" }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                CharacterAvatar(name: name, characterID: characterID, size: 36)
                VStack(alignment: .leading, spacing: 3) {
                    Text(name + " · " + tr("직원 평가 상세")).font(.headline)
                    Text(tr("전체 기간 · 기존 DB 집계 · 추가 AI 호출 없음"))
                        .font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                Button { dismiss() } label: { Image(systemName: "xmark") }
                    .buttonStyle(.plain).keyboardShortcut(.cancelAction)
                    .accessibilityLabel(tr("닫기"))
                    .accessibilityIdentifier("characterEvaluationClose")
            }.padding(20)
            Divider()
            ScrollView {
                if let report = store.averages[characterID]?.report {
                    VStack(alignment: .leading, spacing: 18) {
                        overview(report)
                        scoring(report)
                        facts(report)
                        rules
                    }.padding(20).textSelection(.enabled)
                } else {
                    ContentUnavailableView(tr("평가 데이터 없음"), systemImage: "chart.bar.doc.horizontal")
                        .padding(40)
                }
            }
        }
        .frame(width: 820, height: 700)
        .background(LinearGradient(colors: [tint.opacity(0.055), Color.primary.opacity(0.012)],
                                   startPoint: .topLeading, endPoint: .bottomTrailing))
        .background(Color(nsColor: .windowBackgroundColor))
        .accessibilityIdentifier("characterEvaluationReportSheet")
    }

    private func overview(_ r: CharacterEvaluationReport) -> some View {
        HStack(spacing: 24) {
            ZStack {
                Circle().stroke(tint.opacity(0.12), lineWidth: 8)
                Circle().trim(from: 0, to: CGFloat(r.totalScore ?? 0) / 100)
                    .stroke(tint, style: StrokeStyle(lineWidth: 8, lineCap: .round))
                    .rotationEffect(.degrees(-90))
                VStack(spacing: 2) {
                    Text(count(r.totalScore)).font(.system(size: 32, weight: .bold, design: .rounded))
                    Text("/ 100").font(.caption).foregroundStyle(.secondary)
                }
            }.frame(width: 108, height: 108)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(r.totalScore.map { OfficeLocalization.format("종합 평가 %d점", $0) }
                                    ?? tr("집계 전 또는 종료 상태 미수신"))
            VStack(alignment: .leading, spacing: 9) {
                Text(tr("종합 평가")).font(.title3.bold())
                Text(r.totalScore == nil ? tr("집계 전 또는 종료 상태 미수신")
                     : r.isProvisional ? tr("잠정 평가") : tr("현재 집계 점수"))
                    .font(.subheadline.weight(.semibold)).foregroundStyle(tint)
                Text(tr("만족도 30 · 분당 비용 20 · 실패율 30 · 중단율 20"))
                    .font(.callout)
                Text(tr("평가 10건 미만 또는 분당 비용 미확인 시 잠정 표시합니다. 표본 수가 많아도 업무 난이도까지 보정한 실력 점수는 아닙니다."))
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }.padding(18).background(tint.opacity(0.07), in: RoundedRectangle(cornerRadius: 16))
    }

    private func scoring(_ r: CharacterEvaluationReport) -> some View {
        VStack(alignment: .leading, spacing: 15) {
            Text(tr("점수 구성과 계산식")).font(.headline)
            scoreRow("사용자 만족도", value: r.satisfactionScore, maximum: 30, color: .pink,
                     formula: "(\(r.likes) + 2) ÷ (\(r.likes) + \(r.dislikes) + 4) × 30",
                     note: tr("좋아요·싫어요가 없는 턴을 부정 평가로 세지 않습니다. 평가 0건일 때는 중립 15점입니다."))
            Divider()
            scoreRow("분당 비용", value: r.costScore, maximum: 20, color: .teal,
                     formula: "20 ÷ (1 + C / M) · C = \(money(r.summary.averageCostPerMinuteUsd)) · M = \(money(r.medianCost))",
                     note: tr("C는 이 직원의 분당 비용, M은 비용이 확인된 직원들의 중앙값입니다. 중앙값과 같으면 10점. 비용 미확인은 중립 10점, 중앙값이 0이면 비용 0은 20점·양수는 0점입니다."))
            Divider()
            scoreRow("실패율", value: r.failureScore, maximum: 30, color: .red,
                     formula: "30 × (1 − \(count(r.summary.failedTurnCount)) / \(count(r.summary.finishedTurnCount))) · \(percent(r.failedRate))",
                     note: tr("DB 상태가 failed인 모든 종료 턴을 반영합니다. 서비스 장애·한도 소진을 임의로 제외하지 않습니다."))
            Divider()
            scoreRow("중단율", value: r.interruptionScore, maximum: 20, color: .orange,
                     formula: "20 × (1 − \(count(r.summary.interruptedTurnCount)) / \(count(r.summary.finishedTurnCount))) · \(percent(r.interruptedRate))",
                     note: tr("DB 상태가 interrupted인 모든 턴을 반영합니다. 사용자 중단·백엔드 재시작 등 원인과 관계없이 포함합니다."))
        }.padding(18).background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 16))
    }

    private func scoreRow(_ title: String, value: Double?, maximum: Double, color: Color,
                          formula: String, note: String) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack {
                Text(tr(title)).font(.subheadline.bold())
                Spacer()
                Text(number(value) + " / " + String(Int(maximum))).monospacedDigit().foregroundStyle(color)
            }
            ProgressView(value: min(maximum, max(0, value ?? 0)), total: maximum).tint(color)
            Text(formula).font(.system(.caption, design: .monospaced)).fixedSize(horizontal: false, vertical: true)
            Text(note).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }
    }

    private func facts(_ r: CharacterEvaluationReport) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(tr("원본 집계 상세")).font(.headline)
            LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], spacing: 12) {
                fact("전체 종료 턴", count(r.summary.finishedTurnCount))
                fact("완료 턴", count(r.completedCount))
                fact("실패 턴", count(r.summary.failedTurnCount))
                fact("중단 턴", count(r.summary.interruptedTurnCount))
                fact("좋아요 / 싫어요", "\(r.likes) / \(r.dislikes)")
                fact("직접 평가 건수", String(r.feedbackCount))
                fact("직접 평가 중 좋아요 비율", percent(r.feedbackCount > 0 ? Double(r.likes) / Double(r.feedbackCount) : nil))
                fact("종료 턴 대비 평가 비율", percent((r.summary.finishedTurnCount ?? 0) > 0 ? Double(r.feedbackCount) / Double(r.summary.finishedTurnCount!) : nil))
                fact("누적 환산 비용", r.summary.pricedTurnCount > 0 ? money(r.summary.totalCostUsd) : "—")
                fact("비용이 기록된 턴", String(r.summary.pricedTurnCount))
                fact("1턴당 평균 비용", money(r.summary.averageCostUsd))
                fact("분당 평균 비용", money(r.summary.averageCostPerMinuteUsd))
                fact("분당 계산에 사용한 비용", money(r.summary.timedCostUsd))
                fact("분당 계산에 사용한 시간(분)", number(r.summary.totalDurationSeconds.map { $0 / 60 }))
                fact("직원 분당 비용 중앙값", money(r.medianCost))
            }
            Text(tr("분당 비용 = 시간과 비용이 함께 기록된 턴의 비용 합계 ÷ 해당 소요 시간 합계(분). 턴당 비용은 참고 표시이며 점수에 넣지 않습니다."))
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func fact(_ title: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(tr(title)).font(.caption).foregroundStyle(.secondary)
            Text(value).font(.system(.body, design: .monospaced).weight(.semibold))
        }.frame(maxWidth: .infinity, alignment: .leading).padding(12)
            .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 10))
    }

    private var rules: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label(tr("집계 범위와 해석"), systemImage: "info.circle").font(.headline)
            Text(tr("전체 기간의 직원별 기록입니다. GUI·터미널 및 과거 모델·세션을 함께 집계하며, 현재 모델만의 평가가 아닙니다. 진행 중·대기 중 턴은 종료 비율에 포함하지 않습니다."))
            Text(tr("완료는 실행 종료 상태이며 문제 해결의 보증이 아닙니다. 실패·중단도 원인을 추측해 분류하지 않습니다. 오래 작업한 시간에는 별도 가산점이 없습니다."))
            Text(tr("비용은 앱에 저장된 CLI 보고값 또는 API 환산 추정치이며 실제 구독 청구액이 아닙니다. 로컬 비용 0은 기록값이며 전기·장비 비용을 포함하지 않습니다. 다른 직원의 비용이 변하면 중앙값과 점수도 변할 수 있습니다."))
            Text(tr("종합점수는 네 항목의 반올림 전 값을 합한 뒤 정수로 반올림합니다. 종료 상태 집계가 없으면 점수를 만들지 않습니다. 창이 열려 있는 동안 기존 집계 갱신을 반영하며 추가 분석 요청은 보내지 않습니다."))
        }.font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            .padding(18).background(tint.opacity(0.05), in: RoundedRectangle(cornerRadius: 16))
    }
}
