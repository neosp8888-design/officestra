// 이 파일은 대화 기록을 간결한 타일과 좌우 페이지형 상세 화면으로 표시한다.

import AppKit
import OfficeCore
import SwiftUI

struct ArchiveRecordGrid: View {
    let turns: [LiveFeedTurn]
    let onSelect: (LiveFeedTurn) -> Void

    var body: some View {
        LazyVGrid(
            columns: [
                GridItem(
                    .adaptive(minimum: 220, maximum: 360),
                    spacing: 8
                ),
            ],
            alignment: .leading,
            spacing: 8
        ) {
            ForEach(turns) { turn in
                ArchiveRecordTile(turn: turn) {
                    onSelect(turn)
                }
            }
        }
    }
}

private struct ArchiveRecordTile: View {
    let turn: LiveFeedTurn
    let onSelect: () -> Void

    var body: some View {
        let characterName = turn.characterName
        Button(action: onSelect) {
            HStack(spacing: 10) {
                RoundedRectangle(cornerRadius: 2, style: .continuous)
                    .fill(bookColor)
                    .frame(width: 4)

                VStack(alignment: .leading, spacing: 5) {
                    HStack(spacing: 6) {
                        Text(String(characterName.prefix(1)))
                            .font(
                                .system(
                                    size: 10,
                                    weight: .black,
                                    design: .rounded
                                )
                            )
                            .foregroundStyle(bookColor)
                            .frame(width: 22, height: 22)
                            .background(
                                bookColor.opacity(0.11),
                                in: Circle()
                            )

                        Text(characterName)
                            .font(.system(size: 10, weight: .bold))

                        if turn.status.isRunning {
                            Text(OfficeLocalization.string("업무 중"))
                                .foregroundStyle(Color.green)
                        } else if turn.needsInput {
                            Text(OfficeLocalization.string("답변 필요"))
                                .foregroundStyle(Color.orange)
                        }

                        Spacer()

                        Text(
                            OfficeLocalization.date(turn.startedAt,
                                dateStyle: .omitted,
                                time: .shortened
                            )
                        )
                        .foregroundStyle(.tertiary)
                    }
                    .font(.system(size: 8.5, weight: .bold))

                    EmployeeMessageSenderLabel(sender: EmployeeMessageSender.resolve(turn.promptSender, prompt: turn.prompt))

                    Text(recordTitle)
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Color.primary.opacity(0.82))
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                        .frame(
                            maxWidth: .infinity,
                            alignment: .leading
                        )

                    TaskPromptAttachmentSummary(
                        attachments: promptPresentation.attachments
                    )

                    Text(executionSummary)
                        .font(.system(size: 8.5, weight: .medium))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }

                Image(systemName: "chevron.right")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(.tertiary)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 9)
            .frame(maxWidth: .infinity, minHeight: 82)
            .background(
                Color(nsColor: .controlBackgroundColor).opacity(0.60),
                in: RoundedRectangle(
                    cornerRadius: 10,
                    style: .continuous
                )
            )
            .overlay {
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .stroke(Color.primary.opacity(0.06))
            }
        }
        .buttonStyle(.plain)
        .help("\(characterName) · \(recordTitle)")
        .accessibilityLabel(recordAccessibilityLabel)
        .conversationTextSelectionRegion("archive-tile-\(turn.id)")
    }

    private var bookColor: Color {
        // 대화 화면과 같은 백엔드 색을 쓴다. 코덱스 녹색, 안티그래비티 파랑,
        // Claude 주황이 목록과 펼친 기록에서 일치한다.
        DashboardPalette.providerAccent(
            for: turn.backend ?? turn.characterBackend
        )
    }

    private var recordTitle: String {
        let title = promptPresentation.text.trimmingCharacters(
            in: .whitespacesAndNewlines
        )
        return title.isEmpty ? OfficeLocalization.string("제목 없는 업무") : title
    }

    private var promptPresentation: TaskPromptPresentation {
        TaskPromptPresentation(prompt: turn.prompt)
    }

    private var recordAccessibilityLabel: String {
        let attachmentDetails = promptPresentation.attachments.map {
            OfficeLocalization.format("첨부 파일 %@, 경로 %@", $0.name, $0.path)
        }
        return ([
            OfficeLocalization.format("%@ 기록 · %@", turn.characterName, recordTitle),
            executionSummary,
        ]
            + attachmentDetails)
            .joined(separator: ", ")
    }

    private var executionSummary: String {
        guard let backend = turn.backend else {
            return OfficeLocalization.format("이전 기록 · %@", agentExecutionModeTitle(turn.fastMode))
        }

        var parts = [backend.title]
        if let model = turn.model {
            parts.append(backend.modelTitle(model))
        }
        if let effort = turn.effort {
            parts.append(OfficeLocalization.format("추론 %@", effort))
        }
        parts.append(agentExecutionModeTitle(turn.fastMode))
        return parts.joined(separator: " · ")
    }
}

