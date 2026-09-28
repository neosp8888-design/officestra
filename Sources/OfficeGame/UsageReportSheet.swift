// 이 파일은 화이트보드 제공자 카드에서 여는 사용 현황 상세 시트와 그 집계 규칙을 담는다.

import Charts
import OfficeCore
import SwiftUI

enum UsageReportGranularity: String, CaseIterable, Identifiable {
    case day
    case month

    var id: String {
        rawValue
    }

    var title: String {
        switch self {
        case .day:
            OfficeLocalization.string("일별")
        case .month:
            OfficeLocalization.string("월별")
        }
    }

    var emptyMessage: String {
        switch self {
        case .day:
            OfficeLocalization.string("최근 30일 기록이 없습니다")
        case .month:
            OfficeLocalization.string("최근 12개월 기록이 없습니다")
        }
    }
}

struct UsageReport: Decodable, Equatable, Sendable {
    let backend: String
    let granularity: String
    let timeZone: String
    let generatedAt: Date
    let rows: [UsageReportRow]
}

/// 기간 × 직원 × 모델 × 추론 단위의 원본 집계 행이다.
struct UsageReportRow: Decodable, Equatable, Sendable {
    let period: String
    let characterId: String
    let model: String
    let effort: String
    let turns: Int
    let costUSD: Double
    let inputTokens: Int64
    let cachedInputTokens: Int64
    let outputTokens: Int64
    let liked: Int
    let disliked: Int
}

/// 화면 표 한 줄. 어떤 축으로 묶든 같은 열을 보여 준다.
struct UsageReportSummary: Equatable, Identifiable {
    let id: String
    let primary: String
    let secondary: String
    var turns = 0
    var costUSD = 0.0
    var tokens: Int64 = 0
    var liked = 0
    var disliked = 0

    var rated: Int {
        liked + disliked
    }

    /// 평가된 턴 중 좋아요 비율. 평가가 없으면 nil이라 0%와 구분된다.
    var likeRate: Double? {
        rated > 0 ? Double(liked) / Double(rated) : nil
    }

    var dislikeRate: Double? {
        rated > 0 ? Double(disliked) / Double(rated) : nil
    }
}

enum UsageReportAggregation {
    static func total(_ rows: [UsageReportRow]) -> UsageReportSummary {
        var summary = UsageReportSummary(id: "total", primary: "", secondary: "")
        for row in rows {
            add(row, to: &summary)
        }
        return summary
    }

    /// 사용자가 가장 보고 싶은 축이다. 평가가 많은 조합이 위로 온다.
    static func byCharacterAndModel(
        _ rows: [UsageReportRow]
    ) -> [UsageReportSummary] {
        grouped(rows, primary: \.characterId, secondary: \.model)
            .sorted(by: ratedThenTurns)
    }

    static func byCharacter(_ rows: [UsageReportRow]) -> [UsageReportSummary] {
        grouped(rows, primary: \.characterId, secondary: nil)
            .sorted(by: ratedThenTurns)
    }

    static func byModel(_ rows: [UsageReportRow]) -> [UsageReportSummary] {
        grouped(rows, primary: \.model, secondary: nil)
            .sorted(by: ratedThenTurns)
    }

    static func byEffort(_ rows: [UsageReportRow]) -> [UsageReportSummary] {
        grouped(rows, primary: \.effort, secondary: nil)
            .sorted(by: ratedThenTurns)
    }

    /// 기간은 시간 순서가 의미이므로 평가 수가 아니라 라벨 순으로 둔다.
    static func byPeriod(_ rows: [UsageReportRow]) -> [UsageReportSummary] {
        grouped(rows, primary: \.period, secondary: nil)
            .sorted { $0.primary < $1.primary }
    }

    private static func grouped(
        _ rows: [UsageReportRow],
        primary: KeyPath<UsageReportRow, String>,
        secondary: KeyPath<UsageReportRow, String>?
    ) -> [UsageReportSummary] {
        var order: [String] = []
        var summaries: [String: UsageReportSummary] = [:]
        for row in rows {
            let first = row[keyPath: primary]
            let second = secondary.map { row[keyPath: $0] } ?? ""
            let key = "\(first)\u{1F}\(second)"
            if summaries[key] == nil {
                order.append(key)
                summaries[key] = UsageReportSummary(
                    id: key,
                    primary: first,
                    secondary: second
                )
            }
            add(row, to: &summaries[key]!)
        }
        return order.compactMap { summaries[$0] }
    }

