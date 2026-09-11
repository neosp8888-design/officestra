// 이 파일은 모던 대시보드의 좌측 정보 패널과 우측 실시간 업무 피드를 구성한다.

import AppKit
import Combine
import OfficeCore
import SwiftUI

enum OfficeDetailSelection: String, CaseIterable, Identifiable {
    case archive
    case usage
    case wiki

    var id: String {
        rawValue
    }

    var title: String {
        switch self {
        case .archive:
            OfficeLocalization.string("대화 보관함")
        case .usage:
            OfficeLocalization.string("화이트보드")
        case .wiki:
            OfficeLocalization.string("사내 위키")
        }
    }

    var subtitle: String {
        switch self {
        case .archive:
            OfficeLocalization.string("직원 업무 기록")
        case .usage:
            OfficeLocalization.string("현재 사용 가능량")
        case .wiki:
            OfficeLocalization.string("승인된 지식과 확인")
        }
    }

    var icon: String {
        switch self {
        case .archive:
            "books.vertical.fill"
        case .usage:
            "rectangle.and.pencil.and.ellipsis"
        case .wiki:
            "text.book.closed.fill"
        }
    }
}

enum UsageBoardLayout {
    static let singleColumnThreshold: CGFloat = 560

    static func usesSingleColumn(for width: CGFloat) -> Bool {
        width < singleColumnThreshold
    }
}