enum ArchiveBookSheetLayout {
    // SwiftUI 시트는 idealWidth보다 minWidth를 먼저 채택할 수 있다.
    // 이전 1080pt에서 좌우 40pt씩 넓힌 실제 최소 폭도 함께 지정한다.
    static let minimumWidth: CGFloat = 1_160
    static let idealWidth: CGFloat = 1_160
    static let minimumHeight: CGFloat = 720
    static let idealHeight: CGFloat = 760
}

/// 시트 안에서 이전·다음 기록으로 넘길 수 있는지 정한다. 목록은 12건씩
/// 불러오므로 마지막 칸이어도 더 받아 올 기록이 남았으면 다음으로 갈 수 있다.
enum ArchiveBookPaging {
    static func canGoPrevious(index: Int) -> Bool {
        index > 0
    }

    static func canGoNext(
        index: Int,
        loadedCount: Int,
        totalCount: Int
    ) -> Bool {
        index + 1 < loadedCount || loadedCount < totalCount
    }
}

/// 시트 툴바에 보여 줄 현재 위치와 넘기기 가능 여부.
struct ArchiveBookNavigation {
    let index: Int
    let total: Int
    let canGoPrevious: Bool
    let canGoNext: Bool
}

struct ArchiveOpenBook: View {
    let turn: LiveFeedTurn
    let navigation: ArchiveBookNavigation
    let onPrevious: () -> Void
    let onNext: () -> Void
    let onClose: () -> Void
    @State private var copiedKey: String?

