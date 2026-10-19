// 이 파일은 2D·3D 화이트보드에 세 AI CLI의 잔여 한도를 실시간 표시한다.

import Foundation
import OfficeCore
import SwiftUI

struct WhiteboardUsageLayer: View {
    let isActive: Bool
    let artStyle: OfficeArtStyle
    let databaseBaseURL: URL

    private var textVerticalScale: CGFloat {
        artStyle == .twoD ? 1 : 1.3
    }

    @State private var snapshot: AIUsageSnapshot?
    @State private var updateStatus = CLIUpdateStatus.empty
    @State private var isLoading = true
    @State private var refreshFailed = false

    /// 화이트보드 갱신 주기다. 한도와 CLI 갱신 여부를 같은 주기에 본다.
    static let refreshInterval = TimeInterval(600)

    var body: some View {
        GeometryReader { geometry in
            Canvas { context, _ in
                drawBoard(
                    context: &context,
                    containerSize: geometry.size
                )
            }
        }
        .allowsHitTesting(false)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(OfficeLocalization.string("AI 잔여 한도"))
        .accessibilityValue(accessibilityValue)
        .task(id: isActive) {
            guard isActive else {
                return
            }

            while !Task.isCancelled {
                await refresh()
                do {
                    try await Task.sleep(for: .seconds(Self.refreshInterval))
                } catch {
                    return
                }
            }
        }
    }

    /// CLI 갱신 여부는 실패해도 한도 표시를 막지 않는다.
    private func refreshUpdateStatus() async {
        guard
            let status = try? await OfficeDatabaseClient(
                baseURL: databaseBaseURL
            ).fetchCLIUpdateStatus()
        else {
            return
        }
        if updateStatus != status {
            updateStatus = status
        }
    }

    private var accessibilityValue: String {
        guard let snapshot else {
            return isLoading
                ? OfficeLocalization.string("조회 중")
                : OfficeLocalization.string("조회할 수 없음")
        }

        let providers = [
            accessibilityLimitText(
                provider: "Codex",
                window: "5시간",
                remaining: snapshot.codexFiveHour,
                resetAt: snapshot.codexFiveHourResetAt
            ),
            accessibilityLimitText(
                provider: "Codex",
                window: "주간",
                remaining: snapshot.codexWeekly,
                resetAt: snapshot.codexWeeklyResetAt
            ),
            accessibilityLimitText(
                provider: "Claude Code",
                window: "5시간",
                remaining: snapshot.claudeFiveHour,
                resetAt: snapshot.claudeFiveHourResetAt
            ),
            accessibilityLimitText(
                provider: "Claude Code",
                window: "주간",
                remaining: snapshot.claudeWeekly,
                resetAt: snapshot.claudeWeeklyResetAt
            ),
        ]
        let claudeModels = (snapshot.claudeModelWeeklyLimits ?? []).map { limit in
            accessibilityLimitText(provider: "Claude Code \(limit.name)", window: "주간",
                                   remaining: limit.remaining, resetAt: limit.resetAt)
        }
        let groups = snapshot.displayedAntigravityQuotaGroups
        let antigravity = groups.isEmpty ? [
            accessibilityLimitText(provider: "Antigravity", window: "5시간",
                                   remaining: snapshot.antigravityFiveHour, resetAt: snapshot.antigravityFiveHourResetAt),
            accessibilityLimitText(provider: "Antigravity", window: "주간",
                                   remaining: snapshot.antigravityWeekly, resetAt: snapshot.antigravityWeeklyResetAt),
        ] : groups.flatMap { group in
            group.displayedBuckets.map { bucket in
                accessibilityLimitText(provider: "Antigravity \(group.displayName)", window: bucket.displayName,
                                       remaining: bucket.remaining, resetAt: bucket.resetAt)
            }
        }
        return (providers + claudeModels + antigravity).joined(separator: ", ")
    }