struct OfficeDetailPanel: View {
    let director: AgentDirector
    @Binding var selection: OfficeDetailSelection
    @State private var usageRefreshRequestID = UUID()
    @State private var usageIsRefreshing = false

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 11) {
                Image(systemName: selection.icon)
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(DashboardPalette.accent)
                    .frame(width: 34, height: 34)
                    .background(
                        DashboardPalette.accent.opacity(0.10),
                        in: RoundedRectangle(
                            cornerRadius: 10,
                            style: .continuous
                        )
                    )

                VStack(alignment: .leading, spacing: 2) {
                    Text(selection.title)
                        .font(.system(size: 15, weight: .bold))
                    HStack(spacing: 4) {
                        Text(selection.subtitle)
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(.secondary)

                        if selection == .usage {
                            Button {
                                usageRefreshRequestID = UUID()
                            } label: {
                                // 옆 부제목과 같은 크기·색으로 맞춘다.
                                // 두 상태의 크기를 고정해 눌러도 글자가 밀리지 않는다.
                                Group {
                                    if usageIsRefreshing {
                                        ProgressView()
                                            .controlSize(.mini)
                                            .scaleEffect(0.62)
                                    } else {
                                        Image(systemName: "arrow.clockwise")
                                            .font(
                                                .system(
                                                    size: 9.5,
                                                    weight: .semibold
                                                )
                                            )
                                    }
                                }
                                .frame(width: 12, height: 12)
                                .foregroundStyle(.secondary)
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                            .disabled(usageIsRefreshing)
                            .accessibilityLabel(OfficeLocalization.string("한도 새로고침"))
                            .accessibilityValue(
                                usageIsRefreshing ? OfficeLocalization.string("새로고침 중") : ""
                            )
                            .help(OfficeLocalization.string("한도 새로고침"))
                        }
                    }
                }
                Spacer()

                detailTabs
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 13)

            Divider()
                .opacity(0.55)

            Group {
                switch selection {
                case .archive:
                    ArchiveShelfContent(director: director)
                case .usage:
                    UsageBoardContent(
                        refreshRequestID: usageRefreshRequestID,
                        isRefreshing: $usageIsRefreshing,
                        databaseBaseURL: director.databaseBaseURL,
                        director: director
                    )
                case .wiki:
                    WikiKnowledgeView(
                        databaseBaseURL: director.databaseBaseURL
                    )
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .officePanelStyle()
    }

    private var detailTabs: some View {
        HStack(spacing: 3) {
            ForEach(OfficeDetailSelection.allCases) { item in
                Button {
                    withAnimation(.easeInOut(duration: 0.16)) {
                        selection = item
                    }
                } label: {
                    Image(systemName: item.icon)
                        .font(.system(size: 10.5, weight: .semibold))
                        .frame(width: 25, height: 25)
                        .background(
                            selection == item
                                ? DashboardPalette.accent.opacity(0.14)
                                : Color.clear,
                            in: RoundedRectangle(
                                cornerRadius: 7,
                                style: .continuous
                            )
                        )
                }
                .buttonStyle(.plain)
                .foregroundStyle(
                    selection == item
                        ? DashboardPalette.accent
                        : Color.secondary
                )
                .accessibilityLabel(item.title)
                .accessibilityValue(selection == item ? OfficeLocalization.string("선택됨") : "")
                .accessibilityIdentifier("officeDetailTab-\(item.rawValue)")
                .help(item.title)
            }
        }
        .padding(3)
        .background(
            Color.primary.opacity(0.04),
            in: RoundedRectangle(cornerRadius: 9, style: .continuous)
        )
    }
}

private struct ArchiveShelfContent: View {
    let director: AgentDirector
    @State private var searchText = ""
    @State private var selectedTurnID: String?
    @State private var turns: [LiveFeedTurn] = []
    @State private var totalTurnCount = 0
    @State private var errorMessage: String?
    @State private var isLoading = true
    @State private var isLoadingMore = false

    private static let pageSize = 12

    var body: some View {
        VStack(spacing: 0) {
            searchBar

            if isLoading && turns.isEmpty {
                ProgressView(OfficeLocalization.string("기록을 불러오는 중"))
                    .font(.system(size: 11, weight: .medium))
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if let errorMessage, turns.isEmpty {
                ContentUnavailableView(
                    OfficeLocalization.string("기록을 불러오지 못했습니다"),
                    systemImage: "exclamationmark.triangle",
                    description: Text(OfficeLocalization.systemMessage(errorMessage))
                )
            } else if turns.isEmpty && normalizedSearchText.isEmpty {
                ContentUnavailableView(
                    OfficeLocalization.string("아직 저장된 기록이 없습니다"),
                    systemImage: "tray",
                    description: Text(
                        OfficeLocalization.string("직원에게 업무를 보내면 여기에 쌓입니다.")
                    )
                )
            } else if turns.isEmpty {
                ContentUnavailableView(
                    OfficeLocalization.string("검색 결과가 없습니다"),
                    systemImage: "text.magnifyingglass",
                    description: Text(
                        OfficeLocalization.string("다른 이름이나 대화 내용으로 검색해보세요.")
                    )
                )
            } else {
                ScrollView {
                    VStack(spacing: 10) {
                        ArchiveRecordGrid(turns: turns) { turn in
                            selectedTurnID = turn.id
                        }

                        if turns.count < totalTurnCount {
                            loadMoreButton
                        }
                    }
                    .padding(.horizontal, 12)
                    .padding(.bottom, 12)
                }
            }
        }
        .animation(.easeInOut(duration: 0.18), value: isSearching)
        // 카드를 누르면 바로 넓은 시트로 펼친다. 이전·다음은 지금 목록 안에서만
        // 움직이므로 검색 중이면 검색 결과만 넘긴다.
        .sheet(isPresented: isShowingBook) {
            if let selectedTurn, let selectedIndex {
                ArchiveOpenBook(
                    turn: selectedTurn,
                    navigation: ArchiveBookNavigation(
                        index: selectedIndex,
                        total: totalTurnCount,
                        canGoPrevious: ArchiveBookPaging.canGoPrevious(
                            index: selectedIndex
                        ),
                        canGoNext: !isLoadingMore
                            && ArchiveBookPaging.canGoNext(
                                index: selectedIndex,
                                loadedCount: turns.count,
                                totalCount: totalTurnCount
                            )
                    ),
                    onPrevious: showPreviousTurn,
                    onNext: {
                        Task {
                            await showNextTurn()
                        }
                    },
                    onClose: {
                        selectedTurnID = nil
                    }
                )
                .frame(
                    minWidth: ArchiveBookSheetLayout.minimumWidth,
                    idealWidth: ArchiveBookSheetLayout.idealWidth,
                    minHeight: ArchiveBookSheetLayout.minimumHeight,
                    idealHeight: ArchiveBookSheetLayout.idealHeight
                )
            }
        }
        .task(id: normalizedSearchText) {
            if !normalizedSearchText.isEmpty {
                try? await Task.sleep(for: .milliseconds(250))
            }
            guard !Task.isCancelled else {
                return
            }
            await reload()
        }
    }

    private var searchBar: some View {
        HStack(spacing: 8) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(.secondary)

            TextField(
                OfficeLocalization.string("전체 기록 검색 · 업무, 응답, 세션, 모델"),
                text: $searchText
            )
            .textFieldStyle(.plain)
            .font(.system(size: 11, weight: .semibold))

            if !searchText.isEmpty {
                Button {
                    searchText = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .foregroundStyle(.tertiary)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(OfficeLocalization.string("검색어 지우기"))
            }

            if isLoading {
                ProgressView()
                    .controlSize(.mini)
                    .frame(width: 22, height: 22)
            } else {
                Text(OfficeLocalization.format("%d건", totalTurnCount))
                    .font(.system(size: 9, weight: .bold, design: .rounded))
                    .foregroundStyle(DashboardPalette.accent)
                    .padding(.horizontal, 7)
                    .frame(height: 22)
                    .background(
                        DashboardPalette.accent.opacity(0.09),
                        in: Capsule()
                    )
            }
        }
        .padding(.horizontal, 11)
        .frame(height: 38)
        .background(
            Color.primary.opacity(0.025),
            in: RoundedRectangle(cornerRadius: 11, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .stroke(Color.primary.opacity(0.055))
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
    }

    private var loadMoreButton: some View {
        Button {
            Task {
                await loadMore()
            }
        } label: {
            HStack(spacing: 7) {
                if isLoadingMore {
                    ProgressView()
                        .controlSize(.mini)
                } else {
                    Image(systemName: "chevron.down")
                    Text(OfficeLocalization.string("다음 12건 보기"))
                    Text("\(turns.count)/\(totalTurnCount)")
                        .foregroundStyle(.tertiary)
                }
            }
            .font(.system(size: 10, weight: .bold))
            .foregroundStyle(DashboardPalette.accent)
            .frame(maxWidth: .infinity)
            .frame(height: 32)
            .background(
                DashboardPalette.accent.opacity(0.07),
                in: RoundedRectangle(
                    cornerRadius: 9,
                    style: .continuous
                )
            )
        }
        .buttonStyle(.plain)
        .disabled(isLoadingMore)
        .accessibilityLabel(OfficeLocalization.string("다음 12개 기록 보기"))
    }

    private var normalizedSearchText: String {
        searchText.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private var isSearching: Bool {
        !normalizedSearchText.isEmpty
    }

    private var selectedIndex: Int? {
        guard let selectedTurnID else {
            return nil
        }
        return turns.firstIndex { $0.id == selectedTurnID }
    }

    private var selectedTurn: LiveFeedTurn? {
        selectedIndex.map { turns[$0] }
    }

    private var isShowingBook: Binding<Bool> {
        Binding(
            get: { selectedTurn != nil },
            set: { isShowing in
                if !isShowing {
                    selectedTurnID = nil
                }
            }
        )
    }

    private func showPreviousTurn() {
        guard let selectedIndex, selectedIndex > 0 else {
            return
        }
        selectedTurnID = turns[selectedIndex - 1].id
    }

    // 불러온 목록의 끝이면 다음 12건을 받아 온 뒤에 넘어간다.
    private func showNextTurn() async {
        guard let selectedIndex else {
            return
        }
        if selectedIndex + 1 >= turns.count {
            await loadMore()
        }
        // 받아 오는 동안 목록이 바뀌었을 수 있어 위치를 다시 찾는다.
        guard
            let refreshedIndex = self.selectedIndex,
            refreshedIndex + 1 < turns.count
        else {
            return
        }
        selectedTurnID = turns[refreshedIndex + 1].id
    }

    private func reload() async {
        let query = normalizedSearchText
        isLoading = true
        errorMessage = nil
        do {
            let page = try await director.archiveFeed(
                query: query.isEmpty ? nil : query,
                limit: Self.pageSize,
                offset: 0
            )
            guard !Task.isCancelled, query == normalizedSearchText else {
                return
            }
            turns = page.turns
            totalTurnCount = page.total
        } catch {
            guard !Task.isCancelled, query == normalizedSearchText else {
                return
            }
            turns = []
            totalTurnCount = 0
            errorMessage = error.localizedDescription
        }
        isLoading = false
    }

    private func loadMore() async {
        guard !isLoadingMore, turns.count < totalTurnCount else {
            return
        }
        let query = normalizedSearchText
        isLoadingMore = true
        defer {
            isLoadingMore = false
        }
        do {
            let page = try await director.archiveFeed(
                query: query.isEmpty ? nil : query,
                limit: Self.pageSize,
                offset: turns.count
            )
            guard query == normalizedSearchText else {
                return
            }
            let existingIDs = Set(turns.map(\.id))
            turns.append(contentsOf: page.turns.filter {
                !existingIDs.contains($0.id)
            })
            totalTurnCount = page.total
        } catch {
            guard query == normalizedSearchText else {
                return
            }
            errorMessage = error.localizedDescription
        }
    }
}

private struct UsageBoardContent: View {
    let refreshRequestID: UUID
    @Binding var isRefreshing: Bool
    let databaseBaseURL: URL
    let director: AgentDirector
    @State private var snapshot: AIUsageSnapshot?
    @State private var errorMessage: String?
    /// 카드를 누르면 그 제공자의 사용 현황 상세 시트를 연다.
    @State private var reportBackend: AgentBackend?
    @State private var updateStatus = CLIUpdateStatus.empty
    @State private var updatingPackageID: String?
    @State private var updateErrorMessage: String?
    @State private var updateErrorDismissTask: Task<Void, Never>?

    var body: some View {
        Group {
            if let snapshot {
                GeometryReader { proxy in
                    ScrollView {
                        VStack(spacing: 12) {
                            if UsageBoardLayout.usesSingleColumn(
                                for: proxy.size.width
                            ) {
                                VStack(spacing: 12) {
                                    providerColumns(snapshot)
                                }
                            } else {
                                HStack(alignment: .top, spacing: 12) {
                                    providerColumns(snapshot)
                                }
                            }
                        }
                        .padding(14)
                    }
                }
            } else if let errorMessage {
                VStack(spacing: 10) {
                    ContentUnavailableView(
                        OfficeLocalization.string("한도를 불러오지 못했습니다"),
                        systemImage: "wifi.exclamationmark",
                        description: Text(OfficeLocalization.systemMessage(errorMessage))
                    )
                    Button {
                        Task {
                            await refresh(force: true)
                        }
                    } label: {
                        Label(
                            OfficeLocalization.string("다시 시도"),
                            systemImage: "arrow.clockwise"
                        )
                    }
                    .disabled(isRefreshing)
                }
            } else {
                ProgressView(OfficeLocalization.string("계정 한도를 확인하는 중"))
            }
        }
        .task(id: refreshRequestID) {
            await refresh(force: snapshot != nil)
            await refreshUpdateStatus(force: false)
        }
        .overlay(alignment: .bottom) {
            if let updateErrorMessage {
                // 눌렀는데 아무 일도 안 일어난 것처럼 보이면 안 된다.
                // 눈에 띄게 띄우되 잠시 뒤 스스로 사라진다.
                Label(updateErrorMessage, systemImage: "exclamationmark.circle.fill")
                    .font(.system(size: 11.5, weight: .bold))
                    .foregroundStyle(Color.white)
                    .lineLimit(2)
                    .multilineTextAlignment(.leading)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 9)
                    .background(
                        Color(red: 0.80, green: 0.36, blue: 0.10),
                        in: Capsule()
                    )
                    .shadow(color: .black.opacity(0.22), radius: 8, y: 3)
                    .padding(.bottom, 12)
                    .transition(
                        .move(edge: .bottom).combined(with: .opacity)
                    )
                    .accessibilityLabel(OfficeLocalization.format("업데이트 실패 %@", updateErrorMessage))
            }
        }
        .animation(.easeOut(duration: 0.18), value: updateErrorMessage)
        .sheet(item: $reportBackend) { backend in
            UsageReportSheet(
                backend: backend,
                director: director,
                onClose: { reportBackend = nil }
            )
            .frame(
                minWidth: ArchiveBookSheetLayout.minimumWidth,
                idealWidth: ArchiveBookSheetLayout.idealWidth,
                minHeight: ArchiveBookSheetLayout.minimumHeight,
                idealHeight: ArchiveBookSheetLayout.idealHeight
            )
        }
    }

    /// 알림은 5초만 띄운다. 새 알림이 오면 이전 예약은 취소한다.
    private func showUpdateError(_ message: String) {
        updateErrorMessage = message
        updateErrorDismissTask?.cancel()
        updateErrorDismissTask = Task { @MainActor in
            try? await Task.sleep(for: .seconds(5))
            guard !Task.isCancelled else {
                return
            }
            if updateErrorMessage == message {
                updateErrorMessage = nil
            }
        }
    }

    private func refreshUpdateStatus(force: Bool) async {
        guard
            let status = try? await OfficeDatabaseClient(
                baseURL: databaseBaseURL
            ).fetchCLIUpdateStatus(force: force)
        else {
            return
        }
        updateStatus = status
    }

    /// CLI별 패키지 설치나 자체 갱신은 오래 걸릴 수 있다. 진행 중 업무가 있으면 백엔드가
    /// 막고 그 사유를 그대로 보여 준다.
    private func applyUpdate(packageID: String) {
        guard updatingPackageID == nil else {
            return
        }
        updatingPackageID = packageID
        updateErrorMessage = nil
        Task {
            defer {
                updatingPackageID = nil
            }
            do {
                updateStatus = try await OfficeDatabaseClient(
                    baseURL: databaseBaseURL
                ).applyCLIUpdates(packageID: packageID)
            } catch {
                showUpdateError(error.localizedDescription)
                await refreshUpdateStatus(force: true)
            }
        }
    }

    @ViewBuilder
    private func providerColumns(
        _ snapshot: AIUsageSnapshot
    ) -> some View {
        UsageProviderColumn(
            name: "Claude Code",
            icon: "sparkles",
            update: updateStatus.package(id: "claude"),
            isUpdating: updatingPackageID == "claude",
            applyUpdate: { applyUpdate(packageID: "claude") },
            fiveHour: snapshot.claudeFiveHour,
            fiveHourResetAt: snapshot.claudeFiveHourResetAt,
            weekly: snapshot.claudeWeekly,
            weeklyResetAt: snapshot.claudeWeeklyResetAt,
            fetchedAt: snapshot.fetchedAt,
            plan: snapshot.claudePlan,
            subscriptionExpiresAt: nil,
            activity: snapshot.claudeActivity,
            showsFiveHour: true,
            tint: DashboardPalette.providerAccent(for: .claude),
            openDetail: { reportBackend = .claude }
        )
        UsageProviderColumn(
            name: "Codex",
            icon: "terminal.fill",
            update: updateStatus.package(id: "codex"),
            isUpdating: updatingPackageID == "codex",
            applyUpdate: { applyUpdate(packageID: "codex") },
            fiveHour: snapshot.codexFiveHour,
            fiveHourResetAt: snapshot.codexFiveHourResetAt,
            weekly: snapshot.codexWeekly,
            weeklyResetAt: snapshot.codexWeeklyResetAt,
            fetchedAt: snapshot.fetchedAt,
            plan: snapshot.codexPlan,
            subscriptionExpiresAt: snapshot.codexSubscriptionExpiresAt,
            activity: snapshot.codexActivity,
            showsFiveHour: true,
            tint: DashboardPalette.providerAccent(for: .codex),
            openDetail: { reportBackend = .codex }
        )
        UsageProviderColumn(
            name: "Antigravity",
            icon: "wand.and.stars",
            update: updateStatus.package(id: "antigravity"),
            isUpdating: updatingPackageID == "antigravity",
            applyUpdate: { applyUpdate(packageID: "antigravity") },
            fiveHour: snapshot.antigravityFiveHour,
            fiveHourResetAt: snapshot.antigravityFiveHourResetAt,
            weekly: snapshot.antigravityWeekly,
            weeklyResetAt: snapshot.antigravityWeeklyResetAt,
            imageResetAt: snapshot.antigravityImageResetAt,
            fetchedAt: snapshot.fetchedAt,
            plan: snapshot.antigravityPlan,
            subscriptionExpiresAt: nil,
            activity: snapshot.antigravityActivity,
            showsFiveHour: true,
            tint: DashboardPalette.providerAccent(for: .antigravity),
            openDetail: { reportBackend = .antigravity }
        )
    }

    @MainActor
    private func refresh(
        force: Bool = false
    ) async {
        guard !isRefreshing else {
            return
        }
        isRefreshing = true
        defer {
            isRefreshing = false
        }

        do {
            snapshot = try await OfficeDatabaseClient(
                baseURL: databaseBaseURL
            ).fetchUsageSummary(force: force)
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

private struct UsageProviderColumn: View {
    let name: String
    let icon: String
    let update: CLIUpdateStatus.Package?
    let isUpdating: Bool
    let applyUpdate: () -> Void
    let fiveHour: Int?
    let fiveHourResetAt: Date?
    let weekly: Int?
    let weeklyResetAt: Date?
    var imageResetAt: Date? = nil
    let fetchedAt: Date
    let plan: String?
    let subscriptionExpiresAt: Date?
    let activity: AIUsageActivitySnapshot?
    let showsFiveHour: Bool
    let tint: Color
    let openDetail: () -> Void

    var body: some View {
        UsageProviderCard(
            name: name,
            icon: icon,
            update: update,
            isUpdating: isUpdating,
            applyUpdate: applyUpdate,
            fiveHour: fiveHour,
            fiveHourResetAt: fiveHourResetAt,
            weekly: weekly,
            weeklyResetAt: weeklyResetAt,
            imageResetAt: imageResetAt,
            fetchedAt: fetchedAt,
            plan: plan,
            subscriptionExpiresAt: subscriptionExpiresAt,
            activity: activity,
            showsFiveHour: showsFiveHour,
            tint: tint,
            openDetail: openDetail
        )
        .frame(maxWidth: .infinity)
    }
}

private struct UsageProviderCard: View {
    let name: String
    let icon: String
    let update: CLIUpdateStatus.Package?
    let isUpdating: Bool
    let applyUpdate: () -> Void
    let fiveHour: Int?
    let fiveHourResetAt: Date?
    let weekly: Int?
    let weeklyResetAt: Date?
    var imageResetAt: Date? = nil
    let fetchedAt: Date
    let plan: String?
    let subscriptionExpiresAt: Date?
    let activity: AIUsageActivitySnapshot?
    let showsFiveHour: Bool
    let tint: Color
    let openDetail: () -> Void

    var body: some View {
        // 보관함·위키 칸과 같은 plain 버튼이라 누를 때 같은 눌림 효과가 난다.
        // 안의 Update 버튼은 안쪽 버튼이 먼저 받으므로 상세가 같이 열리지 않는다.
        Button(action: openDetail) {
            card
                .contentShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
        }
        .buttonStyle(.plain)
        .help(OfficeLocalization.string("사용 현황 상세"))
        .accessibilityLabel(
            OfficeLocalization.format("%@ 사용 현황 상세 열기", name)
        )
    }

    private var card: some View {
        VStack(alignment: .leading, spacing: 11) {
            HStack(spacing: 6) {
                Label(name, systemImage: icon)
                    .font(.system(size: 13, weight: .bold))
                    .foregroundStyle(tint)

                if let plan, !plan.isEmpty {
                    Text(plan)
                        .font(.system(size: 8.5, weight: .bold))
                        .foregroundStyle(tint)
                        .lineLimit(1)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 3)
                        .background(tint.opacity(0.10), in: Capsule())
                }

                if let update, update.checkFailed == true {
                    // 조회 실패를 최신처럼 보이게 두면 사용자가 오래된
                    // 버전을 계속 쓰게 된다.
                    Text(OfficeLocalization.string("확인 실패"))
                        .font(.system(size: 8.5, weight: .bold))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .fixedSize()
                        .padding(.horizontal, 6)
                        .padding(.vertical, 3)
                        .background(Color.secondary.opacity(0.12), in: Capsule())
                        .help(OfficeLocalization.string("설치본이나 최신 버전을 확인하지 못했습니다."))
                        .accessibilityLabel(OfficeLocalization.format("%@ 버전 확인 실패", update.label))
                } else if let update, update.updateAvailable {
                    // 옆의 구독 배지와 같은 모양으로 둔다. 버전을 적으면
                    // 길어져 줄이 접히므로 문구는 고정하고 자세한 값은
                    // 도움말로 옮긴다.
                    Button(action: applyUpdate) {
                        Text(isUpdating ? OfficeLocalization.string("업데이트 중") : "Update")
                            .font(.system(size: 8.5, weight: .bold))
                            .foregroundStyle(tint)
                            .lineLimit(1)
                            .fixedSize()
                            .padding(.horizontal, 6)
                            .padding(.vertical, 3)
                            .background(tint.opacity(0.10), in: Capsule())
                    }
                    .buttonStyle(.plain)
                    .disabled(isUpdating)
                    .help(
                        OfficeLocalization.format(
                            "설치본 %@ → %@",
                            update.installedVersion ?? "?",
                            update.latestVersion ?? "?"
                        )
                    )
                    .accessibilityLabel(OfficeLocalization.format("%@ 업데이트", update.label))
                }

                Spacer()

                Text(availabilityText)
                    .font(.system(size: 10, weight: .bold))
                    .foregroundStyle(.secondary)
            }
            if showsFiveHour {
                UsageMeter(
                    label: OfficeLocalization.string("5시간"),
                    value: fiveHour,
                    resetAt: fiveHourResetAt,
                    fetchedAt: fetchedAt,
                    tint: tint
                )
            }
            UsageMeter(
                label: OfficeLocalization.string("7일"),
                value: weekly,
                resetAt: weeklyResetAt,
                fetchedAt: fetchedAt,
                tint: tint
            )
            if let imageResetAt {
                UsageResetCountdown(
                    resetAt: imageResetAt,
                    fetchedAt: fetchedAt
                ) { reset in
                        HStack(spacing: 4) {
                            Label(
                                OfficeLocalization.string("이미지 쿨다운"),
                                systemImage: "photo.badge.exclamationmark"
                            )
                            .foregroundStyle(
                                Color(red: 0.85, green: 0.45, blue: 0.12)
                            )
                            Spacer()
                            Text(OfficeLocalization.format("초기화까지 %@", reset))
                                .font(.system(size: 8.5, weight: .semibold))
                                .foregroundStyle(
                                    Color(red: 0.85, green: 0.45, blue: 0.12)
                                )
                        }
                        .font(.system(size: 8.5, weight: .semibold))
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                        .background(
                            Color(red: 0.85, green: 0.45, blue: 0.12)
                                .opacity(0.10),
                            in: RoundedRectangle(
                                cornerRadius: 6,
                                style: .continuous
                            )
                        )
                }
            }
            UsageActivitySummary(
                activity: activity,
                tint: tint
            )
            if let subscriptionExpiresAt {
                UsageSubscriptionSummary(
                    expiresAt: subscriptionExpiresAt,
                    tint: tint
                )
            }
        }
        .padding(13)
        .background(
            tint.opacity(0.065),
            in: RoundedRectangle(cornerRadius: 14, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .stroke(tint.opacity(0.13))
        }
    }

    private var availabilityText: String {
        let values = showsFiveHour ? [fiveHour, weekly] : [weekly]
        guard let lowest = values.compactMap({ $0 }).min()
        else {
            return OfficeLocalization.string("정보 없음")
        }
        return lowest > 25 ? OfficeLocalization.string("사용 가능") : OfficeLocalization.string("잔여량 낮음")
    }
}

private struct UsageSubscriptionSummary: View {
    let expiresAt: Date?
    let tint: Color

    var body: some View {
        HStack(spacing: 4) {
            Label(
                OfficeLocalization.string("상품 만료"),
                systemImage: "calendar.badge.clock"
            )
            .foregroundStyle(tint)

            Spacer(minLength: 2)

            Text(expirationText)
                .fontWeight(.bold)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .minimumScaleFactor(0.72)
        }
        .font(.system(size: 8.5, weight: .medium))
        .accessibilityElement(children: .combine)
    }

    private var expirationText: String {
        guard let expiresAt else {
            return "–"
        }
        return Self.expirationFormatter.string(from: expiresAt)
    }

    private static let expirationFormatter: DateFormatter = {
        let formatter = DateFormatter()
        formatter.locale = OfficeLocalization.locale
        formatter.timeZone = .autoupdatingCurrent
        formatter.setLocalizedDateFormatFromTemplate("MdHm")
        return formatter
    }()
}

private struct UsageActivitySummary: View {
    let activity: AIUsageActivitySnapshot?
    let tint: Color

    var body: some View {
        HStack(spacing: 4) {
            Label(
                OfficeLocalization.string("API 요금 추정"),
                systemImage: "chart.bar.xaxis"
            )
            .foregroundStyle(tint)

            Spacer(minLength: 2)

            HStack(spacing: 2) {
                Text(OfficeLocalization.string("오늘 비용"))
                Text(costText(activity?.todayCostUSD))
                    .fontWeight(.bold)
                Text("/")
                Text(OfficeLocalization.string("30일 비용"))
                Text(costText(activity?.last30DaysCostUSD))
                    .fontWeight(.bold)
            }
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .minimumScaleFactor(0.72)
        }
        // 현재 한도 초기화 시각과 같은 크기로 카드 하단 한 줄만 쓴다.
        .font(.system(size: 8.5, weight: .medium))
        .accessibilityElement(children: .combine)
    }

    private func costText(_ value: Double?) -> String {
        guard let value else {
            return "–"
        }
        return String(format: "$%.2f", value)
    }

}

private struct UsageResetCountdown<Content: View>: View {
    let resetAt: Date?
    let fetchedAt: Date
    let content: (String) -> Content

    @State private var remainingMinutes: Int?

    init(
        resetAt: Date?,
        fetchedAt: Date,
        @ViewBuilder content: @escaping (String) -> Content
    ) {
        self.resetAt = resetAt
        self.fetchedAt = fetchedAt
        self.content = content
    }

    var body: some View {
        VStack(spacing: 0) {
            if let text = usageResetRemainingText(minutes: remainingMinutes) {
                content(text)
            }
        }
        .task(id: fetchedAt) {
            await countDownFromFetchedSnapshot()
        }
    }

    @MainActor
    private func countDownFromFetchedSnapshot() async {
        remainingMinutes = usageResetRemainingMinutes(
            resetAt,
            relativeTo: fetchedAt
        )

        while let current = remainingMinutes, current > 0 {
            do {
                try await Task.sleep(for: .seconds(60))
            } catch {
                return
            }
            guard !Task.isCancelled else {
                return
            }
            remainingMinutes = current - 1
        }
    }
}

private struct UsageMeter: View {
    let label: String
    let value: Int?
    let resetAt: Date?
    let fetchedAt: Date
    let tint: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 9) {
                Text(label)
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(.secondary)
                    .frame(width: 32, alignment: .leading)

                GeometryReader { geometry in
                    ZStack(alignment: .leading) {
                        Capsule()
                            .fill(Color.primary.opacity(0.075))
                        Capsule()
                            .fill(tint)
                            .frame(
                                width: geometry.size.width
                                    * CGFloat(value ?? 0) / 100
                            )
                    }
                }
                .frame(height: 6)

                Text(value.map { "\($0)%" } ?? "–")
                    .font(
                        .system(size: 10, weight: .bold, design: .rounded)
                    )
                    .frame(width: 34, alignment: .trailing)
            }

            UsageResetCountdown(
                resetAt: resetAt,
                fetchedAt: fetchedAt
            ) { reset in
                Label(
                    OfficeLocalization.format("초기화까지 %@", reset),
                    systemImage: "clock.arrow.circlepath"
                )
                .font(.system(size: 8.5, weight: .semibold))
                .foregroundStyle(.tertiary)
                .frame(maxWidth: .infinity, alignment: .trailing)
            }
        }
    }
}

struct LiveWorkspaceFeedFollowState: Equatable {
    private(set) var isFollowingLatest = true

    mutating func userWillScroll() {
        guard isFollowingLatest else {
            return
        }
        isFollowingLatest = false
    }

    mutating func userDidScroll(
        distanceFromBottom: CGFloat,
        tolerance: CGFloat
    ) {
        let shouldFollowLatest = distanceFromBottom <= tolerance
        guard isFollowingLatest != shouldFollowLatest else {
            return
        }
        isFollowingLatest = shouldFollowLatest
    }

    mutating func resume() {
        guard !isFollowingLatest else {
            return
        }
        isFollowingLatest = true
    }
}

enum LiveWorkspaceFeedContentRevisionAction: Equatable {
    case settleInitialAnchor
    case followLatest
    case revealContentBelow
}

struct LiveWorkspaceFeedContentRevisionPolicy: Equatable {
    static func action(
        didPerformInitialScroll: Bool,
        isFollowingLatest: Bool
    ) -> LiveWorkspaceFeedContentRevisionAction {
        guard didPerformInitialScroll else {
            return .settleInitialAnchor
        }
        return isFollowingLatest ? .followLatest : .revealContentBelow
    }
}

struct LiveWorkspaceFeedSubmissionScrollPolicy: Equatable {
    static func shouldRevealSubmittedTurn(
        isFollowingLatest: Bool
    ) -> Bool {
        // 사용자가 과거 대화를 읽는 중에는 새 optimistic 카드가 들어와도
        // 기존 viewport를 유지한다. 이때 하단 scrollTo를 강제하면 문서
        // 높이 변경과 충돌해 빈 영역·스크롤바 튐·레이아웃 폭주를 만든다.
        isFollowingLatest
    }
}

struct LiveWorkspaceFeedPagingPolicy: Equatable {
    // 평소 대화는 스냅샷이 보유한 최근 10턴을 그대로 보여준다.
    // 다만 긴 프롬프트와 수백 개 활동이 섞인 직원은 첫 mount에서
    // TextKit이 모든 카드를 한꺼번에 측정하며 CPU를 오래 점유한다.
    // 그런 피드만 최근 턴부터 작은 창으로 시작하고, 기존 상단
    // 페이지 불러오기로 나머지를 동일하게 열 수 있게 한다.
    static let initialVisibleTurnCount = 10
    static let minimumInitialVisibleTurnCount = 2
    static let initialLayoutWeightBudget = 64_000
    static let activityLayoutOverhead = 240
    static let pageSize = 3
    static let maximumVisibleTurnCount = 30

    static func initialVisibleTurnCount(
        for turnsNewestFirst: [LiveFeedTurn]
    ) -> Int {
        let weights = turnsNewestFirst
            .prefix(initialVisibleTurnCount)
            .map { turn in
                turn.prompt.utf8.count
                    + turn.response.utf8.count
                    + turn.activities.reduce(0) { partialResult, activity in
                        partialResult
                            + activity.text.utf8.count
                            + activityLayoutOverhead
                    }
            }
        return initialVisibleTurnCount(layoutWeightsNewestFirst: weights)
    }

    static func initialVisibleTurnCount(
        layoutWeightsNewestFirst: [Int]
    ) -> Int {
        guard !layoutWeightsNewestFirst.isEmpty else {
            return initialVisibleTurnCount
        }

        let boundedWeights = layoutWeightsNewestFirst.prefix(
            initialVisibleTurnCount
        )
        var visibleCount = 0
        var accumulatedWeight = 0
        for rawWeight in boundedWeights {
            let weight = max(0, rawWeight)
            if visibleCount >= minimumInitialVisibleTurnCount,
               accumulatedWeight + weight > initialLayoutWeightBudget
            {
                break
            }
            accumulatedWeight += weight
            visibleCount += 1
        }
        return visibleCount
    }

    static func includesTurn(
        at index: Int,
        visibleTurnLimit: Int,
        isRunning: Bool,
        isLatestTerminalTurn: Bool
    ) -> Bool {
        index < visibleTurnLimit
            || isRunning
            || isLatestTerminalTurn
    }

    static func nextVisibleTurnLimit(
        current: Int,
        total: Int
    ) -> Int {
        min(
            current + pageSize,
            maximumVisibleTurnCount,
            total
        )
    }
}

// 최신 N개 인덱스로 매번 다시 자르면 새 턴이 앞에 삽입될 때 기존
// 카드가 밀려나 문서 높이가 급락하고 viewport가 문서 밖에 남는다.
// 가장 오래된 표시 턴의 ID를 앵커로 고정해, 앵커보다 새로운 턴은
// 삽입·ID 교체 중에도 전부 창에 포함시킨다.
struct LiveWorkspaceFeedDisplayAnchor: Equatable {
    private(set) var oldestVisibleTurnID: String?
    private(set) var lastKnownLimit: Int

    init(
        initialLimit: Int =
            LiveWorkspaceFeedPagingPolicy.initialVisibleTurnCount
    ) {
        lastKnownLimit = max(1, initialLimit)
    }

    func effectiveLimit(turnIDsNewestFirst: [String]) -> Int {
        guard
            let oldestVisibleTurnID,
            let index = turnIDsNewestFirst.firstIndex(
                of: oldestVisibleTurnID
            )
        else {
            // 앵커가 아직 없거나(첫 마운트) 목록에서 사라졌으면
            // (optimistic ID 교체·정리) 직전 표시 규모를 유지해
            // 창이 다시 줄어들며 카드가 제거되는 일을 막는다.
            return lastKnownLimit
        }
        return max(index + 1, 1)
    }

    func pinning(turnIDsNewestFirst: [String]) -> Self {
        pinning(
            limit: effectiveLimit(turnIDsNewestFirst: turnIDsNewestFirst),
            turnIDsNewestFirst: turnIDsNewestFirst
        )
    }

    func pinning(
        limit: Int,
        turnIDsNewestFirst: [String]
    ) -> Self {
        let boundedLimit = min(limit, turnIDsNewestFirst.count)
        guard boundedLimit > 0 else {
            return self
        }
        var pinned = self
        pinned.oldestVisibleTurnID = turnIDsNewestFirst[boundedLimit - 1]
        pinned.lastKnownLimit = boundedLimit
        return pinned
    }
}

struct LiveWorkspaceFeedScrollPolicy: Equatable {
    // 제출 직후 하단 보정은 레이아웃 확정 뒤 한 번만 수행한다.
    // 반복 보정은 문서 높이가 변하는 중에 스크롤바 왕복을 만든다.
    static let submittedMaximumAttempts = 1
    static let stablePassesRequired = 2

    private(set) var stablePassCount = 0

    mutating func shouldStop(
        distanceFromBottom: CGFloat,
        tolerance: CGFloat
    ) -> Bool {
        if distanceFromBottom <= tolerance {
            stablePassCount += 1
        } else {
            stablePassCount = 0
        }
        return stablePassCount >= Self.stablePassesRequired
    }
}

struct LiveWorkspaceFeedTopLoadGate: Equatable {
    // 한 번 로드하면 상단 임계 밖으로 나갔다 돌아와야 다시 장전된다.
    // 제스처 시작마다 재장전하면 짧은 휠 세션이 연달아 페이지를
    // 연쇄 로드해 무거운 과거 카드가 한꺼번에 쌓인다. 정상 흐름은
    // 로드 직후 위쪽에 새 카드가 붙어 임계 밖으로 밀려나므로
    // 자연스럽게 재장전된다.
    private(set) var isArmed = true

    mutating func shouldLoad(
        distanceFromTop: CGFloat,
        threshold: CGFloat,
        isProgrammaticScrollInFlight: Bool
    ) -> Bool {
        if distanceFromTop > threshold {
            isArmed = true
            return false
        }
        guard
            !isProgrammaticScrollInFlight,
            isArmed
        else {
            return false
        }
        isArmed = false
        return true
    }
}

struct CachedLiveWorkspaceFeeds: NSViewRepresentable {
    @ObservedObject private var characterSelectionStore:
        CharacterSelectionStore
    private let director: AgentDirector

    @MainActor
    init(director: AgentDirector) {
        self.director = director
        _characterSelectionStore = ObservedObject(
            wrappedValue: director.characterSelectionStore
        )
    }

    func makeNSView(context: Context) -> CachedLiveWorkspaceFeedsNSView {
        let view = CachedLiveWorkspaceFeedsNSView()
        view.configure(
            director: director,
            selectedCharacterID:
                characterSelectionStore.selectedCharacterID
        )
        return view
    }

    func updateNSView(
        _ nsView: CachedLiveWorkspaceFeedsNSView,
        context: Context
    ) {
        nsView.configure(
            director: director,
            selectedCharacterID:
                characterSelectionStore.selectedCharacterID
        )
    }

    func sizeThatFits(
        _ proposal: ProposedViewSize,
        nsView: CachedLiveWorkspaceFeedsNSView,
        context: Context
    ) -> CGSize? {
        guard
            let width = resolvedDimension(
                proposal.width,
                fallback: nsView.bounds.width
            ),
            let height = resolvedDimension(
                proposal.height,
                fallback: nsView.bounds.height
            )
        else {
            return nil
        }
        return CGSize(width: width, height: height)
    }

    static func dismantleNSView(
        _ nsView: CachedLiveWorkspaceFeedsNSView,
        coordinator: ()
    ) {
        nsView.tearDown()
    }

    private func resolvedDimension(
        _ proposed: CGFloat?,
        fallback: CGFloat
    ) -> CGFloat? {
        if let proposed, proposed.isFinite, proposed > 0 {
            return proposed
        }
        guard fallback.isFinite, fallback > 0 else {
            return nil
        }
        return fallback
    }
}

struct LiveWorkspaceFeedMetadata: Equatable {
    let latestTerminalTurnID: String?
    let latestSubmittedCommandID: UUID?
    let latestStartedCommandID: UUID?

    init(
        latestTerminalTurnID: String?,
        latestSubmittedCommandID: UUID?,
        latestStartedCommandID: UUID?
    ) {
        self.latestTerminalTurnID = latestTerminalTurnID
        self.latestSubmittedCommandID = latestSubmittedCommandID
        self.latestStartedCommandID = latestStartedCommandID
    }

    @MainActor
    init(director: AgentDirector) {
        latestTerminalTurnID = director.latestTerminalTurnID
        latestSubmittedCommandID = director.latestSubmittedCommandID
        latestStartedCommandID = director.latestStartedCommandID
    }
}

@MainActor
final class LiveWorkspaceFeedMetadataStore: ObservableObject {
    @Published private(set) var metadata: LiveWorkspaceFeedMetadata
    private var desiredMetadata: LiveWorkspaceFeedMetadata
    private var publicationTask: Task<Void, Never>?

    init(metadata: LiveWorkspaceFeedMetadata) {
        self.metadata = metadata
        desiredMetadata = metadata
    }

    func setMetadata(_ metadata: LiveWorkspaceFeedMetadata) {
        guard desiredMetadata != metadata else {
            return
        }
        desiredMetadata = metadata
        publicationTask?.cancel()
        publicationTask = Task { [weak self] in
            // updateNSView 안에서 ObservableObject를 즉시 발행하면 SwiftUI가
            // 호스트 갱신 도중 다시 무효화될 수 있으므로 다음 MainActor
            // 차례에 마지막 snapshot만 한 번 전달한다.
            await Task.yield()
            guard
                let self,
                !Task.isCancelled,
                self.desiredMetadata == metadata
            else {
                return
            }
            if self.metadata != metadata {
                self.metadata = metadata
            }
            self.publicationTask = nil
        }
    }
}

@MainActor
final class LiveWorkspaceFeedPresentationStore: ObservableObject {
    @Published private(set) var isPresented: Bool
    private var desiredIsPresented: Bool
    private var publicationTask: Task<Void, Never>?

    var isPresentationRequested: Bool {
        desiredIsPresented
    }

    init(isPresented: Bool) {
        self.isPresented = isPresented
        desiredIsPresented = isPresented
    }

    func setPresented(_ isPresented: Bool) {
        guard desiredIsPresented != isPresented else {
            return
        }
        desiredIsPresented = isPresented
        publicationTask?.cancel()
        publicationTask = Task { [weak self] in
            await Task.yield()
            guard
                let self,
                !Task.isCancelled,
                self.desiredIsPresented == isPresented
            else {
                return
            }
            if self.isPresented != isPresented {
                self.isPresented = isPresented
            }
            self.publicationTask = nil
        }
    }
}

private struct HostedLiveWorkspaceFeed: View {
    let director: AgentDirector
    let characterID: OfficeCharacter
    @ObservedObject var metadataStore: LiveWorkspaceFeedMetadataStore
    @ObservedObject private var characterFeedStore: CharacterLiveFeedStore
    let presentationStore: LiveWorkspaceFeedPresentationStore
    let onMountReady: (Int) -> LiveWorkspaceFeedMountReadiness

    init(
        director: AgentDirector,
        characterID: OfficeCharacter,
        metadataStore: LiveWorkspaceFeedMetadataStore,
        presentationStore: LiveWorkspaceFeedPresentationStore,
        onMountReady: @escaping (Int) -> LiveWorkspaceFeedMountReadiness
    ) {
        self.director = director
        self.characterID = characterID
        self.metadataStore = metadataStore
        _characterFeedStore = ObservedObject(
            wrappedValue: director.liveFeedStore.characterStore(
                for: characterID.rawValue
            )
        )
        self.presentationStore = presentationStore
        self.onMountReady = onMountReady
    }

    var body: some View {
        LiveWorkspaceFeed(
            director: director,
            characterID: characterID,
            presentationStore: presentationStore,
            metadata: metadataStore.metadata
        )
        .equatable()
        .environment(
            \.liveWorkspaceFeedPresentationStore,
            presentationStore
        )
        .environment(\.locale, OfficeLocalization.locale)
        // 긴 대화 전체의 Text에 SelectionOverlay가 붙으면 스크롤 중
        // AttributeGraph 레이아웃이 끝나지 않을 수 있다. 피드 경계에서는
        // 선택을 끄고, 질문·응답 카드별로 고정된 선택 영역을 제공한다.
        .textSelection(.disabled)
        .background { ConversationPointerMoveBoundary() }
        .overlay {
            LiveWorkspaceFeedMountReadyReporter(
                readinessRevision:
                    characterFeedStore.mountReadinessRevision,
                onReady: onMountReady
            )
            .allowsHitTesting(false)
        }
    }
}

// 직원 전환 중 새 SwiftUI 트리가 실제 window와 유효한 크기를 얻은
// 시점만 AppKit 컨테이너에 알린다. documentView의 비동기 layout 이벤트가
// overlay까지 전파되지 않는 경우를 위해 mount당 3초로 제한된 재확인만
// 허용하며, 이후에는 자연스러운 layout/update 이벤트만 받는다.
private enum LiveWorkspaceFeedMountReadiness: Equatable {
    case waitingForLayout
    case confirmValidLayout
    case ready
}

private struct LiveWorkspaceFeedMountReadyReporter: NSViewRepresentable {
    let readinessRevision: Int
    let onReady: (Int) -> LiveWorkspaceFeedMountReadiness

    func makeNSView(context: Context) -> AttachmentView {
        let view = AttachmentView()
        view.update(
            readinessRevision: readinessRevision,
            onReady: onReady
        )
        return view
    }

    func updateNSView(_ nsView: AttachmentView, context: Context) {
        nsView.update(
            readinessRevision: readinessRevision,
            onReady: onReady
        )
    }

    static func dismantleNSView(
        _ nsView: AttachmentView,
        coordinator: ()
    ) {
        nsView.onReady = nil
    }

    final class AttachmentView: NSView {
        private static let automaticCheckInterval = TimeInterval(0.05)
        private static let maximumAutomaticCheckCount = 60

        var onReady: ((Int) -> LiveWorkspaceFeedMountReadiness)?
        private var didReportReady = false
        private var isReportScheduled = false
        private var automaticChecksRemaining = maximumAutomaticCheckCount
        private var readinessRevision: Int?

        func update(
            readinessRevision: Int,
            onReady: @escaping (Int) -> LiveWorkspaceFeedMountReadiness
        ) {
            self.onReady = onReady
            if self.readinessRevision != readinessRevision {
                self.readinessRevision = readinessRevision
                automaticChecksRemaining = Self.maximumAutomaticCheckCount
            }
            scheduleReportIfNeeded()
        }

        override func layout() {
            super.layout()
            scheduleReportIfNeeded(isAutomatic: false)
        }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            scheduleReportIfNeeded(isAutomatic: false)
        }

        func scheduleReportIfNeeded(isAutomatic: Bool = false) {
            guard
                !didReportReady,
                !isReportScheduled,
                window != nil,
                bounds.width > 0,
                bounds.height > 0
            else {
                return
            }
            if isAutomatic {
                guard automaticChecksRemaining > 0 else {
                    return
                }
                automaticChecksRemaining -= 1
            }
            isReportScheduled = true
            let report = { [weak self] in
                guard let self else {
                    return
                }
                self.isReportScheduled = false
                guard
                    !self.didReportReady,
                    self.window != nil,
                    self.bounds.width > 0,
                    self.bounds.height > 0
                else {
                    return
                }
                switch self.onReady?(
                    self.readinessRevision ?? -1
                ) ?? .waitingForLayout {
                case .waitingForLayout:
                    // 실제 documentView의 뒤늦은 높이 확정은 이 overlay의
                    // layout을 다시 호출하지 않을 수 있다. mount 동안만
                    // 제한된 횟수로 재확인하고, 예산을 다 쓰면 자연스러운
                    // layout/update 이벤트만 기다린다.
                    self.scheduleReportIfNeeded(isAutomatic: true)
                case .confirmValidLayout:
                    self.scheduleReportIfNeeded(isAutomatic: true)
                case .ready:
                    self.didReportReady = true
                }
            }
            if isAutomatic {
                DispatchQueue.main.asyncAfter(
                    deadline: .now() + Self.automaticCheckInterval,
                    execute: report
                )
            } else {
                DispatchQueue.main.async(execute: report)
            }
        }
    }
}

// SwiftUI 화면을 캡처하면 긴 Claude transcript를 메인 스레드에서 다시
// layout/draw하며 수 초간 멈출 수 있다. 전환 차폐는 캡처나 SwiftUI graph가
// 없는 가벼운 AppKit 뷰로 고정한다.
private final class LiveWorkspaceFeedLoadingGateView: NSView {
    private static let indicatorSize = CGFloat(32)
    private let indicator: NSProgressIndicator

    // isOpaque를 선언하고 상위 레이어에 직접 그리면 AppKit이 뒤쪽
    // 그리기를 건너뛰어 배경 채움이 대화 영역 밖까지 번진다. 자기
    // 레이어의 배경색만 쓰고 불투명 선언은 하지 않는다.
    override var wantsUpdateLayer: Bool {
        true
    }

    override init(frame frameRect: NSRect) {
        indicator = NSProgressIndicator()
        indicator.style = .spinning
        indicator.controlSize = .large
        indicator.isIndeterminate = true
        indicator.isDisplayedWhenStopped = false
        indicator.identifier = NSUserInterfaceItemIdentifier(
            "live-workspace-feed-loading-spinner"
        )
        super.init(frame: frameRect)
        identifier = NSUserInterfaceItemIdentifier(
            "live-workspace-feed-loading-gate"
        )
        wantsLayer = true
        layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        autoresizingMask = [.width, .height]
        addSubview(indicator)
        indicator.startAnimation(nil)
        setAccessibilityElement(true)
        setAccessibilityRole(.group)
        setAccessibilityLabel(
            OfficeLocalization.string("대화를 불러오는 중")
        )
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override func updateLayer() {
        layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
    }

    override func layout() {
        super.layout()
        let side = Self.indicatorSize
        indicator.frame = NSRect(
            x: bounds.midX - side / 2,
            y: bounds.midY - side / 2,
            width: side,
            height: side
        )
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if window == nil {
            indicator.stopAnimation(nil)
        } else {
            indicator.startAnimation(nil)
        }
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        needsDisplay = true
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        bounds.contains(point) ? self : nil
    }

    override func scrollWheel(with event: NSEvent) {}
    override func mouseDown(with event: NSEvent) {}
    override func mouseUp(with event: NSEvent) {}
}

@MainActor
final class CachedLiveWorkspaceFeedsNSView: NSView {
    private struct ViewportLayoutSignature: Equatable {
        let documentWidth: Int
        let documentHeight: Int
        let viewportWidth: Int
        let viewportHeight: Int

        init(documentBounds: NSRect, viewportBounds: NSRect) {
            func bucket(_ value: CGFloat) -> Int {
                Int((value * 2).rounded())
            }
            documentWidth = bucket(documentBounds.width)
            documentHeight = bucket(documentBounds.height)
            viewportWidth = bucket(viewportBounds.width)
            viewportHeight = bucket(viewportBounds.height)
        }
    }

    @MainActor
    private final class Entry {
        let characterID: OfficeCharacter
        let characterFeedStore: CharacterLiveFeedStore
        let metadataStore: LiveWorkspaceFeedMetadataStore
        let presentationStore: LiveWorkspaceFeedPresentationStore
        let transitionID: UUID
        // 전환이 오래 안 풀리는 드문 증상의 증거를 남기기 위한 값이다.
        let startedAt = Date()
        var stallPolicy = LiveWorkspaceFeedStallPolicy()
        // 리사이즈 직전에 하단을 보고 있었는지 기억한다. 크기가 바뀐
        // 뒤에는 이전 상태를 알 수 없어 미리 담아 둔다.
        var wasAtBottomBeforeResize = true
        var stablePreClampPassCount = 0
        var stablePostClampPassCount = 0
        var lastClampedViewportSignature: ViewportLayoutSignature?
        var viewportClampCount = 0
        var lastPreparedReadinessRevision: Int?
        var didPrepareFirstDisplay = false
        var didRequestPostMountRefresh = false
        var lastViewportSignature: ViewportLayoutSignature?
        // NSView로 타입을 소거해 생성 이후 rootView 재할당을 컴파일 단계에서
        // 막는다. 메타데이터 갱신은 metadataStore만 담당한다.
        let hostingView: NSView

        init(
            director: AgentDirector,
            characterID: OfficeCharacter,
            metadata: LiveWorkspaceFeedMetadata,
            transitionID: UUID,
            onMountReady: @escaping (Int) -> LiveWorkspaceFeedMountReadiness
        ) {
            self.characterID = characterID
            characterFeedStore = director.liveFeedStore.characterStore(
                for: characterID.rawValue
            )
            self.transitionID = transitionID
            let metadataStore = LiveWorkspaceFeedMetadataStore(
                metadata: metadata
            )
            self.metadataStore = metadataStore
            // Entry는 선택된 직원에게만 처음 생성된다. 첫 mount부터 표시
            // 상태로 만들면 재시작 뒤 첫 직원 진입에서 false→true 발행을
            // 기다리는 동안 빈 호스트가 노출되지 않는다.
            let presentationStore = LiveWorkspaceFeedPresentationStore(
                isPresented: true
            )
            self.presentationStore = presentationStore
            hostingView = NSHostingView(
                rootView: HostedLiveWorkspaceFeed(
                    director: director,
                    characterID: characterID,
                    metadataStore: metadataStore,
                    presentationStore: presentationStore,
                    onMountReady: onMountReady
                )
            )
        }

        func updateMetadata(_ metadata: LiveWorkspaceFeedMetadata) {
            metadataStore.setMetadata(metadata)
        }
    }

    private weak var director: AgentDirector?
    private var selectionWillChangeCancellable: AnyCancellable?
    private var activeEntry: Entry?
    private var pendingEntry: Entry?
    private var transitionLoadingGateView: LiveWorkspaceFeedLoadingGateView?
    private var transitionLoadingGateInstallCount = 0
    private var selectedCharacterID: OfficeCharacter?
    private var selectionGeneration = 0
    private var lastLaidOutBounds = CGRect.zero
    /// 리사이즈 직후에는 SwiftUI가 카드를 다시 실체화하며 문서 높이를
    /// 늦게 바꾼다. 그동안은 문서 변화가 올 때마다 보기 위치를 다시
    /// 맞춘다. 한 번만 맞추면 그 뒤 급락에서 문서 밖에 남는다.
    private var resizeSettleUntil: Date?
    private static let resizeSettleWindow = TimeInterval(1.2)
    // 드물게 나타나는 백화 증상의 증거를 남기는 함정이다. 증상이
    // 없으면 알림 관찰만 하고 아무것도 기록하지 않는다.
    private let stallRecorder = LiveWorkspaceFeedStallRecorder.live()
    private var blankDetector = LiveWorkspaceFeedBlankDetector()
    private var jitterDetector = LiveWorkspaceFeedJitterDetector()
    private var lastJitterReportAt: Date?
    private var lastResizeSampleAt: Date?
    private var blankStartedAt: Date?
    private var blankObservers: [NSObjectProtocol] = []
    private var blankDeadline: DispatchWorkItem?
    private var didReportCurrentBlank = false
    private var sessionReportCount = 0
    // 대화를 덮고 있는 차폐의 수명을 직접 잰다. 준비 콜백은 3초 뒤
    // 멈추므로 콜백에 기대면 오래 덮인 구간을 놓친다.
    private var gateInstalledAt: Date?
    private var gateWatchdog: Timer?
    private var gatePolicy = LiveWorkspaceFeedStallPolicy()
    /// 차폐 없이 쓰면 어떤 증상이 생기는지 실측하기 위한 실험 스위치다.
    /// 기본값은 꺼짐이며 환경변수를 준 빌드에서만 켜진다.
    static let isTransitionGateDisabled =
        ProcessInfo.processInfo
            .environment["OFFICESTRA_DISABLE_TRANSITION_GATE"] == "1"
    /// 이 시간을 넘기면 정착을 더 기다리지 않고 대화를 드러낸다.
    static let maximumGateCoverage = TimeInterval(0.8)
    private static let resizeBottomTolerance = CGFloat(20)

    var activeCharacterIDForTesting: OfficeCharacter? {
        activeEntry?.characterID
    }

    var pendingCharacterIDForTesting: OfficeCharacter? {
        pendingEntry?.characterID
    }

    var hasTransitionLoadingGateForTesting: Bool {
        transitionLoadingGateView?.superview === self
    }

    var transitionLoadingGateInstallCountForTesting: Int {
        transitionLoadingGateInstallCount
    }

    var liveHostingViewCountForTesting: Int {
        [activeEntry, pendingEntry].compactMap { $0 }.count { entry in
            entry.hostingView.superview === self
        }
    }

    var viewportClampCountForTesting: Int {
        pendingEntry?.viewportClampCount
            ?? activeEntry?.viewportClampCount
            ?? 0
    }

    var mountedCardCountForTesting: Int {
        guard let activeEntry else {
            return 0
        }
        return blankSample(for: activeEntry)?.mountedCardCount ?? 0
    }

    var visibleCardCountForTesting: Int {
        guard let activeEntry else {
            return 0
        }
        return blankSample(for: activeEntry)?.visibleCardCount ?? 0
    }

    func updateMetadataForTesting(_ metadata: LiveWorkspaceFeedMetadata) {
        activeEntry?.updateMetadata(metadata)
        pendingEntry?.updateMetadata(metadata)
    }

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        // NSView는 하위 뷰를 기본적으로 잘라내지 않는다. 전환 차폐와
        // 대화 host가 대화 영역 밖으로 그려져 좌측 사무실 화면이나
        // 아래 입력·직원 선택 영역까지 덮지 않도록 경계를 고정한다.
        clipsToBounds = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    // 크기가 바뀌기 전에 호출되므로 여기서 이전 보기 위치를 기록한다.
    override func setFrameSize(_ newSize: NSSize) {
        if newSize != frame.size {
            captureViewportAnchorBeforeResize()
        }
        super.setFrameSize(newSize)
    }

    private func captureViewportAnchorBeforeResize() {
        guard
            let activeEntry,
            let scrollView = primaryScrollView(in: activeEntry.hostingView),
            let documentView = scrollView.documentView
        else {
            return
        }
        let snapshot = LiveWorkspaceFeedScrollGeometry.snapshot(
            documentBounds: documentView.bounds,
            visibleRect: scrollView.documentVisibleRect,
            isFlipped: documentView.isFlipped
        )
        activeEntry.wasAtBottomBeforeResize =
            snapshot.distanceFromBottom <= Self.resizeBottomTolerance
    }

    override func layout() {
        super.layout()
        let didResize = lastLaidOutBounds != bounds
        for subview in subviews where subview.frame != bounds {
            subview.frame = bounds
        }
        guard didResize else {
            return
        }
        lastLaidOutBounds = bounds
        resizeSettleUntil = Date().addingTimeInterval(Self.resizeSettleWindow)
        // 입력창이 여러 줄로 커지면 대화 영역 높이가 줄어든다. 이때
        // clip view가 이전 높이 기준 좌표에 남으면 문서 밖을 보게 되어
        // 대화가 사라진 것처럼 보인다. 활성 대화의 보기 위치를 문서
        // 안으로 되돌린다.
        if let activeEntry {
            restoreViewportAfterResize(for: activeEntry)
        }
    }

    // 리사이즈 직전 하단에 있었으면 하단을, 아니면 보던 위치를 문서
    // 범위 안으로 제한해 유지한다. 사용자가 과거를 읽는 중에 임의로
    // 하단으로 끌어내리지 않는다.
    private func restoreViewportAfterResize(
        for entry: Entry,
        constrainOnly: Bool = false
    ) {
        let host = entry.hostingView
        guard
            host.bounds.width > 0,
            host.bounds.height > 0,
            let scrollView = primaryScrollView(in: host),
            let documentView = scrollView.documentView
        else {
            return
        }
        host.layoutSubtreeIfNeeded()
        scrollView.layoutSubtreeIfNeeded()
        documentView.layoutSubtreeIfNeeded()

        let clipView = scrollView.contentView
        let documentBounds = documentView.bounds
        let viewportHeight = clipView.bounds.height
        guard
            documentBounds.height.isFinite,
            documentBounds.height > 0,
            viewportHeight.isFinite,
            viewportHeight > 0
        else {
            return
        }

        var proposedBounds = clipView.bounds
        // 정착 구간의 재보정은 문서 범위 안으로 되돌리기만 한다. 여기서
        // 하단으로 끌면 과거를 읽는 중에 화면이 끌려 내려간다.
        if entry.wasAtBottomBeforeResize, !constrainOnly {
            proposedBounds.origin.y = documentView.isFlipped
                ? max(
                    documentBounds.minY,
                    documentBounds.maxY - viewportHeight
                )
                : documentBounds.minY
        }
        let constrainedBounds = clipView.constrainBoundsRect(proposedBounds)
        guard constrainedBounds.origin != clipView.bounds.origin else {
            return
        }
        clipView.scroll(to: constrainedBounds.origin)
        scrollView.reflectScrolledClipView(clipView)
    }

    func configure(
        director: AgentDirector,
        selectedCharacterID: OfficeCharacter?
    ) {
        if self.director !== director {
            tearDown()
            self.director = director
            selectionWillChangeCancellable = director
                .characterSelectionStore
                .selectionWillChange
                .sink { [weak self, weak director] characterID in
                    guard
                        let self,
                        let director,
                        self.director === director
                    else {
                        return
                    }
                    // CharacterSelectionStore의 @Published 상태보다 먼저
                    // 차폐를 설치해 선택 헤더와 대화 화면이 서로 다른
                    // snapshot을 한 프레임이라도 그리지 않게 한다.
                    self.configure(
                        director: director,
                        selectedCharacterID: characterID
                    )
                }
        }

        let previousCharacterID = self.selectedCharacterID
        let didChangeSelection = previousCharacterID != selectedCharacterID
        self.selectedCharacterID = selectedCharacterID
        let metadata = LiveWorkspaceFeedMetadata(director: director)

        guard didChangeSelection else {
            if activeEntry?.characterID == selectedCharacterID {
                activeEntry?.updateMetadata(metadata)
            }
            if pendingEntry?.characterID == selectedCharacterID {
                pendingEntry?.updateMetadata(metadata)
            }
            return
        }

        selectionGeneration &+= 1
        let generation = selectionGeneration
        // 차폐는 이미 보고 있던 직원에서 다른 직원으로 이동할 때만
        // 필요하다. 최초 진입(nil -> 직원), 선택 해제(직원 -> nil),
        // 같은 직원의 컨테이너 재생성에는 전환 아이콘을 만들지 않는다.
        let isEmployeeToEmployeeTransition =
            previousCharacterID != nil && selectedCharacterID != nil
        if isEmployeeToEmployeeTransition {
            if !Self.isTransitionGateDisabled {
            installTransitionLoadingGate()
        }
        } else {
            removeTransitionLoadingGate()
        }

        guard let selectedCharacterID else {
            releasePendingEntry()
            releaseActiveEntry()
            removeTransitionLoadingGate()
            return
        }
        // 실제 직원 전환에서는 이 configure 차례에 가벼운 AppKit 차폐만
        // 그린다. 긴 Claude transcript의 새 NSHostingView 생성은 다음
        // main-queue 차례로 넘겨 차폐가 먼저 표시될 기회를 보장한다.
        transitionLoadingGateView?.needsDisplay = true
        transitionLoadingGateView?.displayIfNeeded()
        DispatchQueue.main.async { [weak self, weak director] in
            guard
                let self,
                let director,
                self.director === director,
                self.selectionGeneration == generation,
                self.selectedCharacterID == selectedCharacterID
            else {
                return
            }
            self.mountSelectedCharacter(
                director: director,
                characterID: selectedCharacterID,
                generation: generation
            )
        }
    }

    func tearDown() {
        selectionGeneration &+= 1
        selectionWillChangeCancellable?.cancel()
        selectionWillChangeCancellable = nil
        releasePendingEntry()
        releaseActiveEntry()
        removeTransitionLoadingGate()
        selectedCharacterID = nil
        director = nil
    }

    private func releaseActiveEntry() {
        stopObservingBlankScreen()
        guard let activeEntry else {
            return
        }
        activeEntry.presentationStore.setPresented(false)
        activeEntry.hostingView.removeFromSuperview()
        self.activeEntry = nil
    }

    private func releasePendingEntry() {
        guard let pendingEntry else {
            return
        }
        release(entry: pendingEntry)
        self.pendingEntry = nil
    }

    private func release(entry: Entry) {
        entry.presentationStore.setPresented(false)
        entry.hostingView.removeFromSuperview()
    }

    private func installTransitionLoadingGate() {
        guard transitionLoadingGateView == nil else {
            return
        }
        let gate = LiveWorkspaceFeedLoadingGateView(frame: bounds)
        transitionLoadingGateView = gate
        addSubview(gate)
        transitionLoadingGateInstallCount &+= 1
        gateInstalledAt = Date()
        gatePolicy.reset()
    }

    private func mountSelectedCharacter(
        director: AgentDirector,
        characterID: OfficeCharacter,
        generation: Int
    ) {
        releasePendingEntry()
        releaseActiveEntry()

        let transitionID = UUID()
        let entry = Entry(
            director: director,
            characterID: characterID,
            metadata: LiveWorkspaceFeedMetadata(director: director),
            transitionID: transitionID,
            onMountReady: { [weak self] readinessRevision in
                self?.completePendingTransition(
                    transitionID: transitionID,
                    generation: generation,
                    readinessRevision: readinessRevision
                ) ?? .waitingForLayout
            }
        )
        entry.hostingView.frame = bounds
        entry.hostingView.autoresizingMask = [.width, .height]
        pendingEntry = entry
        startTransitionWatchdog()
        if let transitionLoadingGateView {
            addSubview(
                entry.hostingView,
                positioned: .below,
                relativeTo: transitionLoadingGateView
            )
        } else {
            addSubview(entry.hostingView)
        }
        needsLayout = true
    }

    private func removeTransitionLoadingGate() {
        transitionLoadingGateView?.removeFromSuperview()
        transitionLoadingGateView = nil
        finishGateWatchdog()
    }

    /// 전환이 대기 중인 동안 준비 확인과 차폐 수명을 함께 본다. 준비
    /// 콜백은 3초 뒤 멈추므로 콜백에만 기대면 정착 보정과 차폐 해제가
    /// 모두 멈춘다.
    private func startTransitionWatchdog() {
        gateWatchdog?.invalidate()
        let timer = Timer.scheduledTimer(
            withTimeInterval: 0.25,
            repeats: true
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.checkGateCoverage()
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        gateWatchdog = timer
    }

    private func finishGateWatchdog() {
        gateWatchdog?.invalidate()
        gateWatchdog = nil
        defer {
            gateInstalledAt = nil
            gatePolicy.reset()
        }
        guard let gateInstalledAt else {
            return
        }
        let elapsed = Date().timeIntervalSince(gateInstalledAt)
        guard elapsed >= LiveWorkspaceFeedStallPolicy.recoveredThreshold else {
            return
        }
        recordGateCoverage(
            kind: "gate-covered-recovered",
            elapsed: elapsed
        )
    }

    /// 차폐가 계속 덮고 있는 동안 무엇이 준비되지 않았는지 남긴다.
    private func checkGateCoverage() {
        // 준비 확인은 재시도 횟수에 기대지 않는다. 전환이 대기 중인
        // 동안에는 계속 확인한다.
        if let pendingEntry {
            _ = completePendingTransition(
                transitionID: pendingEntry.transitionID,
                generation: selectionGeneration,
                readinessRevision:
                    pendingEntry.characterFeedStore.mountReadinessRevision
            )
        }
        if pendingEntry == nil, transitionLoadingGateView == nil {
            gateWatchdog?.invalidate()
            gateWatchdog = nil
            return
        }
        guard
            transitionLoadingGateView?.superview === self,
            let gateInstalledAt
        else {
            return
        }
        let elapsed = Date().timeIntervalSince(gateInstalledAt)

        if gatePolicy.shouldReport(elapsed: elapsed) {
            recordGateCoverage(kind: "gate-covering", elapsed: elapsed)
        }

        // 정착이 끝나지 않아도 상한을 넘기면 덮는 것을 멈춘다. 전환 완료
        // 처리와 분리했으므로 정착 보정은 그대로 이어진다.
        guard elapsed >= Self.maximumGateCoverage else {
            return
        }
        recordGateCoverage(kind: "gate-cap-reached", elapsed: elapsed)
        removeTransitionLoadingGate()
    }

    private func recordGateCoverage(kind: String, elapsed: TimeInterval) {
        let entry = pendingEntry ?? activeEntry
        let stage = gateCoverageStage()
        guard let entry else {
            recordBare(kind: kind, elapsed: elapsed, stage: stage)
            return
        }
        record(
            kind: kind,
            entry: entry,
            elapsed: elapsed,
            readiness: stage,
            readinessRevision:
                entry.characterFeedStore.mountReadinessRevision,
            trace: blankDetector.documentHeightTrace
        )
    }

    /// 준비가 막힌 지점을 그대로 문자열로 남긴다. 어느 조건에서 멈췄는지
    /// 알아야 다음 수정을 고를 수 있다.
    private func gateCoverageStage() -> String {
        guard let pendingEntry else {
            return "pending=none active=\(activeEntry == nil ? "none" : "yes")"
        }
        let host = pendingEntry.hostingView
        let store = pendingEntry.characterFeedStore
        return [
            "pending=\(pendingEntry.characterID.rawValue)",
            "inSelf=\(host.superview === self)",
            "inWindow=\(host.window === window && window != nil)",
            "revision=\(pendingEntry.lastPreparedReadinessRevision.map(String.init) ?? "nil")"
                + "/\(store.mountReadinessRevision)",
            "loading=\(store.isLoadingInitialFeed)",
            "turns=\(store.turns.count)",
            "scroll=\(primaryScrollView(in: host) != nil)",
        ].joined(separator: " ")
    }

    private func recordBare(
        kind: String,
        elapsed: TimeInterval,
        stage: String
    ) {
        guard
            let stallRecorder,
            sessionReportCount
                < LiveWorkspaceFeedStallPolicy.maximumReportsPerSession
        else {
            return
        }
        sessionReportCount += 1
        stallRecorder.record(
            LiveWorkspaceFeedStallReport(
                kind: kind,
                characterID: selectedCharacterID?.rawValue ?? "none",
                elapsedSeconds: elapsed,
                readiness: stage,
                hostWidth: Double(bounds.width),
                hostHeight: Double(bounds.height),
                hasScrollView: false,
                documentHeight: 0,
                viewportHeight: 0,
                visibleIntersectionHeight: 0,
                turnCount: 0,
                isLoadingInitialFeed: false,
                readinessRevision: 0,
                preClampPassCount: 0,
                postClampPassCount: 0,
                viewportClampCount: 0,
                hasLoadingGate: true
            ),
            at: Date()
        )
    }

    private func completePendingTransition(
        transitionID: UUID,
        generation: Int,
        readinessRevision: Int
    ) -> LiveWorkspaceFeedMountReadiness {
        guard
            let director,
            let characterID = selectedCharacterID,
            generation == selectionGeneration,
            let pendingEntry,
            pendingEntry.transitionID == transitionID,
            pendingEntry.characterID == characterID,
            pendingEntry.characterFeedStore.mountReadinessRevision
                == readinessRevision,
            pendingEntry.hostingView.superview === self,
            pendingEntry.hostingView.window === window
        else {
            return .waitingForLayout
        }
        if pendingEntry.lastPreparedReadinessRevision
            != readinessRevision
        {
            pendingEntry.lastPreparedReadinessRevision = readinessRevision
            resetViewportStability(for: pendingEntry)
            pendingEntry.hostingView.needsLayout = true
            pendingEntry.hostingView.needsDisplay = true
            return .confirmValidLayout
        }
        let readiness = prepareInitialViewport(for: pendingEntry)
        guard readiness == .ready else {
            if readiness == .confirmValidLayout {
                pendingEntry.hostingView.needsLayout = true
            }
            recordStallIfNeeded(
                for: pendingEntry,
                readiness: readiness,
                readinessRevision: readinessRevision
            )
            return readiness
        }

        self.pendingEntry = nil
        activeEntry = pendingEntry
        observeBlankScreen(for: pendingEntry)
        removeTransitionLoadingGate()
        director.characterSelectionStore.completeConversationLoading(
            for: characterID
        )
        return .ready
    }

    private func prepareInitialViewport(
        for entry: Entry
    ) -> LiveWorkspaceFeedMountReadiness {
        let host = entry.hostingView
        guard
            host.bounds.width > 0,
            host.bounds.height > 0
        else {
            resetViewportStability(for: entry)
            return .waitingForLayout
        }
        host.layoutSubtreeIfNeeded()

        guard
            let director,
            !director.liveFeedStore.characterStore(
                for: entry.characterID.rawValue
            ).isLoadingInitialFeed
        else {
            resetViewportStability(for: entry)
            return .waitingForLayout
        }

        if !entry.didRequestPostMountRefresh {
            entry.didRequestPostMountRefresh = true
            director.liveFeedStore.refreshSelectedCharacterFeedAfterMount(
                entry.characterID.rawValue
            )
            resetViewportStability(for: entry)
            return .confirmValidLayout
        }

        guard let scrollView = primaryScrollView(in: host) else {
            // 대화가 정말 비어 있는 ContentUnavailableView에는 scroll view가
            // 없다. 이 경우 host 자체가 준비됐으면 바로 교체해도 된다.
            guard let selectedCharacterID else {
                resetViewportStability(for: entry)
                return .waitingForLayout
            }
            let characterStore = director.liveFeedStore.characterStore(
                for: selectedCharacterID.rawValue
            )
            guard
                !characterStore.isLoadingInitialFeed,
                characterStore.turns.isEmpty
            else {
                resetViewportStability(for: entry)
                return .waitingForLayout
            }
            entry.stablePostClampPassCount += 1
            guard entry.stablePostClampPassCount >= 2 else {
                return .confirmValidLayout
            }
            return prepareFirstDisplayIfNeeded(for: entry)
        }

        scrollView.layoutSubtreeIfNeeded()
        scrollView.documentView?.layoutSubtreeIfNeeded()
        guard
            let documentView = scrollView.documentView,
            scrollView.contentView.bounds.width.isFinite,
            scrollView.contentView.bounds.height.isFinite,
            documentView.bounds.width.isFinite,
            documentView.bounds.height.isFinite,
            scrollView.contentView.bounds.width > 0,
            scrollView.contentView.bounds.height > 0,
            documentView.bounds.width > 0,
            documentView.bounds.height > 0
        else {
            resetViewportStability(for: entry)
            return .waitingForLayout
        }

        let clipView = scrollView.contentView
        let documentBounds = documentView.bounds
        let viewportHeight = clipView.bounds.height
        let requiredVisibleHeight = min(
            viewportHeight,
            documentBounds.height
        )
        let visibleIntersectionHeight = documentBounds
            .intersection(scrollView.documentVisibleRect).height
        let hasValidViewport = visibleIntersectionHeight
            >= max(0, requiredVisibleHeight - 1)

        let signature = ViewportLayoutSignature(
            documentBounds: documentBounds,
            viewportBounds: clipView.bounds
        )
        guard entry.lastViewportSignature == signature else {
            entry.lastViewportSignature = signature
            entry.stablePreClampPassCount = 0
            entry.stablePostClampPassCount = 0
            entry.didPrepareFirstDisplay = false
            return .confirmValidLayout
        }

        if entry.lastClampedViewportSignature != signature {
            entry.stablePreClampPassCount += 1
            guard entry.stablePreClampPassCount >= 2 else {
                return .confirmValidLayout
            }
            // 문서 높이가 연속해서 안정된 뒤 AppKit 좌표계에서 해당
            // layout signature당 한 번만 하단으로 정착한다. Markdown의
            // 늦은 grow/shrink가 생기면 새 signature에서 다시 한 번만
            // 보정하고 SwiftUI scrollTo 재시도는 사용하지 않는다.
            entry.lastClampedViewportSignature = signature
            entry.viewportClampCount += 1
            let bottomY = documentView.isFlipped
                ? max(
                    documentBounds.minY,
                    documentBounds.maxY - viewportHeight
                )
                : documentBounds.minY
            var proposedBounds = clipView.bounds
            proposedBounds.origin.y = bottomY
            let constrainedBounds = clipView.constrainBoundsRect(
                proposedBounds
            )
            clipView.scroll(to: constrainedBounds.origin)
            scrollView.reflectScrolledClipView(clipView)
            entry.stablePostClampPassCount = 0
            return .confirmValidLayout
        }

        guard hasValidViewport else {
            // 크기는 그대로인데 AppKit이 문서 밖 origin을 남긴 경우에도
            // 다음 안정 pass에서 한 번 재보정할 수 있어야 한다.
            entry.lastClampedViewportSignature = nil
            entry.stablePreClampPassCount = 0
            entry.stablePostClampPassCount = 0
            entry.didPrepareFirstDisplay = false
            return .confirmValidLayout
        }

        entry.stablePostClampPassCount += 1
        guard entry.stablePostClampPassCount >= 2 else {
            return .confirmValidLayout
        }
        return prepareFirstDisplayIfNeeded(for: entry)
    }

    private func resetViewportStability(for entry: Entry) {
        entry.lastViewportSignature = nil
        entry.lastClampedViewportSignature = nil
        entry.stablePreClampPassCount = 0
        entry.stablePostClampPassCount = 0
        entry.didPrepareFirstDisplay = false
    }

    private func prepareFirstDisplayIfNeeded(
        for entry: Entry
    ) -> LiveWorkspaceFeedMountReadiness {
        guard entry.didPrepareFirstDisplay else {
            entry.didPrepareFirstDisplay = true
            entry.hostingView.needsDisplay = true
            entry.hostingView.displayIfNeeded()
            return .confirmValidLayout
        }
        return .ready
    }

    // MARK: - 백화 증상 함정

    private func recordStallIfNeeded(
        for entry: Entry,
        readiness: LiveWorkspaceFeedMountReadiness,
        readinessRevision: Int
    ) {
        let elapsed = Date().timeIntervalSince(entry.startedAt)
        guard entry.stallPolicy.shouldReport(elapsed: elapsed) else {
            return
        }
        record(
            kind: "transition-stall",
            entry: entry,
            elapsed: elapsed,
            readiness: "\(readiness)",
            readinessRevision: readinessRevision,
            trace: []
        )
    }

    /// 활성 대화의 문서·보기 영역 변화를 관찰한다. 스크롤바가 움찔거리는
    /// 구간이 곧 문서 높이가 흔들리는 구간이라 같은 알림으로 잡힌다.
    private func observeBlankScreen(for entry: Entry) {
        stopObservingBlankScreen()
        guard
            let scrollView = primaryScrollView(in: entry.hostingView),
            let documentView = scrollView.documentView
        else {
            return
        }
        documentView.postsFrameChangedNotifications = true
        scrollView.contentView.postsBoundsChangedNotifications = true
        for (name, object) in [
            (NSView.frameDidChangeNotification, documentView),
            (NSView.boundsDidChangeNotification, scrollView.contentView),
        ] {
            let observer = NotificationCenter.default.addObserver(
                forName: name,
                object: object,
                queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated {
                    self?.evaluateBlankScreen()
                }
            }
            blankObservers.append(observer)
        }
    }

    private func stopObservingBlankScreen() {
        for observer in blankObservers {
            NotificationCenter.default.removeObserver(observer)
        }
        blankObservers.removeAll()
        blankDeadline?.cancel()
        blankDeadline = nil
        blankStartedAt = nil
        didReportCurrentBlank = false
        blankDetector.reset()
    }

    private func evaluateBlankScreen() {
        guard let activeEntry else {
            return
        }

        // 리사이즈 정착 보정은 계측이 아니라 실제 동작이다. 함정을 꺼도
        // 계속 돌아야 한다.
        let isSettlingAfterResize: Bool
        if let resizeSettleUntil {
            if Date() >= resizeSettleUntil {
                self.resizeSettleUntil = nil
                isSettlingAfterResize = false
            } else {
                restoreViewportAfterResize(
                    for: activeEntry,
                    constrainOnly: true
                )
                isSettlingAfterResize = true
            }
        } else {
            isSettlingAfterResize = false
        }

        // 함정이 꺼져 있으면 표본 수집을 하지 않는다. 잎 뷰 계수가
        // 스크롤마다 도는 비용을 없앤다.
        guard
            stallRecorder != nil,
            let sample = blankSample(for: activeEntry)
        else {
            return
        }
        blankDetector.appendTrace(documentHeight: sample.documentHeight)
        recordJitterIfNeeded(for: activeEntry, sample: sample)
        if isSettlingAfterResize {
            recordResizeSettleIfNeeded(for: activeEntry)
        }
        let isBlank = LiveWorkspaceFeedBlankDetector.isBlank(
            turnCount: sample.turnCount,
            documentHeight: sample.documentHeight,
            viewportHeight: sample.viewportHeight,
            visibleIntersectionHeight: sample.visibleIntersectionHeight,
            visibleCardCount: sample.visibleCardCount
        )
        guard isBlank else {
            if let blankStartedAt {
                // 증상이 스스로 풀렸다. 얼마나 오래 비어 있었는지가
                // 사용자가 체감한 시간이다.
                let elapsed = Date().timeIntervalSince(blankStartedAt)
                if elapsed >= LiveWorkspaceFeedStallPolicy.recoveredThreshold {
                    record(
                        kind: "active-blank-recovered",
                        entry: activeEntry,
                        elapsed: elapsed,
                        readiness: "recovered",
                        readinessRevision:
                            activeEntry.characterFeedStore
                            .mountReadinessRevision,
                        trace: blankDetector.documentHeightTrace,
                        sample: sample
                    )
                }
            }
            blankDeadline?.cancel()
            blankDeadline = nil
            blankStartedAt = nil
            didReportCurrentBlank = false
            blankDetector.reset()
            return
        }
        guard blankStartedAt == nil else {
            return
        }
        blankStartedAt = Date()
        // 스스로 풀리지 않는 경우에도 남기도록 마감을 건다.
        let deadline = DispatchWorkItem { [weak self] in
            MainActor.assumeIsolated {
                self?.reportPersistingBlankScreen()
            }
        }
        blankDeadline = deadline
        DispatchQueue.main.asyncAfter(
            deadline: .now() + LiveWorkspaceFeedStallPolicy.firstReportDelay,
            execute: deadline
        )
    }

    private func recordResizeSettleIfNeeded(for entry: Entry) {
        let now = Date()
        if let lastResizeSampleAt,
            now.timeIntervalSince(lastResizeSampleAt) < 0.12
        {
            return
        }
        lastResizeSampleAt = now
        guard let sample = blankSample(for: entry) else {
            return
        }
        record(
            kind: "resize-settle",
            entry: entry,
            elapsed: 0,
            readiness: "offset=\(Int(sample.offsetY))"
                + " cards=\(sample.visibleCardCount)"
                + "/\(sample.mountedCardCount)",
            readinessRevision:
                entry.characterFeedStore.mountReadinessRevision,
            trace: blankDetector.documentHeightTrace,
            sample: sample
        )
    }

    private func recordJitterIfNeeded(for entry: Entry, sample: BlankSample) {
        let now = Date()
        jitterDetector.append(
            at: now.timeIntervalSinceReferenceDate,
            documentHeight: sample.documentHeight,
            offsetY: sample.offsetY
        )
        guard jitterDetector.isJittering else {
            return
        }
        if let lastJitterReportAt,
            now.timeIntervalSince(lastJitterReportAt) < 2
        {
            return
        }
        lastJitterReportAt = now
        let span = jitterDetector.samples
        let elapsed = (span.last?.at ?? 0) - (span.first?.at ?? 0)
        record(
            kind: "active-jitter",
            entry: entry,
            elapsed: elapsed,
            readiness: "offsets="
                + span.map { String(Int($0.offsetY)) }.joined(separator: ">"),
            readinessRevision:
                entry.characterFeedStore.mountReadinessRevision,
            trace: span.map { Int($0.documentHeight.rounded()) },
            sample: sample
        )
        jitterDetector.reset()
    }

    private func reportPersistingBlankScreen() {
        guard
            !didReportCurrentBlank,
            let activeEntry,
            let blankStartedAt,
            let sample = blankSample(for: activeEntry)
        else {
            return
        }
        let isStillBlank = LiveWorkspaceFeedBlankDetector.isBlank(
            turnCount: sample.turnCount,
            documentHeight: sample.documentHeight,
            viewportHeight: sample.viewportHeight,
            visibleIntersectionHeight: sample.visibleIntersectionHeight,
            visibleCardCount: sample.visibleCardCount
        )
        guard isStillBlank else {
            // frame/bounds 알림 없이 SwiftUI가 스스로 실체화한 경우에도
            // 오래된 deadline이 정상 화면을 "persisting"으로 기록하지 않게
            // 마지막 순간의 표식을 다시 확인한다.
            self.blankDeadline = nil
            self.blankStartedAt = nil
            didReportCurrentBlank = false
            blankDetector.reset()
            return
        }
        didReportCurrentBlank = true
        record(
            kind: "active-blank-persisting",
            entry: activeEntry,
            elapsed: Date().timeIntervalSince(blankStartedAt),
            readiness: "blank",
            readinessRevision:
                activeEntry.characterFeedStore.mountReadinessRevision,
            trace: blankDetector.documentHeightTrace,
            sample: sample
        )
    }

    private struct BlankSample {
        let offsetY: Double
        let mountedCardCount: Int
        let visibleCardCount: Int
        let documentHeight: Double
        let viewportHeight: Double
        let visibleIntersectionHeight: Double
        let turnCount: Int
        let isLoadingInitialFeed: Bool
        let hasScrollView: Bool
        let hostSize: CGSize
    }

    private func blankSample(for entry: Entry) -> BlankSample? {
        let host = entry.hostingView
        guard let scrollView = primaryScrollView(in: host) else {
            return BlankSample(
                offsetY: 0,
                mountedCardCount: 0,
                visibleCardCount: 0,
                documentHeight: 0,
                viewportHeight: 0,
                visibleIntersectionHeight: 0,
                turnCount: entry.characterFeedStore.turns.count,
                isLoadingInitialFeed:
                    entry.characterFeedStore.isLoadingInitialFeed,
                hasScrollView: false,
                hostSize: host.bounds.size
            )
        }
        let documentBounds = scrollView.documentView?.bounds ?? .zero
        let visible = scrollView.documentVisibleRect
        let cardCounts = Self.cardMarkerCounts(
            documentView: scrollView.documentView,
            visibleRect: visible
        )
        return BlankSample(
            offsetY: Double(visible.minY),
            mountedCardCount: cardCounts.mounted,
            visibleCardCount: cardCounts.visible,
            documentHeight: documentBounds.height,
            viewportHeight: scrollView.contentView.bounds.height,
            visibleIntersectionHeight:
                documentBounds.intersection(visible).height,
            turnCount: entry.characterFeedStore.turns.count,
            isLoadingInitialFeed:
                entry.characterFeedStore.isLoadingInitialFeed,
            hasScrollView: true,
            hostSize: host.bounds.size
        )
    }

    private func record(
        kind: String,
        entry: Entry,
        elapsed: TimeInterval,
        readiness: String,
        readinessRevision: Int,
        trace: [Int],
        sample: BlankSample? = nil
    ) {
        guard let stallRecorder else {
            return
        }
        let resolved = sample ?? blankSample(for: entry)
        let report = LiveWorkspaceFeedStallReport(
            kind: kind,
            characterID: entry.characterID.rawValue,
            elapsedSeconds: elapsed,
            readiness: readiness,
            hostWidth: Double(resolved?.hostSize.width ?? 0),
            hostHeight: Double(resolved?.hostSize.height ?? 0),
            hasScrollView: resolved?.hasScrollView ?? false,
            documentHeight: resolved?.documentHeight ?? 0,
            viewportHeight: resolved?.viewportHeight ?? 0,
            visibleIntersectionHeight:
                resolved?.visibleIntersectionHeight ?? 0,
            turnCount: resolved?.turnCount ?? 0,
            isLoadingInitialFeed: resolved?.isLoadingInitialFeed ?? false,
            readinessRevision: readinessRevision,
            preClampPassCount: entry.stablePreClampPassCount,
            postClampPassCount: entry.stablePostClampPassCount,
            viewportClampCount: entry.viewportClampCount,
            hasLoadingGate: transitionLoadingGateView?.superview === self,
            mountedCardCount: resolved?.mountedCardCount ?? 0,
            visibleCardCount: resolved?.visibleCardCount ?? 0,
            gateDisabled: Self.isTransitionGateDisabled,
            documentHeightTrace: trace
        )
        guard
            sessionReportCount
                < LiveWorkspaceFeedStallPolicy.maximumReportsPerSession
        else {
            return
        }
        sessionReportCount += 1
        stallRecorder.record(report, at: Date())
    }

    /// 각 대화 묶음에 심은 AppKit 표식만 센다. 임의의 잎 NSView를 세면
    /// SwiftUI 내부 계층 변화에 따라 같은 카드도 0개 또는 여러 개로 보여
    /// 실체화 신호로 쓸 수 없다.
    private static func cardMarkerCounts(
        documentView: NSView?,
        visibleRect: CGRect
    ) -> (mounted: Int, visible: Int) {
        guard let documentView else {
            return (0, 0)
        }
        var mounted = 0
        var visible = 0
        var stack = documentView.subviews
        var visited = 0
        while let view = stack.popLast(), visited < 4_000 {
            visited += 1
            if view is LiveWorkspaceFeedCardMarkerView {
                mounted += 1
                let frame = documentView.convert(view.bounds, from: view)
                if frame.height >= 1,
                    frame.width >= 1,
                    frame.intersects(visibleRect)
                {
                    visible += 1
                }
            }
            stack.append(contentsOf: view.subviews)
        }
        return (mounted, visible)
    }

    private func primaryScrollView(in root: NSView) -> NSScrollView? {
        descendantViews(of: root)
            .compactMap { $0 as? NSScrollView }
            .max { lhs, rhs in
                lhs.bounds.width * lhs.bounds.height
                    < rhs.bounds.width * rhs.bounds.height
            }
    }

    private func descendantViews(of view: NSView) -> [NSView] {
        view.subviews.flatMap { subview in
            [subview] + descendantViews(of: subview)
        }
    }
}

struct LiveWorkspaceFeed: View, Equatable {
    @ObservedObject private var characterFeedStore: CharacterLiveFeedStore
    private let characterFeedRevision: Int
    private let director: AgentDirector
    private let liveFeedStore: LiveFeedStore
    private let characterID: OfficeCharacter
    private let presentationStore: LiveWorkspaceFeedPresentationStore
    private let workspaceDirectory: String
    private let latestTerminalTurnID: String?
    private let latestSubmittedCommandID: UUID?
    private let latestStartedCommandID: UUID?
    private let updateResponseFeedback:
        (String, TurnResponseFeedback?) async -> Void
    @State private var followState = LiveWorkspaceFeedFollowState()
    @State private var scrollMetrics = LiveWorkspaceFeedScrollMetrics()
    @State private var displayAnchor: LiveWorkspaceFeedDisplayAnchor
    @State private var didPerformInitialScroll = false
    @State private var isLoadingOlderTurns = false
    @State private var topLoadGate = LiveWorkspaceFeedTopLoadGate()

    private static let bottomTolerance = CGFloat(20)
    private static let topLoadThreshold = CGFloat(120)
    private static let bottomMarkerID = "live-workspace-feed-bottom"

    fileprivate init(
        director: AgentDirector,
        characterID: OfficeCharacter,
        presentationStore: LiveWorkspaceFeedPresentationStore,
        metadata: LiveWorkspaceFeedMetadata
    ) {
        let liveFeedStore = director.liveFeedStore
        let characterFeedStore = liveFeedStore.characterStore(
            for: characterID.rawValue
        )
        _characterFeedStore = ObservedObject(
            wrappedValue: characterFeedStore
        )
        _displayAnchor = State(
            initialValue: LiveWorkspaceFeedDisplayAnchor(
                initialLimit:
                    LiveWorkspaceFeedPagingPolicy.initialVisibleTurnCount(
                        for: characterFeedStore.turns
                    )
            )
        )
        characterFeedRevision = characterFeedStore.mountReadinessRevision
        self.director = director
        self.liveFeedStore = liveFeedStore
        self.characterID = characterID
        self.presentationStore = presentationStore
        workspaceDirectory = director.workspaceDirectory
        latestTerminalTurnID = metadata.latestTerminalTurnID
        latestSubmittedCommandID = metadata.latestSubmittedCommandID
        latestStartedCommandID = metadata.latestStartedCommandID
        updateResponseFeedback = { turnID, feedback in
            await director.updateResponseFeedback(
                turnID: turnID,
                feedback: feedback
            )
        }
    }

    static func == (
        lhs: LiveWorkspaceFeed,
        rhs: LiveWorkspaceFeed
    ) -> Bool {
        lhs.director === rhs.director
            && lhs.liveFeedStore === rhs.liveFeedStore
            && lhs.characterFeedStore === rhs.characterFeedStore
            && lhs.characterFeedRevision == rhs.characterFeedRevision
            && lhs.characterID == rhs.characterID
            && lhs.presentationStore === rhs.presentationStore
            && lhs.workspaceDirectory == rhs.workspaceDirectory
            && lhs.latestTerminalTurnID == rhs.latestTerminalTurnID
            && lhs.latestSubmittedCommandID == rhs.latestSubmittedCommandID
            && lhs.latestStartedCommandID == rhs.latestStartedCommandID
    }

    private var selectedTurns: [LiveFeedTurn] {
        characterFeedStore.turns
    }

    private var visibleTurnLimit: Int {
        displayAnchor.effectiveLimit(
            turnIDsNewestFirst: selectedTurns.map(\.id)
        )
    }

    private var displayTurns: [LiveFeedTurn] {
        let visibleTurnLimit = visibleTurnLimit
        return Array(
            selectedTurns.enumerated().compactMap { index, turn in
                LiveWorkspaceFeedPagingPolicy.includesTurn(
                    at: index,
                    visibleTurnLimit: visibleTurnLimit,
                    isRunning: turn.status.isRunning,
                    isLatestTerminalTurn:
                        turn.id == latestTerminalTurnID
                )
                    ? turn
                    : nil
            }
            .reversed()
        )
    }

    private func repinDisplayAnchor() {
        let pinned = displayAnchor.pinning(
            turnIDsNewestFirst: selectedTurns.map(\.id)
        )
        if displayAnchor != pinned {
            displayAnchor = pinned
        }
    }

    private var displayItems: [LiveWorkspaceFeedTurnItem] {
        displayTurns.map { turn in
            LiveWorkspaceFeedTurnItem(
                id: liveFeedStore.presentationID(forTurnID: turn.id),
                turn: turn
            )
        }
    }

    private var canLoadOlderTurns: Bool {
        visibleTurnLimit
            < min(
                LiveWorkspaceFeedPagingPolicy.maximumVisibleTurnCount,
                selectedTurns.count
            )
    }

    private var initialLayoutRevision:
        [LiveWorkspaceFeedTurnRevision]
    {
        displayItems.map { item in
            let turn = item.turn
            return LiveWorkspaceFeedTurnRevision(
                id: item.id,
                updatedAt: turn.updatedAt,
                status: turn.status,
                activityCount: turn.activities.count,
                responseLength: turn.response.count,
                workspaceStatus: turn.workspace?.status,
                changedFileCount: turn.workspace?.changedFiles.count ?? 0
            )
        }
    }

    // 로딩 자리표시자는 실행 후 첫 적재가 끝나기 전까지만 쓴다.
    // 그 뒤에는 직원별 재조회가 잠깐 로딩으로 표시되더라도 자리표시자로
    // 바꾸지 않는다. 대화 영역을 통째로 교체하면 화면이 비어 보인다.
    private var shouldShowInitialLoadingPlaceholder: Bool {
        characterFeedStore.isLoadingInitialFeed
            && !liveFeedStore.didCompleteFirstFeedLoad
    }

    var body: some View {
        ScrollViewReader { proxy in
            Group {
                if displayTurns.isEmpty {
                    if shouldShowInitialLoadingPlaceholder {
                        VStack(spacing: 10) {
                            ProgressView()
                                .controlSize(.small)
                            Text(OfficeLocalization.string("대화를 불러오는 중"))
                                .font(
                                    .system(
                                        size: 12,
                                        weight: .semibold,
                                        design: .rounded
                                    )
                                )
                                .foregroundStyle(.secondary)
                        }
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .accessibilityElement(children: .combine)
                        .accessibilityLabel(OfficeLocalization.string("대화를 불러오는 중"))
                    } else {
                        ContentUnavailableView(
                            OfficeLocalization.string("아직 업무 대화가 없습니다"),
                            systemImage:
                                "bubble.left.and.text.bubble.right",
                            description: Text(
                                OfficeLocalization.string("오피스에서 직원을 선택하고 첫 업무를 보내보세요.")
                            )
                        )
                    }
                } else {
                    ScrollView {
                        VStack(spacing: 0) {
                            LiveWorkspaceFeedScrollObserver(
                                onMetrics: { metrics in
                                    handleScrollMetrics(
                                        metrics,
                                        proxy: proxy
                                    )
                                },
                                onUserScrollStarted: {
                                    if !didPerformInitialScroll {
                                        didPerformInitialScroll = true
                                    }
                                    pauseFollowingLatest()
                                },
                                onUserScrollActivity: {
                                    if !didPerformInitialScroll {
                                        didPerformInitialScroll = true
                                    }
                                    pauseFollowingLatest()
                                },
                                onUserScroll: { metrics in
                                    handleUserScroll(
                                        metrics,
                                        proxy: proxy
                                    )
                                }
                            )
                            .frame(height: 1)

                            // 화면에 올리는 턴은 이미 10건, 사용자가 과거를
                            // 펼쳐도 최대 30건으로 제한된다. 여기서 다시
                            // LazyVStack을 쓰면 리사이즈 때 보이는 구간의
                            // 추정 높이만 남고 카드가 실체화되지 않는 macOS
                            // SwiftUI 경로가 생긴다. 바깥 목록은 eager로 두고
                            // 카드 안의 접힌 긴 이력만 lazy 렌더링한다.
                            VStack(spacing: 14) {
                                ForEach(displayItems) { item in
                                    let turn = item.turn
                                    // 질문은 직원 카드 밖에 세운다. 카드
                                    // 안에 있으면 직원이 한 말처럼 보인다.
                                    // 한 턴의 질문과 답변은 한 묶음으로
                                    // 남겨 스크롤 대상 식별자를 유지한다.
                                    VStack(alignment: .leading, spacing: 8) {
                                    LiveTurnPromptBlock(
                                        presentation: TaskPromptPresentation(
                                            prompt: turn.prompt
                                        ),
                                        sentAt: turn.startedAt,
                                        sender: EmployeeMessageSender.resolve(turn.promptSender, prompt: turn.prompt),
                                        selectionID: turn.id
                                    )

                                    EquatableLiveTurnCard(
                                        director: director,
                                        turn: turn,
                                        workspaceDirectory: workspaceDirectory,
                                        shouldAnimateResponse:
                                            liveFeedStore
                                            .shouldAnimateResponse(for: turn),
                                        shouldAnimateInitialResponse:
                                            liveFeedStore
                                            .shouldAnimateInitialResponse(
                                                for: turn
                                            ),
                                        updateResponseFeedback:
                                            updateResponseFeedback
                                    ) {
                                        liveFeedStore
                                            .finishResponseAnimation(
                                                for: turn.id
                                            )
                                    }
                                        .equatable()
                                    }
                                    .id(item.id)
                                    .background {
                                        LiveWorkspaceFeedCardMarker()
                                            .allowsHitTesting(false)
                                            .accessibilityHidden(true)
                                    }
                                }
                            }

                            LiveWorkspaceFeedBottomMarker()
                                .frame(height: 16)
                                .id(Self.bottomMarkerID)
                        }
                        .padding(.horizontal, 18)
                        .padding(.top, 16)
                    }
                    .defaultScrollAnchor(.bottom)
                    .onAppear {
                        settleInitialAnchorIfNeeded()
                    }
                    .onChange(
                        of: characterFeedStore.isLoadingInitialFeed
                    ) { _, isLoading in
                        guard !isLoading else {
                            return
                        }
                        settleInitialAnchorIfNeeded()
                    }
                    .onChange(of: initialLayoutRevision) { _, _ in
                        repinDisplayAnchor()
                        switch LiveWorkspaceFeedContentRevisionPolicy.action(
                            didPerformInitialScroll: didPerformInitialScroll,
                            isFollowingLatest: followState.isFollowingLatest
                        ) {
                        case .settleInitialAnchor:
                            settleInitialAnchorIfNeeded()
                        case .followLatest:
                            scheduleScrollToLatest(proxy)
                        case .revealContentBelow:
                            // 버튼이 항상 보이므로 별도 표시 상태를
                            // 갱신하지 않는다.
                            break
                        }
                    }
                    // 제출 시 revealSubmittedTurn이 하단 보정을 1회
                    // 수행하고, 시작·진행 갱신은 followLatest 경로가
                    // 담당한다. latestStartedCommandID의 별도 하단
                    // 이동은 같은 프레임에 스크롤을 중복 예약해
                    // 깜빡임을 만들므로 두지 않는다.
                    .onChange(of: latestSubmittedCommandID) {
                        _, commandID in
                        guard
                            commandID != nil,
                            LiveWorkspaceFeedSubmissionScrollPolicy
                                .shouldRevealSubmittedTurn(
                                    isFollowingLatest:
                                        followState.isFollowingLatest
                                )
                        else {
                            return
                        }
                        revealSubmittedTurn(proxy: proxy)
                    }
                    .overlay(
                        alignment: LiveWorkspaceFeedJumpButtonLayout.alignment
                    ) {
                        jumpToLatestButton(proxy: proxy)
                            .padding(
                                .leading,
                                LiveWorkspaceFeedJumpButtonLayout
                                    .leadingPadding
                            )
                            .padding(
                                .bottom,
                                LiveWorkspaceFeedJumpButtonLayout.bottomPadding
                            )
                    }
                    .onDisappear {
                        cancelScheduledScrolls()
                    }
                }
            }
            .onReceive(presentationStore.$isPresented) { isPresented in
                if !isPresented {
                    cancelScheduledScrolls()
                }
            }
        }
    }

    private func handleScrollMetrics(
        _ metrics: LiveWorkspaceFeedScrollSnapshot,
        proxy: ScrollViewProxy
    ) {
        guard presentationStore.isPresentationRequested else {
            return
        }
        scrollMetrics.hasSnapshot = true
        scrollMetrics.distanceFromBottom = metrics.distanceFromBottom
        scrollMetrics.viewportHeight = metrics.viewportHeight
        scrollMetrics.contentHeight = metrics.contentHeight

        settleInitialAnchorIfNeeded()
        if !didPerformInitialScroll {
            return
        }

    }

    private func handleUserScroll(
        _ metrics: LiveWorkspaceFeedScrollSnapshot,
        proxy: ScrollViewProxy
    ) {
        guard presentationStore.isPresentationRequested else {
            return
        }
        let shouldFollowLatest =
            metrics.distanceFromBottom <= Self.bottomTolerance
        if followState.isFollowingLatest != shouldFollowLatest {
            followState.userDidScroll(
                distanceFromBottom: metrics.distanceFromBottom,
                tolerance: Self.bottomTolerance
            )
        }
        if topLoadGate.shouldLoad(
            distanceFromTop: metrics.distanceFromTop,
            threshold: Self.topLoadThreshold,
            isProgrammaticScrollInFlight:
                scrollMetrics.isProgrammaticScrollInFlight
        ) {
            loadMoreTurnsIfNeeded(proxy: proxy)
        }
    }

    private func scrollToLatest(
        _ proxy: ScrollViewProxy,
        animated: Bool = true
    ) {
        cancelScheduledScrolls()
        let generation = beginProgrammaticScroll()
        markAtBottom()
        scrollMetrics.followScrollTask = Task { @MainActor in
            defer {
                if scrollMetrics.scrollGeneration == generation {
                    scrollMetrics.followScrollTask = nil
                    finishProgrammaticScroll(generation: generation)
                }
            }
            await Task.yield()
            guard
                !Task.isCancelled,
                presentationStore.isPresentationRequested,
                scrollMetrics.scrollGeneration == generation
            else {
                return
            }
            guard animated else {
                proxy.scrollTo(Self.bottomMarkerID, anchor: .bottom)
                return
            }
            withAnimation(.easeOut(duration: 0.20)) {
                proxy.scrollTo(Self.bottomMarkerID, anchor: .bottom)
            }
            try? await Task.sleep(for: .milliseconds(220))
        }
    }

    private func scheduleScrollToLatest(
        _ proxy: ScrollViewProxy
    ) {
        guard
            presentationStore.isPresentationRequested,
            didPerformInitialScroll,
            followState.isFollowingLatest,
            scrollMetrics.submittedScrollTask == nil
        else {
            return
        }
        guard scrollMetrics.followScrollTask == nil else {
            return
        }
        markAtBottom()
        let generation = beginProgrammaticScroll()
        scrollMetrics.followScrollTask = Task { @MainActor in
            defer {
                if scrollMetrics.scrollGeneration == generation {
                    scrollMetrics.followScrollTask = nil
                    finishProgrammaticScroll(generation: generation)
                }
            }
            await Task.yield()
            try? await Task.sleep(for: .milliseconds(16))
            guard
                !Task.isCancelled,
                presentationStore.isPresentationRequested,
                scrollMetrics.scrollGeneration == generation
            else {
                return
            }
            proxy.scrollTo(Self.bottomMarkerID, anchor: .bottom)
        }
    }

    private func settleInitialAnchorIfNeeded() {
        guard
            !didPerformInitialScroll,
            !characterFeedStore.isLoadingInitialFeed,
            !displayTurns.isEmpty,
            scrollMetrics.hasSnapshot,
            scrollMetrics.viewportHeight > 0,
            scrollMetrics.contentHeight > 0
        else {
            return
        }

        didPerformInitialScroll = true
        repinDisplayAnchor()
    }

    private func cancelScheduledScrolls() {
        scrollMetrics.scrollGeneration &+= 1
        scrollMetrics.isProgrammaticScrollInFlight = false
        scrollMetrics.followScrollTask?.cancel()
        scrollMetrics.followScrollTask = nil
        scrollMetrics.submittedScrollTask?.cancel()
        scrollMetrics.submittedScrollTask = nil
    }

    private func beginProgrammaticScroll() -> Int {
        scrollMetrics.scrollGeneration &+= 1
        scrollMetrics.isProgrammaticScrollInFlight = true
        return scrollMetrics.scrollGeneration
    }

    private func finishProgrammaticScroll(generation: Int) {
        guard scrollMetrics.scrollGeneration == generation else {
            return
        }
        scrollMetrics.isProgrammaticScrollInFlight = false
    }

    private func revealSubmittedTurn(proxy: ScrollViewProxy) {
        cancelScheduledScrolls()
        markAtBottom()
        let generation = beginProgrammaticScroll()
        scrollMetrics.submittedScrollTask = Task { @MainActor in
            defer {
                if scrollMetrics.scrollGeneration == generation {
                    scrollMetrics.submittedScrollTask = nil
                    finishProgrammaticScroll(generation: generation)
                }
            }
            await Task.yield()
            guard
                !Task.isCancelled,
                presentationStore.isPresentationRequested,
                scrollMetrics.scrollGeneration == generation
            else {
                return
            }
            var policy = LiveWorkspaceFeedScrollPolicy()
            for _ in 0..<LiveWorkspaceFeedScrollPolicy.submittedMaximumAttempts {
                guard
                    !Task.isCancelled,
                    presentationStore.isPresentationRequested,
                    scrollMetrics.scrollGeneration == generation
                else {
                    return
                }
                proxy.scrollTo(Self.bottomMarkerID, anchor: .bottom)
                try? await Task.sleep(for: .milliseconds(40))
                if policy.shouldStop(
                    distanceFromBottom:
                        scrollMetrics.distanceFromBottom,
                    tolerance: Self.bottomTolerance
                ) {
                    break
                }
            }
            guard
                !Task.isCancelled,
                presentationStore.isPresentationRequested,
                scrollMetrics.scrollGeneration == generation
            else {
                return
            }
            markAtBottom()
        }
    }

    private func loadMoreTurnsIfNeeded(
        proxy: ScrollViewProxy
    ) {
        guard
            didPerformInitialScroll,
            canLoadOlderTurns,
            !isLoadingOlderTurns
        else {
            return
        }

        let readingAnchorID = displayItems.first?.id
        let currentLimit = visibleTurnLimit
        let nextLimit = LiveWorkspaceFeedPagingPolicy.nextVisibleTurnLimit(
            current: currentLimit,
            total: selectedTurns.count
        )
        guard nextLimit > currentLimit else {
            return
        }

        isLoadingOlderTurns = true
        displayAnchor = displayAnchor.pinning(
            limit: nextLimit,
            turnIDsNewestFirst: selectedTurns.map(\.id)
        )
        // 읽던 위치 복원 scrollTo도 programmatic으로 표시해, 복원
        // 이동이 상단 근접으로 해석돼 다음 페이지를 연쇄 로드하는
        // 재진입을 막는다.
        let generation = beginProgrammaticScroll()
        DispatchQueue.main.async {
            guard presentationStore.isPresentationRequested else {
                isLoadingOlderTurns = false
                finishProgrammaticScroll(generation: generation)
                return
            }
            if let readingAnchorID {
                proxy.scrollTo(readingAnchorID, anchor: .top)
            }
            DispatchQueue.main.async {
                isLoadingOlderTurns = false
                finishProgrammaticScroll(generation: generation)
            }
        }
    }

    private func jumpToLatestButton(
        proxy: ScrollViewProxy
    ) -> some View {
        Button {
            scrollToLatest(proxy)
        } label: {
            Image(systemName: "arrow.down")
                .font(.system(size: 13, weight: .black))
            .foregroundStyle(DashboardPalette.accent)
            .frame(
                width: LiveWorkspaceFeedJumpButtonLayout.diameter,
                height: LiveWorkspaceFeedJumpButtonLayout.diameter
            )
            .background {
                Circle()
                    .fill(.ultraThinMaterial)
                    .overlay {
                        Circle()
                            .stroke(
                                DashboardPalette.accent.opacity(0.62),
                                lineWidth: 1.4
                            )
                    }
            }
            .shadow(
                color: DashboardPalette.accent.opacity(0.28),
                radius: 7,
                y: 3
            )
            .shadow(color: .black.opacity(0.12), radius: 3, y: 2)
        }
        .buttonStyle(.plain)
        .accessibilityLabel(OfficeLocalization.string("맨 아래로 이동"))
        .help(OfficeLocalization.string("맨 아래로 이동"))
    }

    private func markAtBottom() {
        if !followState.isFollowingLatest {
            followState.resume()
        }
    }

    private func pauseFollowingLatest() {
        if followState.isFollowingLatest {
            followState.userWillScroll()
        }
        cancelScheduledScrolls()
    }
}

enum LiveWorkspaceFeedJumpButtonLayout {
    static let alignment = Alignment.bottomLeading
    static let contentHorizontalPadding = CGFloat(18)
    static let avatarDiameter = CGFloat(38)
    static let diameter = CGFloat(32)
    static let bottomPadding = CGFloat(12)
    static let leadingPadding =
        contentHorizontalPadding + (avatarDiameter - diameter) / 2
}

private struct LiveWorkspaceFeedTurnItem: Identifiable {
    let id: String
    let turn: LiveFeedTurn
}

private struct LiveWorkspaceFeedTurnRevision: Equatable {
    let id: String
    let updatedAt: Date
    let status: LiveTurnStatus
    let activityCount: Int
    let responseLength: Int
    let workspaceStatus: WorkspaceReviewStatus?
    let changedFileCount: Int
}

private final class LiveWorkspaceFeedScrollMetrics {
    var hasSnapshot = false
    var distanceFromBottom = CGFloat.zero
    var viewportHeight = CGFloat.zero
    var contentHeight = CGFloat.zero
    var isProgrammaticScrollInFlight = false
    var scrollGeneration = 0
    var followScrollTask: Task<Void, Never>?
    var submittedScrollTask: Task<Void, Never>?
}

private struct LiveWorkspaceFeedBottomMarker: View {
    var body: some View {
        Color.clear
    }
}

struct LiveWorkspaceFeedScrollSnapshot: Equatable {
    let distanceFromTop: CGFloat
    let distanceFromBottom: CGFloat
    let viewportHeight: CGFloat
    let contentHeight: CGFloat

    func isApproximatelyEqual(
        to other: LiveWorkspaceFeedScrollSnapshot,
        tolerance: CGFloat = 0.5
    ) -> Bool {
        abs(distanceFromTop - other.distanceFromTop) <= tolerance
            && abs(distanceFromBottom - other.distanceFromBottom) <= tolerance
            && abs(viewportHeight - other.viewportHeight) <= tolerance
            && abs(contentHeight - other.contentHeight) <= tolerance
    }
}

struct LiveWorkspaceFeedScrollGeometry {
    static func snapshot(
        documentBounds: CGRect,
        visibleRect: CGRect,
        isFlipped: Bool
    ) -> LiveWorkspaceFeedScrollSnapshot {
        let viewportHeight = max(0, visibleRect.height)
        let contentHeight = max(0, documentBounds.height)
        let scrollableHeight = max(0, contentHeight - viewportHeight)
        let rawDistanceFromTop =
            isFlipped
            ? visibleRect.minY - documentBounds.minY
            : documentBounds.maxY - visibleRect.maxY
        let distanceFromTop = min(
            scrollableHeight,
            max(0, rawDistanceFromTop)
        )
        return LiveWorkspaceFeedScrollSnapshot(
            distanceFromTop: distanceFromTop,
            distanceFromBottom: max(
                0,
                scrollableHeight - distanceFromTop
            ),
            viewportHeight: viewportHeight,
            contentHeight: contentHeight
        )
    }
}

struct LiveWorkspaceFeedScrollObserver: NSViewRepresentable {
    let onMetrics: (LiveWorkspaceFeedScrollSnapshot) -> Void
    let onUserScrollStarted: () -> Void
    let onUserScrollActivity: () -> Void
    let onUserScroll: (LiveWorkspaceFeedScrollSnapshot) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(
            onMetrics: onMetrics,
            onUserScrollStarted: onUserScrollStarted,
            onUserScrollActivity: onUserScrollActivity,
            onUserScroll: onUserScroll
        )
    }

    func makeNSView(context: Context) -> AttachmentView {
        let view = AttachmentView()
        view.onHierarchyChange = {
            [weak view, weak coordinator = context.coordinator] in
            coordinator?.attach(
                to: view?.window == nil
                    ? nil
                    : view?.enclosingScrollView
            )
        }
        return view
    }

    func updateNSView(_ nsView: AttachmentView, context: Context) {
        context.coordinator.onMetrics = onMetrics
        context.coordinator.onUserScrollStarted = onUserScrollStarted
        context.coordinator.onUserScrollActivity = onUserScrollActivity
        context.coordinator.onUserScroll = onUserScroll
        context.coordinator.attach(
            to: nsView.window == nil
                ? nil
                : nsView.enclosingScrollView
        )
    }

    static func dismantleNSView(
        _ nsView: AttachmentView,
        coordinator: Coordinator
    ) {
        nsView.onHierarchyChange = nil
        coordinator.detach()
    }

    final class Coordinator {
        var onMetrics: (LiveWorkspaceFeedScrollSnapshot) -> Void
        var onUserScrollStarted: () -> Void
        var onUserScrollActivity: () -> Void
        var onUserScroll: (LiveWorkspaceFeedScrollSnapshot) -> Void
        private weak var scrollView: NSScrollView?
        private weak var documentView: NSView?
        private var boundsObserver: NSObjectProtocol?
        private var clipFrameObserver: NSObjectProtocol?
        private var liveScrollStartObserver: NSObjectProtocol?
        private var liveScrollUpdateObserver: NSObjectProtocol?
        private var liveScrollEndObserver: NSObjectProtocol?
        private var isReportScheduled = false
        private var reportsUserScroll = false
        private var reportsUserScrollActivity = false
        private var lastReportedSnapshot: LiveWorkspaceFeedScrollSnapshot?
        private var attachmentGeneration = 0
        private var isLiveScrollActive = false
        private var didReportLiveScrollActivity = false

        init(
            onMetrics: @escaping (LiveWorkspaceFeedScrollSnapshot) -> Void,
            onUserScrollStarted: @escaping () -> Void,
            onUserScrollActivity: @escaping () -> Void,
            onUserScroll: @escaping (LiveWorkspaceFeedScrollSnapshot) -> Void
        ) {
            self.onMetrics = onMetrics
            self.onUserScrollStarted = onUserScrollStarted
            self.onUserScrollActivity = onUserScrollActivity
            self.onUserScroll = onUserScroll
        }

        deinit {
            // SwiftUI가 빠른 직원 전환 중 dismantle 콜백을 건너뛰더라도
            // NotificationCenter observer가 다음 호스트까지 남지 않게 한다.
            detach()
        }

        func attach(to scrollView: NSScrollView?) {
            let documentView = scrollView?.documentView
            guard
                self.scrollView !== scrollView
                    || self.documentView !== documentView
            else {
                return
            }
            detach()
            guard let scrollView, let documentView else {
                return
            }
            self.scrollView = scrollView
            self.documentView = documentView
            scrollView.contentView.postsBoundsChangedNotifications = true
            scrollView.contentView.postsFrameChangedNotifications = true
            boundsObserver = NotificationCenter.default.addObserver(
                forName: NSView.boundsDidChangeNotification,
                object: scrollView.contentView,
                queue: .main
            ) { [weak self] _ in
                self?.scheduleMetricsReport()
            }
            clipFrameObserver = NotificationCenter.default.addObserver(
                forName: NSView.frameDidChangeNotification,
                object: scrollView.contentView,
                queue: .main
            ) { [weak self] _ in
                self?.scheduleMetricsReport()
            }
            liveScrollStartObserver = NotificationCenter.default.addObserver(
                forName: NSScrollView.willStartLiveScrollNotification,
                object: scrollView,
                queue: .main
            ) { [weak self] _ in
                guard let self else {
                    return
                }
                self.isLiveScrollActive = true
                self.didReportLiveScrollActivity = false
                self.onUserScrollStarted()
            }
            liveScrollUpdateObserver = NotificationCenter.default.addObserver(
                forName: NSScrollView.didLiveScrollNotification,
                object: scrollView,
                queue: .main
            ) { [weak self] _ in
                guard let self else {
                    return
                }
                if !self.isLiveScrollActive {
                    self.isLiveScrollActive = true
                    self.didReportLiveScrollActivity = false
                    self.onUserScrollStarted()
                }
                if !self.didReportLiveScrollActivity {
                    self.didReportLiveScrollActivity = true
                    self.reportsUserScrollActivity = true
                }
                self.scheduleMetricsReport()
            }
            liveScrollEndObserver = NotificationCenter.default.addObserver(
                forName: NSScrollView.didEndLiveScrollNotification,
                object: scrollView,
                queue: .main
            ) { [weak self] _ in
                guard let self, self.isLiveScrollActive else {
                    return
                }
                self.isLiveScrollActive = false
                self.didReportLiveScrollActivity = false
                self.scheduleMetricsReport(reportsUserScroll: true)
            }
            scheduleMetricsReport()
        }

        func detach() {
            attachmentGeneration &+= 1
            if let boundsObserver {
                NotificationCenter.default.removeObserver(boundsObserver)
            }
            if let liveScrollStartObserver {
                NotificationCenter.default.removeObserver(
                    liveScrollStartObserver
                )
            }
            if let liveScrollUpdateObserver {
                NotificationCenter.default.removeObserver(
                    liveScrollUpdateObserver
                )
            }
            if let liveScrollEndObserver {
                NotificationCenter.default.removeObserver(liveScrollEndObserver)
            }
            if let clipFrameObserver {
                NotificationCenter.default.removeObserver(clipFrameObserver)
            }
            boundsObserver = nil
            clipFrameObserver = nil
            liveScrollStartObserver = nil
            liveScrollUpdateObserver = nil
            liveScrollEndObserver = nil
            scrollView = nil
            documentView = nil
            isReportScheduled = false
            reportsUserScroll = false
            reportsUserScrollActivity = false
            lastReportedSnapshot = nil
            isLiveScrollActive = false
            didReportLiveScrollActivity = false
        }

        private func scheduleMetricsReport(
            reportsUserScroll: Bool = false
        ) {
            if reportsUserScroll {
                self.reportsUserScroll = true
            }
            guard !isReportScheduled else {
                return
            }
            isReportScheduled = true
            let generation = attachmentGeneration
            DispatchQueue.main.asyncAfter(
                deadline: .now() + .milliseconds(16)
            ) { [weak self] in
                guard let self else {
                    return
                }
                guard self.attachmentGeneration == generation else {
                    return
                }
                self.isReportScheduled = false
                let reportsUserScroll = self.reportsUserScroll
                let reportsUserScrollActivity =
                    self.reportsUserScrollActivity
                self.reportsUserScroll = false
                self.reportsUserScrollActivity = false
                guard let snapshot = self.scrollSnapshot() else {
                    return
                }
                if reportsUserScrollActivity {
                    self.onUserScrollActivity()
                }
                if
                    self.lastReportedSnapshot?.isApproximatelyEqual(
                        to: snapshot
                    ) != true
                {
                    self.lastReportedSnapshot = snapshot
                    self.onMetrics(snapshot)
                }
                if reportsUserScroll {
                    self.onUserScroll(snapshot)
                }
            }
        }

        private func scrollSnapshot() -> LiveWorkspaceFeedScrollSnapshot? {
            guard
                let scrollView,
                let documentView,
                documentView === scrollView.documentView
            else {
                return nil
            }
            let documentBounds = documentView.bounds
            let visibleRect = scrollView.documentVisibleRect
            return LiveWorkspaceFeedScrollGeometry.snapshot(
                documentBounds: documentBounds,
                visibleRect: visibleRect,
                isFlipped: documentView.isFlipped
            )
        }

    }

    final class AttachmentView: NSView {
        var onHierarchyChange: (() -> Void)?

        override func viewDidMoveToSuperview() {
            super.viewDidMoveToSuperview()
            onHierarchyChange?()
        }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            onHierarchyChange?()
        }
    }
}

/// SwiftUI의 비공개 NSView 잎 개수와 무관하게 대화 묶음 하나의 실체화를
/// 계측하는 표식이다. 배경 전체 크기를 받아 viewport 교차 여부도 잴 수 있다.
private struct LiveWorkspaceFeedCardMarker: NSViewRepresentable {
    func makeNSView(context: Context) -> LiveWorkspaceFeedCardMarkerView {
        LiveWorkspaceFeedCardMarkerView()
    }

    func updateNSView(
        _ nsView: LiveWorkspaceFeedCardMarkerView,
        context: Context
    ) {}
}

private final class LiveWorkspaceFeedCardMarkerView: NSView {
    override func hitTest(_ point: NSPoint) -> NSView? {
        nil
    }
}

private struct EquatableLiveTurnCard: View, Equatable {
    let director: AgentDirector
    let turn: LiveFeedTurn
    let workspaceDirectory: String
    let shouldAnimateResponse: Bool
    let shouldAnimateInitialResponse: Bool
    let updateResponseFeedback:
        (String, TurnResponseFeedback?) async -> Void
    let finishResponseAnimation: () -> Void

    static func == (
        lhs: EquatableLiveTurnCard,
        rhs: EquatableLiveTurnCard
    ) -> Bool {
        lhs.director === rhs.director
            && lhs.turn == rhs.turn
            && lhs.workspaceDirectory == rhs.workspaceDirectory
            && lhs.shouldAnimateResponse == rhs.shouldAnimateResponse
            && lhs.shouldAnimateInitialResponse
                == rhs.shouldAnimateInitialResponse
    }

    var body: some View {
        LiveTurnCard(
            director: director,
            turn: turn,
            workspaceDirectory: workspaceDirectory,
            shouldAnimateResponse: shouldAnimateResponse,
            shouldAnimateInitialResponse:
                shouldAnimateInitialResponse,
            updateResponseFeedback: updateResponseFeedback,
            finishResponseAnimation: finishResponseAnimation
        )
    }
}

/// 사용자 질문은 오른쪽 말풍선으로 따로 세운다. 직원 답변과 시선이
/// 갈려야 누가 한 말인지 한눈에 들어온다.
struct LiveTurnPromptBlock: View {
    let presentation: TaskPromptPresentation
    var sentAt: Date?
    var sender: EmployeeMessageSender? = nil
    var selectionID: String? = nil

    @State private var didCopy = false

    /// 질문이 길어도 왼쪽에 이만큼은 남겨 답변과 시선이 갈린다.
    private static let minimumLeadingGap = CGFloat(96)

    var body: some View {
        // Text는 유연한 뷰라 maxWidth를 주면 그 폭을 그대로 채운다.
        // Spacer와 같은 HStack에 두면 Text가 자기 크기를 먼저 가져가고
        // 남는 폭은 Spacer가 흡수해, 말풍선이 내용만큼만 차지한다.
        HStack(spacing: 0) {
            Spacer(minLength: Self.minimumLeadingGap)

            VStack(alignment: .trailing, spacing: 4) {
                bubble
                footer
            }
        }
        .frame(maxWidth: .infinity, alignment: .trailing)
        .fixedSize(horizontal: false, vertical: true)
        .conversationTextSelectionRegion(
            "live-prompt-\(selectionID ?? fallbackSelectionID)"
        )
    }

    private var bubble: some View {
        VStack(alignment: .leading, spacing: 7) {
            EmployeeMessageSenderLabel(sender: sender)
            if !presentation.text.isEmpty {
                // maxWidth를 주면 짧은 질문에도 말풍선이 최대 폭까지
                // 벌어져 오른쪽이 허전해 보인다. 내용 크기로 둔다.
                Text(presentation.text)
                    .font(.system(size: 13, weight: .semibold))
                    .multilineTextAlignment(.leading)
            }

            if !presentation.attachments.isEmpty {
                TaskPromptAttachmentList(
                    attachments: presentation.attachments
                )
            }
        }
        // 여기서 fixedSize(horizontal:false)를 주면 제안된 폭을 그대로
        // 채워 짧은 질문에도 말풍선이 최대 폭까지 벌어진다.
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
        .background(
            DashboardPalette.accent.opacity(0.12),
            in: RoundedRectangle(cornerRadius: 13, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 13, style: .continuous)
                .stroke(DashboardPalette.accent.opacity(0.28), lineWidth: 1)
        }
    }

    private var footer: some View {
        HStack(spacing: 8) {
            if let sentAt {
                Text(
                    OfficeLocalization.date(sentAt, dateStyle: .omitted, time: .shortened)
                )
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(.tertiary)
            }

            Button(action: copyPrompt) {
                Label(
                    didCopy ? OfficeLocalization.string("복사함") : OfficeLocalization.string("복사"),
                    systemImage: didCopy ? "checkmark" : "doc.on.doc"
                )
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(
                    didCopy ? DashboardPalette.accent : Color.secondary
                )
            }
            .buttonStyle(.plain)
            .accessibilityLabel(OfficeLocalization.string("질문 복사"))
            .help(OfficeLocalization.string("질문 복사"))
        }
    }

    private func copyPrompt() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(presentation.text, forType: .string)
        didCopy = true
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(1.6))
            didCopy = false
        }
    }

    private var fallbackSelectionID: String {
        let timestamp = sentAt?.timeIntervalSinceReferenceDate ?? 0
        return "\(presentation.text.hashValue)-\(timestamp)"
    }
}

