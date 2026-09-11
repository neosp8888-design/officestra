// 이 파일은 캐릭터별 세션과 전체 날짜별 대화 보관함 화면을 제공한다.

import AppKit
import OfficeCore
import SwiftUI

enum ConversationHistoryTarget: Identifiable {
    case character(OfficeCharacter)
    case archive

    var id: String {
        switch self {
        case .character(let character):
            "character-\(character.rawValue)"
        case .archive:
            "archive"
        }
    }
}

struct CharacterConversationHistoryView: View {
    @ObservedObject var director: AgentDirector
    let character: OfficeCharacter
    @Environment(\.dismiss) private var dismiss
    @State private var history: CharacterHistory?
    @State private var errorMessage: String?
    @State private var isLoading = true

    var body: some View {
        VStack(spacing: 0) {
            historyHeader(
                title: OfficeLocalization.format(
                    "%@ 대화 내역",
                    director.displayName(for: character)
                ),
                subtitle: OfficeLocalization.string("모니터에 연결된 CLI 세션")
            )

            Divider()

            Group {
                if isLoading {
                    ProgressView(OfficeLocalization.string("대화 내역을 불러오는 중"))
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else if let errorMessage {
                    ContentUnavailableView(
                        OfficeLocalization.string("대화 내역을 불러오지 못했습니다"),
                        systemImage: "exclamationmark.triangle",
                        description: Text(OfficeLocalization.systemMessage(errorMessage))
                    )
                } else if let history, history.sessions.isEmpty {
                    ContentUnavailableView(
                        OfficeLocalization.string("저장된 세션이 없습니다"),
                        systemImage: "bubble.left.and.bubble.right",
                        description: Text(
                            OfficeLocalization.string("이 캐릭터에게 업무를 보내면 여기에 기록됩니다.")
                        )
                    )
                } else if let history {
                    ScrollView {
                        LazyVStack(spacing: 12) {
                            ForEach(history.sessions) { session in
                                SessionHistoryCard(session: session)
                            }
                        }
                        .padding(18)
                    }
                }
            }
        }
        .font(.system(size: 14))
        .frame(
            minWidth: 960,
            idealWidth: 1_040,
            minHeight: 720,
            idealHeight: 760
        )
        .task {
            await load()
        }
    }

    private func historyHeader(
        title: String,
        subtitle: String
    ) -> some View {
        HStack {
            VStack(alignment: .leading, spacing: 3) {
                Text(title)
                    .font(.system(size: 22, weight: .bold))
                Text(subtitle)
                    .font(.system(size: 14))
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button(OfficeLocalization.string("닫기")) {
                dismiss()
            }
            .keyboardShortcut(.cancelAction)
        }
        .padding(18)
    }

    private func load() async {
        isLoading = true
        errorMessage = nil
        do {
            history = try await director.characterHistory(for: character)
        } catch {
            errorMessage = error.localizedDescription
        }
        isLoading = false
    }
}

private struct SessionHistoryCard: View {
    let session: HistorySession

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(OfficeLocalization.string("세션 ID"))
                        .font(.system(size: 13, weight: .bold))
                        .foregroundStyle(.secondary)
                    Text(
                        session.externalId
                            ?? OfficeLocalization.string("외부 세션 ID 없음")
                    )
                        .font(.system(size: 14, design: .monospaced))
                }

                Spacer()

                if let externalID = session.externalId {
                    Button {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(
                            externalID,
                            forType: .string
                        )
                    } label: {
                        Image(systemName: "doc.on.doc")
                    }
                    .buttonStyle(.borderless)
                    .help(OfficeLocalization.string("세션 ID 복사"))
                }
            }

            HStack {
                Label(
                    OfficeLocalization.date(session.startedAt,
                        dateStyle: .abbreviated,
                        time: .shortened
                    ),
                    systemImage: "calendar"
                )
                Spacer()
                Text(
                    OfficeLocalization.format(
                        "%d개 업무",
                        session.turns.count
                    )
                )
            }
            .font(.system(size: 13, weight: .medium))
            .foregroundStyle(.secondary)

