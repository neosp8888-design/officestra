// 이 파일은 Claude Code 업무를 도구 배지와 계획 중심 타임라인으로 보여준다.

import AppKit
import OfficeCore
import SwiftUI

enum ClaudePalette {
    static func accent(for backend: AgentBackend) -> Color {
        DashboardPalette.providerAccent(for: backend)
    }

    static func nsAccent(for backend: AgentBackend) -> NSColor {
        switch backend {
        case .claude:
            NSColor(calibratedRed: 0.77, green: 0.43, blue: 0.25, alpha: 1)
        case .codex:
            NSColor(calibratedRed: 0.13, green: 0.55, blue: 0.52, alpha: 1)
        case .antigravity:
            NSColor(calibratedRed: 0.19, green: 0.49, blue: 0.88, alpha: 1)
        }
    }

    static func color(for family: ClaudeToolFamily) -> Color {
        switch family {
        case .shell:
            .indigo
        case .read:
            Color(red: 0.28, green: 0.5, blue: 0.72)
        case .edit:
            .green
        case .search:
            .purple
        case .web:
            .teal
        case .plan:
            .orange
        case .delegate:
            .pink
        case .other:
            .secondary
        }
    }

    static func icon(for family: ClaudeToolFamily) -> String {
        switch family {
        case .shell:
            "terminal"
        case .read:
            "doc.text"
        case .edit:
            "square.and.pencil"
        case .search:
            "magnifyingglass"
        case .web:
            "globe"
        case .plan:
            "checklist"
        case .delegate:
            "person.2"
        case .other:
            "wrench.and.screwdriver"
        }
    }

    static func color(for kind: ClaudeToolGroupKind) -> Color {
        color(for: family(of: kind))
    }

    static func icon(for kind: ClaudeToolGroupKind) -> String {
        icon(for: family(of: kind))
    }

    private static func family(
        of kind: ClaudeToolGroupKind
    ) -> ClaudeToolFamily {
        switch kind {
        case .shell:
            .shell
        case .read:
            .read
        case .search:
            .search
        case .web:
            .web
        case .delegate:
            .delegate
        case .other:
            .other
        }
    }
}

extension ClaudeToolGroupKind {
    /// 그룹 카드 제목이다.
    var title: String {
        switch self {
        case .shell:
            OfficeLocalization.string("명령 실행")
        case .read:
            OfficeLocalization.string("파일 읽기")
        case .search:
            OfficeLocalization.string("검색")
        case .web:
            OfficeLocalization.string("웹 조회")
        case .delegate:
            OfficeLocalization.string("서브에이전트")
        case .other:
            OfficeLocalization.string("도구 사용")
        }
    }

    /// `이전 OO 3개 보기`에 들어가는 낱말이다.
    var historyNoun: String {
        switch self {
        case .shell:
            OfficeLocalization.historyNoun("명령")
        case .read:
            OfficeLocalization.historyNoun("읽기")
        case .search:
            OfficeLocalization.historyNoun("검색")
        case .web:
            OfficeLocalization.historyNoun("조회")
        case .delegate:
            OfficeLocalization.historyNoun("위임")
        case .other:
            OfficeLocalization.historyNoun("도구 사용")
        }
    }
}

struct ClaudeTranscriptView: View {
    let backend: AgentBackend
    let turnID: String
    let workspaceDirectory: String
    let activities: [LiveFeedActivity]
    let response: String
    let responseUpdatedAt: Date
    let isRunning: Bool
    let isCompleted: Bool
    let needsInput: Bool
    let animatesResponse: Bool
    let animatesInitialResponse: Bool
    let responseFeedback: TurnResponseFeedback?
    let updateResponseFeedback: (TurnResponseFeedback?) async -> Void
    let onResponsePresented: () -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var showsAllEntries = false

    private static let compactEntryLimit = 18