private struct LiveTurnCard: View {
    let director: AgentDirector
    let turn: LiveFeedTurn
    let workspaceDirectory: String
    let shouldAnimateResponse: Bool
    let shouldAnimateInitialResponse: Bool
    let updateResponseFeedback:
        (String, TurnResponseFeedback?) async -> Void
    let finishResponseAnimation: () -> Void
    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            CharacterBadge(
                name: turn.characterName,
                characterID: turn.characterId,
                size: 38
            )

            VStack(alignment: .leading, spacing: 11) {
                metadata

                if
                    turn.status.isRunning
                        || !transcriptActivities.isEmpty
                        || !turn.response.isEmpty
                {
                    switch effectiveBackend {
                    case .codex:
                        codexTranscript
                    case .claude, .antigravity:
                        claudeTranscript
                    }
                }

                if !promptSuggestions.isEmpty {
                    AgentPromptSuggestionList(
                        director: director,
                        character: OfficeCharacter(
                            rawValue: turn.characterId
                        ),
                        suggestions: promptSuggestions
                    )
                }

                if let warning = turn.responseSourceWarning, !warning.isEmpty {
                    ResponseSourceWarningView(message: warning)
                }

                if let warning = turn.wikiProposalWarning, !warning.isEmpty {
                    ResponseSourceWarningView(
                        message: warning,
                        accessibilityIdentifier: "wikiProposalWarning"
                    )
                }

                if ResponseSourceDisplayPolicy.showsSources(
                    hasSources: !turn.responseSources.isEmpty,
                    isRunning: turn.status.isRunning,
                    animatesResponse: shouldAnimateResponse
                ) {
                    ResponseSourceList(
                        sources: turn.responseSources,
                        workspaceDirectory: effectiveWorkspaceDirectory
                    )
                    .transition(.opacity)
                }

                if let error = turn.errorMessage,
                    turn.response.isEmpty
                        || turn.status == .failed
                        || turn.status == .interrupted
                {
                    errorBlock(error)
                }

                // 확인 질문은 팝업 대신 이 카드 안에서 바로 답변한다.
                if
                    CodexResponseDisplayPolicy
                        .showsInlineQuestionAnswer(
                            needsInput: turn.needsInput,
                            backend: effectiveBackend,
                            animatesResponse: shouldAnimateResponse
                        ),
                    let character = OfficeCharacter(
                        rawValue: turn.characterId
                    )
                {
                    InlineQuestionAnswerView(
                        director: director,
                        character: character,
                        turnID: turn.id,
                        needsInput: turn.needsInput,
                        question: turn.response
                    )
                }

                if turn.status.isRunning || turn.endedAt != nil {
                    LiveTurnElapsedStatusView(
                        startedAt: turn.startedAt,
                        endedAt: turn.endedAt,
                        status: turn.status,
                        estimatedCostUsd: turn.estimatedCostUsd,
                        isLocal: turn.providerKind == "local",
                        sessionContext: turn.sessionContext
                    )
                        .frame(maxWidth: .infinity, alignment: .trailing)
                }
            }
            .animation(
                .easeOut(duration: 0.22),
                value: turn.response.isEmpty
            )
            .padding(14)
            .background(
                Color(nsColor: .controlBackgroundColor).opacity(0.72),
                in: RoundedRectangle(cornerRadius: 17, style: .continuous)
            )
            .overlay {
                RoundedRectangle(cornerRadius: 17, style: .continuous)
                    .stroke(Color.primary.opacity(0.07))
            }
        }
        .conversationTextSelectionRegion("live-turn-\(turn.id)")
    }

    private var metadata: some View {
        HStack(spacing: 7) {
            Text(turn.characterName)
                .font(.system(size: 13, weight: .bold))

            if let backend = turn.backend {
                Text(backend.title)
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(.secondary)

                if let model = turn.model {
                    Text(backend.modelTitle(model))
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(.secondary)
                }
            } else {
                Text(OfficeLocalization.string("이전 기록"))
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(.tertiary)
            }

            if let effort = turn.effort {
                Text(effort)
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(.secondary)
            }

            Text(agentExecutionModeTitle(turn.fastMode))
                .font(.system(size: 10, weight: .bold))
                .foregroundStyle(
                    turn.fastMode == true
                        ? DashboardPalette.accent
                        : Color(nsColor: .secondaryLabelColor)
                )

            if turn.origin == "terminal" {
                Text(OfficeLocalization.string("터미널"))
                    .font(.system(size: 9.5, weight: .bold))
                    .foregroundStyle(DashboardPalette.accent)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(
                        DashboardPalette.accent.opacity(0.10),
                        in: Capsule()
                    )
                    .accessibilityIdentifier("terminalTurnBadge")
            }
            if turn.providerKind == "local" {
                Text(OfficeLocalization.string("로컬 AI"))
                    .font(.system(size: 9.5, weight: .bold))
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(DashboardPalette.accent.opacity(0.10), in: Capsule())
                    .accessibilityIdentifier("localTurnBadge")
            }

            Spacer()

            if !turn.status.isRunning {
                statusBadge
            }

        }
    }

    private var claudeTranscript: some View {
        ClaudeTranscriptView(
            backend: effectiveBackend,
            turnID: turn.id,
            workspaceDirectory: effectiveWorkspaceDirectory,
            activities: transcriptActivities,
            response: turn.response,
            responseUpdatedAt: turn.updatedAt,
            isRunning: turn.status.isRunning,
            isCompleted: turn.status == .completed,
            needsInput: turn.needsInput,
            animatesResponse: shouldAnimateResponse,
            animatesInitialResponse: shouldAnimateInitialResponse,
            responseFeedback: turn.feedback,
            updateResponseFeedback: { feedback in
                await updateResponseFeedback(turn.id, feedback)
            },
            onResponsePresented: finishResponseAnimation
        )
    }

    private var codexTranscript: some View {
        CodexTranscriptView(
            turnID: turn.id,
            workspaceDirectory: effectiveWorkspaceDirectory,
            activities: transcriptActivities,
            response: turn.response,
            responseUpdatedAt: turn.updatedAt,
            isRunning: turn.status.isRunning,
            isCompleted: turn.status == .completed,
            needsInput: turn.needsInput,
            animatesResponse: shouldAnimateResponse,
            animatesInitialResponse: shouldAnimateInitialResponse,
            responseFeedback: turn.feedback,
            updateResponseFeedback: { feedback in
                await updateResponseFeedback(turn.id, feedback)
            },
            onResponsePresented: finishResponseAnimation
        )
    }

    private var effectiveBackend: AgentBackend {
        turn.backend ?? turn.characterBackend
    }

    private var transcriptActivities: [LiveFeedActivity] {
        turn.activities.filter { $0.kind != "suggestion" }
    }

    private var promptSuggestions: [String] {
        var seen = Set<String>()
        return Array(turn.activities.compactMap { activity in
            guard activity.kind == "suggestion" else {
                return nil
            }
            let suggestion = activity.text.trimmingCharacters(
                in: .whitespacesAndNewlines
            )
            guard !suggestion.isEmpty, seen.insert(suggestion).inserted else {
                return nil
            }
            return suggestion
        }.prefix(3))
    }

    private var effectiveWorkspaceDirectory: String {
        turn.workspace?.fileBaseDirectory(
            fallback: turn.conversationWorkdir ?? workspaceDirectory
        )
            ?? turn.conversationWorkdir
            ?? workspaceDirectory
    }

    private func errorBlock(_ error: String) -> some View {
        Label(OfficeLocalization.systemMessage(error), systemImage: "exclamationmark.triangle.fill")
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(Color.red.opacity(0.88))
    }

    private var statusBadge: some View {
        HStack(spacing: 4) {
            Circle()
                .fill(statusColor)
                .frame(width: 6, height: 6)
            Text(statusTitle)
        }
        .font(.system(size: 9, weight: .bold))
        .foregroundStyle(statusColor)
        .padding(.horizontal, 7)
        .padding(.vertical, 4)
        .background(
            statusColor.opacity(0.10),
            in: Capsule()
        )
    }

    private var statusTitle: String {
        switch turn.status {
        case .pending:
            OfficeLocalization.string("대기")
        case .running:
            OfficeLocalization.string("업무 중")
        case .completed:
            turn.needsInput
                ? OfficeLocalization.string("답변 필요")
                : OfficeLocalization.string("완료")
        case .failed:
            OfficeLocalization.string("중단")
        case .interrupted:
            OfficeLocalization.string("연결 종료")
        }
    }

    private var statusColor: Color {
        switch turn.status {
        case .pending, .running:
            DashboardPalette.accent
        case .completed:
            turn.needsInput ? .orange : .green
        case .failed, .interrupted:
            .red
        }
    }

}