            if session.turns.isEmpty {
                Text(OfficeLocalization.string("저장된 업무가 없습니다."))
                    .font(.system(size: 14))
                    .foregroundStyle(.secondary)
            } else {
                ForEach(session.turns) { turn in
                    TurnDisclosure(turn: turn)
                }
            }
        }
        .padding(14)
        .background(
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .fill(Color.primary.opacity(0.045))
        )
        .conversationTextSelectionRegion("history-session-\(session.id)")
    }
}

private struct TurnDisclosure: View {
    let turn: HistoryTurn

    var body: some View {
        DisclosureGroup {
            VStack(alignment: .leading, spacing: 10) {
                taskPromptBlock
                transcriptBlock(
                    title: OfficeLocalization.string("응답"),
                    text: turn.response
                )
                if let warning = turn.responseSourceWarning, !warning.isEmpty {
                    ResponseSourceWarningView(message: warning)
                }
                if let warning = turn.wikiProposalWarning, !warning.isEmpty {
                    ResponseSourceWarningView(
                        message: warning,
                        accessibilityIdentifier: "wikiProposalWarning"
                    )
                }
                if !turn.responseSources.isEmpty {
                    ResponseSourceList(
                        sources: turn.responseSources,
                        workspaceDirectory: turn.conversationWorkdir ?? ""
                    )
                }
            }
            .padding(.top, 8)
        } label: {
            VStack(alignment: .leading, spacing: 3) {
                EmployeeMessageSenderLabel(sender: EmployeeMessageSender.resolve(turn.promptSender, prompt: turn.prompt))
                Text(promptPresentation.text)
                    .font(.system(size: 15, weight: .semibold))
                    .lineLimit(1)
                TaskPromptAttachmentSummary(
                    attachments: promptPresentation.attachments
                )
                HStack(spacing: 5) {
                    Text(
                        OfficeLocalization.date(turn.startedAt,
                            dateStyle: .omitted,
                            time: .shortened
                        )
                    )
                    Text("·")
                    Text(
                        agentExecutionSummary(
                            backend: turn.executionBackend,
                            model: turn.executionModel,
                            effort: turn.executionEffort,
                            fastMode: turn.executionFastMode
                        )
                    )
                }
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(.secondary)
            }
        }
        .padding(10)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(Color.primary.opacity(0.035))
        )
    }

    private var promptPresentation: TaskPromptPresentation {
        TaskPromptPresentation(prompt: turn.prompt)
    }

    private var taskPromptBlock: some View {
        VStack(alignment: .leading, spacing: 5) {
            EmployeeMessageSenderLabel(sender: EmployeeMessageSender.resolve(turn.promptSender, prompt: turn.prompt))
            Text(OfficeLocalization.string("업무"))
                .font(.system(size: 13, weight: .bold))
                .foregroundStyle(.secondary)
            if !promptPresentation.text.isEmpty {
                ConversationMarkdownView(source: promptPresentation.text)
            }
            if !promptPresentation.attachments.isEmpty {
                TaskPromptAttachmentList(
                    attachments: promptPresentation.attachments
                )
            }
        }
    }

    private func transcriptBlock(
        title: String,
        text: String
    ) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title)
                .font(.system(size: 13, weight: .bold))
                .foregroundStyle(.secondary)
            ConversationMarkdownView(
                source: text,
                compactsChangedFileLists: true
            )
        }
    }
}

struct ConversationArchiveView: View {
    @ObservedObject var director: AgentDirector
    @Environment(\.dismiss) private var dismiss
    @State private var selectedCharacter: OfficeCharacter?
    @State private var usesDateRange = false
    @State private var startDate =
        Calendar.current.date(byAdding: .day, value: -7, to: Date()) ?? Date()
    @State private var endDate = Date()
    @State private var turns: [GlobalHistoryTurn] = []
    @State private var errorMessage: String?
    @State private var isLoading = true

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    Text(OfficeLocalization.string("전체 대화 보관함"))
                        .font(.system(size: 22, weight: .bold))
                    Text(
                        OfficeLocalization.string(
                            "캐릭터와 날짜 범위로 저장된 업무를 조회합니다."
                        )
                    )
                        .font(.system(size: 14))
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

