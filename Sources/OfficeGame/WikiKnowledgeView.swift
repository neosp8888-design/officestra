import OfficeCore
import SwiftUI

enum WikiKnowledgeSection: String, CaseIterable, Identifiable {
    case knowledge
    case pending

    var id: String {
        rawValue
    }

    var title: String {
        switch self {
        case .knowledge:
            OfficeLocalization.string("현재 지식")
        case .pending:
            OfficeLocalization.string("확인 대기")
        }
    }

    var icon: String {
        switch self {
        case .knowledge:
            "doc.text.magnifyingglass"
        case .pending:
            "checkmark.seal"
        }
    }
}

enum WikiKnowledgeLayout {
    static let stackedThreshold: CGFloat = 470

    static func usesStackedLayout(for width: CGFloat) -> Bool {
        width < stackedThreshold
    }
}

enum WikiKnowledgeSelection {
    static func resolvedPageID(
        current: String?,
        pages: [WikiPage]
    ) -> String? {
        if let current, pages.contains(where: { $0.id == current }) {
            return current
        }
        return pages.first?.id
    }

    static func normalizedRejectionReason(_ reason: String) -> String {
        reason.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

private struct WikiKnowledgeLoadIdentity: Hashable {
    let section: WikiKnowledgeSection
    let query: String
    let reloadID: UUID
}

struct WikiKnowledgeView: View {
    let databaseBaseURL: URL

    @State private var section = WikiKnowledgeSection.knowledge
    @State private var searchText = ""
    @State private var pages: [WikiPage] = []
    @State private var selectedPageID: String?
    @State private var proposals: [WikiProposal] = []
    @State private var isLoading = true
    @State private var loadError: String?
    @State private var reloadID = UUID()
    @State private var submittingProposalIDs: Set<String> = []
    @State private var rejectionReasons: [String: String] = [:]
    @State private var proposalErrors: [String: String] = [:]
    @State private var actionNotice: String?
    @State private var pendingDeletion: WikiPage?
    @State private var deletingPageIDs: Set<String> = []
    @State private var deletionError: String?

    private static let pageLimit = 60

    var body: some View {
        VStack(spacing: 0) {
            toolbar

            Divider()
                .opacity(0.45)

            content
        }
        .accessibilityIdentifier("wikiKnowledgeView")
        .task(id: loadIdentity) {
            if section == .knowledge && !normalizedSearchText.isEmpty {
                try? await Task.sleep(for: .milliseconds(250))
            }
            guard !Task.isCancelled else {
                return
            }
            await load(section: section, query: normalizedSearchText)
        }
    }

    private var toolbar: some View {
        VStack(spacing: 8) {
            HStack(spacing: 6) {
                ForEach(WikiKnowledgeSection.allCases) { item in
                    Button {
                        withAnimation(.easeInOut(duration: 0.16)) {
                            section = item
                            actionNotice = nil
                            loadError = nil
                        }
                    } label: {
                        Label(item.title, systemImage: item.icon)
                            .font(.system(size: 11, weight: .semibold))
                            .padding(.horizontal, 9)
                            .frame(height: 27)
                            .background(
                                section == item
                                    ? DashboardPalette.accent.opacity(0.14)
                                    : Color.clear,
                                in: Capsule()
                            )
                    }
                    .officeGameTool()
                    .foregroundStyle(
                        section == item
                            ? DashboardPalette.accent
                            : Color.secondary
                    )
                    .accessibilityLabel(item.title)
                    .accessibilityValue(
                        section == item ? OfficeLocalization.string("선택됨") : ""
                    )
                    .accessibilityIdentifier("wikiSection-\(item.rawValue)")
                }

                Spacer(minLength: 4)

                Button {
                    reloadID = UUID()
                } label: {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 11, weight: .semibold))
                        .frame(width: 26, height: 26)
                }
                .officeGameTool()
                .disabled(isLoading)
                .accessibilityLabel(OfficeLocalization.string("위키 새로고침"))
                .accessibilityIdentifier("wikiRefreshButton")
                .help(OfficeLocalization.string("위키 새로고침"))
            }

            if section == .knowledge {
                searchBar
                    .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
    }

    private var searchBar: some View {
        HStack(spacing: 7) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(.secondary)

            TextField(OfficeLocalization.string("제목·본문 검색"), text: $searchText)
                .textFieldStyle(.plain)
                .font(.system(size: 12))
                .accessibilityLabel(OfficeLocalization.string("위키 검색"))
                .accessibilityIdentifier("wikiSearchField")

            if !searchText.isEmpty {
                Button {
                    searchText = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .foregroundStyle(.tertiary)
                }
                .officeGameTool()
                .accessibilityLabel(OfficeLocalization.string("위키 검색어 지우기"))
            }
        }
        .padding(.horizontal, 10)
        .frame(height: 30)
        .background(
            Color.primary.opacity(0.045),
            in: RoundedRectangle(cornerRadius: 9, style: .continuous)
        )
    }

    @ViewBuilder
    private var content: some View {
        if isLoading {
            ProgressView(
                section == .knowledge
                    ? OfficeLocalization.string("지식을 불러오는 중")
                    : OfficeLocalization.string("확인 대상을 불러오는 중")
            )
            .font(.system(size: 11, weight: .medium))
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .accessibilityIdentifier("wikiLoadingState")
        } else if let loadError {
            WikiUnavailableState(
                title: OfficeLocalization.string("위키를 불러오지 못했습니다"),
                systemImage: "exclamationmark.triangle",
                description: loadError,
                actionTitle: OfficeLocalization.string("다시 시도")
            ) {
                reloadID = UUID()
            }
            .accessibilityIdentifier("wikiErrorState")
        } else {
            switch section {
            case .knowledge:
                knowledgeContent
            case .pending:
                pendingContent
            }
        }
    }

    @ViewBuilder
    private var knowledgeContent: some View {
        if pages.isEmpty {
            WikiUnavailableState(
                title: normalizedSearchText.isEmpty
                    ? OfficeLocalization.string("아직 승인된 지식이 없습니다")
                    : OfficeLocalization.string("검색 결과가 없습니다"),
                systemImage: normalizedSearchText.isEmpty
                    ? "books.vertical"
                    : "text.magnifyingglass",
                description: normalizedSearchText.isEmpty
                    ? OfficeLocalization.string("승인된 제안이 이곳에 축적됩니다.")
                    : OfficeLocalization.string("다른 검색어로 다시 찾아보세요.")
            )
            .accessibilityIdentifier("wikiKnowledgeEmptyState")
        } else {
            // 대화 보관함과 같은 방식이다. 좁은 패널에서 목록과 본문을 나눠
            // 보이지 않고, 목록만 넓게 두고 문서를 누르면 넓은 시트로 펼친다.
            pageList
                .confirmationDialog(
                    OfficeLocalization.format(
                        "‘%@’ 문서를 삭제할까요?",
                        pendingDeletion?.title ?? ""
                    ),
                    isPresented: Binding(
                        get: { pendingDeletion != nil },
                        set: { if !$0 { pendingDeletion = nil } }
                    ),
                    titleVisibility: .visible,
                    presenting: pendingDeletion
                ) { page in
                    Button(OfficeLocalization.string("문서 삭제"), role: .destructive) {
                        Task { await delete(page) }
                    }
                    Button(OfficeLocalization.string("취소"), role: .cancel) {}
                } message: { _ in
                    Text(OfficeLocalization.string(WikiKnowledgeView.deletionMessage))
                }
                .sheet(isPresented: isShowingPage) {
                    if let selectedPage, let selectedIndex {
                        WikiOpenBook(
                            page: selectedPage,
                            navigation: ArchiveBookNavigation(
                                index: selectedIndex,
                                total: pages.count,
                                canGoPrevious: selectedIndex > 0,
                                canGoNext: selectedIndex + 1 < pages.count
                            ),
                            isDeleting: deletingPageIDs.contains(selectedPage.id),
                            onPrevious: { showPage(offset: -1) },
                            onNext: { showPage(offset: 1) },
                            onDelete: { Task { await delete(selectedPage) } },
                            onClose: { selectedPageID = nil }
                        )
                        .frame(
                            minWidth: ArchiveBookSheetLayout.minimumWidth,
                            idealWidth: ArchiveBookSheetLayout.idealWidth,
                            minHeight: ArchiveBookSheetLayout.minimumHeight,
                            idealHeight: ArchiveBookSheetLayout.idealHeight
                        )
                    }
                }
        }
    }

    private var isShowingPage: Binding<Bool> {
        Binding(
            get: { selectedPage != nil },
            set: { isPresented in
                if !isPresented {
                    selectedPageID = nil
                }
            }
        )
    }

    private var selectedIndex: Int? {
        guard let selectedPageID else {
            return nil
        }
        return pages.firstIndex(where: { $0.id == selectedPageID })
    }

    private func showPage(offset: Int) {
        guard let selectedIndex else {
            return
        }
        let target = selectedIndex + offset
        guard pages.indices.contains(target) else {
            return
        }
        selectedPageID = pages[target].id
    }

    /// 삭제 확인 대화상자의 설명. 목록 칸과 펼쳐 보기 시트가 같은 문장을 쓴다.
    static let deletionMessage =
        "삭제한 문서는 목록과 검색에서 사라집니다. 같은 키의 제안이 다시 승인되면 다시 나타납니다."

    private var pageList: some View {
        ScrollView {
            LazyVStack(spacing: 6) {
                if let actionNotice {
                    Label(actionNotice, systemImage: "checkmark.circle.fill")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(DashboardPalette.accent)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 4)
                        .accessibilityIdentifier("wikiActionNotice")
                }

                if let deletionError {
                    Label(
                        OfficeLocalization.systemMessage(deletionError),
                        systemImage: "exclamationmark.triangle"
                    )
                    .font(.system(size: 10.5, weight: .medium))
                    .foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 4)
                    .accessibilityIdentifier("wikiDeletionError")
                }

                ForEach(pages) { page in
                    Button {
                        selectedPageID = page.id
                    } label: {
                        VStack(alignment: .leading, spacing: 5) {
                            HStack(alignment: .firstTextBaseline, spacing: 6) {
                                Text(page.title)
                                    .font(.system(size: 12.5, weight: .semibold))
                                    .lineLimit(2)
                                    .multilineTextAlignment(.leading)

                                Spacer(minLength: 4)

                                Image(systemName: "arrow.up.left.and.arrow.down.right")
                                    .font(.system(size: 8.5, weight: .bold))
                                    .foregroundStyle(DashboardPalette.accent)
                                    // 오른쪽 위에 겹쳐 두는 삭제 버튼 자리를 비운다.
                                    .padding(.trailing, 22)
                            }

                            Text(page.pageKey)
                                .font(.system(size: 9.5, design: .monospaced))
                                .foregroundStyle(.secondary)
                                .lineLimit(1)

                            HStack(spacing: 6) {
                                Text(
                                    OfficeLocalization.date(page.updatedAt,
                                        dateStyle: .abbreviated,
                                        time: .shortened
                                    )
                                )
                                Text("·")
                                Text(
                                    OfficeLocalization.format(
                                        "근거 %d건",
                                        page.sources.count
                                    )
                                )
                            }
                            .font(.system(size: 9))
                            .foregroundStyle(.tertiary)
                        }
                        .padding(10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(
                            Color.primary.opacity(0.025),
                            in: RoundedRectangle(
                                cornerRadius: 9,
                                style: .continuous
                            )
                        )
                        .overlay {
                            RoundedRectangle(cornerRadius: 9, style: .continuous)
                                .stroke(Color.primary.opacity(0.06))
                        }
                    }
                    .officeGameTool()
                    .accessibilityLabel(page.title)
                    .accessibilityHint(OfficeLocalization.string("문서를 넓은 창으로 펼치기"))
                    .accessibilityIdentifier("wikiPage-\(page.id)")
                    // 펼치기 버튼 위에 겹친 별도 버튼이라 여기를 누르면
                    // 시트가 열리지 않고 삭제 확인만 뜬다.
                    .overlay(alignment: .topTrailing) {
                        Button {
                            pendingDeletion = page
                        } label: {
                            Image(systemName: "trash")
                                .font(.system(size: 9.5, weight: .semibold))
                                .foregroundStyle(.secondary)
                                .frame(width: 20, height: 20)
                                .contentShape(Rectangle())
                        }
                        .officeGameTool()
                        .disabled(deletingPageIDs.contains(page.id))
                        .padding(6)
                        .accessibilityLabel(OfficeLocalization.string("문서 삭제"))
                        .accessibilityIdentifier("wikiDelete-\(page.id)")
                        .help(OfficeLocalization.string("문서 삭제"))
                    }
                }
            }
            .padding(9)
        }
        .accessibilityIdentifier("wikiPageList")
    }