private struct AgentPromptSuggestionList: View {
    @ObservedObject var director: AgentDirector
    let character: OfficeCharacter?
    let suggestions: [String]

    private var isSending: Bool {
        guard let character else {
            return false
        }
        return director.runningCharacters.contains(character)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(OfficeLocalization.string("다음 질문 추천"), systemImage: "sparkles")
                .font(.system(size: 11, weight: .bold))
                .foregroundStyle(DashboardPalette.accent)

            ForEach(Array(suggestions.enumerated()), id: \.offset) { index, text in
                if let character {
                    Button {
                        director.submit(text, to: character)
                    } label: {
                        suggestionRow(text)
                    }
                    .buttonStyle(.plain)
                    .disabled(isSending)
                    .opacity(isSending ? 0.45 : 1)
                    .help(OfficeLocalization.string("눌러서 이 질문 보내기"))
                    .accessibilityIdentifier(
                        "promptSuggestion.\(index + 1)"
                    )
                    .accessibilityLabel(
                        OfficeLocalization.format("%@ 보내기", text)
                    )
                } else {
                    suggestionRow(text)
                }
            }
        }
        .padding(.horizontal, 11)
        .padding(.vertical, 10)
        .background(
            DashboardPalette.accent.opacity(0.055),
            in: RoundedRectangle(cornerRadius: 11, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .stroke(DashboardPalette.accent.opacity(0.16))
        }
    }

