// 이 파일은 명령 입력 초안의 전송 가능 여부와 초기화 규칙을 검증한다.

import AppKit
import OfficeCore
import SwiftUI
import XCTest
@testable import OfficeGame

@MainActor
final class CommandEntryDraftTests: XCTestCase {
    func testMentionCompletionSelectsRecipientBeforeReturnCanSubmit() throws {
        let textView = CommandComposerTextView()
        textView.mentionCandidates = [EmployeeMention(id: .rightWoman, name: "코대리"), EmployeeMention(id: .leftWoman, name: "로과장")]
        var selected: Set<OfficeCharacter> = []
        var submissions = 0
        textView.onMention = { selected.insert($0) }
        textView.onSubmit = { _ in submissions += 1; return true }
        textView.string = "검토해줘 @코"
        textView.setSelectedRange(NSRange(location: (textView.string as NSString).length, length: 0))
        textView.refreshMentionSuggestions()
        XCTAssertEqual(textView.mentionMatches.map(\.id), [.rightWoman])
        textView.keyDown(with: try makeReturnEvent())
        XCTAssertEqual(textView.string, "검토해줘 @코대리 ")
        XCTAssertEqual(selected, [.rightWoman])
        XCTAssertEqual(submissions, 0)
        textView.insertMention(EmployeeMention(id: .leftWoman, name: "로과장"), replacing: textView.selectedRange())
        XCTAssertEqual(selected, [.rightWoman, .leftWoman])
        textView.keyDown(with: try makeReturnEvent())
        XCTAssertEqual(submissions, 1)
        XCTAssertEqual(textView.string, "")
        XCTAssertEqual(selected, [.rightWoman, .leftWoman], "Sending does not clear saved recipients")
    }

    func testMentionQueryExcludesEmailsAndPastMentionsAndPreservesUTF16Ranges() {
        for text in ["mail@example.com", "@코대리 안녕", "그냥 문장", "코드@코"] {
            XCTAssertNil(EmployeeMentionQuery.atCaret(in: text, selection: NSRange(location: (text as NSString).length, length: 0)))
        }
        let text = "😀 안녕 @코"
        let query = EmployeeMentionQuery.atCaret(in: text, selection: NSRange(location: (text as NSString).length, length: 0))
        XCTAssertEqual(query?.text, "코")
        XCTAssertEqual(query.map { (text as NSString).substring(with: $0.range) }, "@코")
    }

    func testFullyTypedMentionsAtSendSupportMultipleNamesButNotEmailsOrAmbiguousNames() {
        let candidates = [EmployeeMention(id: .rightWoman, name: "코대리"), EmployeeMention(id: .leftWoman, name: "로과장")]
        XCTAssertEqual(EmployeeMention.recipients(in: "이 답변은 @코대리 @로과장", candidates: candidates), [.rightWoman, .leftWoman])
        XCTAssertEqual(EmployeeMention.recipients(in: "mail@코대리.com @로과장님", candidates: candidates), [])
        XCTAssertEqual(EmployeeMention.recipients(in: "@코대리", candidates: candidates + [EmployeeMention(id: .boss, name: "코대리")]), [])
    }

    func testCharacterDragPayloadAndDropResolveByIDWithoutChangingTheSpeaker() async throws {
        let provider = EmployeeMention.dragProvider(for: .rightWoman)
        let payload: Data = try await withCheckedThrowingContinuation { continuation in
            provider.loadDataRepresentation(forTypeIdentifier: EmployeeMention.pasteboardType.rawValue) { data, error in
                if let data { continuation.resume(returning: data) }
                else { continuation.resume(throwing: error ?? NSError(domain: "missing-drag-data", code: 1)) }
            }
        }
        let board = NSPasteboard.withUniqueName()
        defer { board.releaseGlobally() }
        board.setData(payload, forType: EmployeeMention.pasteboardType)
        let textView = CommandComposerTextView()
        let recipient = EmployeeMention(id: .rightWoman, name: "코대리")
        textView.mentionCandidates = [recipient]
        XCTAssertEqual(textView.droppedMention(from: board), recipient)
        var selected: OfficeCharacter?
        textView.onMention = { selected = $0 }
        textView.string = "답변해줘"
        textView.insertMention(recipient, replacing: NSRange(location: 0, length: 0))
        XCTAssertEqual(textView.string, "@코대리 답변해줘")
        XCTAssertEqual(selected, .rightWoman)
        textView.mentionCandidates = []
        XCTAssertNil(textView.droppedMention(from: board), "Self or unknown employees cannot be dropped")
    }