    @ViewBuilder
    private var pendingContent: some View {
        if proposals.isEmpty {
            WikiUnavailableState(
                title: OfficeLocalization.string("확인할 제안이 없습니다"),
                systemImage: "checkmark.circle",
                description: OfficeLocalization.string("사용자 승인이 필요한 지식 제안이 여기에 표시됩니다.")
            )
            .accessibilityIdentifier("wikiPendingEmptyState")
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    if let actionNotice {
                        Label(actionNotice, systemImage: "checkmark.circle.fill")
                            .font(.system(size: 11, weight: .semibold))
                            .foregroundStyle(DashboardPalette.accent)
                            .padding(.horizontal, 4)
                            .accessibilityIdentifier("wikiActionNotice")
                    }

                    ForEach(proposals) { proposal in
                        WikiProposalCard(
                            proposal: proposal,
                            rejectionReason: rejectionReasonBinding(
                                for: proposal.id
                            ),
                            isSubmitting: submittingProposalIDs.contains(
                                proposal.id
                            ),
                            errorMessage: proposalErrors[proposal.id],
                            approve: {
                                Task {
                                    await decide(
                                        proposal,
                                        decision: .approve
                                    )
                                }
                            },
                            reject: {
                                Task {
                                    await decide(
                                        proposal,
                                        decision: .reject
                                    )
                                }
                            }
                        )
                    }
                }
                .padding(12)
            }
            .accessibilityIdentifier("wikiProposalList")
        }
    }

    private var selectedPage: WikiPage? {
        guard let selectedPageID else {
            return nil
        }
        return pages.first(where: { $0.id == selectedPageID })
    }

    private var normalizedSearchText: String {
        searchText.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private var loadIdentity: WikiKnowledgeLoadIdentity {
        WikiKnowledgeLoadIdentity(
            section: section,
            query: section == .knowledge ? normalizedSearchText : "",
            reloadID: reloadID
        )
    }

    private func rejectionReasonBinding(for id: String) -> Binding<String> {
        Binding(
            get: { rejectionReasons[id, default: ""] },
            set: { rejectionReasons[id] = $0 }
        )
    }

    @MainActor
    private func load(
        section requestedSection: WikiKnowledgeSection,
        query: String
    ) async {
        isLoading = true
        loadError = nil
        let client = OfficeDatabaseClient(baseURL: databaseBaseURL)

        do {
            switch requestedSection {
            case .knowledge:
                let nextPages = try await client.fetchWikiPages(
                    query: query,
                    limit: Self.pageLimit
                )
                guard !Task.isCancelled, section == requestedSection else {
                    return
                }
                pages = nextPages
                // 시트는 문서를 눌렀을 때만 열린다. 펼쳐 둔 문서가 새 목록에서
                // 빠졌으면 시트를 닫는다.
                if let selectedPageID,
                    !nextPages.contains(where: { $0.id == selectedPageID })
                {
                    self.selectedPageID = nil
                }
            case .pending:
                let nextProposals = try await client.fetchWikiProposals()
                guard !Task.isCancelled, section == requestedSection else {
                    return
                }
                proposals = nextProposals
                proposalErrors = proposalErrors.filter { id, _ in
                    nextProposals.contains(where: { $0.id == id })
                }
                rejectionReasons = rejectionReasons.filter { id, _ in
                    nextProposals.contains(where: { $0.id == id })
                }
            }
            isLoading = false
        } catch {
            guard !Task.isCancelled, section == requestedSection else {
                return
            }
            isLoading = false
            loadError = error.localizedDescription
        }
    }

    @MainActor
    private func delete(_ page: WikiPage) async {
        guard !deletingPageIDs.contains(page.id) else {
            return
        }
        deletingPageIDs.insert(page.id)
        deletionError = nil
        actionNotice = nil
        let client = OfficeDatabaseClient(baseURL: databaseBaseURL)

        do {
            try await client.deleteWikiPage(id: page.id)
            pages.removeAll(where: { $0.id == page.id })
            if selectedPageID == page.id {
                selectedPageID = nil
            }
            actionNotice = OfficeLocalization.format(
                "‘%@’ 문서를 삭제했습니다.",
                page.title
            )
        } catch {
            deletionError = error.localizedDescription
        }

        deletingPageIDs.remove(page.id)
    }

    @MainActor
    private func decide(
        _ proposal: WikiProposal,
        decision: WikiProposalDecision
    ) async {
        guard !submittingProposalIDs.contains(proposal.id) else {
            return
        }

        submittingProposalIDs.insert(proposal.id)
        proposalErrors[proposal.id] = nil
        actionNotice = nil
        let client = OfficeDatabaseClient(baseURL: databaseBaseURL)

        do {
            switch decision {
            case .approve:
                _ = try await client.approveWikiProposal(id: proposal.id)
                actionNotice = OfficeLocalization.format(
                    "‘%@’ 제안을 승인했습니다.",
                    proposal.title
                )
            case .reject:
                let reason = WikiKnowledgeSelection.normalizedRejectionReason(
                    rejectionReasons[proposal.id, default: ""]
                )
                _ = try await client.rejectWikiProposal(
                    id: proposal.id,
                    reason: reason
                )
                actionNotice = OfficeLocalization.format(
                    "‘%@’ 제안을 거절했습니다.",
                    proposal.title
                )
            }

            proposals.removeAll(where: { $0.id == proposal.id })
            rejectionReasons[proposal.id] = nil
            proposalErrors[proposal.id] = nil
        } catch {
            proposalErrors[proposal.id] = error.localizedDescription
        }

        submittingProposalIDs.remove(proposal.id)
    }
}