    private func suggestionRow(_ text: String) -> some View {
        HStack(alignment: .top, spacing: 7) {
            Image(systemName: "arrow.turn.down.right")
                .font(.system(size: 9, weight: .semibold))
                .foregroundStyle(.secondary)
                .padding(.top, 3)

            Text(text)
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(.primary)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .contentShape(Rectangle())
    }
}

private struct LiveTurnElapsedStatusView: View {
    let startedAt: Date
    let endedAt: Date?
    let status: LiveTurnStatus
    let estimatedCostUsd: Double?
    var isLocal: Bool = false
    let sessionContext: SessionContextUsage?

    @ViewBuilder
    var body: some View {
        VStack(alignment: .trailing, spacing: 2) {
            if status.isRunning {
                TimelineView(.periodic(from: .now, by: 5)) { context in
                    Text(OfficeLocalization.format(
                        "%@째 진행 중",
                        elapsedText(at: context.date)
                    ))
                        .font(statusFont)
                        .foregroundStyle(DashboardPalette.accent)
                        .accessibilityLabel(
                            OfficeLocalization.format(
                                "%@째 진행 중",
                                elapsedText(at: context.date)
                            )
                        )
                }
            } else if let endedAt {
                Text(OfficeLocalization.format(
                    "%@에 %@",
                    elapsedText(at: endedAt),
                    terminalTitle
                ))
                    .font(statusFont)
                    .foregroundStyle(terminalColor)
                    .accessibilityLabel(
                        OfficeLocalization.format(
                            "%@에 %@",
                            elapsedText(at: endedAt),
                            terminalTitle
                        )
                    )

                if isLocal {
                    Text(OfficeLocalization.string("로컬 · API 비용 해당 없음 (전기 비용 미집계)"))
                        .font(supplementFont)
                        .foregroundStyle(.secondary)
                } else if let estimatedCostUsd {
                    Text(estimatedTokenCostText(estimatedCostUsd))
                        .font(supplementFont)
                        .foregroundStyle(
                            estimatedTokenCostColor(estimatedCostUsd)
                        )
                        .accessibilityLabel(
                            estimatedTokenCostText(estimatedCostUsd)
                        )
                }

                if let sessionContext {
                    Text(sessionContextRemainingText(sessionContext))
                        .font(supplementFont)
                        .foregroundStyle(
                            sessionContextColor(sessionContext)
                        )
                        .accessibilityLabel(
                            sessionContextRemainingText(sessionContext)
                        )
                }
            }
        }
    }