    @MainActor
    private func refresh() async {
        await refreshUpdateStatus()
        isLoading = snapshot == nil
        do {
            let refreshed = try await OfficeDatabaseClient(
                baseURL: databaseBaseURL
            ).fetchUsageSummary()
            snapshot = refreshed
            refreshFailed = refreshed.codexLimitError != nil
                || refreshed.claudeLimitError != nil
                || refreshed.antigravityLimitError != nil
        } catch {
            refreshFailed = true
        }
        isLoading = false
    }

    private func drawBoard(
        context: inout GraphicsContext,
        containerSize: CGSize
    ) {
        let fittedFrame = OfficeCanvasGeometry.fittedFrame(
            in: containerSize
        )
        guard fittedFrame.width > 0 else {
            return
        }

        let scale =
            fittedFrame.width / OfficeCanvasGeometry.designSize.width
        let transform = CGAffineTransform(
            a: scale,
            b: 0,
            c: 0,
            d: scale,
            tx: fittedFrame.minX,
            ty: fittedFrame.minY
        )
        context.concatenate(transform)

        let ink = Color(
            red: 0.08,
            green: 0.18,
            blue: 0.22
        )
        let mutedInk = ink.opacity(0.84)

        if let snapshot {
            drawUsageGroup(
                provider: "CODEX",
                hasUpdate: updateStatus
                    .package(id: "codex")?.updateAvailable == true,
                fiveHour: snapshot.codexFiveHour,
                fiveHourResetAt: snapshot.codexFiveHourResetAt,
                weekly: snapshot.codexWeekly,
                weeklyResetAt: snapshot.codexWeeklyResetAt,
                providerY: 0,
                showsFiveHour: true,
                context: &context,
                ink: ink,
                mutedInk: mutedInk
            )
            drawUsageGroup(
                provider: "CLAUDE CODE",
                hasUpdate: updateStatus
                    .package(id: "claude")?.updateAvailable == true,
                fiveHour: snapshot.claudeFiveHour,
                fiveHourResetAt: snapshot.claudeFiveHourResetAt,
                weekly: snapshot.claudeWeekly,
                weeklyResetAt: snapshot.claudeWeeklyResetAt,
                modelWeeklyLimits: snapshot.claudeModelWeeklyLimits ?? [],
                providerY: 26,
                showsFiveHour: true,
                context: &context,
                ink: ink,
                mutedInk: mutedInk
            )
            let groups = snapshot.displayedAntigravityQuotaGroups
            if !groups.isEmpty {
                // 기존 Antigravity 영역 안에 그룹별 한 줄씩 표시한다.
                // 상세 카드에서는 각 창의 초기화 시각도 함께 보여 준다.
                let lineHeight = 24 / CGFloat(groups.count)
                for (index, group) in groups.enumerated() {
                    let limits = group.displayedBuckets.map { bucket in
                        "\(bucket.compactWindow) \(compactPercentText(bucket.remaining))"
                    }.joined(separator: "  ")
                    drawText("AG \(group.compactName)  \(limits)", in: &context,
                             at: CGPoint(x: 0, y: 52 + CGFloat(index) * lineHeight),
                             size: min(8, lineHeight - 1), weight: .semibold, color: ink)
                }
            } else {
                drawUsageGroup(
                    provider: "ANTIGRAVITY",
                    hasUpdate: updateStatus
                        .package(id: "antigravity")?.updateAvailable == true,
                    fiveHour: snapshot.antigravityFiveHour,
                    fiveHourResetAt: snapshot.antigravityFiveHourResetAt,
                    weekly: snapshot.antigravityWeekly,
                    weeklyResetAt: snapshot.antigravityWeeklyResetAt,
                    imageResetAt: snapshot.antigravityImageResetAt,
                    providerY: 52,
                    showsFiveHour: true,
                    context: &context,
                    ink: ink,
                    mutedInk: mutedInk
                )
            }
        } else {
            drawText(
                "AI LIMIT",
                in: &context,
                at: CGPoint(x: 0, y: 11),
                size: 12.5,
                weight: .bold,
                color: ink
            )
            drawText(
                isLoading ? "CHECKING…" : "LIMIT OFF",
                in: &context,
                at: CGPoint(x: 0, y: 44),
                size: 13,
                weight: .bold,
                color: mutedInk
            )
        }

        if refreshFailed {
            drawText(
                "!",
                in: &context,
                at: CGPoint(x: 128, y: 1),
                anchor: .topTrailing,
                size: 7,
                weight: .bold,
                color: Color(red: 0.77, green: 0.38, blue: 0.08)
            )
        }
    }

