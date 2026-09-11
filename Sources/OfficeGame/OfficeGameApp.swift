// 이 파일은 2D·3D 오피스와 명령 입력창을 네이티브 macOS 창에 표시한다.

import AppKit
import OfficeCore
import SwiftUI

@main
struct OfficeGameApp: App {
    @StateObject private var launchCoordinator = OfficeLaunchCoordinator()

    var body: some Scene {
        WindowGroup("OFFICESTRA") {
            OfficeLaunchRootView(coordinator: launchCoordinator)
                .environment(\.locale, OfficeLocalization.locale)
        }
        .defaultSize(width: 1_440, height: 900)
        .windowResizability(.contentMinSize)
    }
}

private struct OfficeLaunchRootView: View {
    @ObservedObject var coordinator: OfficeLaunchCoordinator

    var body: some View {
        switch coordinator.state {
        case .needsWorkspace:
            OfficeWorkspaceSetupView(
                validationError: coordinator.validationError,
                chooseWorkspace: coordinator.chooseWorkspace
            )
        case .preparing(let stage):
            OfficeSetupPreparingView(stage: stage)
        case .needsSetup(let snapshot):
            OfficeEnvironmentSetupView(
                snapshot: snapshot,
                retry: coordinator.retrySetup,
                chooseWorkspace: coordinator.chooseWorkspace,
                useCurrentDatabase: coordinator.useCurrentDatabase,
                useLegacyDatabase: coordinator.useConfirmedLegacyDatabase,
                remapExistingCharacters: coordinator.remapExistingCharacters,
                replaceIdleBackend: coordinator.replaceIdleBackend,
                openDocker: coordinator.openDocker,
                setupCodex: {
                    coordinator.openProviderSetup(.codex)
                },
                setupClaude: {
                    coordinator.openProviderSetup(.claude)
                },
                setupAntigravity: {
                    coordinator.openProviderSetup(.antigravity)
                },
                openLogs: coordinator.openSetupLogs
            )
        case .ready:
            if let director = coordinator.director {
                OfficeGameView(director: director)
                    .overlay(alignment: .top) {
                        if let notice = coordinator.backendCompatibilityNotice {
                            OfficeBackendCompatibilityBanner(
                                notice: notice,
                                replaceBackend: coordinator.replaceIdleBackend
                            )
                            .padding(.top, 12)
                        }
                    }
            } else {
                ProgressView()
            }
        case .failed(let message):
            OfficeLaunchFailureView(message: message)
        }
    }
}

private struct OfficeBackendCompatibilityBanner: View {
    let notice: OfficeBackendCompatibilityNotice
    let replaceBackend: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .padding(.top, 2)

            VStack(alignment: .leading, spacing: 3) {
                Text(OfficeLocalization.string(notice.message))
                    .font(.system(size: 12, weight: .semibold))
                Text(
                    OfficeLocalization.string(
                        "현재 백엔드로 계속 사용할 수 있습니다. 업무가 없을 때만 안전 전환됩니다."
                    )
                )
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                if let errorMessage = notice.errorMessage {
                    Text(OfficeLocalization.systemMessage(errorMessage))
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(.red)
                }
            }

            Spacer(minLength: 6)

            if notice.isReplacing {
                ProgressView()
                    .controlSize(.small)
                    .padding(.top, 3)
            } else {
                Button(
                    notice.replacement.actionTitle,
                    action: replaceBackend
                )
                .controlSize(.small)
                .accessibilityIdentifier(
                    "officeReadyReplaceBackendButton"
                )
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .frame(maxWidth: 620)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay {
            RoundedRectangle(cornerRadius: 12)
                .stroke(Color.orange.opacity(0.55), lineWidth: 1)
        }
        .shadow(color: .black.opacity(0.16), radius: 8, y: 3)
        .accessibilityIdentifier("officeBackendCompatibilityNotice")
    }
}

enum OfficeSplitLayout {
    static let minimumLeftColumnWidth: CGFloat = 300

    static func columnWidths(
        availableWidth: CGFloat,
        fraction: Double,
        minimumColumnWidth: CGFloat
    ) -> (left: CGFloat, right: CGFloat) {
        guard availableWidth > 0 else {
            return (.zero, .zero)
        }
        let requestedLeftWidth = availableWidth
            * CGFloat(min(max(fraction, 0), 1))
        let left = clampedLeftWidth(
            requestedLeftWidth,
            availableWidth: availableWidth,
            minimumColumnWidth: minimumColumnWidth
        )
        return (left, availableWidth - left)
    }

    static func clampedLeftWidth(
        _ width: CGFloat,
        availableWidth: CGFloat,
        minimumColumnWidth: CGFloat
    ) -> CGFloat {
        guard availableWidth > 0 else {
            return .zero
        }
        let minimum = min(max(0, minimumColumnWidth), availableWidth / 2)
        return min(max(width, minimum), availableWidth - minimum)
    }

    static func rowHeights(
        availableHeight: CGFloat,
        fraction: Double
    ) -> (top: CGFloat, bottom: CGFloat) {
        guard availableHeight > 0 else {
            return (.zero, .zero)
        }
        let requestedTopHeight = availableHeight
            * CGFloat(min(max(fraction, 0), 1))
        let top = clampedTopHeight(
            requestedTopHeight,
            availableHeight: availableHeight
        )
        return (top, availableHeight - top)
    }

    static func clampedTopHeight(
        _ height: CGFloat,
        availableHeight: CGFloat
    ) -> CGFloat {
        guard availableHeight > 0 else {
            return .zero
        }
        let minimum = availableHeight / 3
        return min(max(height, minimum), availableHeight - minimum)
    }
}

enum OfficePanelControl {
    case theme
    case backend
    case artStyle
    case terminal
}

/// 터미널 모드에서는 CLI가 직원 세션을 직접 잡고 있으므로 실행 설정을 잠근다.
/// 직원 선택은 각 직원의 터미널로 옮겨 가는 유일한 통로라 잠그지 않는다.
/// 하단 입력창은 글을 CLI에 타이핑한 것처럼 넘기므로 남기되, 첨부는 CLI로
/// 넘길 통로가 없어 잠근다.
enum LiveWorkspaceCommandAvailability {
    enum Control {
        case characterSelector
        case quickSettings
        case profile
        case identitySettings
        case attachments
    }

    static let lockedOpacity: Double = 0.52

    static func isEnabled(
        _ control: Control,
        in mode: OfficeConversationMode
    ) -> Bool {
        guard mode == .terminal else {
            return true
        }
        switch control {
        case .characterSelector, .profile, .identitySettings:
            return true
        case .quickSettings, .attachments:
            return false
        }
    }

    static func opacity(
        _ control: Control,
        in mode: OfficeConversationMode
    ) -> Double {
        isEnabled(control, in: mode) ? 1 : lockedOpacity
    }
}

/// 대화 모드에서 직원이 일하는 중이면 터미널 모드로 들어가지 않는다. 터미널이
/// 열리면 CLI가 직원 세션을 잡아 진행 중인 응답이 끊기고, 백엔드도 일하는
/// 직원의 터미널 열기를 거부한다. 작업이 끝난 뒤에만 전환한다.
enum ConversationModeSwitchPolicy {
    static func terminalEntryBlockMessage(
        runningCharacterNames: [String]
    ) -> String? {
        guard !runningCharacterNames.isEmpty else {
            return nil
        }
        return OfficeLocalization.format(
            "직원이 일하는 중입니다. 작업이 끝난 뒤 다시 시도하세요.\n일하는 직원: %@",
            runningCharacterNames.joined(separator: ", ")
        )
    }

    static func toggleOpacity(
        mode: OfficeConversationMode,
        hasRunningWork: Bool
    ) -> Double {
        mode == .chat && hasRunningWork
            ? LiveWorkspaceCommandAvailability.lockedOpacity
            : 1
    }
}

enum OfficePanelControlLayout {
    static let artStyleControlDiameter: CGFloat = 36

    static func alignment(for control: OfficePanelControl) -> Alignment {
        switch control {
        case .theme:
            .topLeading
        case .backend:
            .bottomLeading
        case .artStyle:
            .bottomTrailing
        case .terminal:
            .topTrailing
        }
    }
}

private enum TerminalModeAlert: Identifiable {
    case confirmRunningTurn
    case blockedByRunningWork(String)
    case statusError(String)

    var id: String {
        switch self {
        case .confirmRunningTurn:
            "confirm-running-turn"
        case .blockedByRunningWork:
            "blocked-by-running-work"
        case .statusError(let message):
            "status-error:\(message)"
        }
    }
}