    private var supplementFont: Font {
        .system(size: 9.5, weight: .medium, design: .monospaced)
    }

    private func sessionContextColor(
        _ usage: SessionContextUsage
    ) -> Color {
        if usage.remainingRatio <= 0.1 {
            return .red
        }
        if usage.remainingRatio <= 0.25 {
            return .orange
        }
        return .secondary
    }

    private func estimatedTokenCostColor(_ costUsd: Double) -> Color {
        switch estimatedTokenCostEmphasis(costUsd) {
        case .standard:
            .secondary
        case .warning:
            .orange
        case .critical:
            .red
        }
    }

    private var statusFont: Font {
        .system(
            size: 10.5,
            weight: .bold,
            design: .monospaced
        )
    }

    private var terminalTitle: String {
        switch status {
        case .completed:
            OfficeLocalization.string("완료")
        case .failed:
            OfficeLocalization.string("실패")
        case .interrupted:
            OfficeLocalization.string("중단")
        case .pending, .running:
            OfficeLocalization.string("진행 중")
        }
    }

    private var terminalColor: Color {
        switch status {
        case .completed:
            .green
        case .failed, .interrupted:
            .red
        case .pending, .running:
            DashboardPalette.accent
        }
    }

    private func elapsedText(at date: Date) -> String {
        let seconds = max(0, Int(date.timeIntervalSince(startedAt)))
        if seconds < 60 {
            return OfficeLocalization.format("%d초", seconds)
        }

        let minutes = seconds / 60
        if minutes < 60 {
            return OfficeLocalization.format(
                "%d분 %d초",
                minutes,
                seconds % 60
            )
        }

        return OfficeLocalization.format(
            "%d시간 %d분",
            minutes / 60,
            minutes % 60
        )
    }
}

enum EstimatedTokenCostEmphasis: Equatable {
    case standard
    case warning
    case critical
}

func estimatedTokenCostEmphasis(
    _ costUsd: Double
) -> EstimatedTokenCostEmphasis {
    if costUsd >= 10 {
        return .critical
    }
    if costUsd >= 5 {
        return .warning
    }
    return .standard
}

func estimatedTokenCostText(_ costUsd: Double) -> String {
    let precision = costUsd < 0.0001 ? 8 : costUsd < 1 ? 6 : 4
    return OfficeLocalization.format(
        "토큰 환산 비용(추정) %@",
        String(format: "$%.\(precision)f", costUsd)
    )
}

func sessionContextRemainingText(_ usage: SessionContextUsage) -> String {
    let percent = String(
        format: "%.1f",
        usage.remainingRatio * 100
    )
    return OfficeLocalization.format(
        "컨텍스트 잔량 %@ / %@ (%@%%)",
        groupedTokenCount(usage.remainingTokens),
        groupedTokenCount(usage.limitTokens),
        percent
    )
}

private func groupedTokenCount(_ value: Int) -> String {
    let digits = Array(String(max(0, value)))
    var grouped: [String] = []
    var index = digits.count
    while index > 3 {
        grouped.insert(String(digits[(index - 3)..<index]), at: 0)
        index -= 3
    }
    grouped.insert(String(digits[0..<index]), at: 0)
    return grouped.joined(separator: ",")
}

struct EquatableLiveTypingResponseView: View, Equatable {
    let typingIdentity: String
    let source: String
    let fileBaseDirectory: String?
    let animates: Bool
    let animatesInitialSource: Bool
    let isStreaming: Bool
    let onFinishedTyping: () -> Void

