// 이 파일은 흔들림 없는 다중 행 명령 입력과 전송 키 동작을 제공한다.

import AppKit
import OfficeCore
import SwiftUI

struct CommandEntryDraft: Equatable {
    var text = ""

    func submissionPrompt(
        hasAttachments: Bool,
        isSubmissionAllowed: Bool
    ) -> String? {
        guard isSubmissionAllowed else {
            return nil
        }
        let enteredPrompt = text.trimmingCharacters(
            in: .whitespacesAndNewlines
        )
        guard !enteredPrompt.isEmpty || hasAttachments else {
            return nil
        }
        return enteredPrompt.isEmpty
            ? OfficeLocalization.string("첨부 파일을 확인해줘.")
            : enteredPrompt
    }

    mutating func clearAfterSubmission(accepted: Bool) {
        if accepted {
            text = ""
        }
    }
}

struct CommandEntryAvailability: Equatable {
    let isReady: Bool
    let isUpdatingConfiguration: Bool
    let hasSelectedCharacter: Bool
    let isSelectedCharacterRunning: Bool
    let isSelectedCharacterCompacting: Bool
    let canQueueForSelectedCharacter: Bool

    init(
        isReady: Bool,
        isUpdatingConfiguration: Bool,
        hasSelectedCharacter: Bool,
        isSelectedCharacterRunning: Bool,
        isSelectedCharacterCompacting: Bool = false,
        canQueueForSelectedCharacter: Bool
    ) {
        self.isReady = isReady
        self.isUpdatingConfiguration = isUpdatingConfiguration
        self.hasSelectedCharacter = hasSelectedCharacter
        self.isSelectedCharacterRunning = isSelectedCharacterRunning
        self.isSelectedCharacterCompacting = isSelectedCharacterCompacting
        self.canQueueForSelectedCharacter = canQueueForSelectedCharacter
    }

    var canSubmit: Bool {
        isReady
            && !isUpdatingConfiguration
            && hasSelectedCharacter
            && !isSelectedCharacterRunning
            && !isSelectedCharacterCompacting
    }

    /// 응답 생성 중에는 같은 입력이 다음 턴 예약으로 넘어간다.
    var canQueue: Bool {
        isReady
            && !isUpdatingConfiguration
            && hasSelectedCharacter
            && isSelectedCharacterRunning
            && !isSelectedCharacterCompacting
            && canQueueForSelectedCharacter
    }

    var acceptsInput: Bool {
        canSubmit || canQueue
    }

    func canChooseAttachments(currentCount: Int) -> Bool {
        acceptsInput && currentCount < 20
    }
}

enum SelectedCharacterQueueAvailability: Equatable {
    case unavailable
    case available
    case full

    static func resolve(
        isReady: Bool,
        isUpdatingConfiguration: Bool,
        hasSelectedCharacter: Bool,
        isSelectedCharacterRunning: Bool,
        isFull: Bool
    ) -> Self {
        guard
            isReady,
            !isUpdatingConfiguration,
            hasSelectedCharacter,
            isSelectedCharacterRunning
        else {
            return .unavailable
        }
        return isFull ? .full : .available
    }
}

enum CommandComposerLayout {
    static let minimumHeight: CGFloat = 40
    static let maximumHeight: CGFloat = 160

    static func measuredHeight(for textView: NSTextView) -> CGFloat {
        guard
            !textView.string.isEmpty,
            let layoutManager = textView.layoutManager,
            let textContainer = textView.textContainer
        else {
            return minimumHeight
        }

        layoutManager.ensureLayout(for: textContainer)
        let glyphRange = layoutManager.glyphRange(for: textContainer)
        var lineCount = 0
        var lineHeight: CGFloat = 0
        layoutManager.enumerateLineFragments(
            forGlyphRange: glyphRange
        ) { lineRect, _, _, _, _ in
            lineCount += 1
            lineHeight = max(lineHeight, ceil(lineRect.height))
        }
        if textView.string.hasSuffix("\n") {
            lineCount += 1
            lineHeight = max(
                lineHeight,
                ceil(layoutManager.extraLineFragmentRect.height)
            )
        }
        let additionalLineHeight = CGFloat(max(0, lineCount - 1))
            * lineHeight
        return min(
            minimumHeight + additionalLineHeight,
            maximumHeight
        )
    }
}