private enum WikiProposalDecision {
    case approve
    case reject
}

/// 대화 보관함의 펼쳐 보기와 같은 두 쪽 시트다. 왼쪽에 본문, 오른쪽에 근거를 둔다.
struct WikiOpenBook: View {
    let page: WikiPage
    let navigation: ArchiveBookNavigation
    let isDeleting: Bool
    let onPrevious: () -> Void
    let onNext: () -> Void
    let onDelete: () -> Void
    let onClose: () -> Void

    @State private var isConfirmingDeletion = false

    private let bookColor = DashboardPalette.accent

    var body: some View {
        VStack(spacing: 0) {
            bookToolbar

            GeometryReader { geometry in
                let pageWidth = max(0, (geometry.size.width - 30) / 2)

                HStack(spacing: 0) {
                    leftPage
                        .frame(width: pageWidth)

                    bookBinding
                        .frame(width: 14)

                    rightPage
                        .frame(width: pageWidth)
                }
                .padding(.horizontal, 8)
                .padding(.bottom, 10)
            }
        }
        .background(
            LinearGradient(
                colors: [
                    bookColor.opacity(0.055),
                    Color.primary.opacity(0.012),
                ],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
        )
        .textSelection(.enabled)
        .accessibilityIdentifier("wikiOpenBook")
    }

    private var bookToolbar: some View {
        HStack(spacing: 9) {
            Image(systemName: "doc.text.magnifyingglass")
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(bookColor)

            VStack(alignment: .leading, spacing: 1) {
                Text(page.title)
                    .font(.system(size: 11, weight: .bold))
                    .lineLimit(1)
                HStack(spacing: 5) {
                    Text(page.pageKey)
                        .font(.system(size: 8.5, design: .monospaced))
                    Text("·")
                    Text(
                        OfficeLocalization.date(page.updatedAt,
                            dateStyle: .abbreviated,
                            time: .shortened
                        )
                    )
                }
                .font(.system(size: 8.5, weight: .medium))
                .foregroundStyle(.secondary)
            }

            Spacer()

            HStack(spacing: 2) {
                pagingButton(
                    systemImage: "chevron.left",
                    label: "이전 문서 보기",
                    isEnabled: navigation.canGoPrevious,
                    shortcut: .leftArrow,
                    action: onPrevious
                )

                Text("\(navigation.index + 1) / \(navigation.total)")
                    .font(.system(size: 9, weight: .bold, design: .rounded))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 3)

                pagingButton(
                    systemImage: "chevron.right",
                    label: "다음 문서 보기",
                    isEnabled: navigation.canGoNext,
                    shortcut: .rightArrow,
                    action: onNext
                )
            }
            .padding(.horizontal, 3)
            .frame(height: 26)
            .background(
                Color.primary.opacity(0.055),
                in: Capsule()
            )

            Button {
                isConfirmingDeletion = true
            } label: {
                Label(OfficeLocalization.string("문서 삭제"), systemImage: "trash")
                    .font(.system(size: 10, weight: .bold))
                    .padding(.horizontal, 9)
                    .frame(height: 26)
                    .background(
                        Color.primary.opacity(0.055),
                        in: Capsule()
                    )
            }
            .officeGameTool()
            .disabled(isDeleting)
            .opacity(isDeleting ? 0.5 : 1)
            .accessibilityLabel(OfficeLocalization.string("문서 삭제"))
            .accessibilityIdentifier("wikiOpenBookDelete")
            .confirmationDialog(
                OfficeLocalization.format("‘%@’ 문서를 삭제할까요?", page.title),
                isPresented: $isConfirmingDeletion,
                titleVisibility: .visible
            ) {
                Button(OfficeLocalization.string("문서 삭제"), role: .destructive) {
                    onDelete()
                }
                Button(OfficeLocalization.string("취소"), role: .cancel) {}
            } message: {
                Text(OfficeLocalization.string(WikiKnowledgeView.deletionMessage))
            }

            Button(action: onClose) {
                Label(OfficeLocalization.string("닫기"), systemImage: "xmark")
                    .font(.system(size: 10, weight: .bold))
                    .padding(.horizontal, 9)
                    .frame(height: 26)
                    .background(
                        Color.primary.opacity(0.055),
                        in: Capsule()
                    )
            }
            .officeGameTool()
            .keyboardShortcut(.cancelAction)
            .accessibilityLabel(OfficeLocalization.string("닫기"))
            .accessibilityIdentifier("wikiOpenBookClose")
        }
        .padding(.horizontal, 12)
        .frame(height: 43)
    }