    static func == (
        lhs: EquatableLiveTypingResponseView,
        rhs: EquatableLiveTypingResponseView
    ) -> Bool {
        lhs.typingIdentity == rhs.typingIdentity
            && lhs.source == rhs.source
            && lhs.fileBaseDirectory == rhs.fileBaseDirectory
            && lhs.animates == rhs.animates
            && lhs.animatesInitialSource == rhs.animatesInitialSource
            && lhs.isStreaming == rhs.isStreaming
    }

    var body: some View {
        LiveTypingResponseView(
            typingIdentity: typingIdentity,
            source: source,
            fileBaseDirectory: fileBaseDirectory,
            animates: animates,
            animatesInitialSource: animatesInitialSource,
            isStreaming: isStreaming,
            onFinishedTyping: onFinishedTyping
        )
    }
}

enum CompletedResponseLineRenderKind: Equatable {
    case markdown
    case blank
    case codeFence
    case code
    case table
}

struct CompletedResponseLine: Identifiable, Equatable {
    let index: Int
    let source: String
    let renderKind: CompletedResponseLineRenderKind

    var id: Int { index }
}

struct CompletedResponseLineSequence: Equatable {
    let lines: [CompletedResponseLine]

    init(source: String) {
        let rawLines = source.split(
            separator: "\n",
            omittingEmptySubsequences: false
        ).map(String.init)
        var inCodeFence = false
        lines = rawLines.enumerated().map { index, line in
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            let isFence = trimmed.hasPrefix("```")
                || trimmed.hasPrefix("~~~")
            let renderKind: CompletedResponseLineRenderKind
            if isFence {
                renderKind = .codeFence
                inCodeFence.toggle()
            } else if inCodeFence {
                renderKind = .code
            } else if trimmed.isEmpty {
                renderKind = .blank
            } else if trimmed.hasPrefix("|"), trimmed.hasSuffix("|") {
                // 표는 여러 줄을 다시 합쳐 렌더링하면 완료된 앞줄이
                // 교체된다. 각 행을 고정 폭 한 줄로 보존한다.
                renderKind = .table
            } else {
                renderKind = .markdown
            }
            return CompletedResponseLine(
                index: index,
                source: line,
                renderKind: renderKind
            )
        }
    }