    var body: some View {
        let presentation = TranscriptPresentationCache.shared.presentation(
            provider: .claude,
            turnID: turnID,
            activities: activities,
            response: response,
            responseUpdatedAt: responseUpdatedAt,
            isRunning: isRunning
        ) {
            ClaudeTranscriptPresentation.make(
                turnID: turnID,
                activities: activities,
                response: response,
                responseUpdatedAt: responseUpdatedAt,
                isRunning: isRunning
            )
        }
        let hiddenCount = max(
            0,
            presentation.entries.count - Self.compactEntryLimit
        )
        let visibleEntries = showsAllEntries
            ? presentation.entries
            : Array(presentation.entries.suffix(Self.compactEntryLimit))
        let conclusionMessageID = isCompleted
            ? presentation.latestMessage?.id
            : nil
        let responseCompletionRevision =
            ClaudeResponseCompletionRevision(
                response: response,
                isRunning: isRunning,
                animatesResponse: animatesResponse
            )

        VStack(alignment: .leading, spacing: 13) {
            if hiddenCount > 0, !showsAllEntries {
                Button {
                    withAnimation(.easeInOut(duration: 0.16)) {
                        showsAllEntries = true
                    }
                } label: {
                    Label(
                        OfficeLocalization.format("이전 기록 %d개 보기", hiddenCount),
                        systemImage: "clock.arrow.circlepath"
                    )
                    .font(.system(size: 10.5, weight: .semibold))
                    .foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
            }

            ForEach(visibleEntries) { entry in
                transcriptEntry(
                    entry,
                    isConclusion: entry.id == conclusionMessageID,
                    isStreaming: entry.id == presentation.streamingMessageID
                )
            }

            if presentation.showsWaiting {
                ClaudeWaitingView(backend: backend)
                    .transition(
                        reduceMotion
                            ? .opacity
                            : .opacity.combined(with: .scale(scale: 0.98))
                    )
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .animation(
            reduceMotion ? nil : .easeOut(duration: 0.18),
            value: presentation.showsWaiting
        )
        .task(id: responseCompletionRevision) {
            guard
                animatesResponse,
                !isRunning
            else {
                return
            }
            onResponsePresented()
        }
        .onDisappear {
            if animatesResponse, !isRunning {
                onResponsePresented()
            }
        }
    }

    @ViewBuilder
    private func transcriptEntry(
        _ entry: ClaudeTranscriptEntry,
        isConclusion: Bool,
        isStreaming: Bool
    ) -> some View {
        switch entry {
        case .thoughts(let run):
            ClaudeThoughtRunView(run: run, backend: backend)
                .equatable()
        case .tools(let run):
            ClaudeToolRunView(run: run)
                .equatable()
        case .edits(let run):
            ClaudeEditRunView(
                run: run,
                workspaceDirectory: workspaceDirectory,
                backend: backend
            )
        case .plan(let board):
            ClaudePlanBoardView(board: board, backend: backend)
        case .message(let message):
            ClaudeMessageView(
                turnID: turnID,
                workspaceDirectory: workspaceDirectory,
                message: message,
                isConclusion: isConclusion,
                needsInput: isConclusion && needsInput,
                isStreaming: isStreaming,
                animates: animatesResponse,
                animatesInitialSource: animatesInitialResponse,
                responseFeedback: responseFeedback,
                updateResponseFeedback: updateResponseFeedback,
                onFinishedTyping: onResponsePresented,
                backend: backend
            )
        }
    }
}

struct ClaudeResponseCompletionRevision: Hashable {
    let response: String
    let isRunning: Bool
    let animatesResponse: Bool
}

private struct ClaudeWaitingView: View {
    let backend: AgentBackend

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(spacing: 9) {
            CoreAnimationDotsView(
                dotSize: 4,
                spacing: 2.5,
                travel: 2.5,
                color: ClaudePalette.nsAccent(for: backend),
                isAnimated: !reduceMotion
            )
            .frame(width: 20, height: 16)
            .accessibilityHidden(true)

            Text(OfficeLocalization.string("생각 중"))
                .font(.system(size: 12.5, weight: .semibold))
                .foregroundStyle(.secondary)

            Spacer(minLength: 0)
        }
        .padding(.horizontal, 10)
        .frame(height: 36)
        .background(
            ClaudePalette.accent(for: backend).opacity(0.06),
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .accessibilityElement(children: .combine)
        .accessibilityLabel(OfficeLocalization.string("생각 중"))
    }
}

/// 추론은 Claude Code의 강점이라 최신 원문은 접지 않고 이전 기록만 접는다.
private struct ClaudeThoughtRunView: View, Equatable {
    let run: ClaudeThoughtRun
    let backend: AgentBackend

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var isExpanded = false
    @State private var showsAllHistory = false

    private static let compactHistoryLimit = 20

    static func == (
        lhs: ClaudeThoughtRunView,
        rhs: ClaudeThoughtRunView
    ) -> Bool {
        lhs.run == rhs.run && lhs.backend == rhs.backend
    }

    var body: some View {
        let historyCount = max(0, run.thoughts.count - 1)
        let hiddenCount = run.hiddenHistoryThoughtCount(
            limit: Self.compactHistoryLimit
        )

        VStack(alignment: .leading, spacing: 8) {
            groupHeader

            if let latestThought = run.latestThought {
                thoughtRow(latestThought, isLatest: true)
            }

            if historyCount > 0 {
                DisclosureGroup(isExpanded: $isExpanded) {
                    if isExpanded {
                        LazyVStack(alignment: .leading, spacing: 5) {
                            if hiddenCount > 0, !showsAllHistory {
                                Button {
                                    showsAllHistory = true
                                } label: {
                                    Label(
                                        OfficeLocalization.format("더 이전 추론 %d개 보기", hiddenCount),
                                        systemImage: "clock.arrow.circlepath"
                                    )
                                    .font(
                                        .system(size: 9.5, weight: .semibold)
                                    )
                                    .foregroundStyle(.secondary)
                                }
                                .buttonStyle(.plain)
                                .padding(.vertical, 5)
                            }

                            ForEach(visibleHistory) { thought in
                                thoughtRow(thought, isLatest: false)
                            }
                        }
                        .padding(.top, 5)
                    }
                } label: {
                    Text(
                        isExpanded
                            ? OfficeLocalization.string("이전 추론 숨기기")
                            : OfficeLocalization.format("이전 추론 %d개 보기", historyCount)
                    )
                        .font(.system(size: 9.5, weight: .semibold))
                        .foregroundStyle(.secondary)
                        .contentShape(Rectangle())
                }
                .tint(.secondary)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 9)
        .background(
            ClaudePalette.accent(for: backend).opacity(0.05),
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(ClaudePalette.accent(for: backend).opacity(0.14))
        }
        .onChange(of: run.isRunning) { _, running in
            isExpanded = transcriptGroupExpansionState(
                current: isExpanded,
                isRunning: running
            )
        }
    }

    private var groupHeader: some View {
        HStack(spacing: 8) {
            if run.isRunning {
                CoreAnimationDotsView(
                    dotSize: 3,
                    spacing: 2,
                    travel: 2,
                    color: ClaudePalette.nsAccent(for: backend),
                    isAnimated: !reduceMotion
                )
                .frame(width: 18, height: 14)
                .accessibilityHidden(true)
            } else {
                Image(systemName: "brain.head.profile")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(.secondary)
                    .frame(width: 18)
            }

            Text(OfficeLocalization.string("추론"))
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(.secondary)

            Spacer(minLength: 6)

            Text(OfficeLocalization.format("%d개", run.thoughts.count))
                .font(.system(size: 9, weight: .semibold))
                .foregroundStyle(.tertiary)
        }
    }

    private func thoughtRow(
        _ thought: ClaudeThought,
        isLatest: Bool
    ) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "sparkle")
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(
                    ClaudePalette.accent(for: backend).opacity(0.65)
                )
                .frame(width: 15, height: 15)

            Text(thought.text)
                .font(.system(size: 12.5, weight: .regular))
                .italic()
                .foregroundStyle(.secondary)
                .lineLimit(isLatest ? nil : 4)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)

            Text(OfficeLocalization.date(thought.occurredAt,
                dateStyle: .omitted,
                time: .standard
            ))
                .font(.system(size: 8.5, design: .monospaced))
                .foregroundStyle(.tertiary)
        }
        .padding(.vertical, isLatest ? 4 : 3)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(OfficeLocalization.format("추론, %@", thought.text))
    }