    private static func add(
        _ row: UsageReportRow,
        to summary: inout UsageReportSummary
    ) {
        summary.turns += row.turns
        summary.costUSD += row.costUSD
        summary.tokens += row.inputTokens + row.cachedInputTokens
            + row.outputTokens
        summary.liked += row.liked
        summary.disliked += row.disliked
    }

    private static func ratedThenTurns(
        _ left: UsageReportSummary,
        _ right: UsageReportSummary
    ) -> Bool {
        if left.rated != right.rated {
            return left.rated > right.rated
        }
        if left.turns != right.turns {
            return left.turns > right.turns
        }
        return left.id < right.id
    }

    static func percentText(_ rate: Double?) -> String {
        guard let rate else {
            return "–"
        }
        return "\(Int((rate * 100).rounded()))%"
    }

    static func costText(_ value: Double) -> String {
        String(format: "$%.2f", value)
    }

    static func tokenText(_ value: Int64) -> String {
        let formatter = NumberFormatter()
        formatter.numberStyle = .decimal
        formatter.locale = OfficeLocalization.locale
        return formatter.string(from: NSNumber(value: value)) ?? "\(value)"
    }
}

/// 제공자별 일·월 사용 현황과 직원·모델·추론별 평가율을 보여 주는 시트다.
struct UsageReportSheet: View {
    let director: AgentDirector
    let onClose: () -> Void

    @State private var backend: AgentBackend
    @State private var granularity = UsageReportGranularity.day
    @State private var report: UsageReport?
    @State private var isLoading = true
    @State private var errorMessage: String?

    init(
        backend: AgentBackend,
        director: AgentDirector,
        onClose: @escaping () -> Void
    ) {
        self.director = director
        self.onClose = onClose
        _backend = State(initialValue: backend)
    }

    private var tint: Color {
        DashboardPalette.providerAccent(for: backend)
    }