    private func drawUsageGroup(
        provider: String,
        hasUpdate: Bool,
        fiveHour: Int?,
        fiveHourResetAt: Date?,
        weekly: Int?,
        weeklyResetAt: Date?,
        imageResetAt: Date? = nil,
        modelWeeklyLimits: [AIUsageModelWeeklyLimit] = [],
        providerY: CGFloat,
        showsFiveHour: Bool,
        context: inout GraphicsContext,
        ink: Color,
        mutedInk: Color
    ) {
        drawText(
            provider,
            in: &context,
            at: CGPoint(x: 0, y: providerY),
            size: 9.5,
            weight: .bold,
            color: ink
        )
        if hasUpdate {
            // 새 버전이 있으면 제공자 이름 옆에 표시한다. 자리가 좁아
            // 문구 대신 짧은 표식만 둔다.
            drawText(
                "UP",
                in: &context,
                at: CGPoint(x: CGFloat(provider.count) * 5.7 + 4, y: providerY + 1),
                size: 7.5,
                weight: .black,
                color: Color(red: 0.10, green: 0.48, blue: 0.30)
            )
        } else if let imageResetAt, imageResetAt > Date() {
            drawText(
                "IMG COOL",
                in: &context,
                at: CGPoint(x: CGFloat(provider.count) * 5.7 + 4, y: providerY + 1),
                size: 6.5,
                weight: .bold,
                color: Color(red: 0.85, green: 0.45, blue: 0.12)
            )
        }
        let limits = [
            showsFiveHour
                ? "5H \(compactPercentText(fiveHour))"
                : nil,
            "7D \(compactPercentText(weekly))",
        ].compactMap { $0 }.joined(separator: "  ")
        drawText(
            limits,
            in: &context,
            at: CGPoint(x: 0, y: providerY + (modelWeeklyLimits.isEmpty ? 12 : 10)),
            size: modelWeeklyLimits.isEmpty ? 8.5 : 7.5,
            weight: .semibold,
            color: mutedInk
        )
        if !modelWeeklyLimits.isEmpty {
            let models = modelWeeklyLimits.map {
                "\($0.name.prefix(8).uppercased()) 7D \(compactPercentText($0.remaining))"
            }.joined(separator: "  ")
            drawText(models, in: &context, at: CGPoint(x: 0, y: providerY + 18),
                     size: 6.5, weight: .semibold, color: mutedInk)
        }
    }

    private func drawText(
        _ text: String,
        in context: inout GraphicsContext,
        at point: CGPoint,
        anchor: UnitPoint = .topLeading,
        size: CGFloat,
        weight: Font.Weight,
        color: Color
    ) {
        var textContext = context
        textContext.concatenate(
            OfficeWhiteboardGeometry.usageTransform(
                for: artStyle,
                at: point
            )
        )
        textContext.concatenate(
            CGAffineTransform(
                a: 1,
                b: 0,
                c: 0,
                d: textVerticalScale,
                tx: 0,
                ty: 0
            )
        )
        textContext.draw(
            Text(text)
                .font(
                    .system(
                        size: size,
                        weight: weight,
                        design: .rounded
                    )
                )
                .fontWidth(.condensed)
                .foregroundStyle(color),
            at: .zero,
            anchor: anchor
        )
    }