    func testKoreanMarkedMentionCommitsWithoutSendingOrLosingTheLastSyllable() async throws {
        let textView = CommandComposerTextView()
        textView.mentionCandidates = [EmployeeMention(id: .rightWoman, name: "코대리")]
        textView.string = "@코대"
        textView.setSelectedRange(NSRange(location: 3, length: 0))
        textView.setMarkedText("리", selectedRange: NSRange(location: 1, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        var selected: OfficeCharacter?
        var submissions = 0
        textView.onMention = { selected = $0 }
        textView.onSubmit = { _ in submissions += 1; return true }
        textView.keyDown(with: try makeReturnEvent())
        await Task.yield()
        // AppKit commits marked text on the next main-queue pass.
        await withCheckedContinuation { continuation in DispatchQueue.main.async { continuation.resume() } }
        XCTAssertEqual(textView.string, "@코대리 ")
        XCTAssertEqual(selected, .rightWoman)
        XCTAssertEqual(submissions, 0)
    }

    func testReplyTagsRenderAtCompactWidthWithoutStartingAgents() throws {
        let director = AgentDirector(startBackgroundTasks: false)
        director.selectedCharacterID = .leftWoman
        director.replyRecipients[.leftWoman] = [.rightWoman, .boss]
        for scheme in [ColorScheme.light, .dark] {
            let host = NSHostingView(rootView: VStack(alignment: .leading, spacing: 16) {
              ReplyRoutingControl(director: director)
              CommandEntryRow(
                director: director, placeholder: "자연스럽게 대화를 이어가세요",
                attachmentCount: 0, isPreparingAttachments: false,
                onChooseAttachments: {}, onSubmit: { _ in false }
              )
            }.padding(12).environment(\.colorScheme, scheme)
                .frame(width: 500, height: 140, alignment: .topLeading)
                .background(scheme == .dark ? Color(white: 0.13) : Color(white: 0.96)))
            host.frame = NSRect(x: 0, y: 0, width: 500, height: 140)
            host.appearance = NSAppearance(named: scheme == .dark ? .darkAqua : .aqua)
            host.layoutSubtreeIfNeeded()
            let bitmap = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
            host.cacheDisplay(in: host.bounds, to: bitmap)
            XCTAssertGreaterThan(bitmap.pixelsWide, 0)
            if let directory = ProcessInfo.processInfo.environment["OFFICESTRA_REPLY_PREVIEW_DIR"] {
                let url = URL(fileURLWithPath: directory).appendingPathComponent("reply-tags-\(scheme == .dark ? "dark" : "light").png")
                try XCTUnwrap(bitmap.representation(using: .png, properties: [:])).write(to: url)
            }
        }
        XCTAssertEqual(director.replyRecipients[.leftWoman], [.rightWoman, .boss])
        director.selectedCharacterID = .boss
        XCTAssertNil(director.replyRecipients[.boss], "Recipient choices belong to the sending employee")
    }

    func testAvailabilityAllowsReadyIdleSelectedCharacter() {
        let availability = CommandEntryAvailability(
            isReady: true,
            isUpdatingConfiguration: false,
            hasSelectedCharacter: true,
            isSelectedCharacterRunning: false,
            canQueueForSelectedCharacter: false
        )

        XCTAssertTrue(availability.canSubmit)
        XCTAssertTrue(availability.canChooseAttachments(currentCount: 19))
        XCTAssertFalse(availability.canChooseAttachments(currentCount: 20))
    }

    func testAvailabilityRejectsEveryBlockedState() {
        let blockedStates = [
            CommandEntryAvailability(
                isReady: false,
                isUpdatingConfiguration: false,
                hasSelectedCharacter: true,
                isSelectedCharacterRunning: false,
                canQueueForSelectedCharacter: false
            ),
            CommandEntryAvailability(
                isReady: true,
                isUpdatingConfiguration: true,
                hasSelectedCharacter: true,
                isSelectedCharacterRunning: false,
                canQueueForSelectedCharacter: false
            ),
            CommandEntryAvailability(
                isReady: true,
                isUpdatingConfiguration: false,
                hasSelectedCharacter: false,
                isSelectedCharacterRunning: false,
                canQueueForSelectedCharacter: false
            ),
            CommandEntryAvailability(
                isReady: true,
                isUpdatingConfiguration: false,
                hasSelectedCharacter: true,
                isSelectedCharacterRunning: true,
                canQueueForSelectedCharacter: false
            ),
        ]

        for availability in blockedStates {
            XCTAssertFalse(availability.canSubmit)
            XCTAssertFalse(availability.canQueue)
            XCTAssertFalse(
                availability.canChooseAttachments(currentCount: 0)
            )
        }
    }

    func testRunningCharacterAcceptsInputAsQueuedReservation() {
        let availability = CommandEntryAvailability(
            isReady: true,
            isUpdatingConfiguration: false,
            hasSelectedCharacter: true,
            isSelectedCharacterRunning: true,
            canQueueForSelectedCharacter: true
        )

        XCTAssertFalse(
            availability.canSubmit,
            "응답 생성 중에는 즉시 제출이 아니라 예약이어야 합니다."
        )
        XCTAssertTrue(availability.canQueue)
        XCTAssertTrue(availability.acceptsInput)
        XCTAssertTrue(
            availability.canChooseAttachments(currentCount: 0),
            "예약에도 첨부를 실을 수 있어야 합니다."
        )
    }

    func testFullQueueStopsAcceptingMoreInputWhileRunning() {
        let availability = CommandEntryAvailability(
            isReady: true,
            isUpdatingConfiguration: false,
            hasSelectedCharacter: true,
            isSelectedCharacterRunning: true,
            canQueueForSelectedCharacter: false
        )

        XCTAssertFalse(availability.canQueue)
        XCTAssertFalse(availability.acceptsInput)
        XCTAssertFalse(
            availability.canChooseAttachments(currentCount: 0)
        )
    }

    func testCompactingCharacterCannotReceiveOrQueueWork() {
        let availability = CommandEntryAvailability(
            isReady: true,
            isUpdatingConfiguration: false,
            hasSelectedCharacter: true,
            isSelectedCharacterRunning: false,
            isSelectedCharacterCompacting: true,
            canQueueForSelectedCharacter: false
        )

        XCTAssertFalse(availability.canSubmit)
        XCTAssertFalse(availability.canQueue)
        XCTAssertFalse(availability.acceptsInput)
    }

    func testEmptyDraftWithoutAttachmentsCannotSubmit() {
        let draft = CommandEntryDraft(text: "  \n ")

        XCTAssertNil(
            draft.submissionPrompt(
                hasAttachments: false,
                isSubmissionAllowed: true
            )
        )
    }

    func testAttachmentOnlyDraftUsesDefaultPrompt() {
        let draft = CommandEntryDraft(text: " \n ")

        XCTAssertEqual(
            draft.submissionPrompt(
                hasAttachments: true,
                isSubmissionAllowed: true
            ),
            "첨부 파일을 확인해줘."
        )
    }

    func testDraftTrimsPromptBeforeSubmission() {
        let draft = CommandEntryDraft(text: "  업무를 확인해줘. \n")

        XCTAssertEqual(
            draft.submissionPrompt(
                hasAttachments: false,
                isSubmissionAllowed: true
            ),
            "업무를 확인해줘."
        )
    }

    func testUnavailableDraftDoesNotSubmit() {
        let draft = CommandEntryDraft(text: "업무")

        XCTAssertNil(
            draft.submissionPrompt(
                hasAttachments: true,
                isSubmissionAllowed: false
            )
        )
    }

    func testDraftClearsOnlyAfterAcceptedSubmission() {
        var draft = CommandEntryDraft(text: "업무")

        draft.clearAfterSubmission(accepted: false)
        XCTAssertEqual(draft.text, "업무")

        draft.clearAfterSubmission(accepted: true)
        XCTAssertEqual(draft.text, "")
    }

    func testTypingWithinNonemptyDraftDoesNotRequestRedraw() {
        let (coordinator, textBox) = makeCoordinator(initialText: "업")
        let textView = TrackingTextView()
        textView.string = "업무"
        textView.resetDisplayRequestCount()

        coordinator.textDidChange(
            Notification(
                name: NSText.didChangeNotification,
                object: textView
            )
        )

        XCTAssertEqual(textBox.value, "업무")
        XCTAssertEqual(textView.displayRequestCount, 0)
    }

    func testPlaceholderVisibilityChangesRequestRedraw() {
        for (oldValue, newValue) in [("", "업무"), ("업무", "")] {
            let (coordinator, textBox) = makeCoordinator(
                initialText: oldValue
            )
            let textView = TrackingTextView()
            textView.string = newValue
            textView.resetDisplayRequestCount()

            coordinator.textDidChange(
                Notification(
                    name: NSText.didChangeNotification,
                    object: textView
                )
            )

            XCTAssertEqual(textBox.value, newValue)
            XCTAssertEqual(textView.displayRequestCount, 1)
        }
    }

    func testSwiftUIUpdatePreservesMarkedKoreanComposition() {
        let textBox = TextBox(value: "")
        let composer = makeComposer(textBox: textBox)
        let textView = CommandComposerTextView()
        textView.setMarkedText(
            "한",
            selectedRange: NSRange(location: 1, length: 0),
            replacementRange: NSRange(location: NSNotFound, length: 0)
        )

        XCTAssertTrue(textView.hasMarkedText())
        XCTAssertFalse(composer.updateTextView(textView))
        XCTAssertEqual(textView.string, "한")
        XCTAssertTrue(
            textView.hasMarkedText(),
            "SwiftUI의 이전 draft가 한글 조합을 취소하면 안 됩니다."
        )
    }

    func testReturnSubmitsWithoutInsertingNewline() throws {
        let textView = CommandComposerTextView()
        textView.string = "업무"
        var submittedText: String?
        textView.onSubmit = {
            submittedText = $0
            return true
        }

        textView.keyDown(with: try makeReturnEvent())

        XCTAssertEqual(submittedText, "업무")
        XCTAssertEqual(textView.string, "")
    }

    func testReturnCommitsMarkedKoreanTextAndSubmits() async throws {
        let textView = CommandComposerTextView()
        textView.setMarkedText(
            "한글",
            selectedRange: NSRange(location: 2, length: 0),
            replacementRange: NSRange(location: NSNotFound, length: 0)
        )
        var submittedText: String?
        let didSubmit = expectation(description: "조합 완료 뒤 전송")
        textView.onSubmit = {
            submittedText = $0
            didSubmit.fulfill()
            return true
        }

        textView.keyDown(with: try makeReturnEvent())

        XCTAssertFalse(textView.hasMarkedText())
        XCTAssertNil(
            submittedText,
            "한글 조합 완료 이벤트보다 전송이 먼저 실행되면 안 됩니다."
        )
        await fulfillment(of: [didSubmit], timeout: 1)
        XCTAssertEqual(submittedText, "한글")
        XCTAssertEqual(textView.string, "")
    }

    func testLateKoreanCompositionChangeCannotRestoreSubmittedFinalSyllable()
        async throws
    {
        let textBox = TextBox(value: "요청")
        let didSubmit = expectation(description: "마지막 음절 확정 뒤 전송")
        let composer = makeComposer(textBox: textBox) {
            textBox.value = ""
            didSubmit.fulfill()
            return true
        }
        let coordinator = composer.makeCoordinator()
        let textView = CommandComposerTextView()
        textView.delegate = coordinator
        textView.string = "요청"
        textView.setSelectedRange(NSRange(location: 2, length: 0))
        textView.setMarkedText(
            "됨",
            selectedRange: NSRange(location: 1, length: 0),
            replacementRange: NSRange(location: NSNotFound, length: 0)
        )
        _ = composer.updateTextView(textView)

        textView.keyDown(with: try makeReturnEvent())
        await fulfillment(of: [didSubmit], timeout: 1)
        XCTAssertEqual(textBox.value, "")
        XCTAssertEqual(textView.string, "")

        // Chrome 원격 IME가 전송 완료 뒤 이전 마지막 음절을 다시
        // insertText한 상황을 그대로 흉내 낸다.
        textView.string = "됨"
        coordinator.textDidChange(
            Notification(
                name: NSText.didChangeNotification,
                object: textView
            )
        )

        XCTAssertEqual(
            textBox.value,
            "",
            "늦은 IME 변경이 전송 뒤 마지막 음절을 초안에 복원했습니다."
        )
        XCTAssertEqual(textView.string, "")

        textView.keyDown(with: try makeTextEvent("n", keyCode: 45))
        // window 밖의 단위 테스트 NSTextView는 keyDown만으로 문자를
        // 삽입하지 않으므로, 실제 키 이벤트가 차단을 해제한 뒤 입력기가
        // 보내는 insertText를 별도로 재현한다.
        textView.insertText(
            "n",
            replacementRange: NSRange(location: 0, length: 0)
        )
        coordinator.textDidChange(
            Notification(
                name: NSText.didChangeNotification,
                object: textView
            )
        )
        XCTAssertEqual(
            textBox.value,
            "n",
            "다음 실제 키 입력까지 잔여 IME 차단이 막으면 안 됩니다."
        )
    }

    func testMousePasteStartsNextEditAfterAcceptedSubmission() throws {
        let textBox = TextBox(value: "이전 요청")
        let composer = makeComposer(textBox: textBox) {
            textBox.value = ""
            return true
        }
        let coordinator = composer.makeCoordinator()
        let textView = CommandComposerTextView()
        textView.delegate = coordinator
        textView.string = textBox.value
        textView.setSelectedRange(NSRange(location: 5, length: 0))
        _ = composer.updateTextView(textView)

        textView.keyDown(with: try makeReturnEvent())
        XCTAssertEqual(textBox.value, "")
        XCTAssertEqual(textView.string, "")

        let pasteboard = NSPasteboard.withUniqueName()
        defer { pasteboard.releaseGlobally() }
        pasteboard.clearContents()
        XCTAssertTrue(
            pasteboard.setString("마우스로 붙인 다음 요청", forType: .string)
        )

        XCTAssertTrue(textView.readSelection(from: pasteboard, type: .string))
        coordinator.textDidChange(
            Notification(
                name: NSText.didChangeNotification,
                object: textView
            )
        )

        XCTAssertEqual(textView.string, "마우스로 붙인 다음 요청")
        XCTAssertEqual(
            textBox.value,
            "마우스로 붙인 다음 요청",
            "우클릭 붙여넣기를 IME 잔여 변경으로 오인하면 안 됩니다."
        )
    }

    func testRejectedSubmissionKeepsCommittedKoreanText() async throws {
        let textView = CommandComposerTextView()
        textView.setMarkedText(
            "보존됨",
            selectedRange: NSRange(location: 3, length: 0),
            replacementRange: NSRange(location: NSNotFound, length: 0)
        )
        let didReject = expectation(description: "전송 거절")
        textView.onSubmit = { _ in
            didReject.fulfill()
            return false
        }

        textView.keyDown(with: try makeReturnEvent())
        await fulfillment(of: [didReject], timeout: 1)

        XCTAssertEqual(textView.string, "보존됨")
        XCTAssertFalse(textView.hasMarkedText())
    }

    func testShiftReturnIsTheOnlyReturnThatInsertsNewline() throws {
        let textView = CommandComposerTextView()
        textView.string = "업무"
        textView.setSelectedRange(NSRange(location: 2, length: 0))
        var submittedText: String?
        textView.onSubmit = {
            submittedText = $0
            return true
        }

        textView.keyDown(with: try makeReturnEvent(modifiers: [.shift]))

        XCTAssertNil(submittedText)
        XCTAssertEqual(textView.string, "업무\n")
    }

    func testComposerHeightGrowsAfterTrailingNewlineAndCapsAtMaximum() {
        let textView = makeComposerTextView()

        textView.string = "첫째 줄"
        let singleLineHeight = CommandComposerLayout.measuredHeight(
            for: textView
        )

        textView.string = "첫째 줄\n"
        let twoLineHeight = CommandComposerLayout.measuredHeight(
            for: textView
        )

        textView.string = Array(repeating: "여러 줄", count: 30)
            .joined(separator: "\n")
        let cappedHeight = CommandComposerLayout.measuredHeight(for: textView)

        XCTAssertEqual(
            singleLineHeight,
            CommandComposerLayout.minimumHeight
        )
        XCTAssertGreaterThan(twoLineHeight, singleLineHeight)
        XCTAssertEqual(cappedHeight, CommandComposerLayout.maximumHeight)
    }

    private func makeCoordinator(
        initialText: String
    ) -> (CommandComposerView.Coordinator, TextBox) {
        let textBox = TextBox(value: initialText)
        let composer = makeComposer(textBox: textBox)
        return (composer.makeCoordinator(), textBox)
    }

    private func makeComposer(
        textBox: TextBox,
        onSubmit: @escaping () -> Bool = { true }
    ) -> CommandComposerView {
        CommandComposerView(
            text: Binding(
                get: { textBox.value },
                set: { textBox.value = $0 }
            ),
            measuredHeight: .constant(CommandComposerLayout.minimumHeight),
            placeholder: "업무를 입력하세요",
            isEnabled: true,
            onSubmit: onSubmit
        )
    }

    private func makeReturnEvent(
        modifiers: NSEvent.ModifierFlags = []
    ) throws -> NSEvent {
        try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown,
                location: .zero,
                modifierFlags: modifiers,
                timestamp: 0,
                windowNumber: 0,
                context: nil,
                characters: "\r",
                charactersIgnoringModifiers: "\r",
                isARepeat: false,
                keyCode: 36
            )
        )
    }