    private var visibleHistory: [ClaudeThought] {
        run.visibleHistoryThoughts(
            showsAll: showsAllHistory,
            limit: Self.compactHistoryLimit
        )
    }
}

/// 연속된 도구 호출은 실제로 사용한 도구 이름을 제목으로 묶는다.
private struct ClaudeToolRunView: View, Equatable {
    let run: ClaudeToolRun

    @State private var isExpanded = false
    @State private var showsAllSteps = false

    private static let compactStepLimit = 20

    static func == (
        lhs: ClaudeToolRunView,
        rhs: ClaudeToolRunView
    ) -> Bool {
        lhs.run == rhs.run
    }

    var body: some View {
        let historyCount = max(0, run.steps.count - 1)
        let hiddenCount = run.hiddenHistoryStepCount(
            limit: Self.compactStepLimit
        )

        VStack(alignment: .leading, spacing: 8) {
            groupHeader

            if let latestStep = run.latestStep {
                stepRow(latestStep)
            }

            if historyCount > 0 {
                DisclosureGroup(isExpanded: $isExpanded) {
                    if isExpanded {
                        LazyVStack(alignment: .leading, spacing: 0) {
                            if hiddenCount > 0, !showsAllSteps {
                                Button {
                                    showsAllSteps = true
                                } label: {
                                    Label(
                                        OfficeLocalization.format("더 이전 호출 %d개 보기", hiddenCount),
                                        systemImage: "clock.arrow.circlepath"
                                    )
                                    .font(
                                        .system(size: 9.5, weight: .semibold)
                                    )
                                    .foregroundStyle(.secondary)
                                }
                                .buttonStyle(.plain)
                                .padding(.vertical, 6)
                            }

                            ForEach(visibleHistory) { step in
                                stepRow(step)
                            }
                        }
                        .padding(.top, 5)
                    }
                } label: {
                    Text(
                        isExpanded
                            ? OfficeLocalization.format("이전 %@ 숨기기", run.kind.historyNoun)
                            : OfficeLocalization.format("이전 %@ %d개 보기", run.kind.historyNoun, historyCount)
                    )
                        .font(.system(size: 9.5, weight: .semibold))
                        .foregroundStyle(.secondary)
                        .contentShape(Rectangle())
                }
                .tint(.secondary)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 9)
        .background(
            Color.primary.opacity(0.035),
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(Color.primary.opacity(0.06))
        }
        .onChange(of: run.isRunning) { _, running in
            isExpanded = transcriptGroupExpansionState(
                current: isExpanded,
                isRunning: running
            )
        }
    }

    private var groupHeader: some View {
        HStack(spacing: 8) {
            Image(systemName: ClaudePalette.icon(for: run.kind))
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(
                    run.isRunning
                        ? ClaudePalette.color(for: run.kind)
                        : .secondary
                )
                .frame(width: 18)

            Text(run.kind.title)
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(.secondary)

            // 어떤 도구를 실제로 썼는지는 Claude 기록의 핵심이라 함께 남긴다.
            Text(run.title)
                .font(.system(size: 9.5, weight: .semibold, design: .monospaced))
                .foregroundStyle(.tertiary)
                .lineLimit(1)

            Spacer(minLength: 6)

            Text(OfficeLocalization.format("%d개", run.steps.count))
                .font(.system(size: 9, weight: .semibold))
                .foregroundStyle(.tertiary)
        }
    }

    private func stepRow(_ step: ClaudeToolStep) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Group {
                if step.status == .running {
                    ProgressView()
                        .controlSize(.mini)
                        .tint(ClaudePalette.color(for: step.call.family))
                } else {
                    Image(
                        systemName: step.status == .failed
                            ? "xmark.circle.fill"
                            : "checkmark.circle.fill"
                    )
                    .foregroundStyle(
                        step.status == .failed
                            ? Color.red
                            : ClaudePalette.color(for: step.call.family)
                    )
                }
            }
            .frame(width: 15, height: 15)

            ClaudeToolBadge(call: step.call, isCompact: false)

            Text(detailText(step))
                .font(.system(size: 10.5, design: .monospaced))
                .foregroundStyle(.secondary)
                .lineLimit(4)
                .frame(maxWidth: .infinity, alignment: .leading)

            Text(OfficeLocalization.date(step.occurredAt,
                dateStyle: .omitted,
                time: .standard
            ))
                .font(.system(size: 8.5, design: .monospaced))
                .foregroundStyle(.tertiary)
        }
        .padding(.vertical, 5)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(
            "\(step.call.displayName), \(detailText(step))"
        )
    }

    private var visibleHistory: [ClaudeToolStep] {
        run.visibleHistorySteps(
            showsAll: showsAllSteps,
            limit: Self.compactStepLimit
        )
    }

    /// Bash만 셸 프롬프트를 붙여 명령임을 드러낸다.
    private func detailText(_ step: ClaudeToolStep) -> String {
        guard !step.call.detail.isEmpty else {
            return OfficeLocalization.string("실행")
        }
        return step.call.family == .shell
            ? "$ \(step.call.detail)"
            : step.call.displayDetail
    }
}