    private func compactPercentText(_ value: Int?) -> String {
        value.map { "\($0)%" } ?? "–"
    }

    private func percentText(_ value: Int?) -> String {
        value.map {
            OfficeLocalization.format("%d퍼센트", $0)
        } ?? OfficeLocalization.string("정보 없음")
    }

    private func compactLimitText(
        label: String,
        remaining: Int?,
        resetAt: Date?
    ) -> String {
        let limit = "\(label) \(compactPercentText(remaining))"
        guard let reset = usageResetRemainingText(resetAt) else {
            return limit
        }
        return "\(limit) · \(reset)"
    }

    private func accessibilityLimitText(
        provider: String,
        window: String,
        remaining: Int?,
        resetAt: Date?
    ) -> String {
        let localizedWindow = OfficeLocalization.string(window)
        let limit = "\(provider) \(localizedWindow) \(percentText(remaining))"
        guard let reset = usageResetRemainingText(resetAt) else {
            return limit
        }
        return OfficeLocalization.format("%@, 초기화까지 %@", limit, reset)
    }
}

func usageResetRemainingText(
    _ resetAt: Date?,
    relativeTo now: Date = Date()
) -> String? {
    usageResetRemainingText(
        minutes: usageResetRemainingMinutes(resetAt, relativeTo: now)
    )
}

func usageResetRemainingMinutes(
    _ resetAt: Date?,
    relativeTo fetchedAt: Date
) -> Int? {
    guard let resetAt, resetAt > fetchedAt else {
        return nil
    }

    return max(
        1,
        Int(ceil(resetAt.timeIntervalSince(fetchedAt) / 60))
    )
}

func usageResetRemainingText(minutes totalMinutes: Int?) -> String? {
    guard let totalMinutes, totalMinutes > 0 else {
        return nil
    }

    let days = totalMinutes / (24 * 60)
    let hours = (totalMinutes % (24 * 60)) / 60
    let minutes = totalMinutes % 60
    if days > 0 {
        return OfficeLocalization.format("%d일 %d시간 %d분", days, hours, minutes)
    }
    if hours == 0 {
        return OfficeLocalization.format("%d분", minutes)
    }
    return OfficeLocalization.format("%d시간 %d분", hours, minutes)
}

/// 현재 속도로 한도를 고르게 쓴다면 남아 있어야 하는 할당량 비율이다.
/// 막대 자체가 남은 할당량을 나타내므로 남은 시간 비율과 같은 위치에 둔다.
func usagePaceRemainingFraction(
    remainingMinutes: Int?,
    windowMinutes: Int
) -> Double? {
    guard let remainingMinutes, remainingMinutes > 0, windowMinutes > 0 else {
        return nil
    }
    return min(1, Double(remainingMinutes) / Double(windowMinutes))
}

enum UsagePaceStatus: Equatable {
    case comfortable
    case behind
}

func usagePaceStatus(
    remainingPercent: Int?,
    expectedFraction: Double?
) -> UsagePaceStatus? {
    guard let remainingPercent, let expectedFraction else {
        return nil
    }
    let expectedPercent = Int((expectedFraction * 100).rounded())
    return remainingPercent >= expectedPercent ? .comfortable : .behind
}

/// 직원이 쓰는 CLI의 설치본과 배포 최신본 비교 결과다.
struct CLIUpdateStatus: Decodable, Equatable, Sendable {
    struct Package: Decodable, Equatable, Sendable, Identifiable {
        let id: String
        let label: String
        let packageName: String
        let installedVersion: String?
        let latestVersion: String?
        let checkFailed: Bool?
        let updateAvailable: Bool
    }

    let checkedAt: Date?
    let updateAvailable: Bool
    let checkFailed: Bool?
    let packages: [Package]