struct CommandEntryRow: View {
    @ObservedObject var director: AgentDirector
    @ObservedObject private var composerStore: EmployeeDraftStore
    let placeholder: String
    let attachmentCount: Int
    let isPreparingAttachments: Bool
    /// 터미널 모드는 글만 CLI로 넘기므로 첨부 버튼을 두지 않는다.
    var supportsAttachments = true
    let onChooseAttachments: () -> Void
    let onSubmit: (String) -> Bool

    @State private var composerHeight = CommandComposerLayout.minimumHeight
    @State private var isSubmittingMentions = false

    init(director: AgentDirector, placeholder: String, attachmentCount: Int,
         isPreparingAttachments: Bool, supportsAttachments: Bool = true,
         onChooseAttachments: @escaping () -> Void, onSubmit: @escaping (String) -> Bool) {
        self.director = director
        self.placeholder = placeholder
        self.attachmentCount = attachmentCount
        self.isPreparingAttachments = isPreparingAttachments
        self.supportsAttachments = supportsAttachments
        self.onChooseAttachments = onChooseAttachments
        self.onSubmit = onSubmit
        _composerStore = ObservedObject(wrappedValue: director.employeeComposerStore.draftStore)
    }

    private var draft: CommandEntryDraft {
        get { composerStore.drafts[director.selectedCharacterID ?? .boss] ?? CommandEntryDraft() }
        nonmutating set { composerStore.drafts[director.selectedCharacterID ?? .boss] = newValue }
    }

    private var availability: CommandEntryAvailability {
        CommandEntryAvailability(
            isReady: director.isReadyForSubmissions,
            isUpdatingConfiguration: director.isUpdatingConfiguration,
            hasSelectedCharacter: director.selectedCharacter != nil,
            isSelectedCharacterRunning: director.isSelectedCharacterRunning,
            isSelectedCharacterCompacting:
                director.isSelectedCharacterCompacting,
            canQueueForSelectedCharacter:
                director.canQueueForSelectedCharacter
        )
    }

    private var submissionPrompt: String? {
        draft.submissionPrompt(
            hasAttachments: attachmentCount > 0,
            isSubmissionAllowed:
                availability.acceptsInput && !isPreparingAttachments && !isSubmittingMentions
                    && !director.savingReplyRoutes.contains(director.selectedCharacterID ?? .boss)
        )
    }

    private var attachmentSelectionIsDisabled: Bool {
        isPreparingAttachments
            || !availability.canChooseAttachments(
                currentCount: attachmentCount
            )
    }

    private var queueHelp: String {
        switch director.selectedCharacterQueueAvailability {
        case .available:
            return OfficeLocalization.format(
                "지금 응답이 끝나면 이어서 보냅니다 · 최대 %d개",
                QueuedCommandQueue.maximumCount
            )
        case .full:
            return OfficeLocalization.format(
                "예약이 가득 찼습니다 · 최대 %d개",
                QueuedCommandQueue.maximumCount
            )
        case .unavailable:
            return OfficeLocalization.string(
                "현재는 다음 업무를 예약할 수 없습니다"
            )
        }
    }