            filterBar
                .padding(14)

            Divider()

            Group {
                if isLoading {
                    ProgressView(OfficeLocalization.string("전체 대화를 불러오는 중"))
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else if let errorMessage {
                    ContentUnavailableView(
                        OfficeLocalization.string("대화 내역을 불러오지 못했습니다"),
                        systemImage: "exclamationmark.triangle",
                        description: Text(OfficeLocalization.systemMessage(errorMessage))
                    )
                } else if turns.isEmpty {
                    ContentUnavailableView(
                        OfficeLocalization.string("조건에 맞는 대화가 없습니다"),
                        systemImage: "archivebox",
                        description: Text(
                            OfficeLocalization.string("캐릭터 또는 날짜 범위를 바꿔보세요.")
                        )
                    )
                } else {
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 16) {
                            ForEach(dayGroups) { group in
                                VStack(alignment: .leading, spacing: 9) {
                                    Text(
                                        OfficeLocalization.date(group.date,
                                            dateStyle: .complete,
                                            time: .omitted
                                        )
                                    )
                                    .font(.system(size: 16, weight: .bold))

                                    ForEach(group.turns) { turn in
                                        ArchiveTurnCard(turn: turn)
                                    }
                                }
                            }
                        }
                        .padding(18)
                    }
                }
            }
        }
        .font(.system(size: 14))
        .frame(
            minWidth: 1_080,
            idealWidth: 1_120,
            minHeight: 760,
            idealHeight: 780
        )
        .task {
            await load()
        }
    }

    private var filterBar: some View {
        HStack(spacing: 12) {
            Picker(OfficeLocalization.string("캐릭터"), selection: $selectedCharacter) {
                Text(OfficeLocalization.string("전체 캐릭터"))
                    .tag(nil as OfficeCharacter?)
                ForEach(director.characters) { character in
                    Text(director.displayName(for: character.id))
                        .tag(character.id as OfficeCharacter?)
                }
            }
            .frame(width: 210)

            Toggle(OfficeLocalization.string("날짜 지정"), isOn: $usesDateRange)
                .toggleStyle(.checkbox)

            DatePicker(
                OfficeLocalization.string("시작"),
                selection: $startDate,
                displayedComponents: .date
            )
            .disabled(!usesDateRange)

            DatePicker(
                OfficeLocalization.string("종료"),
                selection: $endDate,
                in: startDate...,
                displayedComponents: .date
            )
            .disabled(!usesDateRange)

            Button(OfficeLocalization.string("조회")) {
                Task {
                    await load()
                }
            }
            .keyboardShortcut(.defaultAction)
        }
        .controlSize(.regular)
    }

    private var dayGroups: [HistoryDayGroup] {
        let grouped = Dictionary(grouping: turns) {
            Calendar.current.startOfDay(for: $0.startedAt)
        }
        return grouped.map {
            HistoryDayGroup(date: $0.key, turns: $0.value)
        }
        .sorted { $0.date > $1.date }
    }

    private func load() async {
        isLoading = true
        errorMessage = nil
        let calendar = Calendar.current
        let from = usesDateRange
            ? calendar.startOfDay(for: startDate)
            : nil
        let to = usesDateRange
            ? calendar.date(
                byAdding: .day,
                value: 1,
                to: calendar.startOfDay(for: endDate)
            )?.addingTimeInterval(-0.001)
            : nil

        do {
            turns = try await director.globalHistory(
                character: selectedCharacter,
                from: from,
                to: to
            )
        } catch {
            errorMessage = error.localizedDescription
        }
        isLoading = false
    }
}

private struct HistoryDayGroup: Identifiable {
    let date: Date
    let turns: [GlobalHistoryTurn]

    var id: Date {
        date
    }
}

private struct ArchiveTurnCard: View {
    let turn: GlobalHistoryTurn