private struct OfficeGameView: View {
    @ObservedObject var director: AgentDirector
    @StateObject private var backendController = OfficeBackendController()
    @State private var profileCharacter: OfficeCharacter?
    @State private var historyTarget: ConversationHistoryTarget?
    @State private var bubbleDetail: BubbleDetail?
    @State private var detailSelection = OfficeDetailSelection.archive
    @State private var outgoingArtStyle: OfficeArtStyle?
    @State private var artStyleRevealProgress: CGFloat = 1
    @State private var splitDragStartLeftWidth: CGFloat?
    @State private var splitDragStartTopHeight: CGFloat?
    @State private var terminalModeAlert: TerminalModeAlert?
    @AppStorage("officeTheme") private var selectedThemeRawValue =
        OfficeTheme.modernDay.rawValue
    @AppStorage("officeArtStyle") private var selectedArtStyleRawValue =
        OfficeArtStyle.twoD.rawValue
    @AppStorage("officeConversationMode")
    private var selectedConversationModeRawValue =
        OfficeConversationMode.defaultValue.rawValue
    @AppStorage("officeLeftColumnFraction") private var leftColumnFraction =
        0.5
    @AppStorage("officeTopRowFraction") private var topRowFraction = 0.5
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        GeometryReader { geometry in
            let outerPadding: CGFloat = 14
            let columnDividerWidth: CGFloat = 14
            let rowDividerHeight: CGFloat = 14
            let availableColumnWidth = max(
                0,
                geometry.size.width - outerPadding * 2 - columnDividerWidth
            )
            let columns = OfficeSplitLayout.columnWidths(
                availableWidth: availableColumnWidth,
                fraction: leftColumnFraction,
                minimumColumnWidth: OfficeSplitLayout.minimumLeftColumnWidth
            )
            let availableRowHeight = max(
                0,
                geometry.size.height - outerPadding * 2 - rowDividerHeight
            )
            let rows = OfficeSplitLayout.rowHeights(
                availableHeight: availableRowHeight,
                fraction: topRowFraction
            )

            HStack(spacing: 0) {
                VStack(spacing: 0) {
                    officePanel
                        .frame(height: rows.top)

                    OfficeRowResizeHandle(
                        onDragChanged: { translation in
                            updateTopRowFraction(
                                translation: translation,
                                topRowHeight: rows.top,
                                availableRowHeight: availableRowHeight
                            )
                        },
                        onDragEnded: {
                            splitDragStartTopHeight = nil
                        }
                    )
                    .frame(height: rowDividerHeight)

                    OfficeDetailPanel(
                        director: director,
                        selection: $detailSelection
                    )
                    .frame(height: rows.bottom)
                }
                .frame(width: columns.left)

                OfficeColumnResizeHandle(
                    onDragChanged: { translation in
                        updateLeftColumnFraction(
                            translation: translation,
                            leftColumnWidth: columns.left,
                            availableColumnWidth: availableColumnWidth
                        )
                    },
                    onDragEnded: {
                        splitDragStartLeftWidth = nil
                    }
                )
                .frame(width: columnDividerWidth)

                liveWorkspacePanel
                    .frame(width: columns.right)
                    .frame(height: availableRowHeight + rowDividerHeight)
            }
            .padding(outerPadding)
        }
        .background(
            DashboardPalette.canvas(isNight: theme.isNight)
                .ignoresSafeArea()
        )
        .preferredColorScheme(theme.isNight ? .dark : .light)
        .frame(minWidth: 1_180, minHeight: 760)
        .environment(\.presentCharacterProfile) { characterID in
            guard let character = OfficeCharacter(rawValue: characterID) else {
                return
            }
            profileCharacter = character
        }
        .sheet(item: $historyTarget) { target in
            switch target {
            case .character(let character):
                CharacterConversationHistoryView(
                    director: director,
                    character: character
                )
            case .archive:
                ConversationArchiveView(director: director)
            }
        }
        .sheet(item: $bubbleDetail) { detail in
            BubbleDetailView(
                director: director,
                character: detail.character,
                name: detail.name,
                message: detail.message,
                isQuestion: detail.isQuestion,
                isFailure: detail.isFailure,
                isOffDuty: detail.isOffDuty
            )
        }
        .sheet(item: $profileCharacter) { characterID in
            if let character = director.characters.first(where: {
                $0.id == characterID
            }) {
                CharacterFullBodyProfileView(
                    character: character,
                    name: director.displayName(for: characterID)
                )
            }
        }
        .onAppear {
            director.selectDefaultCharacterIfNeeded()
        }
        // 확인 질문은 대화 카드 안에서 답변하므로 창을 새로 띄우지 않는다.
        // 말풍선으로 직접 연 질문 창만 답변이 끝나면 닫는다.
        .onChange(of: director.latestQuestion) { _, _ in
            guard
                let detail = bubbleDetail,
                detail.isQuestion,
                director.pendingQuestion(for: detail.character) == nil
            else {
                return
            }
            bubbleDetail = nil
        }
        .alert(item: $terminalModeAlert) { alert in
            switch alert {
            case .confirmRunningTurn:
                Alert(
                    title: Text(OfficeLocalization.string("터미널을 종료할까요?")),
                    message: Text(
                        OfficeLocalization.string(
                            "응답 중인 터미널이 있습니다. 대화 모드로 돌아가면 해당 CLI 프로세스가 종료됩니다."
                        )
                    ),
                    primaryButton: .cancel(Text(OfficeLocalization.string("계속 사용"))),
                    secondaryButton: .destructive(Text(OfficeLocalization.string("대화로 전환"))) {
                        selectedConversationModeRawValue =
                            OfficeConversationMode.chat.rawValue
                    }
                )
            case .blockedByRunningWork(let message):
                Alert(
                    title: Text(OfficeLocalization.string("터미널 모드로 전환할 수 없습니다")),
                    message: Text(OfficeLocalization.string(message)),
                    dismissButton: .default(Text(OfficeLocalization.string("확인")))
                )
            case .statusError(let message):
                Alert(
                    title: Text(OfficeLocalization.string("터미널 상태를 확인하지 못했습니다")),
                    message: Text(OfficeLocalization.string(message)),
                    dismissButton: .default(Text(OfficeLocalization.string("확인")))
                )
            }
        }
    }

    private func updateLeftColumnFraction(
        translation: CGFloat,
        leftColumnWidth: CGFloat,
        availableColumnWidth: CGFloat
    ) {
        guard availableColumnWidth > 0 else {
            return
        }
        let start = splitDragStartLeftWidth ?? leftColumnWidth
        if splitDragStartLeftWidth == nil {
            splitDragStartLeftWidth = start
        }
        let updatedWidth = OfficeSplitLayout.clampedLeftWidth(
            start + translation,
            availableWidth: availableColumnWidth,
            minimumColumnWidth: OfficeSplitLayout.minimumLeftColumnWidth
        )
        leftColumnFraction = Double(updatedWidth / availableColumnWidth)
    }

    private func updateTopRowFraction(
        translation: CGFloat,
        topRowHeight: CGFloat,
        availableRowHeight: CGFloat
    ) {
        guard availableRowHeight > 0 else {
            return
        }
        let start = splitDragStartTopHeight ?? topRowHeight
        if splitDragStartTopHeight == nil {
            splitDragStartTopHeight = start
        }
        let updatedHeight = OfficeSplitLayout.clampedTopHeight(
            start + translation,
            availableHeight: availableRowHeight
        )
        topRowFraction = Double(updatedHeight / availableRowHeight)
    }

    private var officePanel: some View {
        ZStack {
            if let outgoingArtStyle {
                officeArtwork(artStyle: outgoingArtStyle)
                    .id("style-\(outgoingArtStyle.rawValue)")

                officeArtwork(artStyle: artStyle)
                    .id("style-\(artStyle.rawValue)")
                    .mask {
                        OfficeArtStyleRevealMask(
                            progress: artStyleRevealProgress
                        )
                    }
            } else {
                officeArtwork(artStyle: artStyle)
                    .id("style-\(artStyle.rawValue)")
            }

            CharacterInteractionLayer(
                director: director,
                artStyle: artStyle,
                onMonitorTapped: {
                    historyTarget = .character($0)
                },
                onArchiveCabinetTapped: {
                    withAnimation(.easeInOut(duration: 0.18)) {
                        detailSelection = .archive
                    }
                },
                onWhiteboardTapped: {
                    withAnimation(.easeInOut(duration: 0.18)) {
                        detailSelection = .usage
                    }
                },
                onBubbleTapped: { character, message in
                    let offDutyReason = director.offDutyReason(
                        for: character
                    )
                    let failureMessage = director.failureMessage(
                        for: character
                    )
                    bubbleDetail = BubbleDetail(
                        character: character,
                        name: director.displayName(for: character),
                        message: offDutyReason ?? message,
                        isQuestion:
                            director.pendingQuestion(for: character) != nil,
                        isFailure: failureMessage != nil,
                        isOffDuty: offDutyReason != nil
                    )
                    director.dismissViewedBubble(for: character)
                }
            )
            .equatable()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipShape(
            RoundedRectangle(cornerRadius: 20, style: .continuous)
        )
        .overlay(
            alignment: OfficePanelControlLayout.alignment(for: .theme)
        ) {
            themeToggle
            .padding(12)
        }
        .overlay(
            alignment: OfficePanelControlLayout.alignment(for: .backend)
        ) {
            backendToggle
                .padding(12)
        }
        .overlay(
            alignment: OfficePanelControlLayout.alignment(for: .artStyle)
        ) {
            artStyleToggle
                .padding(12)
        }
        .overlay(
            alignment: OfficePanelControlLayout.alignment(for: .terminal)
        ) {
            conversationModeToggle
                .padding(12)
        }
        .officePanelStyle()
        .onAppear {
            backendController.activate(
                workdir: director.workspaceDirectory,
                healthURL: director.databaseBaseURL.appending(path: "health")
            )
        }
    }

    private var liveWorkspacePanel: some View {
        VStack(spacing: 0) {
            LiveWorkspaceHeader(
                director: director,
                conversationMode: conversationMode
            )
            .task { await director.refreshLocalProviderStatuses() }

            ForEach(director.localProviderStatuses) { status in
                Text(status.displayText)
                    .font(.caption)
                    .foregroundStyle(status.state == "error" ? Color.orange : Color.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 4)
                    .accessibilityIdentifier("localProviderStatus")
            }

            Divider()
                .opacity(0.55)

            Group {
                if conversationMode == .terminal {
                    CachedTerminalWorkspaces(director: director)
                } else {
                    CachedLiveWorkspaceFeeds(director: director)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            CharacterTurnCostFooter(
                store: director.turnCostStore,
                selection: director.characterSelectionStore
            )

            Divider()
                .opacity(0.55)

            LiveWorkspaceCommandBar(
                director: director,
                conversationMode: conversationMode,
                onShowProfile: { profileCharacter = $0 }
            )
        }
        .officePanelStyle()
    }

    private var theme: OfficeTheme {
        let storedTheme =
            OfficeTheme(rawValue: selectedThemeRawValue)
            ?? .modernDay
        return storedTheme.isNight ? .modernNight : .modernDay
    }

    private var artStyle: OfficeArtStyle {
        OfficeArtStyle(rawValue: selectedArtStyleRawValue)
            ?? .twoD
    }

    private var conversationMode: OfficeConversationMode {
        OfficeConversationMode(
            rawValue: selectedConversationModeRawValue
        ) ?? .defaultValue
    }

    @ViewBuilder
    private func officeArtwork(
        artStyle: OfficeArtStyle
    ) -> some View {
        ZStack {
            letterboxBackground(for: artStyle)
                .animation(.easeInOut(duration: 0.18), value: theme)

            OfficeRealtimeView(
                theme: theme,
                style: artStyle,
                isActive: scenePhase == .active,
                reduceMotion: reduceMotion,
                bossActivity: director.runningCharacters.contains(.boss)
                    ? .speaking
                    : .working
            )
            .accessibilityHidden(true)

            WhiteboardUsageLayer(
                isActive: scenePhase == .active,
                artStyle: artStyle,
                databaseBaseURL: director.databaseBaseURL
            )
        }
    }

    private func letterboxBackground(
        for artStyle: OfficeArtStyle
    ) -> LinearGradient {
        let colors: [Color]
        if artStyle == .twoD {
            colors = theme.isNight
                ? [
                    Color(
                        red: 87 / 255,
                        green: 106 / 255,
                        blue: 154 / 255
                    ),
                    Color(
                        red: 87 / 255,
                        green: 106 / 255,
                        blue: 153 / 255
                    ),
                    Color(
                        red: 86 / 255,
                        green: 105 / 255,
                        blue: 151 / 255
                    ),
                ]
                : [
                    Color(
                        red: 239 / 255,
                        green: 219 / 255,
                        blue: 205 / 255
                    ),
                    Color(
                        red: 240 / 255,
                        green: 220 / 255,
                        blue: 204 / 255
                    ),
                    Color(
                        red: 238 / 255,
                        green: 218 / 255,
                        blue: 202 / 255
                    ),
                ]
        } else {
            colors = theme.edgeBackdropColors.map {
                Color(nsColor: $0)
            }
        }

        return LinearGradient(
            colors: colors,
            startPoint: .top,
            endPoint: .bottom
        )
    }

    private var themeToggle: some View {
        Button {
            withAnimation(.easeInOut(duration: 0.18)) {
                selectedThemeRawValue = theme.isNight
                    ? OfficeTheme.modernDay.rawValue
                    : OfficeTheme.modernNight.rawValue
            }
        } label: {
            Image(
                systemName: theme.isNight
                    ? "moon.stars.fill"
                    : "sun.max.fill"
            )
            .font(.system(size: 15, weight: .semibold))
            .frame(width: 20, height: 20)
        }
        .buttonStyle(.plain)
        .environment(\.colorScheme, theme.isNight ? .dark : .light)
        .foregroundStyle(theme.isNight ? Color.white : Color.black.opacity(0.72))
        .padding(.horizontal, 8)
        .frame(height: 32)
        .background(
            theme.isNight
                ? Color.black.opacity(0.72)
                : Color.white.opacity(0.92),
            in: Capsule()
        )
        .overlay {
            Capsule()
                .stroke(
                    theme.isNight
                        ? Color.white.opacity(0.25)
                        : Color.black.opacity(0.08)
                )
        }
        .shadow(color: .black.opacity(0.14), radius: 7, y: 3)
        .fixedSize()
        .accessibilityLabel(OfficeLocalization.string("오피스 테마"))
        .accessibilityValue(OfficeLocalization.string(theme.title))
        .accessibilityIdentifier("officeThemeMenu")
        .help(
            theme.isNight
                ? OfficeLocalization.string("낮 테마로 전환")
                : OfficeLocalization.string("밤 테마로 전환")
        )
    }

    private var conversationModeToggle: some View {
        Button(action: toggleConversationMode) {
            Image(
                systemName: conversationMode == .terminal
                    ? "bubble.left.and.text.bubble.right"
                    : "terminal"
            )
            .font(.system(size: 15, weight: .semibold))
            .frame(width: 20, height: 20)
        }
        .buttonStyle(.plain)
        .environment(\.colorScheme, theme.isNight ? .dark : .light)
        .foregroundStyle(theme.isNight ? Color.white : Color.black.opacity(0.72))
        .padding(.horizontal, 8)
        .frame(height: 32)
        .background(
            theme.isNight
                ? Color.white.opacity(0.14)
                : Color.white.opacity(0.92),
            in: RoundedRectangle(cornerRadius: 11, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .stroke(
                    theme.isNight
                        ? Color.white.opacity(0.25)
                        : Color.black.opacity(0.08)
                )
        }
        .shadow(color: .black.opacity(0.14), radius: 7, y: 3)
        .opacity(
            ConversationModeSwitchPolicy.toggleOpacity(
                mode: conversationMode,
                hasRunningWork: !director.runningCharacters.isEmpty
            )
        )
        .fixedSize()
        .accessibilityLabel(OfficeLocalization.string("대화 표시 방식"))
        .accessibilityValue(
            conversationMode == .terminal
                ? OfficeLocalization.string("터미널")
                : OfficeLocalization.string("대화")
        )
        .accessibilityIdentifier("officeConversationModeToggle")
        .help(
            conversationMode == .terminal
                ? OfficeLocalization.string("대화 모드로 전환")
                : director.runningCharacters.isEmpty
                    ? OfficeLocalization.string("터미널 모드로 전환")
                    : OfficeLocalization.string("직원이 일하는 중이라 터미널 모드로 전환할 수 없습니다")
        )
    }

    private func toggleConversationMode() {
        guard conversationMode == .terminal else {
            let runningNames = OfficeCharacter.allCases
                .filter { director.runningCharacters.contains($0) }
                .map { director.displayName(for: $0) }
            if let message = ConversationModeSwitchPolicy
                .terminalEntryBlockMessage(runningCharacterNames: runningNames)
            {
                terminalModeAlert = .blockedByRunningWork(message)
                return
            }
            selectedConversationModeRawValue =
                OfficeConversationMode.terminal.rawValue
            return
        }
        Task {
            do {
                let sessions = try await director.fetchTerminalSessions()
                if sessions.contains(where: { $0.runningTurnId != nil }) {
                    terminalModeAlert = .confirmRunningTurn
                } else {
                    selectedConversationModeRawValue =
                        OfficeConversationMode.chat.rawValue
                }
            } catch {
                terminalModeAlert = .statusError(error.localizedDescription)
            }
        }
    }

    private var artStyleToggle: some View {
        Button {
            setArtStyle(
                artStyle == .twoD
                    ? .threeD
                    : .twoD
            )
        } label: {
            Text(artStyle.title)
                .font(.system(size: 12, weight: .bold, design: .rounded))
                .frame(
                    width: OfficePanelControlLayout.artStyleControlDiameter,
                    height: OfficePanelControlLayout.artStyleControlDiameter
                )
        }
        .buttonStyle(.plain)
        .disabled(outgoingArtStyle != nil)
        .environment(\.colorScheme, theme.isNight ? .dark : .light)
        .foregroundStyle(theme.isNight ? Color.white : Color.black.opacity(0.72))
        .background(
            theme.isNight
                ? Color.black.opacity(0.72)
                : Color.white.opacity(0.92),
            in: Circle()
        )
        .overlay {
            Circle()
                .stroke(
                    theme.isNight
                        ? Color.white.opacity(0.25)
                        : Color.black.opacity(0.08)
                )
        }
        .shadow(color: .black.opacity(0.14), radius: 7, y: 3)
        .accessibilityLabel(OfficeLocalization.string("오피스 표현 방식"))
        .accessibilityValue(artStyle.title)
        .accessibilityIdentifier("officeArtStyleToggle")
        .help(
            artStyle == .twoD
                ? OfficeLocalization.string("3D 오피스로 전환")
                : OfficeLocalization.string("2D 오피스로 전환")
        )
    }

    private var backendToggle: some View {
        Button {
            backendController.toggle()
        } label: {
            Image(systemName: backendController.status.systemImage)
                .font(.system(size: 15, weight: .bold))
                .frame(
                    width: OfficePanelControlLayout.artStyleControlDiameter,
                    height: OfficePanelControlLayout.artStyleControlDiameter
                )
        }
        .buttonStyle(.plain)
        .disabled(backendController.status == .changing)
        .environment(\.colorScheme, theme.isNight ? .dark : .light)
        .foregroundStyle(backendForegroundStyle)
        .background(
            backendBackgroundStyle,
            in: Circle()
        )
        .overlay {
            Circle()
                .stroke(backendStrokeStyle)
        }
        .shadow(color: .black.opacity(0.16), radius: 7, y: 3)
        .accessibilityLabel(OfficeLocalization.string("백엔드 서버"))
        .accessibilityValue(backendAccessibilityValue)
        .accessibilityIdentifier("officeBackendToggle")
        .help(backendHelpText)
    }

    private var backendForegroundStyle: Color {
        if backendController.status.showsStoppedWarning {
            return Color.white
        }
        return theme.isNight ? Color.white : Color.black.opacity(0.72)
    }

    private var backendBackgroundStyle: Color {
        if backendController.status.showsStoppedWarning {
            return Color(red: 0.87, green: 0.53, blue: 0.53)
        }
        return theme.isNight ? Color.black.opacity(0.72) : Color.white.opacity(0.92)
    }

    private var backendStrokeStyle: Color {
        if backendController.status.showsStoppedWarning {
            return Color.white.opacity(0.32)
        }
        return theme.isNight ? Color.white.opacity(0.25) : Color.black.opacity(0.08)
    }

    private var backendAccessibilityValue: String {
        switch backendController.status {
        case .running:
            return OfficeLocalization.string("실행 중")
        case .stopped:
            return OfficeLocalization.string("중지됨")
        default:
            return OfficeLocalization.string("상태 확인 중")
        }
    }

    private var backendHelpText: String {
        backendController.status == .running
            ? OfficeLocalization.string("백엔드 중지. 실행 중인 업무도 중단될 수 있습니다.")
            : OfficeLocalization.string("백엔드 시작")
    }

    private func setArtStyle(_ nextStyle: OfficeArtStyle) {
        guard outgoingArtStyle == nil, artStyle != nextStyle else {
            return
        }

        let previousStyle = artStyle

        guard !reduceMotion else {
            selectedArtStyleRawValue = nextStyle.rawValue
            return
        }

        outgoingArtStyle = previousStyle
        artStyleRevealProgress = 0
        selectedArtStyleRawValue = nextStyle.rawValue

        Task { @MainActor in
            await Task.yield()
            withAnimation(.easeInOut(duration: 1.55)) {
                artStyleRevealProgress = 1
            }
            try? await Task.sleep(for: .seconds(1.6))
            guard artStyle == nextStyle else {
                return
            }
            outgoingArtStyle = nil
            artStyleRevealProgress = 1
        }
    }

}

private struct LiveWorkspaceHeader: View {
    @ObservedObject private var director: AgentDirector
    @ObservedObject private var characterSelectionStore:
        CharacterSelectionStore
    let conversationMode: OfficeConversationMode

    init(
        director: AgentDirector,
        conversationMode: OfficeConversationMode
    ) {
        self.director = director
        self.conversationMode = conversationMode
        _characterSelectionStore = ObservedObject(
            wrappedValue: director.characterSelectionStore
        )
    }

    var body: some View {
        HStack(spacing: 12) {
            Image(
                systemName: conversationMode == .terminal
                    ? "terminal.fill"
                    : "bubble.left.and.text.bubble.right.fill"
            )
                .font(.system(size: 16, weight: .semibold))
                .foregroundStyle(DashboardPalette.accent)
                .frame(width: 36, height: 36)
                .background(
                    DashboardPalette.accent.opacity(0.10),
                    in: RoundedRectangle(
                        cornerRadius: 11,
                        style: .continuous
                    )
                )

            VStack(alignment: .leading, spacing: 2) {
                    Text(OfficeLocalization.string("실시간 대화"))
                        .font(.system(size: 17, weight: .bold))
                    Text(subtitle)
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(.secondary)
                }

                Spacer()

                HStack(spacing: 5) {
                    Circle()
                        .fill(
                            director.isRealtimeConnected
                                ? Color.green
                                : Color.orange
                        )
                        .frame(width: 7, height: 7)
                    Text(director.isRealtimeConnected ? "LIVE" : OfficeLocalization.string("연결 중"))
                }
                .font(.system(size: 10, weight: .bold, design: .rounded))
                .foregroundStyle(.secondary)
                .help(
                    director.realtimeConnectionError
                        ?? OfficeLocalization.string("백엔드 WebSocket 실시간 연결")
                )
            }
            .padding(.horizontal, 17)
            .padding(.vertical, 13)
        }

    private var selectedName: String {
        guard
            let characterID = characterSelectionStore.selectedCharacterID
        else {
            return OfficeLocalization.string("백부장")
        }
        return director.displayName(for: characterID)
    }

    private var subtitle: String {
        if conversationMode == .terminal {
            return String(
                format: OfficeLocalization.string("%@의 터미널"),
                selectedName
            )
        }
        return String(
            format: OfficeLocalization.string("%@의 대화와 진행 기록"),
            selectedName
        )
    }
}

private struct LiveWorkspaceCommandBar: View {
    @ObservedObject private var director: AgentDirector
    @ObservedObject private var characterSelectionStore:
        CharacterSelectionStore
    @State private var attachments: [PendingAttachment] = []
    @State private var attachmentSelectionError: String?
    @State private var isPreparingAttachments = false
    @State private var identitySettingsCharacter: OfficeCharacter?
    @State private var contextCompactionAlert: ContextCompactionAlert?
    @State private var terminalRestartRequiredCharacters:
        Set<OfficeCharacter> = []
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.colorScheme) private var colorScheme

    let onShowProfile: (OfficeCharacter) -> Void
    let conversationMode: OfficeConversationMode

    init(
        director: AgentDirector,
        conversationMode: OfficeConversationMode,
        onShowProfile: @escaping (OfficeCharacter) -> Void
    ) {
        self.director = director
        self.conversationMode = conversationMode
        self.onShowProfile = onShowProfile
        _characterSelectionStore = ObservedObject(
            wrappedValue: director.characterSelectionStore
        )
    }

    var body: some View {
        commandBar
            .disabled(characterSelectionStore.isConversationLoading)
            .opacity(
                characterSelectionStore.isConversationLoading ? 0.52 : 1
            )
            .task {
                await Task.detached(priority: .utility) {
                    guard let inbox = try? AttachmentInbox.live() else {
                        return
                    }
                    inbox.removeStaleItems()
                }.value
            }
            .sheet(item: $identitySettingsCharacter) { character in
                CharacterIdentitySettingsView(
                    director: director,
                    character: character
                )
            }
            .alert(
                item: $contextCompactionAlert,
                content: contextCompactionAlertView
            )
    }

    private func contextCompactionAlertView(
        _ alert: ContextCompactionAlert
    ) -> Alert {
        Alert(
            title: Text(OfficeLocalization.string(alert.title)),
            message: Text(OfficeLocalization.string(alert.message)),
            dismissButton: .default(Text(OfficeLocalization.string("확인")))
        )
    }

    private func compactContext(for character: OfficeCharacter) async {
        do {
            let result = try await director.compactContext(for: character)
            let detail: String
            if let before = result.preTokens, let after = result.postTokens {
                detail = OfficeLocalization.format(
                    "%@ → %@ 토큰",
                    compactTokenCount(before),
                    compactTokenCount(after)
                )
            } else {
                detail = OfficeLocalization.string("활성 세션을 요약 압축했습니다.")
            }
            contextCompactionAlert = .message(
                title: OfficeLocalization.string("컨텍스트 압축 완료"),
                message: detail
            )
        } catch {
            contextCompactionAlert = .message(
                title: OfficeLocalization.string("컨텍스트 압축 실패"),
                message: error.localizedDescription
            )
        }
    }

    private func isEnabled(
        _ control: LiveWorkspaceCommandAvailability.Control
    ) -> Bool {
        LiveWorkspaceCommandAvailability.isEnabled(
            control,
            in: conversationMode
        )
    }

    private func controlOpacity(
        _ control: LiveWorkspaceCommandAvailability.Control
    ) -> Double {
        LiveWorkspaceCommandAvailability.opacity(
            control,
            in: conversationMode
        )
    }

    private var selectedCharacterID: OfficeCharacter? {
        characterSelectionStore.selectedCharacterID
    }

    private var selectedCharacter: CharacterConfiguration? {
        guard let selectedCharacterID else {
            return nil
        }
        return director.characters.first { $0.id == selectedCharacterID }
    }

    private var commandBar: some View {
        VStack(alignment: .leading, spacing: 9) {
            characterSelector
                .disabled(!isEnabled(.characterSelector))
                .opacity(controlOpacity(.characterSelector))
                // 터미널은 응답 바닥글이 없으므로 방금 끝난 턴을 각 직원 버튼
                // 바로 위에 떠 있는 토스트로 평가한다. 자리를 차지하지 않아
                // 화면이 밀리지 않는다.
                .overlay(alignment: .top) {
                    if conversationMode == .terminal {
                        // 높이 0 프레임의 아래쪽에 붙여 토스트 전체가
                        // 직원 선택 줄 위쪽 바깥으로 올라가게 한다.
                        TerminalFeedbackToastRow(director: director)
                            .frame(height: 0, alignment: .bottom)
                    }
                }

            if conversationMode == .chat, !attachments.isEmpty {
                attachmentStrip
            }

            if conversationMode == .chat, let attachmentSelectionError {
                Label(
                    attachmentSelectionError,
                    systemImage: "exclamationmark.triangle.fill"
                )
                .font(.system(size: 10.5, weight: .semibold))
                .foregroundStyle(.red)
                .accessibilityLabel(
                    OfficeLocalization.format(
                        "첨부 오류: %@",
                        attachmentSelectionError
                    )
                )
            }

            if let selectedCharacterID {
                QueuedCommandStrip(
                    director: director,
                    character: selectedCharacterID
                )
            }

            // 터미널 모드에서도 같은 입력창을 쓴다. 터미널의 한글 입력이
            // 불편해서, 여기서 쓴 글을 CLI에 타이핑한 것처럼 넘긴다.
            CommandEntryRow(
                director: director,
                placeholder: commandPlaceholder,
                attachmentCount: attachments.count,
                isPreparingAttachments: isPreparingAttachments,
                supportsAttachments: isEnabled(.attachments),
                onChooseAttachments: chooseAttachments,
                onSubmit: submitCommand
            )

            if let character = selectedCharacter {
                HStack {
                    AgentQuickSettingsView(
                        director: director,
                        character: character,
                        onChanged: {
                            guard conversationMode == .terminal else {
                                return
                            }
                            terminalRestartRequiredCharacters.insert(
                                character.id
                            )
                        }
                    )
                    .disabled(!isEnabled(.quickSettings))
                    .opacity(controlOpacity(.quickSettings))

                    if conversationMode == .chat,
                        director.localProfileID(for: character.id) == nil,
                        ContextCompactionPresentation.supportsManualCompaction(
                        backend: character.backend
                    ) {
                        ContextCompactionControls(
                            director: director,
                            character: character,
                            onCompact: {
                                Task {
                                    await compactContext(for: character.id)
                                }
                            },
                            onAlert: { contextCompactionAlert = $0 }
                        )
                        .id(character.id)
                    }

                    if PixelOfficeAsset.fullBodyProfileURL(for: character.id) != nil
                        || PixelOfficeAsset.fullBodyProfileVideoURL(
                            for: character.id
                        ) != nil {
                        Button {
                            onShowProfile(character.id)
                        } label: {
                            Label(
                                OfficeLocalization.string("프로필"),
                                systemImage: "person.crop.rectangle"
                            )
                            .font(.system(size: 11, weight: .semibold))
                        }
                        .buttonStyle(.plain)
                        .foregroundStyle(
                            DashboardPalette.providerAccent(
                                for: character.backend
                            )
                        )
                        .accessibilityLabel(OfficeLocalization.string("직원 프로필"))
                        .help(OfficeLocalization.string("직원 프로필 보기"))
                    }

                    Button {
                        identitySettingsCharacter = character.id
                    } label: {
                        Label(
                            OfficeLocalization.string("설정"),
                            systemImage: "gearshape"
                        )
                            .font(.system(size: 11, weight: .semibold))
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(
                        DashboardPalette.providerAccent(
                            for: character.backend
                        )
                    )
                    .accessibilityLabel(OfficeLocalization.string("직원 설정"))
                    .help(OfficeLocalization.string("이름과 업무 지침 설정"))

                    Spacer(minLength: 0)
                }
                .frame(
                    height: ConversationFooterLayout.controlRowHeight(
                        for: character.backend
                    )
                )

                if
                    conversationMode == .terminal,
                    terminalRestartRequiredCharacters.contains(character.id)
                {
                    terminalRestartNotice(for: character.id)
                }
            }

        }
        .padding(14)
    }

    private func terminalRestartNotice(
        for character: OfficeCharacter
    ) -> some View {
        HStack(spacing: 8) {
            Label(
                OfficeLocalization.string("변경한 설정은 터미널을 다시 시작해야 적용됩니다."),
                systemImage: "arrow.clockwise.circle"
            )
            .font(.system(size: 10.5, weight: .medium))
            .foregroundStyle(.secondary)

            Spacer(minLength: 0)

            Button(OfficeLocalization.string("다시 시작")) {
                terminalRestartRequiredCharacters.remove(character)
                director.requestTerminalRestart(for: character)
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
        }
        .accessibilityIdentifier("terminalRestartRequiredNotice")
    }

    private var attachmentStrip: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 7) {
                ForEach(attachments) { attachment in
                    HStack(spacing: 5) {
                        Image(systemName: "doc.fill")
                            .font(.system(size: 10))
                            .foregroundStyle(DashboardPalette.accent)
                        Text(attachment.displayName)
                            .font(.system(size: 11, weight: .medium))
                            .lineLimit(1)
                        Button {
                            removeAttachment(attachment)
                        } label: {
                            Image(systemName: "xmark.circle.fill")
                                .font(.system(size: 11))
                                .foregroundStyle(.secondary)
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel(
                            OfficeLocalization.format(
                                "%@ 첨부 제거",
                                attachment.displayName
                            )
                        )
                    }
                    .padding(.horizontal, 9)
                    .frame(height: 27)
                    .background(
                        Color.primary.opacity(0.055),
                        in: Capsule()
                    )
                }
            }
        }
    }

    private var characterSelector: some View {
        ZStack {
            CoreAnimationSelectionHighlight(
                selectedIndex: director.characters.firstIndex {
                    $0.id == selectedCharacterID
                },
                itemCount: director.characters.count,
                spacing: 3,
                reduceMotion: reduceMotion
            )
            .frame(maxWidth: .infinity)
            .frame(height: 38)
            .allowsHitTesting(false)
            .accessibilityHidden(true)

            HStack(spacing: 3) {
                ForEach(director.characters) { character in
                let isSelected =
                    selectedCharacterID == character.id
                let isRunning =
                    director.runningCharacters.contains(character.id)
                let isCompacting =
                    director.compactingCharacters.contains(character.id)
                let compactionNotice =
                    director.contextCompactionNotice(for: character.id)
                let name = director.displayName(for: character.id)
                let badgeColor = DashboardPalette.characterAccent(
                    for: character.id.rawValue
                )
                let isCompleted =
                    director.unreviewedCompletedCharacters.contains(
                        character.id
                    )

                Button {
                    director.select(character)
                } label: {
                    HStack(spacing: 7) {
                        CharacterAvatar(
                            name: name,
                            characterID: character.id.rawValue,
                            size: 24
                        )
                        .overlay {
                            Circle()
                                .stroke(
                                    isSelected
                                        ? badgeColor
                                        : .clear,
                                    lineWidth: 2
                                )
                        }

                        Text(name)
                            .font(
                                .system(
                                    size: 13,
                                    weight: isSelected
                                        ? .bold
                                        : .semibold
                                )
                            )
                            .foregroundStyle(
                                CharacterSelectorLabelStyle.color(
                                    isSelected: isSelected,
                                    colorScheme: colorScheme
                                )
                            )
                            .lineLimit(1)
                            .truncationMode(.tail)

                        if
                            isRunning || isCompacting || isCompleted
                                || compactionNotice != nil
                        {
                            CharacterTaskStatusIndicator(
                                isRunning: isRunning,
                                isCompacting: isCompacting,
                                isCompleted: isCompleted,
                                compactionNotice: compactionNotice,
                                reduceMotion: reduceMotion
                            )
                        }
                    }
                    .padding(.horizontal, 7)
                    .frame(maxWidth: .infinity)
                    .frame(height: 38)
                    .contentShape(
                        RoundedRectangle(
                            cornerRadius: 10,
                            style: .continuous
                        )
                    )
                    }
                    .buttonStyle(.plain)
                    .help(OfficeLocalization.format("%@ 선택", name))
                    .accessibilityLabel(
                        OfficeLocalization.format("%@ 선택", name)
                    )
                    .accessibilityValue(
                        isSelected
                            ? OfficeLocalization.string("선택됨")
                            : OfficeLocalization.string("선택되지 않음")
                    )
                    .accessibilityIdentifier(
                        "commandCharacter-\(character.id.rawValue)"
                    )
                }
            }
        }
        .padding(4)
        .background {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .fill(
                    LinearGradient(
                        colors: [
                            Color.primary.opacity(0.055),
                            Color.primary.opacity(0.025),
                        ],
                        startPoint: .top,
                        endPoint: .bottom
                    )
                )
                .overlay {
                    RoundedRectangle(
                        cornerRadius: 14,
                        style: .continuous
                    )
                    .stroke(Color.primary.opacity(0.055))
                }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(OfficeLocalization.string("직원 선택"))
    }

    private var commandPlaceholder: String {
        if let restoreError = director.sessionRestoreError {
            return OfficeLocalization.format(
                "세션 복구 실패 · %@",
                restoreError
            )
        }
        if !director.isReadyForSubmissions {
            return OfficeLocalization.string("저장된 세션을 복구하는 중입니다")
        }
        if
            let selectedCharacterID,
            let persistenceError =
                director.turnPersistenceErrors[selectedCharacterID]
        {
            return OfficeLocalization.format(
                "대화 기록 저장 실패 · %@",
                persistenceError
            )
        }
        if let selectedCharacterID {
            let selectedName = director.displayName(for: selectedCharacterID)
            if
                director.pendingQuestion(for: selectedCharacterID) != nil
            {
                return OfficeLocalization.format(
                    "%@의 질문에 답변하세요",
                    selectedName
                )
            }
            if
                director.offDutyReason(for: selectedCharacterID) != nil
            {
                return OfficeLocalization.format(
                    "%@은 모델 한도 소진으로 퇴근했습니다",
                    selectedName
                )
            }
            if
                director.failureMessage(for: selectedCharacterID) != nil
            {
                return OfficeLocalization.format(
                    "%@에게 새 업무를 보내 다시 시작하세요",
                    selectedName
                )
            }
            if director.compactingCharacters.contains(selectedCharacterID) {
                return OfficeLocalization.format(
                    "%@의 컨텍스트 압축이 끝나면 업무를 보낼 수 있습니다",
                    selectedName
                )
            }
            if director.runningCharacters.contains(selectedCharacterID) {
                switch director.selectedCharacterQueueAvailability {
                case .available:
                    return OfficeLocalization.format(
                        "%@의 다음 턴에 예약할 업무를 입력하세요",
                        selectedName
                    )
                case .full:
                    return OfficeLocalization.format(
                        "%@의 예약이 가득 찼습니다",
                        selectedName
                    )
                case .unavailable:
                    return OfficeLocalization.format(
                        "%@의 다음 업무 예약을 준비하는 중입니다",
                        selectedName
                    )
                }
            }
            return OfficeLocalization.format(
                "%@에게 업무를 입력하세요",
                selectedName
            )
        }
        return OfficeLocalization.string("캐릭터를 선택하세요")
    }

}

enum CharacterSelectorLabelStyle {
    static func usesDarkSelectedText(
        isSelected: Bool,
        colorScheme: ColorScheme
    ) -> Bool {
        isSelected && colorScheme == .dark
    }

    static func color(
        isSelected: Bool,
        colorScheme: ColorScheme
    ) -> Color {
        if usesDarkSelectedText(
            isSelected: isSelected,
            colorScheme: colorScheme
        ) {
            return Color.black
        }
        return Color.primary.opacity(isSelected ? 0.88 : 0.56)
    }
}

private extension LiveWorkspaceCommandBar {
    private func submitCommand(_ prompt: String) -> Bool {
        guard
            director.isReadyForSubmissions,
            !director.isUpdatingConfiguration,
            let selectedCharacterID,
            !director.compactingCharacters.contains(selectedCharacterID)
        else {
            return false
        }

        // 응답 생성 중이면 같은 입력을 다음 턴 예약으로 넘긴다.
        // 첨부는 예약이 실제로 제출될 때까지 director가 들고 있다가
        // 정리하므로 여기서 목록만 비운다. 터미널 모드도 같은 예약을 쓰고,
        // 응답이 끝나면 director가 CLI에 타이핑한 것처럼 흘려보낸다.
        if director.runningCharacters.contains(selectedCharacterID) {
            let queued = director.enqueueCommand(
                prompt,
                attachments: attachments,
                for: selectedCharacterID
            )
            if queued {
                attachments = []
            }
            return queued
        }

        if conversationMode == .terminal {
            return director.submitToTerminal(prompt, for: selectedCharacterID)
        }

        let submittedAttachments = attachments
        let attachmentPaths = submittedAttachments.map(\.stagedURL.path)
        attachments = []
        director.submit(
            prompt,
            attachmentPaths: attachmentPaths,
            onRequestFinished: {
                guard let inbox = try? AttachmentInbox.live() else {
                    return
                }
                for attachment in submittedAttachments {
                    inbox.remove(attachment)
                }
            }
        )
        return true
    }

    private func chooseAttachments() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = true
        panel.allowsOtherFileTypes = true
        panel.prompt = OfficeLocalization.string("첨부")
        panel.message = OfficeLocalization.string(
            "Antigravity, Claude Code 또는 Codex가 확인할 파일을 선택하세요."
        )

        guard panel.runModal() == .OK else {
            return
        }
        attachmentSelectionError = nil
        let existingPaths = Set(attachments.map(\.id))
        let newAttachments = panel.urls.filter {
            !existingPaths.contains($0.standardizedFileURL.path)
        }
        let selectedURLs = Array(
            newAttachments.prefix(20 - attachments.count)
        )
        guard !selectedURLs.isEmpty else {
            return
        }
        // 예약에 실려 아직 제출되지 않은 첨부도 살아 있어야 한다.
        let activeAttachments =
            attachments + director.queuedAttachments
        isPreparingAttachments = true
        Task {
            let batch = await Task.detached(priority: .userInitiated) {
                do {
                    let inbox = try AttachmentInbox.live()
                    inbox.removeStaleItems(
                        excluding: activeAttachments
                    )
                    return inbox.stage(selectedURLs)
                } catch {
                    return AttachmentStagingBatch(
                        attachments: [],
                        errorDescriptions: [error.localizedDescription]
                    )
                }
            }.value
            attachments.append(contentsOf: batch.attachments)
            attachmentSelectionError = batch.errorDescriptions.first
            isPreparingAttachments = false
        }
    }

    private func removeAttachment(_ attachment: PendingAttachment) {
        attachments.removeAll {
            $0.id == attachment.id
        }
        try? AttachmentInbox.live().remove(attachment)
    }
    }

private struct CharacterTaskStatusIndicator: View {
    let isRunning: Bool
    let isCompacting: Bool
    let isCompleted: Bool
    let compactionNotice: ContextCompactionNotice?
    let reduceMotion: Bool

    private let completedColor = Color(
        red: 0.94,
        green: 0.52,
        blue: 0.16
    )
    private let compactionColor = Color(
        red: 0.48,
        green: 0.38,
        blue: 0.92
    )

    var body: some View {
        Group {
            if isCompacting {
                ProgressView()
                    .controlSize(.mini)
                    .tint(compactionColor)
                    .frame(width: 19, height: 19)
                    .accessibilityLabel(OfficeLocalization.string("컨텍스트 압축 중"))
                    .help(OfficeLocalization.string("컨텍스트 압축 중"))
            } else if let compactionNotice {
                switch compactionNotice {
                case .completed:
                    Image(systemName: "checkmark")
                        .font(.system(size: 10, weight: .black))
                        .foregroundStyle(.white)
                        .frame(width: 19, height: 19)
                        .background(Color.green, in: Circle())
                        .accessibilityLabel(OfficeLocalization.string("컨텍스트 압축 완료"))
                        .help(compactionNoticeHelp(compactionNotice))
                case .failed:
                    Image(systemName: "exclamationmark")
                        .font(.system(size: 10, weight: .black))
                        .foregroundStyle(.white)
                        .frame(width: 19, height: 19)
                        .background(Color.red, in: Circle())
                        .accessibilityLabel(OfficeLocalization.string("컨텍스트 압축 실패"))
                        .help(compactionNoticeHelp(compactionNotice))
                }
            } else if isRunning {
                CoreAnimationDotsView(
                    dotSize: 4,
                    spacing: 2.5,
                    travel: 2.5,
                    color: NSColor(
                        calibratedRed: 0.13,
                        green: 0.55,
                        blue: 0.52,
                        alpha: 1
                    ),
                    isAnimated: !reduceMotion
                )
                .frame(width: 20, height: 14)
                .accessibilityLabel(OfficeLocalization.string("업무 중"))
            } else if isCompleted {
                Image(systemName: "exclamationmark")
                    .font(.system(size: 11, weight: .black))
                    .foregroundStyle(.white)
                    .frame(width: 19, height: 19)
                    .background(completedColor, in: Circle())
                    .shadow(
                        color: completedColor.opacity(0.28),
                        radius: 4,
                        y: 2
                    )
                    .accessibilityLabel(OfficeLocalization.string("업무 완료"))
            }
        }
        .frame(width: 24, height: 24)
    }

    private func compactionNoticeHelp(
        _ notice: ContextCompactionNotice
    ) -> String {
        switch notice {
        case let .completed(automatic, preTokens, postTokens):
            let title = automatic
                ? OfficeLocalization.string("자동 컨텍스트 압축 완료")
                : OfficeLocalization.string("컨텍스트 압축 완료")
            guard let preTokens, let postTokens else {
                return title
            }
            return OfficeLocalization.format(
                "%@ · %@ → %@ 토큰",
                title,
                compactTokenCount(preTokens),
                compactTokenCount(postTokens)
            )
        case let .failed(automatic, message):
            let title = automatic
                ? OfficeLocalization.string("자동 컨텍스트 압축 실패")
                : OfficeLocalization.string("컨텍스트 압축 실패")
            guard let message, !message.isEmpty else {
                return title
            }
            return "\(title) · \(message)"
        }
    }
}

private struct OfficeColumnResizeHandle: View {
    let onDragChanged: (CGFloat) -> Void
    let onDragEnded: () -> Void

    var body: some View {
        ZStack {
            Capsule()
                .fill(Color.primary.opacity(0.16))
                .frame(width: 2, height: 46)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .contentShape(Rectangle())
        .officeColumnResizeCursor()
        .gesture(
            DragGesture(minimumDistance: 0)
                .onChanged { value in
                    onDragChanged(value.translation.width)
                }
                .onEnded { _ in
                    onDragEnded()
                }
        )
        .accessibilityLabel(OfficeLocalization.string("좌우 화면 폭 조절"))
        .accessibilityHint(
            OfficeLocalization.string("드래그해서 사무실과 실시간 대화 영역의 폭을 조절합니다")
        )
    }
}

private struct OfficeRowResizeHandle: View {
    let onDragChanged: (CGFloat) -> Void
    let onDragEnded: () -> Void

    var body: some View {
        ZStack {
            Capsule()
                .fill(Color.primary.opacity(0.16))
                .frame(width: 46, height: 2)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .contentShape(Rectangle())
        .officeRowResizeCursor()
        .gesture(
            DragGesture(minimumDistance: 0)
                .onChanged { value in
                    onDragChanged(value.translation.height)
                }
                .onEnded { _ in
                    onDragEnded()
                }
        )
        .accessibilityLabel(OfficeLocalization.string("상하 화면 높이 조절"))
        .accessibilityHint(
            OfficeLocalization.string("드래그해서 사무실과 상세 영역의 높이를 조절합니다")
        )
    }
}

private struct OfficeArtStyleRevealMask: View, Animatable {
    var progress: CGFloat

    var animatableData: CGFloat {
        get { progress }
        set { progress = newValue }
    }

    var body: some View {
        GeometryReader { geometry in
            let width = geometry.size.width
            let feather = min(72, width * 0.13)
            let clampedProgress = min(max(progress, 0), 1)
            let sweep = clampedProgress * (width + feather)
            let solidWidth = min(width, max(0, sweep - feather))
            let softWidth = min(feather, max(0, sweep - solidWidth))

            HStack(spacing: 0) {
                Color.white
                    .frame(width: solidWidth)

                if softWidth > 0 {
                    LinearGradient(
                        colors: [.white, .clear],
                        startPoint: .topLeading,
                        endPoint: .bottomTrailing
                    )
                    .frame(width: softWidth)
                }

                Spacer(minLength: 0)
            }
            .frame(
                maxWidth: .infinity,
                maxHeight: .infinity,
                alignment: .leading
            )
        }
    }
}

private struct BubbleDetail: Identifiable {
    let id = UUID()
    let character: OfficeCharacter
    let name: String
    let message: String
    let isQuestion: Bool
    let isFailure: Bool
    let isOffDuty: Bool
}

private struct BubbleDetailView: View {
    @ObservedObject var director: AgentDirector
    let character: OfficeCharacter
    let name: String
    let message: String
    let isQuestion: Bool
    let isFailure: Bool
    let isOffDuty: Bool
    @State private var answer = ""
    @FocusState private var answerIsFocused: Bool
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        let presentation = AgentQuestionPresentation(text: message)

        VStack(spacing: 0) {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    Text(detailTitle)
                        .font(.system(size: 19, weight: .bold))
                    Text(
                        isQuestion
                            ? OfficeLocalization.string("질문 원문을 확인하고 아래에 답변하세요")
                            : isOffDuty
                            ? OfficeLocalization.string("모델 한도 소진 원문과 재설정 안내")
                            : isFailure
                            ? OfficeLocalization.string("CLI 작업이 중단된 원인 전문")
                            : OfficeLocalization.string("말풍선에 표시된 응답 전문")
                    )
                        .font(.system(size: 12))
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button(OfficeLocalization.string("닫기")) {
                    dismiss()
                }
                .keyboardShortcut(.cancelAction)
            }
            .padding(18)

            Divider()

            ScrollView {
                ConversationMarkdownView(
                    source: isQuestion ? presentation.question : message,
                    fontSize: 13
                )
                    .conversationTextSelectionRegion(
                        isQuestion
                            ? "question-dialog"
                            : "message-dialog"
                    )
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(18)
            }

            if isQuestion {
                Divider()

                VStack(alignment: .leading, spacing: 10) {
                    Text(
                        presentation.choices.isEmpty
                            ? OfficeLocalization.string("답변")
                            : OfficeLocalization.string("선택지")
                    )
                        .font(.system(size: 13, weight: .bold))

                    if
                        let error = director.questionSubmissionError(
                            for: character
                        )
                    {
                        Label(
                            OfficeLocalization.format("전송하지 못했습니다. %@", error),
                            systemImage: "exclamationmark.triangle.fill"
                        )
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Color.red)
                    }

                    if !presentation.choices.isEmpty {
                        VStack(spacing: 8) {
                            ForEach(
                                Array(presentation.choices.enumerated()),
                                id: \.offset
                            ) { index, choice in
                                Button {
                                    submitAnswer(choice.response)
                                } label: {
                                    HStack(spacing: 10) {
                                        Text("\(index + 1)")
                                            .font(
                                                .system(
                                                    size: 11,
                                                    weight: .bold,
                                                    design: .rounded
                                                )
                                            )
                                            .foregroundStyle(Color.accentColor)
                                            .frame(width: 24, height: 24)
                                            .background(
                                                Circle()
                                                    .fill(
                                                        Color.accentColor
                                                            .opacity(0.12)
                                                    )
                                            )
                                        Text(choice.title)
                                            .font(.system(size: 13))
                                            .multilineTextAlignment(.leading)
                                        Spacer(minLength: 8)
                                        Image(systemName: "arrow.right")
                                            .font(
                                                .system(
                                                    size: 11,
                                                    weight: .semibold
                                                )
                                            )
                                            .foregroundStyle(.secondary)
                                    }
                                    .padding(.horizontal, 12)
                                    .padding(.vertical, 9)
                                    .frame(
                                        maxWidth: .infinity,
                                        alignment: .leading
                                    )
                                    .contentShape(Rectangle())
                                }
                                .buttonStyle(.plain)
                                .background(
                                    RoundedRectangle(
                                        cornerRadius: 9,
                                        style: .continuous
                                    )
                                    .fill(Color.accentColor.opacity(0.055))
                                )
                                .overlay {
                                    RoundedRectangle(
                                        cornerRadius: 9,
                                        style: .continuous
                                    )
                                    .stroke(Color.accentColor.opacity(0.22))
                                }
                                .disabled(
                                    director.runningCharacters.contains(
                                        character
                                    )
                                )
                                .accessibilityIdentifier(
                                    "needsInputChoice.\(index + 1)"
                                )
                                .accessibilityLabel(
                                    OfficeLocalization.format("%d번 %@", index + 1, choice.title)
                                )
                            }
                        }

                        Divider()

                        Text(OfficeLocalization.string("직접 입력"))
                            .font(.system(size: 13, weight: .bold))
                    }

                    TextEditor(text: $answer)
                        .font(.system(size: 13))
                        .scrollContentBackground(.hidden)
                        .padding(8)
                        .frame(minHeight: 92, maxHeight: 150)
                        .background(
                            RoundedRectangle(
                                cornerRadius: 9,
                                style: .continuous
                            )
                            .fill(Color(nsColor: .textBackgroundColor))
                        )
                        .overlay {
                            RoundedRectangle(
                                cornerRadius: 9,
                                style: .continuous
                            )
                            .stroke(Color.primary.opacity(0.14))
                        }
                        .focused($answerIsFocused)

                    HStack {
                        Text(OfficeLocalization.string("답변은 같은 CLI 세션으로 이어집니다."))
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                        Spacer()
                        Button(OfficeLocalization.string("답변 보내기"), action: submitTypedAnswer)
                            .buttonStyle(.borderedProminent)
                            .keyboardShortcut(.defaultAction)
                            .disabled(
                                answer.trimmingCharacters(
                                    in: .whitespacesAndNewlines
                                ).isEmpty
                                    || director.runningCharacters.contains(
                                        character
                                    )
                            )
                    }
                }
                .padding(18)
            }
        }
        .frame(
            minWidth: 820,
            idealWidth: 1_000,
            minHeight: isQuestion ? 540 : 420
        )
        .onAppear {
            answerIsFocused = isQuestion
                && presentation.choices.isEmpty
        }
    }

    private func submitTypedAnswer() {
        submitAnswer(answer)
    }

    private func submitAnswer(_ response: String) {
        let value = response.trimmingCharacters(
            in: .whitespacesAndNewlines
        )
        guard !value.isEmpty else {
            return
        }
        director.submit(value, to: character)
        dismiss()
    }

    private var detailTitle: String {
        if isQuestion {
            return OfficeLocalization.format("%@ 확인 질문", name)
        }
        if isOffDuty {
            return OfficeLocalization.format("%@ 퇴근", name)
        }
        if isFailure {
            return OfficeLocalization.format("%@ 업무 중단", name)
        }
        return OfficeLocalization.format("%@ 응답", name)
    }
}

struct AgentQuickSettingsAvailability: Equatable {
    let isReady: Bool
    let isUpdatingConfiguration: Bool
    let isRunning: Bool
    let isCompacting: Bool

    init(
        isReady: Bool,
        isUpdatingConfiguration: Bool,
        isRunning: Bool,
        isCompacting: Bool = false
    ) {
        self.isReady = isReady
        self.isUpdatingConfiguration = isUpdatingConfiguration
        self.isRunning = isRunning
        self.isCompacting = isCompacting
    }

    var canChangeCurrentBackendSettings: Bool {
        isReady && !isUpdatingConfiguration && !isRunning && !isCompacting
    }

    var canChangeBackend: Bool {
        canChangeCurrentBackendSettings
    }
}

struct ContextCompactionAvailability: Equatable {
    let isReady: Bool
    let isUpdatingConfiguration: Bool
    let isRunning: Bool
    let hasActiveSession: Bool
    let isCompacting: Bool

    var canAdjustThreshold: Bool {
        isReady && !isUpdatingConfiguration && !isRunning && !isCompacting
    }

    var canCompactNow: Bool {
        canAdjustThreshold && hasActiveSession
    }

    var canRequestCompaction: Bool {
        canAdjustThreshold
    }
}

enum ConversationFooterLayout {
    static func controlRowHeight(for _: AgentBackend) -> CGFloat {
        return 28
    }
}

private struct ContextCompactionControls: View {
    @ObservedObject var director: AgentDirector
    let character: CharacterConfiguration
    let onCompact: () -> Void
    let onAlert: (ContextCompactionAlert) -> Void
    @State private var draftPercent: Double

    init(
        director: AgentDirector,
        character: CharacterConfiguration,
        onCompact: @escaping () -> Void,
        onAlert: @escaping (ContextCompactionAlert) -> Void
    ) {
        self.director = director
        self.character = character
        self.onCompact = onCompact
        self.onAlert = onAlert
        _draftPercent = State(
            initialValue: Double(
                director.autoCompactPercent(for: character.id)
            )
        )
    }

    private var availability: ContextCompactionAvailability {
        ContextCompactionAvailability(
            isReady: director.isReadyForSubmissions,
            isUpdatingConfiguration: director.isUpdatingConfiguration,
            isRunning: director.runningCharacters.contains(character.id),
            hasActiveSession: director.hasActiveSession(for: character.id),
            isCompacting: director.compactingCharacters.contains(character.id)
        )
    }

    private var roundedPercent: Int {
        Int(draftPercent.rounded())
    }

    private var contextLimit: Int? {
        director.sessionContextLimit(for: character.id)
    }

    private var compactionNotice: ContextCompactionNotice? {
        director.contextCompactionNotice(for: character.id)
    }

    private var accent: Color {
        DashboardPalette.providerAccent(for: character.backend)
    }

    private var thresholdText: String {
        guard let contextLimit else {
            return "\(roundedPercent)%"
        }
        let tokens = contextLimit * roundedPercent / 100
        return "\(roundedPercent)% · \(compactTokenCount(tokens))"
    }

    private var thresholdHelp: String {
        guard let contextLimit else {
            return OfficeLocalization.string("세션의 실제 최대 컨텍스트 대비 자동 압축 기준")
        }
        return OfficeLocalization.format(
            "실제 최대 %@ 토큰 중 %@에서 자동 압축",
            compactTokenCount(contextLimit),
            thresholdText
        )
    }

    var body: some View {
        HStack(spacing: 6) {
            if ContextCompactionPresentation.usesAdjustableThreshold(
                backend: character.backend
            ) {
                Label(OfficeLocalization.string("자동"), systemImage: "gauge.with.dots.needle.33percent")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(accent)

                Slider(
                    value: $draftPercent,
                    in: 20 ... 95,
                    step: 5,
                    onEditingChanged: thresholdEditingChanged
                )
                .frame(width: 92)
                .tint(accent)
                .disabled(!availability.canAdjustThreshold)
                .accessibilityLabel(OfficeLocalization.string("자동 컨텍스트 압축 기준"))
                .accessibilityValue(OfficeLocalization.format("%d퍼센트", roundedPercent))
                .help(thresholdHelp)

                Text(thresholdText)
                    .font(
                        .system(
                            size: 10.5,
                            weight: .semibold,
                            design: .monospaced
                        )
                    )
                    .foregroundStyle(accent)
                    .frame(minWidth: 28, alignment: .trailing)
            }

            Button {
                onCompact()
            } label: {
                if availability.isCompacting {
                    HStack(spacing: 4) {
                        ProgressView()
                            .controlSize(.small)
                        Text(OfficeLocalization.string("압축 중"))
                    }
                    .font(.system(size: 11, weight: .semibold))
                } else {
                    Label(
                        OfficeLocalization.string("지금 압축"),
                        systemImage: "arrow.down.right.and.arrow.up.left"
                    )
                    .font(.system(size: 11, weight: .semibold))
                }
            }
            .buttonStyle(.plain)
            .foregroundStyle(accent)
            .disabled(!availability.canRequestCompaction)
            .accessibilityLabel(OfficeLocalization.string("컨텍스트 지금 압축"))
            .help(
                availability.hasActiveSession
                    ? OfficeLocalization.string("현재 CLI 세션의 컨텍스트를 즉시 요약 압축")
                    : OfficeLocalization.string("첫 대화 뒤 활성 세션이 생기면 압축할 수 있습니다")
            )

            if !availability.isCompacting, let compactionNotice {
                switch compactionNotice {
                case .completed:
                    Label(OfficeLocalization.string("압축 완료"), systemImage: "checkmark.circle.fill")
                        .foregroundStyle(.green)
                case .failed:
                    Label(OfficeLocalization.string("압축 실패"), systemImage: "exclamationmark.circle.fill")
                        .foregroundStyle(.red)
                }
            }
        }
        .font(.system(size: 10.5, weight: .semibold))
        .onChange(
            of: director.autoCompactPercent(for: character.id)
        ) { _, value in
            draftPercent = Double(value)
        }
    }

    private func thresholdEditingChanged(_ editing: Bool) {
        guard !editing else {
            return
        }
        let requested = roundedPercent
        Task {
            do {
                try await director.updateAutoCompactPercent(
                    requested,
                    for: character.id
                )
            } catch {
                draftPercent = Double(
                    director.autoCompactPercent(for: character.id)
                )
                onAlert(.message(
                    title: OfficeLocalization.string("자동 압축 기준 저장 실패"),
                    message: error.localizedDescription
                ))
            }
        }
    }
}

enum ContextCompactionPresentation {
    static func supportsManualCompaction(backend: AgentBackend) -> Bool {
        backend != .antigravity
    }

    static func usesAdjustableThreshold(backend: AgentBackend) -> Bool {
        backend == .claude
    }

    static func confirmationMessage(
        displayName: String,
        backendTitle: String
    ) -> String {
        OfficeLocalization.format(
            "%@의 %@ 대화 내용을 요약으로 바꿉니다. 이전 세부 내용은 되돌릴 수 없습니다.",
            displayName,
            backendTitle
        )
    }
}

private enum ContextCompactionAlert: Identifiable {
    case message(title: String, message: String)

    var id: String {
        "message:\(title):\(message)"
    }

    var title: String {
        switch self {
        case let .message(title, _):
            title
        }
    }

    var message: String {
        switch self {
        case let .message(_, message):
            message
        }
    }
}

private func compactTokenCount(_ value: Int) -> String {
    if value >= 1_000_000 {
        return String(format: "%.2fM", Double(value) / 1_000_000)
    }
    if value >= 1_000 {
        return String(format: "%.1fK", Double(value) / 1_000)
    }
    return String(value)
}

private struct AgentQuickSettingsView: View {
    @ObservedObject var director: AgentDirector
    let character: CharacterConfiguration
    var onChanged: (() -> Void)? = nil
    @State private var isShowingModelVisibilitySettings = false
    @State private var isConfirmingLocalSwitch = false
    @State private var pendingLocalProfileID: String?
    @State private var pendingLocalCharacter: OfficeCharacter?
    @State private var isShowingLocalResult = false

    private var isLocal: Bool { director.localProfileID(for: character.id) != nil }

    private var localModelTitle: String {
        director.localModelOptions.first { $0.id == director.localProfileID(for: character.id) }?.displayTitle ?? "로컬 AI"
    }

    private var settings: CharacterAgentSettings {
        director.agentSettings(for: character.id)
    }

    private var availability: AgentQuickSettingsAvailability {
        AgentQuickSettingsAvailability(
            isReady: director.isReadyForSubmissions,
            isUpdatingConfiguration: director.isUpdatingConfiguration,
            isRunning: director.runningCharacters.contains(character.id),
            isCompacting:
                director.compactingCharacters.contains(character.id)
        )
    }

    private var selectableModels: [AgentModelOption] {
        var models = director.modelOptions(for: settings.backend)
        if let selected = settings.model,
           !models.contains(where: { $0.id == selected })
        {
            models.append(
                director.modelOption(
                    for: settings.backend,
                    model: selected
                ) ?? AgentModelOption(
                    id: selected,
                    title: settings.backend.modelTitle(selected),
                    efforts: settings.backend.effortOptions(for: selected),
                    defaultEffort: settings.effort,
                    supportsFastMode: settings.backend.supportsFastMode(
                        model: selected
                    )
                )
            )
        }
        return models
    }

    private var selectedModelOption: AgentModelOption? {
        director.modelOption(
            for: settings.backend,
            model: settings.model
        )
    }

    var body: some View {
        HStack(spacing: 4) {
            Menu {
                if isLocal {
                    Button(OfficeLocalization.string("이전 클라우드 설정으로 돌아가기")) {
                        requestLocalSwitch(nil)
                    }
                } else {
                ForEach(AgentBackend.allCases) { backend in
                    Button {
                        let model = director.defaultModelOption(for: backend)
                        apply(
                            CharacterAgentSettings(
                                backend: backend,
                                model: model.id,
                                effort: model.defaultEffort,
                                fastMode: settings.fastMode
                                    && model.supportsFastMode,
                                permission: settings.permission
                            )
                        )
                    } label: {
                        if settings.backend == backend {
                            Label(backend.title, systemImage: "checkmark")
                        } else {
                            Text(backend.title)
                        }
                    }
                }
                }
                if !director.localModelOptions.isEmpty {
                    Divider()
                    ForEach(director.localModelOptions) { profile in
                        Button {
                            requestLocalSwitch(profile.id)
                        } label: {
                            Label("\(OfficeLocalization.string("로컬 AI")) · \(profile.displayTitle)", systemImage: director.localProfileID(for: character.id) == profile.id ? "checkmark" : "desktopcomputer")
                        }
                        .disabled(director.localProfileID(for: character.id) == profile.id)
                    }
                }
            } label: {
                QuickSettingLabel(
                    text: isLocal ? OfficeLocalization.string("로컬 AI") : settings.backend.title,
                    systemImage: "terminal"
                )
            }
            .disabled(!availability.canChangeBackend)
            .help(OfficeLocalization.string("직원 CLI 선택"))
            .accessibilityIdentifier("agentProviderPicker")

            if isLocal {
                QuickSettingLabel(text: localModelTitle, systemImage: "cpu")
                QuickSettingLabel(text: "32K", systemImage: "memorychip")
                QuickSettingLabel(text: OfficeLocalization.string("기본 추론"), systemImage: "brain")
                QuickSettingLabel(text: OfficeLocalization.string(settings.permission.title), systemImage: "shield")
            } else {
            Menu {
                ForEach(selectableModels) { model in
                    Button {
                        var updated = settings
                        updated.selectModel(model)
                        apply(updated)
                    } label: {
                        if settings.model == model.id {
                            Label(
                                modelTitle(model),
                                systemImage: "checkmark"
                            )
                        } else {
                            Text(modelTitle(model))
                        }
                    }
                }
                Divider()
                Button {
                    isShowingModelVisibilitySettings = true
                } label: {
                    Label(OfficeLocalization.string("표시 모델 관리…"), systemImage: "slider.horizontal.3")
                }
            } label: {
                QuickSettingLabel(
                    text: director.modelTitle(
                        for: settings.backend,
                        model: settings.model ?? settings.backend.defaultModel
                    ),
                    systemImage: "cpu"
                )
            }
            .disabled(!availability.canChangeCurrentBackendSettings)

            if director.supportsFastMode(
                for: settings.backend,
                model: settings.model
            ) {
                Button {
                    var updated = settings
                    if let selectedModelOption {
                        updated.setFastMode(
                            !settings.fastMode,
                            option: selectedModelOption
                        )
                    } else {
                        updated.setFastMode(!settings.fastMode)
                    }
                    apply(updated)
                } label: {
                    QuickSettingLabel(
                        text: settings.fastMode ? "Fast" : "Standard",
                        systemImage: settings.fastMode ? "bolt.fill" : "bolt"
                    )
                }
                .buttonStyle(.plain)
                .help(
                    settings.fastMode
                        ? OfficeLocalization.string("Fast 모드 끄기")
                        : OfficeLocalization.string("Fast 모드 켜기")
                )
                .disabled(!availability.canChangeCurrentBackendSettings)
            }

            Menu {
                ForEach(
                    director.effortOptions(
                        for: settings.backend,
                        model: settings.model
                    ),
                    id: \.self
                ) { effort in
                    Button {
                        var updated = settings
                        updated.effort = effort
                        apply(updated)
                    } label: {
                        if settings.effort == effort {
                            Label(effort, systemImage: "checkmark")
                        } else {
                            Text(effort)
                        }
                    }
                }
            } label: {
                QuickSettingLabel(
                    text: settings.effort,
                    systemImage: "brain"
                )
            }
            .disabled(!availability.canChangeCurrentBackendSettings)

            Menu {
                ForEach(AgentPermission.allCases) { permission in
                    Button {
                        var updated = settings
                        updated.permission = permission
                        apply(updated)
                    } label: {
                        if settings.permission == permission {
                            Label(
                                OfficeLocalization.string(permission.title),
                                systemImage: "checkmark"
                            )
                        } else {
                            Text(OfficeLocalization.string(permission.title))
                        }
                    }
                }
            } label: {
                QuickSettingLabel(
                    text: OfficeLocalization.string(settings.permission.title),
                    systemImage: "shield"
                )
            }
            .disabled(!availability.canChangeCurrentBackendSettings)

            }
        }
        .menuStyle(.borderlessButton)
        // Menu는 남는 가로 폭을 나눠 가지므로 내용 폭으로 고정해 좌측에 붙인다.
        .fixedSize(horizontal: true, vertical: false)
        .task { await director.refreshLocalProviderStatuses() }
        .confirmationDialog(OfficeLocalization.string("모델을 전환하고 새 세션을 시작할까요?"), isPresented: $isConfirmingLocalSwitch, titleVisibility: .visible) {
            Button(OfficeLocalization.string("전환")) {
                let target = pendingLocalCharacter ?? character.id
                let profileID = pendingLocalProfileID
                Task {
                    _ = await director.selectLocalProfile(profileID, for: target)
                    isShowingLocalResult = true
                }
            }
            Button(OfficeLocalization.string("취소"), role: .cancel) {}
        } message: {
            Text(OfficeLocalization.string("기존 대화 기록은 보존됩니다. 작업을 마치고 터미널을 닫은 뒤 전환하세요."))
        }
        .alert(OfficeLocalization.string("로컬 모델 전환"), isPresented: $isShowingLocalResult) {
            Button(OfficeLocalization.string("확인"), role: .cancel) {}
        } message: {
            Text(director.settingsStatus ?? OfficeLocalization.string("잠시 후 다시 시도하세요."))
        }
        .sheet(isPresented: $isShowingModelVisibilitySettings) {
            AgentModelVisibilitySettingsView(
                director: director,
                initialBackend: settings.backend
            )
        }
    }

    private func apply(_ settings: CharacterAgentSettings) {
        var normalized = settings
        if let model = director.modelOption(
            for: normalized.backend,
            model: normalized.model
        ) {
            normalized.selectModel(model)
        }
        Task {
            await director.updateAgentSettings(
                normalized,
                for: character.id
            )
            if director.agentSettings(for: character.id) == normalized {
                onChanged?()
            }
        }
    }

    private func requestLocalSwitch(_ profileID: String?) {
        pendingLocalProfileID = profileID
        pendingLocalCharacter = character.id
        isConfirmingLocalSwitch = true
    }

    private func modelTitle(_ model: AgentModelOption) -> String {
        model.isRecentlyRevised()
            ? OfficeLocalization.format("%@ · 새 버전", model.title)
            : model.title
    }
}

enum AgentModelVisibilitySettingsLayout {
    static let width: CGFloat = 640
    static let height: CGFloat = 560
}

private struct AgentModelVisibilitySettingsView: View {
    @ObservedObject var director: AgentDirector
    @Environment(\.dismiss) private var dismiss
    @State private var selectedBackend: AgentBackend
    @State private var draftExclusions: [AgentBackend: Set<String>]
    @State private var isRefreshing = false
    @State private var isSaving = false
    @State private var errorMessage: String?

    private let managedBackends: [AgentBackend] = AgentBackend.allCases

    init(director: AgentDirector, initialBackend: AgentBackend) {
        self.director = director
        _selectedBackend = State(initialValue: initialBackend)
        _draftExclusions = State(
            initialValue: Dictionary(
                uniqueKeysWithValues: AgentBackend.allCases.map {
                    ($0, director.excludedModels(for: $0))
                }
            )
        )
    }

    private var models: [AgentModelOption] {
        director.allDiscoveredModelOptions(for: selectedBackend)
            .filter(\.available)
    }

    private var providerCatalog: AgentModelProviderCatalog? {
        director.modelCatalogs[selectedBackend]
    }

    private var canSave: Bool {
        !isSaving && managedBackends.allSatisfy { backend in
            let available = director.allDiscoveredModelOptions(for: backend)
                .filter(\.available)
            let excluded = draftExclusions[backend] ?? []
            return available.isEmpty
                || available.contains { !excluded.contains($0.id) }
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                VStack(alignment: .leading, spacing: 4) {
                    Text(OfficeLocalization.string("표시 모델 관리"))
                        .font(.system(size: 18, weight: .bold))
                    Text(OfficeLocalization.string("체크한 모델은 하단 선택기에서 제외됩니다. 새 모델은 자동으로 표시됩니다."))
                        .font(.system(size: 12))
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if isRefreshing {
                    ProgressView()
                        .controlSize(.small)
                }
            }

            Picker("CLI", selection: $selectedBackend) {
                ForEach(managedBackends) { backend in
                    Text(backend.title).tag(backend)
                }
            }
            .pickerStyle(.segmented)

            ScrollView {
                LazyVStack(spacing: 8) {
                    ForEach(models) { model in
                        Toggle(isOn: exclusionBinding(for: model.id)) {
                            HStack(spacing: 10) {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(model.title)
                                        .font(.system(size: 13, weight: .semibold))
                                    Text(model.id)
                                        .font(.system(size: 10, design: .monospaced))
                                        .foregroundStyle(.secondary)
                                    Text(modelDetail(model))
                                        .font(.system(size: 10))
                                        .foregroundStyle(.secondary)
                                }
                                Spacer()
                                if isModelInUse(model.id) {
                                    Text(OfficeLocalization.string("직원 사용 중"))
                                        .font(.system(size: 10, weight: .semibold))
                                        .foregroundStyle(.orange)
                                }
                                if isExcluded(model.id) {
                                    Text(OfficeLocalization.string("선택에서 제외됨"))
                                        .font(.system(size: 11, weight: .medium))
                                        .foregroundStyle(.red)
                                }
                            }
                        }
                        .toggleStyle(.checkbox)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 9)
                        .background(
                            Color.primary.opacity(0.045),
                            in: RoundedRectangle(cornerRadius: 9)
                        )
                    }
                }
                .padding(.vertical, 2)
            }
            .frame(maxHeight: .infinity)

            if let catalog = providerCatalog {
                HStack(spacing: 8) {
                    if let fetchedAt = catalog.fetchedAt {
                        Text(
                            OfficeLocalization.format(
                                "최근 수집 %@",
                                OfficeLocalization.date(fetchedAt, dateStyle: .abbreviated, time: .shortened)
                            )
                        )
                    } else {
                        Text(OfficeLocalization.string("내장 기본 목록 사용 중"))
                    }
                    if let lastError = catalog.lastError, !lastError.isEmpty {
                        Text(OfficeLocalization.string("· 마지막 갱신 실패"))
                            .foregroundStyle(.orange)
                            .help(OfficeLocalization.systemMessage(lastError))
                    }
                }
                .font(.system(size: 10))
                .foregroundStyle(.secondary)
            }

            if let errorMessage {
                Text(OfficeLocalization.systemMessage(errorMessage))
                    .font(.system(size: 11, weight: .medium))
                    .foregroundStyle(.red)
                    .lineLimit(2)
            }

            HStack {
                Button {
                    refresh()
                } label: {
                    Label(OfficeLocalization.string("목록 새로고침"), systemImage: "arrow.clockwise")
                }
                .disabled(isRefreshing || isSaving)

                Spacer()

                Button(OfficeLocalization.string("취소")) {
                    dismiss()
                }
                .keyboardShortcut(.cancelAction)

                Button(isSaving ? OfficeLocalization.string("저장 중…") : OfficeLocalization.string("저장")) {
                    save()
                }
                .keyboardShortcut(.defaultAction)
                .disabled(!canSave)
            }
        }
        .padding(20)
        .frame(
            width: AgentModelVisibilitySettingsLayout.width,
            height: AgentModelVisibilitySettingsLayout.height
        )
        .accessibilityIdentifier("agentModelVisibilitySettings")
    }

    private func exclusionBinding(for modelID: String) -> Binding<Bool> {
        Binding(
            get: {
                draftExclusions[selectedBackend, default: []]
                    .contains(modelID)
            },
            set: { isExcluded in
                var values = draftExclusions[selectedBackend, default: []]
                if isExcluded {
                    values.insert(modelID)
                } else {
                    values.remove(modelID)
                }
                draftExclusions[selectedBackend] = values
            }
        )
    }

    private func modelDetail(_ model: AgentModelOption) -> String {
        var parts = [OfficeLocalization.format("추론 %@", model.efforts.joined(separator: " · "))]
        if model.supportsFastMode {
            parts.append(OfficeLocalization.string("Fast 지원"))
        }
        if let resolved = model.resolvedModel {
            parts.append(OfficeLocalization.format("실제 모델 %@", resolved))
        }
        if let previous = model.previousResolvedModel,
           let changedAt = model.resolvedModelChangedAt
        {
            parts.append(
                OfficeLocalization.format(
                    "%@에 %@에서 바뀜",
                    OfficeLocalization.date(changedAt, dateStyle: .abbreviated, time: .omitted),
                    previous
                )
            )
        }
        return parts.joined(separator: "  |  ")
    }

    private func isExcluded(_ modelID: String) -> Bool {
        draftExclusions[selectedBackend, default: []].contains(modelID)
    }

    private func isModelInUse(_ modelID: String) -> Bool {
        director.characters.contains {
            $0.backend == selectedBackend && $0.model == modelID
        }
    }

    private func reloadDrafts() {
        draftExclusions = Dictionary(
            uniqueKeysWithValues: managedBackends.map {
                ($0, director.excludedModels(for: $0))
            }
        )
    }

    private func refresh() {
        isRefreshing = true
        errorMessage = nil
        Task {
            defer { isRefreshing = false }
            do {
                try await director.refreshModelCatalog(force: true)
                reloadDrafts()
            } catch {
                errorMessage = error.localizedDescription
            }
        }
    }

    private func save() {
        isSaving = true
        errorMessage = nil
        Task {
            defer { isSaving = false }
            do {
                for backend in managedBackends {
                    try await director.updateModelExclusions(
                        draftExclusions[backend] ?? [],
                        for: backend
                    )
                }
                dismiss()
            } catch {
                errorMessage = error.localizedDescription
            }
        }
    }
}