    var body: some View {
        let canSubmit = submissionPrompt != nil
        let inputCharacter = director.selectedCharacterID ?? .boss

        VStack(alignment: .leading, spacing: 7) {
        HStack(spacing: 9) {
            if supportsAttachments {
                Button(action: onChooseAttachments) {
                    Group {
                        if isPreparingAttachments {
                            ProgressView()
                                .controlSize(.small)
                        } else {
                            Image(systemName: "paperclip")
                                .font(.system(size: 15, weight: .semibold))
                                .foregroundStyle(.secondary)
                        }
                    }
                    .frame(width: 32, height: 32)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(
                    isPreparingAttachments
                        ? OfficeLocalization.string("첨부 준비 중")
                        : OfficeLocalization.string("파일 첨부")
                )
                .help(OfficeLocalization.string("파일 첨부 · 한 번에 최대 20개"))
                .disabled(attachmentSelectionIsDisabled)
                .opacity(attachmentSelectionIsDisabled ? 0.42 : 1)
            }

            CommandComposerView(
                text: Binding(
                    get: { composerStore.drafts[inputCharacter]?.text ?? "" },
                    set: { composerStore.drafts[inputCharacter, default: CommandEntryDraft()].text = $0 }
                ),
                measuredHeight: $composerHeight,
                placeholder: placeholder,
                isEnabled:
                    director.isReadyForSubmissions
                        && !director.isUpdatingConfiguration,
                mentions: director.characters.filter { $0.id != director.selectedCharacterID }.map {
                    EmployeeMention(id: $0.id, name: director.displayName(for: $0.id))
                },
                onMention: { recipient in
                    guard let source = director.selectedCharacterID, source == inputCharacter else { return }
                    director.setReplyRecipient(recipient, selected: true, for: source)
                },
                focusOnMount: supportsAttachments && director.employeeComposerStore.focusesComposerOnSelection,
                onSubmit: {
                    guard director.selectedCharacterID == inputCharacter else { return false }
                    return submitDraft()
                }
            )
            .id(director.selectedCharacterID)
            .frame(height: composerHeight)

            if director.isSelectedCharacterRunning {
                Button(action: director.cancelSelectedJob) {
                    Image(systemName: "stop.fill")
                        .font(.system(size: 13, weight: .bold))
                        .foregroundStyle(.white)
                        .frame(width: 36, height: 36)
                        .background(
                            Color.red.opacity(0.88),
                            in: RoundedRectangle(
                                cornerRadius: 11,
                                style: .continuous
                            )
                        )
                }
                .buttonStyle(.plain)
                .accessibilityLabel(OfficeLocalization.string("대화 중단"))
                .help(OfficeLocalization.string("현재 직원의 업무 중단"))
                .disabled(director.isCancellingSelectedCharacter)
                .opacity(
                    director.isCancellingSelectedCharacter ? 0.42 : 1
                )

                Button(action: { _ = submitDraft() }) {
                    Image(systemName: "clock.badge.checkmark.fill")
                        .font(.system(size: 13, weight: .bold))
                        .foregroundStyle(.white)
                        .frame(width: 36, height: 36)
                        .background(
                            DashboardPalette.accent.opacity(0.82),
                            in: RoundedRectangle(
                                cornerRadius: 11,
                                style: .continuous
                            )
                        )
                }
                .buttonStyle(.plain)
                .accessibilityLabel(OfficeLocalization.string("다음 턴에 예약"))
                .help(queueHelp)
                .disabled(!canSubmit)
                .opacity(canSubmit ? 1 : 0.42)
            } else {
                Button(action: { _ = submitDraft() }) {
                    Image(systemName: "paperplane.fill")
                        .font(.system(size: 14, weight: .bold))
                        .foregroundStyle(.white)
                        .frame(width: 36, height: 36)
                        .background(
                            DashboardPalette.accent,
                            in: RoundedRectangle(
                                cornerRadius: 11,
                                style: .continuous
                            )
                        )
                }
                .buttonStyle(.plain)
                .accessibilityLabel(OfficeLocalization.string("보내기"))
                .disabled(!canSubmit)
                .opacity(canSubmit ? 1 : 0.42)
            }
        }
        // 양끝이 같은 32pt 버튼이라 좌우 여백을 맞춘다.
        .padding(.leading, 7)
        .padding(.trailing, 7)
        .padding(.vertical, 6)
        .background(
            Color.primary.opacity(0.045),
            in: RoundedRectangle(cornerRadius: 14, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .stroke(Color.primary.opacity(0.07))
        }
        }
    }

    @discardableResult
    private func submitDraft() -> Bool {
        guard let submissionPrompt else {
            return false
        }
        if let source = director.selectedCharacterID {
            let candidates = director.characters.filter { $0.id != source }.map {
                EmployeeMention(id: $0.id, name: director.displayName(for: $0.id))
            }
            let recipients = EmployeeMention.recipients(in: submissionPrompt, candidates: candidates)
            let additions = recipients.subtracting(director.replyRecipients[source] ?? [])
            if !additions.isEmpty {
                let originalDraft = draft.text
                isSubmittingMentions = true
                for recipient in additions { director.setReplyRecipient(recipient, selected: true, for: source) }
                Task { @MainActor in
                    let saved = await director.waitForReplyRouteSave(for: source)
                    isSubmittingMentions = false
                    guard saved, director.selectedCharacterID == source, draft.text == originalDraft,
                          (director.replyRecipients[source] ?? []).isSuperset(of: recipients) else { return }
                    draft.clearAfterSubmission(accepted: onSubmit(submissionPrompt))
                }
                return false
            }
        }
        let accepted = onSubmit(submissionPrompt)
        draft.clearAfterSubmission(accepted: accepted)
        return accepted
    }
}

struct ReplyDeliveryStatusView: View {
    @ObservedObject var store: LiveFeedStore
    let character: OfficeCharacter
    let onCancel: (String) -> Void

    private var displayedTurn: LiveFeedTurn? {
        let turns = store.turns(for: character.rawValue).filter { !deliveries(for: $0).isEmpty }
        return turns.first { deliveries(for: $0).contains { ["pending", "sending", "uncertain"].contains($0.status) } }
            ?? turns.first
    }

    private func deliveries(for turn: LiveFeedTurn) -> [ReplyDelivery] {
        turn.replyDeliveries ?? turn.replyDelivery.map { [$0] } ?? []
    }

    var body: some View {
        if let turn = displayedTurn {
          ForEach(deliveries(for: turn), id: \.recipientId) { delivery in
            let status = delivery.status == "delivered" ? "전달 완료"
                : delivery.status == "uncertain" ? "전달 확인 필요"
                : delivery.status == "failed" ? "전달 실패"
                : delivery.status == "cancelled" ? "전달 취소"
                : delivery.status == "sending" ? "전달 중"
                : turn.status == .running ? "답변 후 전달" : "전달 대기"
            HStack(spacing: 6) {
            Label("\(OfficeLocalization.string(status)) · @\(delivery.recipientName)",
                  systemImage: delivery.status == "delivered" ? "checkmark.circle" : "paperplane")
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(delivery.status == "failed" ? Color.red : Color.secondary)
                .help(delivery.error ?? OfficeLocalization.string("상대 직원이 바쁘면 자동으로 기다립니다"))
            }
          }
          if deliveries(for: turn).contains(where: { $0.status == "pending" }) {
              Button(OfficeLocalization.string("이 답변의 대기 중인 전달 모두 취소")) { onCancel(turn.id) }
                  .font(.caption)
          }
        }
    }
}

/// 예약된 다음 업무를 보여주고 취소·즉시 적용을 받는다.
struct QueuedCommandStrip: View {
    @ObservedObject var director: AgentDirector
    let character: OfficeCharacter

    private var commands: [QueuedCommand] {
        director.queuedCommands(for: character)
    }

    var body: some View {
        if !commands.isEmpty {
            VStack(alignment: .leading, spacing: 5) {
                Text(
                    OfficeLocalization.format(
                        "다음 턴 예약 %d/%d",
                        commands.count,
                        QueuedCommandQueue.maximumCount
                    )
                )
                .font(.system(size: 10, weight: .bold))
                .foregroundStyle(.secondary)

                ForEach(Array(commands.enumerated()), id: \.element.id) {
                    index, command in
                    chip(index: index, command: command)
                }
            }
        }
    }

    private func chip(
        index: Int,
        command: QueuedCommand
    ) -> some View {
        HStack(spacing: 6) {
            Text("\(index + 1)")
                .font(
                    .system(size: 9.5, weight: .black, design: .rounded)
                )
                .foregroundStyle(DashboardPalette.accent)
                .frame(width: 15, height: 15)
                .background(
                    DashboardPalette.accent.opacity(0.16),
                    in: Circle()
                )

            Text(command.summary)
                .font(.system(size: 11, weight: .medium))
                .lineLimit(1)

            if !command.attachments.isEmpty {
                Label(
                    "\(command.attachments.count)",
                    systemImage: "paperclip"
                )
                .font(.system(size: 9.5, weight: .semibold))
                .foregroundStyle(.secondary)
            }

            Spacer(minLength: 4)

            Button {
                director.applyQueuedCommandNow(
                    id: command.id,
                    for: character
                )
            } label: {
                Image(systemName: "bolt.fill")
                    .font(.system(size: 10.5, weight: .bold))
                    .foregroundStyle(DashboardPalette.accent)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(
                OfficeLocalization.format(
                    "%@ 바로 적용",
                    command.summary
                )
            )
            .help(OfficeLocalization.string("지금 작업을 중단하고 이 예약으로 다시 질문"))

            Button {
                director.cancelQueuedCommand(
                    id: command.id,
                    for: character
                )
            } label: {
                Image(systemName: "xmark.circle.fill")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(
                OfficeLocalization.format(
                    "%@ 예약 취소",
                    command.summary
                )
            )
            .help(OfficeLocalization.string("예약 취소"))
        }
        .padding(.horizontal, 9)
        .frame(height: 27)
        .background(
            DashboardPalette.accent.opacity(0.075),
            in: RoundedRectangle(cornerRadius: 9, style: .continuous)
        )
    }
}

struct CommandComposerView: NSViewRepresentable {
    @Binding var text: String
    @Binding var measuredHeight: CGFloat
    let placeholder: String
    let isEnabled: Bool
    var mentions: [EmployeeMention] = []
    var onMention: ((OfficeCharacter) -> Void)? = nil
    var focusOnMount = false
    let onSubmit: () -> Bool

    func makeCoordinator() -> Coordinator {
        Coordinator(parent: self)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSScrollView()
        scrollView.borderType = .noBorder
        scrollView.drawsBackground = false
        scrollView.hasHorizontalScroller = false
        scrollView.hasVerticalScroller = false
        scrollView.autohidesScrollers = true

        let textView = CommandComposerTextView()
        textView.delegate = context.coordinator
        textView.onMeasuredHeightChange = context.coordinator.updateMeasuredHeight
        textView.drawsBackground = false
        textView.isRichText = false
        textView.importsGraphics = false
        textView.allowsUndo = true
        textView.isHorizontallyResizable = false
        textView.isVerticallyResizable = true
        textView.autoresizingMask = [.width]
        textView.frame = NSRect(
            origin: .zero,
            size: scrollView.contentSize
        )
        textView.minSize = NSSize(
            width: 0,
            height: CommandComposerLayout.minimumHeight
        )
        textView.maxSize = NSSize(
            width: CGFloat.greatestFiniteMagnitude,
            height: CGFloat.greatestFiniteMagnitude
        )
        textView.textContainer?.widthTracksTextView = true
        textView.textContainer?.containerSize = NSSize(
            width: scrollView.contentSize.width,
            height: CGFloat.greatestFiniteMagnitude
        )
        textView.textContainer?.lineFragmentPadding = 0
        textView.textContainerInset = NSSize(width: 0, height: 12)
        textView.font = .systemFont(ofSize: 14, weight: .medium)
        textView.textColor = .labelColor
        textView.insertionPointColor = .labelColor
        textView.isSelectable = true
        textView.isAutomaticQuoteSubstitutionEnabled = false
        textView.isAutomaticDashSubstitutionEnabled = false
        textView.isAutomaticTextReplacementEnabled = false
        textView.setAccessibilityLabel(OfficeLocalization.string("업무 입력"))
        textView.registerForDraggedTypes(textView.registeredDraggedTypes + [EmployeeMention.pasteboardType])
        scrollView.documentView = textView

        updateTextView(textView)
        if focusOnMount {
            DispatchQueue.main.async { [weak textView] in
                guard let textView, textView.isEditable else { return }
                textView.window?.makeFirstResponder(textView)
            }
        }
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        context.coordinator.parent = self
        guard let textView = scrollView.documentView as? CommandComposerTextView
        else {
            return
        }
        let textChanged = updateTextView(textView)
        if textChanged {
            DispatchQueue.main.async { [weak textView] in
                textView?.reportMeasuredHeight()
            }
        }
    }

    @discardableResult
    func updateTextView(_ textView: CommandComposerTextView) -> Bool {
        textView.mentionCandidates = mentions
        textView.onMention = onMention
        let textBinding = _text
        textView.onSubmit = { submittedText in
            if textBinding.wrappedValue != submittedText {
                textBinding.wrappedValue = submittedText
            }
            return onSubmit()
        }
        if textView.placeholder != placeholder {
            textView.placeholder = placeholder
        }
        if textView.isEditable != isEnabled {
            textView.isEditable = isEnabled
        }
        let desiredTextColor: NSColor = isEnabled
            ? .labelColor
            : .disabledControlTextColor
        if textView.textColor != desiredTextColor {
            textView.textColor = desiredTextColor
        }

        // SwiftUI 갱신이 한글 입력기의 조합 문자열보다 먼저 도착할 수
        // 있다. 조합 중 NSTextView.string을 다시 쓰면 marked range가
        // 취소되어 초성이나 마지막 글자가 Backspace처럼 사라진다.
        guard !textView.hasMarkedText(), textView.string != text else {
            return false
        }
        let previousLocation = textView.selectedRange().location
        textView.string = text
        textView.setSelectedRange(
            NSRange(
                location: min(previousLocation, (text as NSString).length),
                length: 0
            )
        )
        textView.needsDisplay = true
        if text.isEmpty {
            textView.scrollToBeginningOfDocument(nil)
        }
        return true
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: CommandComposerView

        init(parent: CommandComposerView) {
            self.parent = parent
        }

        func textDidChange(_ notification: Notification) {
            guard let textView = notification.object as? NSTextView else {
                return
            }
            if let composerTextView = textView as? CommandComposerTextView,
                composerTextView.discardPostSubmissionTextChangeIfNeeded()
            {
                if !parent.text.isEmpty {
                    parent.text = ""
                }
                composerTextView.reportMeasuredHeight()
                return
            }
            (textView as? CommandComposerTextView)?.reportMeasuredHeight()
            (textView as? CommandComposerTextView)?.refreshMentionSuggestions()
            guard parent.text != textView.string else {
                return
            }
            let changesPlaceholderVisibility =
                parent.text.isEmpty != textView.string.isEmpty
            parent.text = textView.string
            if changesPlaceholderVisibility {
                textView.needsDisplay = true
            }
        }

        func updateMeasuredHeight(_ newHeight: CGFloat) {
            guard abs(parent.measuredHeight - newHeight) >= 0.5 else {
                return
            }
            parent.measuredHeight = newHeight
        }

        func textViewDidChangeSelection(_ notification: Notification) {
            (notification.object as? CommandComposerTextView)?.refreshMentionSuggestions()
        }
    }
}

final class CommandComposerTextView: NSTextView {
    var placeholder = "" {
        didSet {
            if placeholder != oldValue {
                needsDisplay = true
            }
        }
    }
    var onSubmit: ((String) -> Bool)?
    var onMeasuredHeightChange: ((CGFloat) -> Void)?
    var mentionCandidates: [EmployeeMention] = []
    var onMention: ((OfficeCharacter) -> Void)?
    var mentionPopover: NSPopover?
    var mentionMatches: [EmployeeMention] = []
    var mentionRange: NSRange?
    var mentionIndex = 0
    private var discardsTextChangesUntilNextUserEdit = false

    func reportMeasuredHeight() {
        onMeasuredHeightChange?(
            CommandComposerLayout.measuredHeight(for: self)
        )
    }

    override func keyDown(with event: NSEvent) {
        if handleMentionKey(event) { return }
        let isReturn = event.keyCode == 36 || event.keyCode == 76
        guard isReturn else {
            beginNextUserEdit()
            super.keyDown(with: event)
            return
        }

        // Return을 AppKit에 넘기면 한글 조합 중에는 줄바꿈 명령까지
        // 함께 처리될 수 있다. 조합을 먼저 확정한 뒤 일반 Return은
        // 전송하고, Shift+Return만 명시적으로 줄바꿈한다.
        if event.modifierFlags.contains(.shift) {
            beginNextUserEdit()
            if hasMarkedText() {
                unmarkText()
            }
            insertNewline(nil)
            return
        }

        guard hasMarkedText() else {
            if onSubmit?(string) == true {
                completeAcceptedSubmission()
            }
            return
        }

        // unmarkText 직후에는 macOS 입력기가 마지막 한글 음절의
        // textDidChange를 뒤늦게 보낼 수 있다. 같은 호출 스택에서 초안을
        // 지우면 그 이벤트가 빈 초안 위에 마지막 음절 하나를 되살린다.
        // 조합 완료 이벤트를 먼저 소진한 다음 확정 문자열을 전송한다.
        unmarkText()
        DispatchQueue.main.async { [weak self] in
            guard let self else {
                return
            }
            if self.onSubmit?(self.string) == true {
                self.completeAcceptedSubmission()
            }
        }
    }

    override func readSelection(
        from pasteboard: NSPasteboard,
        type: NSPasteboard.PasteboardType
    ) -> Bool {
        // 우클릭 메뉴 붙여넣기는 keyDown을 거치지 않는다. 전송 직후의
        // IME 잔여 변경 차단을 먼저 해제하지 않으면 붙인 문장 전체를
        // 이전 조합의 늦은 변경으로 오인해 즉시 지우게 된다.
        beginNextUserEdit()
        return super.readSelection(from: pasteboard, type: type)
    }

    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation {
        if sender.draggingPasteboard.types?.contains(EmployeeMention.pasteboardType) == true {
            return isEditable && droppedMention(from: sender.draggingPasteboard) != nil ? .copy : []
        }
        return super.draggingEntered(sender)
    }

    override func draggingUpdated(_ sender: NSDraggingInfo) -> NSDragOperation {
        if sender.draggingPasteboard.types?.contains(EmployeeMention.pasteboardType) == true {
            return draggingEntered(sender)
        }
        return super.draggingUpdated(sender)
    }

    override func prepareForDragOperation(_ sender: NSDraggingInfo) -> Bool {
        if sender.draggingPasteboard.types?.contains(EmployeeMention.pasteboardType) == true {
            return isEditable && droppedMention(from: sender.draggingPasteboard) != nil
        }
        return super.prepareForDragOperation(sender)
    }

    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        guard sender.draggingPasteboard.types?.contains(EmployeeMention.pasteboardType) == true else {
            return super.performDragOperation(sender)
        }
        guard isEditable, let mention = droppedMention(from: sender.draggingPasteboard) else { return false }
        beginNextUserEdit()
        let location = characterIndexForInsertion(at: convert(sender.draggingLocation, from: nil))
        insertMention(mention, replacing: NSRange(location: location, length: 0))
        window?.makeFirstResponder(self)
        return true
    }

    /// 전송 성공 뒤 원격 입력기가 이전 조합의 마지막 음절을 다시 보내는
    /// 경우가 있다. 다음 실제 keyDown 전까지 도착한 텍스트 변경은 이전
    /// 조합의 잔여 이벤트로 보고 버린다.
    private func completeAcceptedSubmission() {
        discardsTextChangesUntilNextUserEdit = true
        // 원격 입력기는 화면의 marked range가 끝난 뒤에도 내부 조합을
        // 보존할 수 있다. 다음 키에서 재확정되지 않도록 입력기 문맥도
        // 함께 폐기한다. 이 호출 중 발생하는 변경은 위 플래그가 막는다.
        inputContext?.discardMarkedText()
        replaceTextAfterAcceptedSubmissionIfNeeded()
    }

    private func beginNextUserEdit() {
        guard discardsTextChangesUntilNextUserEdit else {
            return
        }
        // 다음 키를 처리하기 직전에도 남은 원격 IME 조합을 한 번 더
        // 비운 뒤 차단을 해제한다. 실제 새 키는 이후 super.keyDown에서
        // 정상 처리된다.
        inputContext?.discardMarkedText()
        discardsTextChangesUntilNextUserEdit = false
    }

    func discardPostSubmissionTextChangeIfNeeded() -> Bool {
        guard discardsTextChangesUntilNextUserEdit else {
            return false
        }
        replaceTextAfterAcceptedSubmissionIfNeeded()
        return true
    }

    private func replaceTextAfterAcceptedSubmissionIfNeeded() {
        guard !string.isEmpty else {
            return
        }
        string = ""
        setSelectedRange(NSRange(location: 0, length: 0))
        needsDisplay = true
        scrollToBeginningOfDocument(nil)
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        guard string.isEmpty, !placeholder.isEmpty else {
            return
        }
        let attributes: [NSAttributedString.Key: Any] = [
            .font: font ?? NSFont.systemFont(ofSize: 14, weight: .medium),
            .foregroundColor: NSColor.placeholderTextColor,
        ]
        (placeholder as NSString).draw(
            at: NSPoint(x: 0, y: textContainerInset.height),
            withAttributes: attributes
        )
    }
}