private struct ClaudeToolBadge: View {
    let call: ClaudeToolCall
    let isCompact: Bool

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: ClaudePalette.icon(for: call.family))
                .font(.system(size: 9, weight: .bold))

            if !isCompact {
                Text(call.displayName)
                    .font(.system(size: 9, weight: .bold, design: .monospaced))
                    .lineLimit(1)
            }
        }
        .foregroundStyle(ClaudePalette.color(for: call.family))
        .padding(.horizontal, isCompact ? 5 : 6)
        .frame(height: 17)
        .background(
            ClaudePalette.color(for: call.family).opacity(0.12),
            in: RoundedRectangle(cornerRadius: 5, style: .continuous)
        )
        .accessibilityHidden(isCompact)
    }
}

/// 같은 파일을 여러 번 고쳐도 한 줄로 합쳐 보여주고, 백엔드가 도구
/// 입력에서 센 편집 줄 수를 Codex 카드와 같은 형태로 함께 보여준다.
private struct ClaudeEditRunView: View {
    let run: ClaudeEditRun
    let workspaceDirectory: String
    let backend: AgentBackend

    @State private var copied = false
    @State private var copyResetTask: Task<Void, Never>?

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 9) {
                Image(systemName: "doc.badge.gearshape")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(statusColor)
                    .frame(width: 28, height: 28)
                    .background(
                        statusColor.opacity(0.10),
                        in: RoundedRectangle(cornerRadius: 8)
                    )

                VStack(alignment: .leading, spacing: 2) {
                    Text(run.title)
                        .font(.system(size: 12.5, weight: .bold))

                    if let totals = run.totals {
                        HStack(spacing: 5) {
                            Text("+\(totals.additions)")
                                .foregroundStyle(.green)
                            Text("-\(totals.deletions)")
                                .foregroundStyle(.red)
                        }
                        .font(
                            .system(
                                size: 10.5,
                                weight: .bold,
                                design: .monospaced
                            )
                        )
                    }
                }

                if run.status == .running {
                    ProgressView()
                        .controlSize(.mini)
                        .tint(ClaudePalette.accent(for: backend))
                }

                Spacer(minLength: 6)

                Button {
                    copySummary()
                } label: {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        .officeToolFont(size: 11)
                        .foregroundStyle(
                            copied
                                ? ClaudePalette.accent(for: backend)
                                : Color.secondary
                        )
                }
                .officeTextTool()
                .help(
                    copied
                        ? OfficeLocalization.string("편집 목록 복사됨")
                        : OfficeLocalization.string("편집 목록 복사")
                )
                .accessibilityLabel(
                    copied
                        ? OfficeLocalization.string("편집 목록 복사됨")
                        : OfficeLocalization.string("편집 목록 복사")
                )
                .accessibilityIdentifier("copyEdits-\(run.id)")
            }

            VStack(alignment: .leading, spacing: 5) {
                ForEach(run.files) { file in
                    HStack(spacing: 7) {
                        WorkspaceFileRevealButton(
                            title: file.path,
                            path: file.path,
                            workspaceDirectory: workspaceDirectory,
                            foregroundColor: file.status == .failed
                                ? .red
                                : .secondary,
                            accessibilityIdentifier:
                                "revealEdit-\(run.id)-\(file.id)"
                        )

                        if file.editCount > 1 {
                            Text("×\(file.editCount)")
                                .font(
                                    .system(
                                        size: 10,
                                        weight: .bold,
                                        design: .monospaced
                                    )
                                )
                                .foregroundStyle(.tertiary)
                        }

                        Spacer(minLength: 6)

                        if let additions = file.additions,
                            let deletions = file.deletions
                        {
                            HStack(spacing: 5) {
                                Text("+\(additions)")
                                    .foregroundStyle(.green)
                                Text("-\(deletions)")
                                    .foregroundStyle(.red)
                            }
                            .font(
                                .system(
                                    size: 10,
                                    weight: .bold,
                                    design: .monospaced
                                )
                            )
                        }
                    }
                }
            }
        }
        .padding(11)
        .background(
            Color.primary.opacity(0.035),
            in: RoundedRectangle(cornerRadius: 11, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .stroke(Color.primary.opacity(0.07))
        }
        .onChange(of: run.copyText) { _, _ in
            copyResetTask?.cancel()
            copied = false
        }
        .onDisappear {
            copyResetTask?.cancel()
        }
    }

    private var statusColor: Color {
        switch run.status {
        case .running:
            ClaudePalette.accent(for: backend)
        case .completed:
            .green
        case .failed:
            .red
        }
    }

    private func copySummary() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(run.copyText, forType: .string)
        copied = true

        copyResetTask?.cancel()
        copyResetTask = Task {
            try? await Task.sleep(for: .seconds(1.4))
            if !Task.isCancelled {
                copied = false
            }
        }
    }
}