    private func pagingButton(
        systemImage: String,
        label: String,
        isEnabled: Bool,
        shortcut: KeyEquivalent,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            Image(systemName: systemImage)
                .font(.system(size: 9, weight: .bold))
                .frame(width: 22, height: 22)
                .contentShape(Rectangle())
        }
        .officeGameTool()
        .disabled(!isEnabled)
        .opacity(isEnabled ? 1 : 0.35)
        .keyboardShortcut(shortcut, modifiers: [])
        .accessibilityLabel(OfficeLocalization.string(label))
        .help(OfficeLocalization.string(label))
    }

    private var leftPage: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                pageHeading("본문", systemImage: "text.book.closed.fill")

                Text(page.title)
                    .font(.system(size: 17, weight: .bold))
                    .frame(maxWidth: .infinity, alignment: .leading)

                ConversationMarkdownView(source: page.body, fontSize: 14)
            }
            .padding(12)
        }
        .background(
            pageBackground,
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(Color.primary.opacity(0.07))
        }
        .shadow(color: .black.opacity(0.08), radius: 6, x: -2, y: 3)
        .accessibilityIdentifier("wikiPageDetail")
    }

    private var rightPage: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 9) {
                Label(
                    OfficeLocalization.format("근거 %d건", page.sources.count),
                    systemImage: "link"
                )
                .font(.system(size: 10, weight: .bold))
                .foregroundStyle(bookColor)

                if page.sources.isEmpty {
                    Text(OfficeLocalization.string("연결된 업무 기록이 없습니다."))
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(page.sources) { source in
                        WikiSourceCard(source: source)
                    }
                }
            }
            .padding(12)
            .accessibilityIdentifier("wikiPageSources")
        }
        .background(
            pageBackground,
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(Color.primary.opacity(0.07))
        }
        .shadow(color: .black.opacity(0.08), radius: 6, x: 2, y: 3)
    }

    private var bookBinding: some View {
        LinearGradient(
            colors: [
                Color.black.opacity(0.04),
                bookColor.opacity(0.22),
                Color.black.opacity(0.07),
            ],
            startPoint: .leading,
            endPoint: .trailing
        )
        .overlay {
            Rectangle()
                .fill(Color.white.opacity(0.28))
                .frame(width: 1)
        }
        .padding(.vertical, 5)
    }

    private var pageBackground: some ShapeStyle {
        LinearGradient(
            colors: [
                Color(nsColor: .textBackgroundColor),
                Color(red: 0.98, green: 0.96, blue: 0.89)
                    .opacity(0.56),
            ],
            startPoint: .top,
            endPoint: .bottom
        )
    }

    private func pageHeading(
        _ title: String,
        systemImage: String
    ) -> some View {
        Label(OfficeLocalization.string(title), systemImage: systemImage)
            .font(.system(size: 10, weight: .bold))
            .foregroundStyle(bookColor)
    }
}