    func isLastLine(_ lineIndex: Int) -> Bool {
        lineIndex >= lines.count - 1
    }
}

enum CompletedResponseRenderPlan {
    /// 줄 단위 렌더는 타자 중 높이를 고정하려는 수단이라 표·목록·코드펜스처럼
    /// 여러 줄이 모여야 의미가 생기는 블록을 복원하지 못한다.
    /// 타자가 끝났거나 아예 재생하지 않으면 원문 전체를 한 번에 그린다.
    static func rendersWholeSourceMarkdown(
        playsSequence: Bool,
        reduceMotion: Bool,
        hasLines: Bool,
        didFinishTyping: Bool
    ) -> Bool {
        !playsSequence || reduceMotion || !hasLines || didFinishTyping
    }
}

private struct CompletedResponseCommittedLineView: View, Equatable {
    let line: CompletedResponseLine
    let fontSize: CGFloat
    let fileBaseDirectory: String?

    var body: some View {
        Group {
            switch line.renderKind {
            case .markdown:
                ConversationMarkdownView(
                    source: line.source,
                    fontSize: fontSize,
                    fileBaseDirectory: fileBaseDirectory
                )
            case .blank:
                Color.clear.frame(height: 4)
            case .codeFence:
                Color.clear.frame(height: 0)
            case .code, .table:
                Text(line.source.isEmpty ? " " : line.source)
                    .font(.system(size: fontSize, design: .monospaced))
                    .padding(.horizontal, 8)
                    .padding(.vertical, 3)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(
                        Color.primary.opacity(0.045),
                        in: RoundedRectangle(
                            cornerRadius: 4,
                            style: .continuous
                        )
                    )
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// 완성된 Codex 응답을 한 줄씩 빠르게 타이핑하고, 끝난 줄은 Markdown으로
/// 고정한다. 긴 한 줄은 프레임당 글자 수를 늘려 1초 안에 마친다.
struct CompletedResponseLineTypingView: View {
    let typingIdentity: String
    let source: String
    let fontSize: CGFloat
    let fileBaseDirectory: String?
    let compactsChangedFileLists: Bool
    let animates: Bool
    let animatesInitialSource: Bool
    let presentsTyping: Bool
    let onFinishedTyping: () -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var sequence: CompletedResponseLineSequence
    @State private var committedLines: [CompletedResponseLine]
    @State private var currentLineIndex = 0
    @State private var didFinish = false
    @State private var playsSequence: Bool

    init(
        typingIdentity: String,
        source: String,
        fontSize: CGFloat,
        fileBaseDirectory: String?,
        compactsChangedFileLists: Bool = false,
        animates: Bool,
        animatesInitialSource: Bool,
        presentsTyping: Bool,
        onFinishedTyping: @escaping () -> Void
    ) {
        let sequence = CompletedResponseLineSequence(source: source)
        let playsSequence =
            presentsTyping && animates && animatesInitialSource
        self.typingIdentity = typingIdentity
        self.source = source
        self.fontSize = fontSize
        self.fileBaseDirectory = fileBaseDirectory
        self.compactsChangedFileLists = compactsChangedFileLists
        self.animates = animates
        self.animatesInitialSource = animatesInitialSource
        self.presentsTyping = presentsTyping
        self.onFinishedTyping = onFinishedTyping
        _sequence = State(initialValue: sequence)
        _committedLines = State(
            initialValue: playsSequence ? [] : sequence.lines
        )
        _currentLineIndex = State(
            initialValue: playsSequence ? 0 : sequence.lines.count
        )
        _didFinish = State(initialValue: !presentsTyping)
        _playsSequence = State(initialValue: playsSequence)
    }

    var body: some View {
        Group {
            if rendersWholeSourceMarkdown {
                wholeSourceMarkdownBody
                    .task(id: "presented:\(typingIdentity):\(source.hashValue)") {
                        if presentsTyping {
                            finishSequence()
                        }
                    }
            } else {
                typingBody(sequence: sequence)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .onChange(of: source) { _, newSource in
            let newSequence = CompletedResponseLineSequence(
                source: newSource
            )
            let shouldPlay =
                presentsTyping && animates && animatesInitialSource
            sequence = newSequence
            committedLines = shouldPlay ? [] : newSequence.lines
            currentLineIndex = shouldPlay ? 0 : newSequence.lines.count
            didFinish = !presentsTyping
            playsSequence = shouldPlay
        }
    }

    private var rendersWholeSourceMarkdown: Bool {
        CompletedResponseRenderPlan.rendersWholeSourceMarkdown(
            playsSequence: playsSequence,
            reduceMotion: reduceMotion,
            hasLines: !sequence.lines.isEmpty,
            didFinishTyping: didFinish
        )
    }

    /// 타자가 끝난 뒤의 최종 화면이다. 표·목록·코드블록이 여기서 복원된다.
    private var wholeSourceMarkdownBody: some View {
        ConversationMarkdownView(
            source: source,
            fontSize: fontSize,
            fileBaseDirectory: fileBaseDirectory,
            compactsChangedFileLists: compactsChangedFileLists
        )
    }

    private func committedBody(
        lines: [CompletedResponseLine]
    ) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            ForEach(lines) { line in
                CompletedResponseCommittedLineView(
                    line: line,
                    fontSize: fontSize,
                    fileBaseDirectory: fileBaseDirectory
                )
                .equatable()
            }
        }
    }

    @ViewBuilder
    private func currentLineView(
        _ line: CompletedResponseLine,
        in sequence: CompletedResponseLineSequence
    ) -> some View {
        switch line.renderKind {
        case .blank, .codeFence:
            Color.clear
                .frame(height: line.renderKind == .blank ? 4 : 0)
                .task(id: "skip:\(typingIdentity):\(line.index)") {
                    advanceLine(line.index, in: sequence)
                }
        default:
            // 최종 Markdown 줄이 처음부터 높이를 맡고, 타이핑 텍스트는
            // 크기에 관여하지 않는 overlay에서만 그린다. 줄을 치는 동안
            // ScrollView 문서 높이가 16ms마다 변하지 않는다.
            ZStack(alignment: .topLeading) {
                CompletedResponseCommittedLineView(
                    line: line,
                    fontSize: fontSize,
                    fileBaseDirectory: fileBaseDirectory
                )
                .equatable()
                .accessibilityHidden(true)
                .hidden()

                // 링크 URL처럼 원문이 최종 Markdown보다 길어도 타이핑
                // 텍스트가 잘리지 않도록 두 높이 중 큰 쪽을 예약한다.
                Text(line.source.isEmpty ? " " : line.source)
                    .font(.system(size: fontSize))
                    .lineSpacing(3)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityHidden(true)
                    .hidden()
            }
            .overlay(alignment: .topLeading) {
                StreamingPlainTextView(
                    source: line.source,
                    animates: true,
                    animatesInitialSource: true,
                    fontSize: fontSize,
                    lineSpacing: 3,
                    revealMode: .fullLine,
                    onFinishedTyping: {
                        advanceLine(line.index, in: sequence)
                    }
                )
                .id("\(typingIdentity):line:\(line.index)")
                .frame(
                    maxWidth: .infinity,
                    maxHeight: .infinity,
                    alignment: .topLeading
                )
                .clipped()
            }
        }
    }

    @ViewBuilder
    private func typingBody(
        sequence: CompletedResponseLineSequence
    ) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            committedBody(lines: committedLines)

            if currentLineIndex >= sequence.lines.count {
                Color.clear.frame(height: 0)
            } else {
                let line = sequence.lines[currentLineIndex]
                currentLineView(line, in: sequence)
            }
        }
    }

    private func advanceLine(
        _ expectedLineIndex: Int,
        in sequence: CompletedResponseLineSequence
    ) {
        guard
            !didFinish,
            expectedLineIndex == currentLineIndex
        else {
            return
        }
        committedLines.append(sequence.lines[expectedLineIndex])
        currentLineIndex += 1
        while currentLineIndex < sequence.lines.count {
            let nextLine = sequence.lines[currentLineIndex]
            guard
                nextLine.renderKind == .blank
                    || nextLine.renderKind == .codeFence
            else {
                break
            }
            committedLines.append(nextLine)
            currentLineIndex += 1
        }
        if currentLineIndex >= sequence.lines.count {
            finishSequence()
        }
    }

    private func finishSequence() {
        guard !didFinish else {
            return
        }
        didFinish = true
        onFinishedTyping()
    }
}

struct LiveTypingResponseView: View {
    let typingIdentity: String
    let source: String
    let fileBaseDirectory: String?
    let animates: Bool
    let isStreaming: Bool
    let onFinishedTyping: () -> Void

    private static let responseFontSize = CGFloat(14)
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    private let animatesInitialSource: Bool

    init(
        typingIdentity: String,
        source: String,
        fileBaseDirectory: String? = nil,
        animates: Bool,
        animatesInitialSource: Bool = true,
        isStreaming: Bool,
        onFinishedTyping: @escaping () -> Void
    ) {
        self.typingIdentity = typingIdentity
        self.source = source
        self.fileBaseDirectory = fileBaseDirectory
        self.animates = animates
        self.isStreaming = isStreaming
        self.onFinishedTyping = onFinishedTyping
        self.animatesInitialSource =
            animates
                && animatesInitialSource
                && LiveTypingAppearanceCache.shared
                    .shouldAnimateInitialSource(for: typingIdentity)
    }

    var body: some View {
        Group {
            if isStreaming {
                let segments = StreamingMarkdownSplitter.split(source)

                VStack(alignment: .leading, spacing: 9) {
                    // 더 바뀌지 않는 앞부분은 한 번만 렌더링한다.
                    if !segments.settledMarkdown.isEmpty {
                        ConversationMarkdownView(
                            source: segments.settledMarkdown,
                            fontSize: Self.responseFontSize,
                            fileBaseDirectory: fileBaseDirectory
                        )
                    }

                    // 작성 중 블록은 줄이 완성될 때마다 Markdown으로 갱신한다.
                    if !segments.activeMarkdown.isEmpty {
                        ConversationMarkdownView(
                            source: segments.activeMarkdown,
                            fontSize: Self.responseFontSize,
                            fileBaseDirectory: fileBaseDirectory
                        )
                    }

                    // 개행 전 마지막 조각만 평문으로 타자 출력한다.
                    if segments.openLine.isEmpty {
                        // 타자로 출력할 조각이 없으면 곧바로 완료로 알린다.
                        Color.clear
                            .frame(height: 0)
                            .task { onFinishedTyping() }
                    } else {
                        StreamingPlainTextView(
                            source: segments.openLine,
                            animates: animates && !reduceMotion,
                            animatesInitialSource:
                                animatesInitialSource && !reduceMotion,
                            fontSize: Self.responseFontSize,
                            lineSpacing: 3,
                            revealMode: .trailingCharacters,
                            onFinishedTyping: onFinishedTyping
                        )
                        .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                ConversationMarkdownView(
                    source: source,
                    fontSize: Self.responseFontSize,
                    fileBaseDirectory: fileBaseDirectory,
                    compactsChangedFileLists: true
                )
            }
        }
    }
}

@MainActor
private final class LiveTypingAppearanceCache {
    static let shared = LiveTypingAppearanceCache()

    private let storage = NSCache<NSString, NSNumber>()

    private init() {
        storage.countLimit = 256
    }

    func shouldAnimateInitialSource(for identity: String) -> Bool {
        let key = identity as NSString
        guard storage.object(forKey: key) == nil else {
            return false
        }
        storage.setObject(NSNumber(value: true), forKey: key)
        return true
    }
}

struct CharacterBadge: View {
    let name: String
    let characterID: String
    let size: CGFloat

    @Environment(\.presentCharacterProfile) private var presentProfile
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var isHovered = false

    @ViewBuilder
    var body: some View {
        if let presentProfile {
            Button {
                presentProfile(characterID)
            } label: {
                avatar
                    .scaleEffect(
                        isHovered
                            ? CharacterFullBodyProfilePresentationMetrics
                                .avatarHoverScale
                            : 1
                    )
                    .overlay {
                        Circle()
                            .stroke(
                                DashboardPalette.characterAccent(
                                    for: characterID
                                ).opacity(isHovered ? 0.78 : 0),
                                lineWidth: 1.5
                            )
                            .scaleEffect(isHovered ? 1.12 : 0.92)
                    }
                    .shadow(
                        color: DashboardPalette.characterAccent(
                            for: characterID
                        ).opacity(isHovered ? 0.42 : 0),
                        radius: isHovered ? 9 : 0
                    )
            }
            .buttonStyle(CharacterProfileBadgeButtonStyle())
            .onHover { hovered in
                if reduceMotion {
                    isHovered = hovered
                } else {
                    withAnimation(
                        .spring(response: 0.32, dampingFraction: 0.68)
                    ) {
                        isHovered = hovered
                    }
                }
            }
            .help(OfficeLocalization.format("%@ 프로필 보기", name))
            .accessibilityLabel(OfficeLocalization.format("%@ 프로필 보기", name))
        } else {
            avatar
        }
    }

    private var avatar: some View {
        CharacterAvatar(
            name: name,
            characterID: characterID,
            size: size
        )
    }
}

private struct CharacterProfileBadgeButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(
                configuration.isPressed
                    ? CharacterFullBodyProfilePresentationMetrics
                        .avatarPressedScale
                    : 1
            )
            .animation(
                .spring(response: 0.22, dampingFraction: 0.60),
                value: configuration.isPressed
            )
    }
}

private struct CharacterProfilePresentationActionKey: EnvironmentKey {
    static let defaultValue: ((String) -> Void)? = nil
}

extension EnvironmentValues {
    var presentCharacterProfile: ((String) -> Void)? {
        get { self[CharacterProfilePresentationActionKey.self] }
        set { self[CharacterProfilePresentationActionKey.self] = newValue }
    }
}

struct CharacterAvatar: View {
    let name: String
    let characterID: String
    let size: CGFloat

    private var avatarImage: NSImage? {
        CharacterAvatarImageCache.image(for: characterID)
    }

    private var framing: CharacterAvatarFraming {
        CharacterAvatarFraming.forCharacter(characterID)
    }

    var body: some View {
        ZStack {
            Circle()
                .fill(
                    DashboardPalette.characterAccent(for: characterID)
                        .opacity(0.15)
                )

            if let avatarImage {
                Image(nsImage: avatarImage)
                    .resizable()
                    .interpolation(.high)
                    .scaledToFill()
                    .frame(width: size, height: size)
                    .scaleEffect(framing.scale)
                    .offset(
                        y: size * (
                            framing.verticalOffset + 5.0 / 38.0
                        )
                    )
            } else {
                Text(String(name.prefix(1)))
                    .font(
                        .system(
                            size: size * 0.38,
                            weight: .bold,
                            design: .rounded
                        )
                    )
                    .foregroundStyle(.white)
            }
        }
        .frame(width: size, height: size)
        .clipShape(Circle())
        .overlay {
            Circle()
                .stroke(.white.opacity(0.82), lineWidth: 2)
        }
        .shadow(
            color: DashboardPalette.characterAccent(for: characterID)
                .opacity(0.20),
            radius: 5,
            y: 2
        )
    }
}

private struct CharacterAvatarFraming {
    let scale: CGFloat
    let verticalOffset: CGFloat

    static func forCharacter(
        _ characterID: String
    ) -> CharacterAvatarFraming {
        switch OfficeCharacter(rawValue: characterID) {
        case .boss:
            .init(scale: 1.764, verticalOffset: 0.182)
        case .leftMan:
            .init(scale: 1.557, verticalOffset: 0.108)
        case .leftWoman:
            .init(scale: 1.620, verticalOffset: 0.080)
        case .rightWoman:
            .init(scale: 1.791, verticalOffset: 0.199)
        case .rightMan:
            .init(scale: 1.674, verticalOffset: 0.136)
        case nil:
            .init(scale: 1, verticalOffset: 0)
        }
    }
}

@MainActor
private enum CharacterAvatarImageCache {
    private static let images: [OfficeCharacter: NSImage] = Dictionary(
        uniqueKeysWithValues: OfficeCharacter.allCases.compactMap {
            character in
            guard
                let url = PixelOfficeAsset.avatarURL(for: character),
                let image = NSImage(contentsOf: url)
            else {
                return nil
            }
            return (character, image)
        }
    )

    static func image(for characterID: String) -> NSImage? {
        guard let character = OfficeCharacter(rawValue: characterID) else {
            return nil
        }
        return images[character]
    }
}

enum DashboardPalette {
    static let accent = Color(red: 0.13, green: 0.55, blue: 0.52)
    static let claudeAccent = Color(red: 0.77, green: 0.43, blue: 0.25)
    static let antigravityAccent = Color(red: 0.19, green: 0.49, blue: 0.88)

    static func providerAccent(for backend: AgentBackend) -> Color {
        switch backend {
        case .claude:
            claudeAccent
        case .codex:
            accent
        case .antigravity:
            antigravityAccent
        }
    }

    static func canvas(isNight: Bool) -> Color {
        isNight
            ? Color(red: 0.065, green: 0.073, blue: 0.09)
            : Color(red: 0.94, green: 0.945, blue: 0.955)
    }

    static func characterAccent(for characterID: String) -> Color {
        switch characterID {
        case "boss":
            Color(red: 0.34, green: 0.29, blue: 0.52)
        case "left-man":
            Color(red: 0.19, green: 0.50, blue: 0.47)
        case "left-woman":
            Color(red: 0.80, green: 0.43, blue: 0.31)
        case "right-woman":
            Color(red: 0.56, green: 0.35, blue: 0.66)
        default:
            Color(red: 0.78, green: 0.57, blue: 0.22)
        }
    }
}

private struct OfficePanelStyle: ViewModifier {
    @Environment(\.colorScheme) private var colorScheme

    func body(content: Content) -> some View {
        content
            .background(
                Color(nsColor: .windowBackgroundColor).opacity(0.94),
                in: RoundedRectangle(
                    cornerRadius: 20,
                    style: .continuous
                )
            )
            .overlay {
                RoundedRectangle(
                    cornerRadius: 20,
                    style: .continuous
                )
                .stroke(
                    colorScheme == .dark
                        ? Color.white.opacity(0.12)
                        : Color.white.opacity(0.74)
                )
            }
            .shadow(
                color: .black.opacity(
                    colorScheme == .dark ? 0.28 : 0.075
                ),
                radius: 18,
                y: 7
            )
    }
}

extension View {
    func officePanelStyle() -> some View {
        modifier(OfficePanelStyle())
    }
}