    private func makeTextEvent(
        _ characters: String,
        keyCode: UInt16
    ) throws -> NSEvent {
        try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown,
                location: .zero,
                modifierFlags: [],
                timestamp: 0,
                windowNumber: 0,
                context: nil,
                characters: characters,
                charactersIgnoringModifiers: characters,
                isARepeat: false,
                keyCode: keyCode
            )
        )
    }

    private func makeComposerTextView() -> NSTextView {
        let textView = NSTextView(
            frame: NSRect(x: 0, y: 0, width: 400, height: 40)
        )
        textView.isHorizontallyResizable = false
        textView.isVerticallyResizable = true
        textView.font = .systemFont(ofSize: 14, weight: .medium)
        textView.textContainer?.widthTracksTextView = true
        textView.textContainer?.containerSize = NSSize(
            width: 400,
            height: CGFloat.greatestFiniteMagnitude
        )
        textView.textContainer?.lineFragmentPadding = 0
        textView.textContainerInset = NSSize(width: 0, height: 12)
        return textView
    }
}

private final class TextBox {
    var value: String

    init(value: String) {
        self.value = value
    }
}

private final class TrackingTextView: NSTextView {
    private(set) var displayRequestCount = 0

    override var needsDisplay: Bool {
        get {
            super.needsDisplay
        }
        set {
            if newValue {
                displayRequestCount += 1
            }
            super.needsDisplay = newValue
        }
    }

    func resetDisplayRequestCount() {
        displayRequestCount = 0
    }
}