    var body: some View {
        VStack(spacing: 0) {
            bookToolbar

            GeometryReader { geometry in
                let pageWidth = max(
                    0,
                    (geometry.size.width - 30) / 2
                )

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
        // 모달에는 기록 하나만 보이므로 선택을 호버에 따라 켜고 끌 필요가 없다.
        // 공용 region의 조건 분기를 쓰면 커서 출입마다 두 ScrollView와
        // 본문·표·코드 뷰가 재생성된다. 선택을 유지해 본문과 스크롤 위치를 보존한다.
        .textSelection(.enabled)
    }

    private var bookToolbar: some View {
        HStack(spacing: 9) {
            CharacterBadge(
                name: turn.characterName,
                characterID: turn.characterId,
                size: 25
            )

            VStack(alignment: .leading, spacing: 1) {
                Text(turn.characterName)
                    .font(.system(size: 11, weight: .bold))
                Text(
                    OfficeLocalization.date(turn.startedAt,
                        dateStyle: .abbreviated,
                        time: .shortened
                    )
                )
                .font(.system(size: 8.5, weight: .medium))
                .foregroundStyle(.secondary)
            }

            Spacer()

            Label(OfficeLocalization.string(statusTitle), systemImage: "bookmark.fill")
                .font(.system(size: 9, weight: .bold))
                .foregroundStyle(bookColor)

            HStack(spacing: 2) {
                pagingButton(
                    systemImage: "chevron.left",
                    label: "이전 기록 보기",
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
                    label: "다음 기록 보기",
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
            .buttonStyle(.plain)
            .keyboardShortcut(.cancelAction)
            .accessibilityLabel(OfficeLocalization.string("닫기"))
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
        .buttonStyle(.plain)
        .disabled(!isEnabled)
        .opacity(isEnabled ? 1 : 0.35)
        .keyboardShortcut(shortcut, modifiers: [])
        .accessibilityLabel(OfficeLocalization.string(label))
        .help(OfficeLocalization.string(label))
    }

    private var leftPage: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                pageHeading("장서 정보", systemImage: "info.circle.fill")

                VStack(spacing: 7) {
                    metadataRow(
                        label: "세션 ID",
                        value: turn.externalSessionId ?? OfficeLocalization.string("기록 없음"),
                        monospaced: true,
                        copyKey: "session",
                        copyText: turn.externalSessionId
                    )
                    metadataRow(
                        label: "모델",
                        value: modelTitle
                    )
                    metadataRow(
                        label: "추론",
                        value: turn.effort ?? OfficeLocalization.string("기록 없음")
                    )
                    metadataRow(
                        label: "모드",
                        value: agentExecutionModeTitle(turn.fastMode)
                    )
                }
                .padding(9)
                .background(
                    Color.primary.opacity(0.035),
                    in: RoundedRectangle(cornerRadius: 9)
                )

                pageHeading("업무", systemImage: "text.quote")

                EmployeeMessageSenderLabel(sender: EmployeeMessageSender.resolve(turn.promptSender, prompt: turn.prompt))

                if !promptPresentation.text.isEmpty {
                    Text(promptPresentation.text)
                        .font(.system(size: 11, weight: .semibold))
                        .lineSpacing(2)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }

                if !promptPresentation.attachments.isEmpty {
                    TaskPromptAttachmentList(
                        attachments: promptPresentation.attachments
                    )
                }
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
    }

    private var rightPage: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 9) {
                HStack {
                    pageHeading(
                        turn.response.isEmpty ? "기록" : "응답",
                        systemImage:
                            turn.response.isEmpty
                            ? "ellipsis.bubble"
                            : "checkmark.bubble.fill"
                    )

                    Spacer()

                    if !turn.response.isEmpty {
                        copyButton(
                            key: "response",
                            text: turn.response,
                            label: "응답 복사"
                        )
                    }
                }

                if !turn.response.isEmpty {
                    ConversationMarkdownView(
                        source: turn.response,
                        fontSize: 14
                    )
                } else if let error = turn.errorMessage {
                    Text(OfficeLocalization.systemMessage(error))
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(Color.red.opacity(0.86))
                } else {
                    Text(OfficeLocalization.string("업무가 진행 중입니다."))
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(.secondary)
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

                if !turn.responseSources.isEmpty {
                    ResponseSourceList(
                        sources: turn.responseSources,
                        workspaceDirectory: archiveWorkspaceDirectory
                    )
                }
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
        .shadow(color: .black.opacity(0.08), radius: 6, x: 2, y: 3)
    }

    private var promptPresentation: TaskPromptPresentation {
        TaskPromptPresentation(prompt: turn.prompt)
    }

    private var archiveWorkspaceDirectory: String {
        turn.workspace?.fileBaseDirectory(
            fallback: turn.conversationWorkdir ?? ""
        )
            ?? turn.conversationWorkdir
            ?? ""
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

    private var modelTitle: String {
        guard let backend = turn.backend, let model = turn.model else {
            return OfficeLocalization.string("기록 없음")
        }
        return "\(backend.title) · \(backend.modelTitle(model))"
    }

    private func metadataRow(
        label: String,
        value: String,
        monospaced: Bool = false,
        copyKey: String? = nil,
        copyText: String? = nil
    ) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 7) {
            Text(OfficeLocalization.string(label))
                .font(.system(size: 8.5, weight: .bold))
                .foregroundStyle(.secondary)
                .frame(width: OfficeLocalization.usesKorean ? 42 : 66, alignment: .leading)

            Text(value)
                .font(
                    monospaced
                        ? .system(size: 8.5, design: .monospaced)
                        : .system(size: 9.5, weight: .medium)
                )
                .lineLimit(monospaced ? 1 : 2)
                .truncationMode(.middle)

            Spacer(minLength: 3)

            if let copyKey, let copyText {
                copyButton(
                    key: copyKey,
                    text: copyText,
                    label: OfficeLocalization.format("%@ 복사", OfficeLocalization.string(label)),
                    compact: true
                )
            }
        }
    }

    private func copyButton(
        key: String,
        text: String,
        label: String,
        compact: Bool = false
    ) -> some View {
        Button {
            copyToPasteboard(text, key: key)
        } label: {
            Group {
                if compact {
                    Image(
                        systemName:
                            copiedKey == key
                            ? "checkmark"
                            : "doc.on.doc"
                    )
                } else {
                    Label(
                        copiedKey == key
                            ? OfficeLocalization.string("복사됨")
                            : OfficeLocalization.string("복사"),
                        systemImage:
                            copiedKey == key
                            ? "checkmark"
                            : "doc.on.doc"
                    )
                }
            }
            .font(.system(size: 8.5, weight: .bold))
            .padding(.horizontal, compact ? 4 : 7)
            .frame(height: 22)
            .background(
                Color.primary.opacity(0.05),
                in: Capsule()
            )
        }
        .buttonStyle(.plain)
        .accessibilityLabel(OfficeLocalization.string(label))
        .help(OfficeLocalization.string(label))
    }

    private func copyToPasteboard(_ text: String, key: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        copiedKey = key

        Task {
            try? await Task.sleep(for: .seconds(1.3))
            if !Task.isCancelled, copiedKey == key {
                copiedKey = nil
            }
        }
    }

    private var statusTitle: String {
        switch turn.status {
        case .pending:
            "대기 중"
        case .running:
            "업무 중"
        case .completed:
            turn.needsInput ? "답변 필요" : "완료"
        case .failed:
            "중단"
        case .interrupted:
            "연결 종료"
        }
    }

    private var bookColor: Color {
        // 대화 화면과 같은 백엔드 색을 쓴다. 코덱스 녹색, 안티그래비티 파랑,
        // Claude 주황이 목록과 펼친 기록에서 일치한다.
        DashboardPalette.providerAccent(
            for: turn.backend ?? turn.characterBackend
        )
    }
}