    var body: some View {
        VStack(spacing: 0) {
            toolbar

            Divider().opacity(0.5)

            content
        }
        .background(
            LinearGradient(
                colors: [tint.opacity(0.055), Color.primary.opacity(0.012)],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
        )
        .task(id: "\(backend.rawValue):\(granularity.rawValue)") {
            await load()
        }
        .accessibilityIdentifier("usageReportSheet")
    }

    private var toolbar: some View {
        HStack(spacing: 10) {
            Label(
                OfficeLocalization.string("사용 현황 상세"),
                systemImage: "chart.bar.xaxis"
            )
            .font(.system(size: 12, weight: .bold))
            .foregroundStyle(tint)

            Picker("", selection: $backend) {
                ForEach(AgentBackend.allCases) { item in
                    Text(item.title).tag(item)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .frame(width: 300)
            .accessibilityLabel(OfficeLocalization.string("제공자"))
            .accessibilityIdentifier("usageReportBackend")

            Picker("", selection: $granularity) {
                ForEach(UsageReportGranularity.allCases) { item in
                    Text(item.title).tag(item)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .frame(width: 130)
            .accessibilityLabel(OfficeLocalization.string("집계 단위"))
            .accessibilityIdentifier("usageReportGranularity")

            Spacer()

            Button(action: onClose) {
                Label(OfficeLocalization.string("닫기"), systemImage: "xmark")
                    .font(.system(size: 10, weight: .bold))
                    .padding(.horizontal, 9)
                    .frame(height: 26)
                    .background(Color.primary.opacity(0.055), in: Capsule())
            }
            .officeGameTool()
            .keyboardShortcut(.cancelAction)
            .accessibilityLabel(OfficeLocalization.string("닫기"))
        }
        .padding(.horizontal, 12)
        .frame(height: 44)
    }

    @ViewBuilder
    private var content: some View {
        if let report {
            if report.rows.isEmpty {
                ContentUnavailableView(
                    granularity.emptyMessage,
                    systemImage: "chart.bar",
                    description: Text(
                        OfficeLocalization.string("이 제공자로 진행한 업무가 쌓이면 여기에 집계됩니다.")
                    )
                )
            } else {
                reportBody(report)
            }
        } else if let errorMessage {
            ContentUnavailableView(
                OfficeLocalization.string("사용 현황을 불러오지 못했습니다"),
                systemImage: "wifi.exclamationmark",
                description: Text(OfficeLocalization.systemMessage(errorMessage))
            )
        } else {
            ProgressView(OfficeLocalization.string("사용 현황을 불러오는 중"))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func reportBody(_ report: UsageReport) -> some View {
        let rows = report.rows
        let total = UsageReportAggregation.total(rows)
        let periods = UsageReportAggregation.byPeriod(rows)

        return ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                // 일별과 월별은 보는 창이 달라 총합이 다르다. 창을 그대로 적는다.
                if let first = periods.first, let last = periods.last {
                    Text(
                        OfficeLocalization.format(
                            granularity == .day
                                ? "오늘 포함 최근 30일 · %@ ~ %@"
                                : "이번 달 포함 최근 12개월 · %@ ~ %@",
                            first.primary,
                            last.primary
                        )
                    )
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("usageReportRange")
                }

                statTiles(total)

                section(
                    OfficeLocalization.string("직원 × 모델 평가"),
                    systemImage: "person.2.fill",
                    hint: OfficeLocalization.string("평가가 많은 조합이 위에 옵니다.")
                ) {
                    UsageReportTable(
                        summaries: UsageReportAggregation.byCharacterAndModel(rows),
                        primaryTitle: OfficeLocalization.string("직원"),
                        secondaryTitle: OfficeLocalization.string("모델"),
                        primaryLabel: characterName,
                        secondaryLabel: modelTitle,
                        characterColumn: true,
                        tint: tint
                    )
                }

                section(
                    granularity == .day
                        ? OfficeLocalization.string("일별 사용")
                        : OfficeLocalization.string("월별 사용"),
                    systemImage: "calendar",
                    hint: nil
                ) {
                    costChart(periods)
                    UsageReportTable(
                        summaries: periods.reversed(),
                        primaryTitle: OfficeLocalization.string("기간"),
                        secondaryTitle: nil,
                        primaryLabel: { $0 },
                        secondaryLabel: { $0 },
                        characterColumn: false,
                        tint: tint
                    )
                }

                HStack(alignment: .top, spacing: 14) {
                    section(
                        OfficeLocalization.string("모델별"),
                        systemImage: "cpu",
                        hint: nil
                    ) {
                        UsageReportTable(
                            summaries: UsageReportAggregation.byModel(rows),
                            primaryTitle: OfficeLocalization.string("모델"),
                            secondaryTitle: nil,
                            primaryLabel: modelTitle,
                            secondaryLabel: { $0 },
                            characterColumn: false,
                            tint: tint
                        )
                    }
                    section(
                        OfficeLocalization.string("추론별"),
                        systemImage: "brain",
                        hint: nil
                    ) {
                        UsageReportTable(
                            summaries: UsageReportAggregation.byEffort(rows),
                            primaryTitle: OfficeLocalization.string("추론"),
                            secondaryTitle: nil,
                            primaryLabel: effortTitle,
                            secondaryLabel: { $0 },
                            characterColumn: false,
                            tint: tint
                        )
                    }
                }

                section(
                    OfficeLocalization.string("직원별"),
                    systemImage: "person.fill",
                    hint: nil
                ) {
                    UsageReportTable(
                        summaries: UsageReportAggregation.byCharacter(rows),
                        primaryTitle: OfficeLocalization.string("직원"),
                        secondaryTitle: nil,
                        primaryLabel: characterName,
                        secondaryLabel: { $0 },
                        characterColumn: true,
                        tint: tint
                    )
                }
            }
            .padding(14)
        }
    }

    private func statTiles(_ total: UsageReportSummary) -> some View {
        HStack(spacing: 10) {
            statTile(
                OfficeLocalization.string("턴"),
                value: "\(total.turns)",
                detail: nil
            )
            statTile(
                OfficeLocalization.string("비용"),
                value: UsageReportAggregation.costText(total.costUSD),
                detail: OfficeLocalization.format(
                    "토큰 %@",
                    UsageReportAggregation.tokenText(total.tokens)
                )
            )
            statTile(
                OfficeLocalization.string("좋아요율"),
                value: UsageReportAggregation.percentText(total.likeRate),
                detail: OfficeLocalization.format("좋아요 %d건", total.liked)
            )
            statTile(
                OfficeLocalization.string("싫어요율"),
                value: UsageReportAggregation.percentText(total.dislikeRate),
                detail: OfficeLocalization.format("싫어요 %d건", total.disliked)
            )
        }
    }

    private func statTile(
        _ title: String,
        value: String,
        detail: String?
    ) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title)
                .font(.system(size: 9.5, weight: .bold))
                .foregroundStyle(.secondary)
            Text(value)
                .font(.system(size: 20, weight: .bold, design: .rounded))
                .monospacedDigit()
            if let detail {
                Text(detail)
                    .font(.system(size: 9.5))
                    .foregroundStyle(.tertiary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
        .background(
            Color(nsColor: .textBackgroundColor).opacity(0.7),
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(Color.primary.opacity(0.07))
        }
        .accessibilityElement(children: .combine)
    }

    private func section<Content: View>(
        _ title: String,
        systemImage: String,
        hint: String?,
        @ViewBuilder content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Label(title, systemImage: systemImage)
                    .font(.system(size: 11, weight: .bold))
                    .foregroundStyle(tint)
                if let hint {
                    Text(hint)
                        .font(.system(size: 9.5))
                        .foregroundStyle(.secondary)
                }
            }
            content()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
        .background(
            Color(nsColor: .textBackgroundColor).opacity(0.7),
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(Color.primary.opacity(0.07))
        }
    }

    /// 기간별 비용 막대다. 한 계열이라 제공자 색 하나만 쓰고 범례는 두지 않는다.
    private func costChart(_ periods: [UsageReportSummary]) -> some View {
        Chart(periods) { summary in
            BarMark(
                x: .value(OfficeLocalization.string("기간"), summary.primary),
                y: .value(OfficeLocalization.string("비용"), summary.costUSD)
            )
            .foregroundStyle(tint)
            .cornerRadius(3)
        }
        .chartYAxis {
            AxisMarks(position: .leading) { value in
                AxisGridLine().foregroundStyle(Color.primary.opacity(0.08))
                AxisValueLabel {
                    if let cost = value.as(Double.self) {
                        Text(String(format: "$%.0f", cost))
                            .font(.system(size: 8.5))
                    }
                }
            }
        }
        .chartXAxis {
            AxisMarks { value in
                AxisValueLabel {
                    if let label = value.as(String.self) {
                        Text(shortPeriodLabel(label))
                            .font(.system(size: 8.5))
                    }
                }
            }
        }
        .frame(height: 150)
        .accessibilityLabel(OfficeLocalization.string("기간별 비용"))
    }

    private func shortPeriodLabel(_ period: String) -> String {
        // 일별은 "09-06", 월별은 "2026-09"처럼 짧게 둔다.
        granularity == .day ? String(period.dropFirst(5)) : period
    }

    private func characterName(_ id: String) -> String {
        guard let character = OfficeCharacter(rawValue: id) else {
            return id.isEmpty ? OfficeLocalization.string("기록 없음") : id
        }
        return director.displayName(for: character)
    }

    private func modelTitle(_ model: String) -> String {
        model.isEmpty ? OfficeLocalization.string("기록 없음") : backend.modelTitle(model)
    }

    private func effortTitle(_ effort: String) -> String {
        effort.isEmpty ? OfficeLocalization.string("기록 없음") : effort
    }

    @MainActor
    private func load() async {
        isLoading = true
        do {
            report = try await OfficeDatabaseClient(
                baseURL: director.databaseBaseURL
            ).fetchUsageReport(backend: backend, granularity: granularity)
            errorMessage = nil
        } catch {
            report = nil
            errorMessage = error.localizedDescription
        }
        isLoading = false
    }
}

/// 어떤 축이든 같은 열(턴·비용·좋아요·싫어요·좋아요율)을 보여 주는 표다.
private struct UsageReportTable: View {
    let summaries: [UsageReportSummary]
    let primaryTitle: String
    let secondaryTitle: String?
    let primaryLabel: (String) -> String
    let secondaryLabel: (String) -> String
    let characterColumn: Bool
    let tint: Color

    private let numberWidth: CGFloat = 54
    private let rateWidth: CGFloat = 150

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider().opacity(0.4)
            ForEach(summaries) { summary in
                row(summary)
                Divider().opacity(0.25)
            }
        }
        .font(.system(size: 10.5))
    }

    private var header: some View {
        HStack(spacing: 8) {
            Text(primaryTitle)
                .frame(maxWidth: .infinity, alignment: .leading)
            if let secondaryTitle {
                Text(secondaryTitle)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            Text(OfficeLocalization.string("턴"))
                .frame(width: numberWidth, alignment: .trailing)
            Text(OfficeLocalization.string("비용"))
                .frame(width: numberWidth + 12, alignment: .trailing)
            Text(OfficeLocalization.string("좋아요"))
                .frame(width: numberWidth, alignment: .trailing)
            Text(OfficeLocalization.string("싫어요"))
                .frame(width: numberWidth, alignment: .trailing)
            Text(OfficeLocalization.string("좋아요율"))
                .frame(width: rateWidth, alignment: .leading)
        }
        .font(.system(size: 9, weight: .bold))
        .foregroundStyle(.secondary)
        .padding(.vertical, 5)
    }

    private func row(_ summary: UsageReportSummary) -> some View {
        HStack(spacing: 8) {
            HStack(spacing: 6) {
                if characterColumn, !summary.primary.isEmpty {
                    CharacterBadge(
                        name: primaryLabel(summary.primary),
                        characterID: summary.primary,
                        size: 18
                    )
                }
                Text(primaryLabel(summary.primary))
                    .fontWeight(.semibold)
                    .lineLimit(1)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if secondaryTitle != nil {
                Text(secondaryLabel(summary.secondary))
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            Text("\(summary.turns)")
                .monospacedDigit()
                .frame(width: numberWidth, alignment: .trailing)
            Text(UsageReportAggregation.costText(summary.costUSD))
                .monospacedDigit()
                .frame(width: numberWidth + 12, alignment: .trailing)
            Text("\(summary.liked)")
                .monospacedDigit()
                .frame(width: numberWidth, alignment: .trailing)
            Text("\(summary.disliked)")
                .monospacedDigit()
                .frame(width: numberWidth, alignment: .trailing)
            rateCell(summary)
                .frame(width: rateWidth, alignment: .leading)
        }
        .padding(.vertical, 5)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(rowAccessibilityLabel(summary))
    }

    /// 좋아요 비율은 제공자 색, 싫어요는 회색으로 나눈 막대와 숫자를 같이 둔다.
    /// 평가가 없는 줄은 막대 없이 "–"만 보여 0%와 구분한다.
    private func rateCell(_ summary: UsageReportSummary) -> some View {
        HStack(spacing: 6) {
            if let likeRate = summary.likeRate {
                GeometryReader { proxy in
                    HStack(spacing: 2) {
                        Capsule()
                            .fill(tint)
                            .frame(width: max(0, proxy.size.width * likeRate - 1))
                        Capsule()
                            .fill(Color.secondary.opacity(0.35))
                            .frame(maxWidth: .infinity)
                    }
                }
                .frame(height: 6)
            } else {
                Capsule()
                    .fill(Color.primary.opacity(0.06))
                    .frame(height: 6)
            }
            Text(UsageReportAggregation.percentText(summary.likeRate))
                .monospacedDigit()
                .fontWeight(.bold)
                .frame(width: 34, alignment: .trailing)
        }
    }

    private func rowAccessibilityLabel(_ summary: UsageReportSummary) -> String {
        let name = secondaryTitle == nil
            ? primaryLabel(summary.primary)
            : "\(primaryLabel(summary.primary)) \(secondaryLabel(summary.secondary))"
        return OfficeLocalization.format(
            "%@, 턴 %d, 좋아요 %d, 싫어요 %d, 좋아요율 %@",
            name,
            summary.turns,
            summary.liked,
            summary.disliked,
            UsageReportAggregation.percentText(summary.likeRate)
        )
    }
}