private struct QuickSettingLabel: View {
    let text: String
    let systemImage: String

    var body: some View {
        Label(text, systemImage: systemImage)
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(Color.primary.opacity(0.68))
            .padding(.horizontal, 7)
            .padding(.vertical, 5)
            .background(Color.primary.opacity(0.055), in: Capsule())
            .fixedSize()
    }
}

enum CharacterIdentitySettingsLayout {
    static let width: CGFloat = 620
    static let height: CGFloat = 500
    static let identityPromptHeight: CGFloat = 280
}

private struct CharacterIdentitySettingsView: View {
    let director: AgentDirector
    let character: OfficeCharacter

    @Environment(\.dismiss) private var dismiss
    @State private var nameDraft = ""
    @State private var identityPromptDraft = ""
    @State private var isLoading = true
    @State private var isSaving = false
    @State private var hasLoaded = false
    @State private var errorMessage: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(OfficeLocalization.format("%@ 설정", director.displayName(for: character)))
                .font(.system(size: 17, weight: .bold))

            Text(OfficeLocalization.string("선택한 직원의 이름과 업무 지침만 저장합니다."))
                .font(.system(size: 12))
                .foregroundStyle(.secondary)

            Group {
                if isLoading {
                    ProgressView(OfficeLocalization.string("최신 설정을 불러오는 중입니다."))
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else if hasLoaded {
                    VStack(alignment: .leading, spacing: 14) {
                        VStack(alignment: .leading, spacing: 6) {
                            Text(OfficeLocalization.string("이름"))
                                .font(.system(size: 11, weight: .semibold))
                                .foregroundStyle(.secondary)
                            TextField(OfficeLocalization.string("이름"), text: $nameDraft)
                                .textFieldStyle(.roundedBorder)
                        }

                        VStack(alignment: .leading, spacing: 6) {
                            HStack {
                                Text(OfficeLocalization.string("업무 지침"))
                                    .font(
                                        .system(size: 11, weight: .semibold)
                                    )
                                    .foregroundStyle(.secondary)
                                Spacer()
                                Text("\(identityPromptDraft.count) / 1,200")
                                    .font(.system(size: 10, weight: .medium))
                                    .foregroundStyle(
                                        identityPromptDraft.count > 1_200
                                            ? Color.red
                                            : Color.secondary
                                    )
                            }

                            TextEditor(text: $identityPromptDraft)
                                .font(.system(size: 12))
                                .frame(
                                    height: CharacterIdentitySettingsLayout
                                        .identityPromptHeight
                                )
                                .padding(6)
                                .background(
                                    Color(nsColor: .textBackgroundColor),
                                    in: RoundedRectangle(
                                        cornerRadius: 8,
                                        style: .continuous
                                    )
                                )
                                .overlay {
                                    RoundedRectangle(
                                        cornerRadius: 8,
                                        style: .continuous
                                    )
                                    .stroke(
                                        Color.primary.opacity(0.14),
                                        lineWidth: 1
                                    )
                                }
                        }
                    }
                } else {
                    VStack(spacing: 10) {
                        Text(OfficeLocalization.string("최신 설정을 불러오지 못했습니다."))
                            .font(.system(size: 12, weight: .semibold))
                        Button(OfficeLocalization.string("다시 시도")) {
                            Task {
                                await load()
                            }
                        }
                    }
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            if let errorMessage {
                Text(OfficeLocalization.systemMessage(errorMessage))
                    .font(.system(size: 11, weight: .medium))
                    .foregroundStyle(.red)
            }

            HStack {
                Spacer()
                Button(OfficeLocalization.string("취소")) {
                    dismiss()
                }
                Button(OfficeLocalization.string("저장")) {
                    Task {
                        await save()
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(!canSave)
            }
        }
        .padding(18)
        .frame(
            width: CharacterIdentitySettingsLayout.width,
            height: CharacterIdentitySettingsLayout.height
        )
        .task(id: character) {
            await load()
        }
        .interactiveDismissDisabled(isSaving)
    }

    private var canSave: Bool {
        hasLoaded
            && !isLoading
            && !isSaving
            && (1...30).contains(
                nameDraft.trimmingCharacters(in: .whitespacesAndNewlines).count
            )
            && (1...1_200).contains(
                identityPromptDraft
                    .trimmingCharacters(in: .whitespacesAndNewlines).count
            )
    }

    private func load() async {
        isLoading = true
        hasLoaded = false
        errorMessage = nil
        defer {
            isLoading = false
        }
        do {
            let draft = try await director.fetchCharacterIdentitySettings(
                for: character
            )
            nameDraft = draft.name
            identityPromptDraft = OfficeLocalization.displayIdentityPrompt(
                draft.identityPrompt
            )
            hasLoaded = true
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func save() async {
        isSaving = true
        errorMessage = nil
        defer {
            isSaving = false
        }
        do {
            try await director.saveCharacterIdentitySettings(
                name: nameDraft,
                identityPrompt: OfficeLocalization.canonicalIdentityPrompt(
                    identityPromptDraft
                ),
                for: character
            )
            dismiss()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}