    static let empty = CLIUpdateStatus(
        checkedAt: nil,
        updateAvailable: false,
        checkFailed: nil,
        packages: []
    )

    func package(id: String) -> Package? {
        packages.first { $0.id == id }
    }
}

struct CLIUpdateApplyResponse: Decodable, Equatable, Sendable {
    let ok: Bool
    let status: CLIUpdateStatus
}

struct AIUsageSnapshot: Decodable, Equatable, Sendable {
    let codexFiveHour: Int?
    let codexFiveHourResetAt: Date?
    let codexWeekly: Int?
    let codexWeeklyResetAt: Date?
    let claudeFiveHour: Int?
    let claudeFiveHourResetAt: Date?
    let claudeWeekly: Int?
    let claudeWeeklyResetAt: Date?
    let claudeModelWeeklyLimits: [AIUsageModelWeeklyLimit]?
    let antigravityFiveHour: Int?
    let antigravityFiveHourResetAt: Date?
    let antigravityWeekly: Int?
    let antigravityWeeklyResetAt: Date?
    let antigravityImageResetAt: Date?
    let antigravityQuotaGroups: [AIUsageQuotaGroup]?
    let codexPlan: String?
    let claudePlan: String?
    let antigravityPlan: String?
    let codexSubscriptionExpiresAt: Date?
    let codexActivity: AIUsageActivitySnapshot?
    let claudeActivity: AIUsageActivitySnapshot?
    let antigravityActivity: AIUsageActivitySnapshot?
    let codexLimitError: String?
    let claudeLimitError: String?
    let antigravityLimitError: String?
    let fetchedAt: Date

    var displayedAntigravityQuotaGroups: [AIUsageQuotaGroup] {
        let groups = (antigravityQuotaGroups ?? []).filter { $0.id != "third-party" }
        return groups.filter { $0.id == "argon" } + groups.filter { $0.id != "argon" }
    }
}

struct AIUsageModelWeeklyLimit: Decodable, Equatable, Identifiable, Sendable {
    let id: String
    let name: String
    let remaining: Int?
    let resetAt: Date?

    var displayName: String { OfficeLocalization.format("%@ 주간", name) }
}

struct AIUsageQuotaGroup: Decodable, Equatable, Identifiable, Sendable {
    let id: String
    let name: String
    let buckets: [AIUsageQuotaBucket]

    var displayedBuckets: [AIUsageQuotaBucket] {
        buckets.filter { $0.window == "fiveHour" }
            + buckets.filter { $0.window == "weekly" }
            + buckets.filter { $0.window != "fiveHour" && $0.window != "weekly" }
    }

    var displayName: String {
        switch id {
        case "gemini": OfficeLocalization.string("일반 Gemini")
        case "third-party": "Claude · GPT"
        default: name
        }
    }

    var compactName: String {
        switch id {
        case "gemini": "GEM"
        case "third-party": "C/GPT"
        case "argon": "ARGON"
        default: String(name.prefix(12))
        }
    }
}

struct AIUsageQuotaBucket: Decodable, Equatable, Identifiable, Sendable {
    let id: String
    let name: String
    let window: String
    let remaining: Int?
    let resetAt: Date?

    var displayName: String {
        switch window {
        case "fiveHour": OfficeLocalization.string("5시간")
        case "weekly": OfficeLocalization.string("7일")
        default: name
        }
    }

    var compactWindow: String {
        switch window {
        case "fiveHour": "5H"
        case "weekly": "7D"
        default: name
        }
    }

    var windowMinutes: Int {
        switch window {
        case "fiveHour": 5 * 60
        case "weekly": 7 * 24 * 60
        default: 0
        }
    }
}

struct AIUsageActivitySnapshot: Decodable, Equatable, Sendable {
    let todayCostUSD: Double?
    let recentTokens: Int64?
    let last30DaysCostUSD: Double?
    let last30DaysTokens: Int64?
    let costEstimateSupported: Bool?
}