    var body: some View {
        DisclosureGroup {
            VStack(alignment: .leading, spacing: 10) {
                taskPromptBlock
                transcriptBlock(title: OfficeLocalization.string("응답"), text: turn.response)
                if let warning = turn.responseSourceWarning, !warning.isEmpty {
                    ResponseSourceWarningView(message: warning)
                }
                if let warning = turn.wikiProposalWarning, !warning.isEmpty {
                    ResponseSourceWarningView(
                        message: warning,
                        accessibilityIdentifier: "wikiProposalWarning"
                    )
                }
                if !turn.responseSources.isEmpty {
                    ResponseSourceList(
                        sources: turn.responseSources,
                        workspaceDirectory: turn.conversationWorkdir ?? ""
                    )
                }
                if let sessionID = turn.externalSessionId {
                    VStack(alignment: .leading, spacing: 3) {
                        Text(OfficeLocalization.string("세션 ID"))
                            .font(.system(size: 13, weight: .bold))
                            .foregroundStyle(.secondary)
                        Text(sessionID)
                            .font(.system(size: 13, design: .monospaced))
                    }
                }
            }
            .padding(.top, 8)
        } label: {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        Text(turn.characterName)
                            .font(.system(size: 15, weight: .bold))
                        Text(
                            agentExecutionSummary(
                                backend: turn.executionBackend,
                                model: turn.executionModel,
                                effort: turn.executionEffort,
                                fastMode: turn.executionFastMode
                            )
                        )
                            .font(.system(size: 12, weight: .medium))
                            .foregroundStyle(.secondary)
                    }
                    EmployeeMessageSenderLabel(sender: EmployeeMessageSender.resolve(turn.promptSender, prompt: turn.prompt))
                    Text(promptPresentation.text)
                        .font(.system(size: 14, weight: .medium))
                        .lineLimit(1)
                    TaskPromptAttachmentSummary(
                        attachments: promptPresentation.attachments
                    )
                }
                Spacer()
                Text(
                    OfficeLocalization.date(turn.startedAt,
                        dateStyle: .omitted,
                        time: .shortened
                    )
                )
                .font(.system(size: 13))
                .foregroundStyle(.secondary)
            }
        }
        .padding(12)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(Color.primary.opacity(0.045))
        )
        .conversationTextSelectionRegion("archive-turn-\(turn.id)")
    }

    private var promptPresentation: TaskPromptPresentation {
        TaskPromptPresentation(prompt: turn.prompt)
    }

    private var taskPromptBlock: some View {
        VStack(alignment: .leading, spacing: 5) {
            EmployeeMessageSenderLabel(sender: EmployeeMessageSender.resolve(turn.promptSender, prompt: turn.prompt))
            Text(OfficeLocalization.string("업무"))
                .font(.system(size: 13, weight: .bold))
                .foregroundStyle(.secondary)
            if !promptPresentation.text.isEmpty {
                ConversationMarkdownView(source: promptPresentation.text)
            }
            if !promptPresentation.attachments.isEmpty {
                TaskPromptAttachmentList(
                    attachments: promptPresentation.attachments
                )
            }
        }
    }

    private func transcriptBlock(
        title: String,
        text: String
    ) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title)
                .font(.system(size: 13, weight: .bold))
                .foregroundStyle(.secondary)
            ConversationMarkdownView(
                source: text,
                compactsChangedFileLists: true
            )
        }
    }
}

private func agentExecutionSummary(
    backend: AgentBackend?,
    model: String?,
    effort: String?,
    fastMode: Bool?
) -> String {
    guard let backend else {
        return OfficeLocalization.format(
            "실행 정보 기록 없음 · %@",
            agentExecutionModeTitle(fastMode)
        )
    }

    var parts = [backend.title]
    if
        let model = model?.trimmingCharacters(in: .whitespacesAndNewlines),
        !model.isEmpty
    {
        parts.append(backend.modelTitle(model))
    }
    if
        let effort = effort?.trimmingCharacters(in: .whitespacesAndNewlines),
        !effort.isEmpty
    {
        parts.append(OfficeLocalization.format("추론 %@", effort))
    }
    parts.append(agentExecutionModeTitle(fastMode))
    return parts.joined(separator: " · ")
}