private struct WikiSourceCard: View {
    let source: WikiPageSource

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(source.title)
                .font(.system(size: 11, weight: .semibold))
                .textSelection(.enabled)

            Text(source.excerpt)
                .font(.system(size: 10.5))
                .foregroundStyle(.secondary)
                .lineLimit(5)
                .textSelection(.enabled)

            Text(source.workRecordId)
                .font(.system(size: 9, design: .monospaced))
                .foregroundStyle(.tertiary)
                .textSelection(.enabled)
        }
        .padding(9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            Color.primary.opacity(0.035),
            in: RoundedRectangle(cornerRadius: 8, style: .continuous)
        )
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("wikiSource-\(source.workRecordId)")
    }
}

private struct WikiProposalCard: View {
    let proposal: WikiProposal
    @Binding var rejectionReason: String
    let isSubmitting: Bool
    let errorMessage: String?
    let approve: () -> Void
    let reject: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 11) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                VStack(alignment: .leading, spacing: 3) {
                    Text(proposal.title)
                        .font(.system(size: 14, weight: .bold))
                        .textSelection(.enabled)

                    Text(proposal.pageKey)
                        .font(.system(size: 9.5, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }

                Spacer(minLength: 6)

                Text(proposal.approvalTier == "user" ? OfficeLocalization.string("사용자 승인") : proposal.approvalTier)
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(DashboardPalette.accent)
                    .padding(.horizontal, 7)
                    .frame(height: 21)
                    .background(
                        DashboardPalette.accent.opacity(0.11),
                        in: Capsule()
                    )
            }

            ConversationMarkdownView(source: proposal.body, fontSize: 11.5)
                .textSelection(.enabled)

            if !proposal.sourceRecordIds.isEmpty {
                VStack(alignment: .leading, spacing: 3) {
                    Text(OfficeLocalization.string("근거 업무 기록"))
                        .font(.system(size: 10, weight: .semibold))
                    Text(proposal.sourceRecordIds.joined(separator: " · "))
                        .font(.system(size: 9, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
            }

            HStack(spacing: 7) {
                TextField(
                    OfficeLocalization.string("거절 사유 (선택)"),
                    text: $rejectionReason
                )
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 11))
                .disabled(isSubmitting)
                .accessibilityIdentifier(
                    "wikiRejectReason-\(proposal.id)"
                )

                Button(OfficeLocalization.string("승인")) {
                    approve()
                }
                .buttonStyle(.borderedProminent)
                .tint(DashboardPalette.accent)
                .disabled(isSubmitting)
                .accessibilityIdentifier("wikiApprove-\(proposal.id)")

                Button(
                    OfficeLocalization.string("거절"),
                    role: .destructive
                ) {
                    reject()
                }
                .buttonStyle(.bordered)
                .disabled(isSubmitting)
                .accessibilityIdentifier("wikiReject-\(proposal.id)")

                if isSubmitting {
                    ProgressView()
                        .controlSize(.small)
                        .accessibilityLabel(OfficeLocalization.string("처리 중"))
                }
            }

            if let errorMessage {
                Label(OfficeLocalization.systemMessage(errorMessage), systemImage: "exclamationmark.triangle")
                    .font(.system(size: 10.5, weight: .medium))
                    .foregroundStyle(.red)
                    .accessibilityIdentifier(
                        "wikiProposalError-\(proposal.id)"
                    )
            }

            Text(
                OfficeLocalization.date(proposal.createdAt,
                    dateStyle: .abbreviated,
                    time: .shortened
                )
            )
            .font(.system(size: 9))
            .foregroundStyle(.tertiary)
        }
        .padding(12)
        .background(
            Color.primary.opacity(0.035),
            in: RoundedRectangle(cornerRadius: 12, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .stroke(Color.primary.opacity(0.07))
        }
        .accessibilityIdentifier("wikiProposal-\(proposal.id)")
    }
}

private struct WikiUnavailableState: View {
    let title: String
    let systemImage: String
    let description: String
    let actionTitle: String?
    let action: (() -> Void)?

    init(
        title: String,
        systemImage: String,
        description: String,
        actionTitle: String? = nil,
        action: (() -> Void)? = nil
    ) {
        self.title = title
        self.systemImage = systemImage
        self.description = description
        self.actionTitle = actionTitle
        self.action = action
    }

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: systemImage)
                .font(.system(size: 24, weight: .medium))
                .foregroundStyle(.secondary)

            Text(title)
                .font(.system(size: 13, weight: .bold))

            Text(description)
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 300)

            if let actionTitle, let action {
                Button(actionTitle, action: action)
                    .buttonStyle(.bordered)
                    .accessibilityIdentifier("wikiRetryButton")
            }
        }
        .padding(18)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