/// Claude Code의 할 일 목록은 진행 상황을 그대로 드러내는 것이 핵심이다.
private struct ClaudePlanBoardView: View {
    let board: ClaudePlanBoard
    let backend: AgentBackend

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Image(systemName: "checklist")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(ClaudePalette.accent(for: backend))

                Text(OfficeLocalization.string("작업 계획"))
                    .font(.system(size: 12, weight: .bold))

                Text("\(board.doneCount)/\(board.steps.count)")
                    .font(
                        .system(
                            size: 9.5,
                            weight: .bold,
                            design: .monospaced
                        )
                    )
                    .foregroundStyle(ClaudePalette.accent(for: backend))
                    .padding(.horizontal, 6)
                    .frame(height: 17)
                    .background(
                        ClaudePalette.accent(for: backend).opacity(0.12),
                        in: Capsule()
                    )

                Spacer(minLength: 6)

                Text(OfficeLocalization.date(board.occurredAt,
                    dateStyle: .omitted,
                    time: .standard
                ))
                    .font(.system(size: 8.5, design: .monospaced))
                    .foregroundStyle(.tertiary)
            }

            VStack(alignment: .leading, spacing: 5) {
                ForEach(board.steps) { step in
                    HStack(alignment: .top, spacing: 7) {
                        Image(systemName: icon(for: step.state))
                            .font(.system(size: 10, weight: .semibold))
                            .foregroundStyle(color(for: step.state))
                            .frame(width: 14)

                        Text(step.text)
                            .font(
                                .system(
                                    size: 11,
                                    weight: step.state == .active
                                        ? .semibold
                                        : .regular
                                )
                            )
                            .foregroundStyle(color(for: step.state))
                            .strikethrough(step.state == .done, color: .secondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
        }
        .padding(11)
        .background(
            ClaudePalette.accent(for: backend).opacity(0.05),
            in: RoundedRectangle(cornerRadius: 11, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .stroke(ClaudePalette.accent(for: backend).opacity(0.16))
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            OfficeLocalization.format(
                "작업 계획 %d단계 중 %d단계 완료",
                board.steps.count,
                board.doneCount
            )
                + (board.activeStep.map {
                    OfficeLocalization.format(", 진행 중 %@", $0.text)
                } ?? "")
        )
    }

    private func icon(for state: ClaudePlanStep.State) -> String {
        switch state {
        case .done:
            "checkmark.square.fill"
        case .active:
            "arrow.right.square.fill"
        case .todo:
            "square"
        }
    }

    private func color(for state: ClaudePlanStep.State) -> Color {
        switch state {
        case .done:
            .secondary
        case .active:
            ClaudePalette.accent(for: backend)
        case .todo:
            .primary
        }
    }
}

/// 진행 중인 마지막 응답만 Claude가 주는 글자 단위 흐름으로 표시한다.
private struct ClaudeMessageView: View {
    let turnID: String
    let workspaceDirectory: String
    let message: ClaudeTranscriptMessage
    let isConclusion: Bool
    let needsInput: Bool
    let isStreaming: Bool
    let animates: Bool
    let animatesInitialSource: Bool
    let responseFeedback: TurnResponseFeedback?
    let updateResponseFeedback: (TurnResponseFeedback?) async -> Void
    let onFinishedTyping: () -> Void
    let backend: AgentBackend

    @State private var copied = false
    @State private var copyResetTask: Task<Void, Never>?

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            if isConclusion || needsInput || isStreaming {
                Label(
                    headerTitle,
                    systemImage: headerIcon
                )
                .font(.system(size: 10.5, weight: .bold))
                .foregroundStyle(
                    needsInput
                        ? Color.orange
                        : ClaudePalette.accent(for: backend)
                )
            }

            if isStreaming {
                EquatableLiveTypingResponseView(
                    typingIdentity: turnID,
                    source: message.text,
                    fileBaseDirectory: workspaceDirectory,
                    animates: animates,
                    animatesInitialSource: animatesInitialSource,
                    isStreaming: true,
                    onFinishedTyping: onFinishedTyping
                )
                .equatable()
            } else {
                ConversationMarkdownView(
                    source: message.text,
                    fontSize: 14,
                    fileBaseDirectory: workspaceDirectory,
                    compactsChangedFileLists: isConclusion
                )
            }

            ResponseMessageFooter(
                occurredAt: message.occurredAt,
                copied: copied,
                accentColor: ClaudePalette.accent(for: backend),
                accessibilityID: "copyMessage-\(message.id)",
                showsFeedback: isConclusion && !needsInput,
                feedback: responseFeedback,
                feedbackAccessibilityIDPrefix: turnID,
                copy: copyMessage,
                feedbackChanged: updateResponseFeedback
            )
        }
        .padding(.vertical, 2)
        .onChange(of: message.text) { _, _ in
            copyResetTask?.cancel()
            copied = false
        }
        .onDisappear {
            copyResetTask?.cancel()
        }
    }

    private var headerTitle: String {
        if needsInput {
            return OfficeLocalization.string("답변 필요")
        }
        return isStreaming
            ? OfficeLocalization.string("작성 중인 응답")
            : OfficeLocalization.string("최종 응답")
    }

    private var headerIcon: String {
        if needsInput {
            return "questionmark.bubble.fill"
        }
        return isStreaming ? "text.cursor" : "checkmark.bubble.fill"
    }

    private func copyMessage() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(message.text, forType: .string)
        copied = true

        copyResetTask?.cancel()
        copyResetTask = Task {
            try? await Task.sleep(for: .seconds(1.4))
            if !Task.isCancelled {
                copied = false
            }
        }
    }
}
