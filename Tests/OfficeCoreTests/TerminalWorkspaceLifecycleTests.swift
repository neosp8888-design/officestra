import XCTest
import AppKit
@testable import OfficeGame

final class TerminalWorkspaceLifecycleTests: XCTestCase {
    @MainActor
    func testActualTerminalBufferAPIReadiness() {
        let terminal = APIProcessTerminalView(frame: NSRect(x: 0, y: 0, width: 800, height: 400))
        terminal.feed(text: "\u{1b}[?2004h❯ ")
        let cursor = terminal.getTerminal().getCursorLocation()
        XCTAssertTrue(terminal.hasEmptyInput, "cursor=\(cursor), line=\(terminal.getTerminal().getLine(row: cursor.y)?.translateToString().debugDescription ?? "nil"), paste=\(terminal.getTerminal().bracketedPasteMode), marked=\(terminal.hasMarkedText()), scroll=\(terminal.scrollPosition), canScroll=\(terminal.canScroll)")
        terminal.feed(text: "unfinished")
        XCTAssertFalse(terminal.hasEmptyInput)
        terminal.feed(text: "\r\u{1b}[2K❯ Allow command?")
        XCTAssertFalse(terminal.hasEmptyInput)
        terminal.feed(text: "\r\u{1b}[2K❯ ")
        XCTAssertTrue(terminal.hasEmptyInput)
        terminal.feed(text: "\u{1b}[?2004l")
        XCTAssertFalse(terminal.hasEmptyInput)
        for placeholder in ["Try \"fix typecheck errors\"", "Ask Codex to do anything"] {
            terminal.feed(text: "\u{1b}[?2004h\r\u{1b}[2K❯ \u{1b}[2m\(placeholder)\u{1b}[22m\r\u{1b}[2C")
            XCTAssertTrue(terminal.hasEmptyInput)
            terminal.feed(text: "\r\u{1b}[2K❯ \(placeholder)\r\u{1b}[2C")
            XCTAssertFalse(terminal.hasEmptyInput, "User text at cursor zero must not be treated as a suggestion")
        }
        terminal.feed(text: "\r\u{1b}[2K❯ ")
        XCTAssertTrue(terminal.hasEmptyInput)
        terminal.recordInput()
        XCTAssertFalse(terminal.hasEmptyInput, "Wait for manual input's hook/watch status without queuing API input")
    }
    func testAPIInputRequiresAnEmptyPromptNotDraftOrMenu() {
        for prefix in ["> ", "❯ ", "› "] {
            XCTAssertTrue(TerminalAPIInputPolicy.isEmptyPrompt(prefix: prefix, suffix: "", bracketedPaste: true, atBottom: true))
        }
        XCTAssertTrue(TerminalAPIInputPolicy.isEmptyPrompt(prefix: "› ", suffix: "Ask Codex to do anything", bracketedPaste: true, atBottom: true, dimPlaceholder: true))
        XCTAssertFalse(TerminalAPIInputPolicy.isEmptyPrompt(prefix: "› ", suffix: "Ask Codex to do anything", bracketedPaste: true, atBottom: true))
        for (prefix, suffix) in [("❯ draft", ""), ("❯ ", "draft"), ("", ""), ("1. ", "Allow"), ("❯ ", "Allow command?")] {
            XCTAssertFalse(TerminalAPIInputPolicy.isEmptyPrompt(prefix: prefix, suffix: suffix, bracketedPaste: true, atBottom: true))
        }
        XCTAssertFalse(TerminalAPIInputPolicy.isEmptyPrompt(prefix: "❯ ", suffix: "", bracketedPaste: false, atBottom: true))
        XCTAssertFalse(TerminalAPIInputPolicy.isEmptyPrompt(prefix: "❯ ", suffix: "", bracketedPaste: true, atBottom: false))
    }

    func testTerminalDispatchRealtimeEnvelopeKeepsSessionEpoch() throws {
        let json = Data(#"{"type":"terminal.dispatch","characterId":"boss","dispatchId":"request","terminalSessionId":"epoch"}"#.utf8)
        let event = try JSONDecoder().decode(RealtimeFeedEvent.self, from: json)
        XCTAssertEqual(event.dispatchId, "request")
        XCTAssertEqual(event.terminalSessionId, "epoch")
        XCTAssertEqual(event.officeCharacter, .boss)
        let old = try JSONDecoder().decode(RealtimeFeedEvent.self, from: Data(#"{"type":"terminal.changed"}"#.utf8))
        XCTAssertNil(old.dispatchId)
    }
    @MainActor
    func testMountedTerminalReleasesSelectionGateForRepeatedSwitching() async {
        let selectionStore = CharacterSelectionStore()
        let view = CachedTerminalWorkspacesNSView()
        view.startsProcesses = false
        let baseURL = URL(string: "http://127.0.0.1:4317")!

        XCTAssertFalse(selectionStore.canSelect(.leftMan))
        view.configure(
            selectedCharacterID: .boss,
            characterSelectionStore: selectionStore,
            databaseBaseURL: baseURL,
            sessionRevision: 0,
            restartRequest: nil
        )
        await Task.yield()
        XCTAssertTrue(selectionStore.canSelect(.leftMan))

        selectionStore.select(.leftMan)
        XCTAssertFalse(selectionStore.canSelect(.rightMan))
        view.configure(
            selectedCharacterID: .leftMan,
            characterSelectionStore: selectionStore,
            databaseBaseURL: baseURL,
            sessionRevision: 0,
            restartRequest: nil
        )
        await Task.yield()
        XCTAssertTrue(selectionStore.canSelect(.rightMan))
    }

    // 직원을 바꾸면 그 직원의 터미널로 포커스 요청이 가야 한다.
    @MainActor
    func testSwitchingCharacterRoutesFocusToSelectedTerminal() async {
        let selectionStore = CharacterSelectionStore()
        let view = CachedTerminalWorkspacesNSView()
        view.startsProcesses = false
        let baseURL = URL(string: "http://127.0.0.1:4317")!

        view.configure(
            selectedCharacterID: .boss,
            characterSelectionStore: selectionStore,
            databaseBaseURL: baseURL,
            sessionRevision: 0,
            restartRequest: nil
        )
        await Task.yield()
        XCTAssertEqual(view.lastFocusRequestCharacterForTesting, .boss)

        selectionStore.select(.leftMan)
        view.configure(
            selectedCharacterID: .leftMan,
            characterSelectionStore: selectionStore,
            databaseBaseURL: baseURL,
            sessionRevision: 0,
            restartRequest: nil
        )
        await Task.yield()
        XCTAssertEqual(view.lastFocusRequestCharacterForTesting, .leftMan)

        // 이미 떠 있는 직원으로 되돌아와도 다시 그 직원으로 포커스가 간다.
        selectionStore.select(.boss)
        view.configure(
            selectedCharacterID: .boss,
            characterSelectionStore: selectionStore,
            databaseBaseURL: baseURL,
            sessionRevision: 0,
            restartRequest: nil
        )
        await Task.yield()
        XCTAssertEqual(view.lastFocusRequestCharacterForTesting, .boss)
    }

    func testTerminalAppearanceKeepsDefaultTextReadableOnBlack() {
        let foreground = TerminalWorkspaceAppearance.foregroundColor
            .usingColorSpace(.deviceRGB)
        let background = TerminalWorkspaceAppearance.backgroundColor
            .usingColorSpace(.deviceRGB)

        XCTAssertNotNil(foreground)
        XCTAssertNotNil(background)
        let foregroundLuminance = [
            foreground?.redComponent,
            foreground?.greenComponent,
            foreground?.blueComponent,
        ]
        .compactMap { $0 }
        .reduce(0, +) / 3
        let backgroundLuminance = [
            background?.redComponent,
            background?.greenComponent,
            background?.blueComponent,
        ]
        .compactMap { $0 }
        .reduce(0, +) / 3

        XCTAssertGreaterThan(foregroundLuminance, 0.9)
        XCTAssertLessThan(backgroundLuminance, 0.1)
        XCTAssertEqual(TerminalWorkspaceAppearance.font.pointSize, 13)
        XCTAssertTrue(
            TerminalWorkspaceAppearance.font.fontDescriptor.symbolicTraits
                .contains(.monoSpace)
        )
    }

    func testSelectedCharactersAreCreatedLazilyAndCached() {
        var policy = TerminalWorkspaceCachePolicy()
        XCTAssertTrue(policy.select(.boss))
        XCTAssertFalse(policy.select(.boss))
        XCTAssertTrue(policy.select(.leftMan))
        XCTAssertEqual(policy.cachedCharacters, [.boss, .leftMan])
        XCTAssertEqual(policy.selectedCharacter, .leftMan)
    }

    func testRestartRevisionAppliesOnceToSelectedCharacter() {
        var policy = TerminalWorkspaceCachePolicy()
        _ = policy.select(.boss)
        XCTAssertTrue(policy.shouldRestart(character: .boss, revision: 1))
        XCTAssertFalse(policy.shouldRestart(character: .boss, revision: 1))
        XCTAssertFalse(policy.shouldRestart(character: .leftMan, revision: 2))
    }

    func testTearDownReturnsEveryCachedTerminal() {
        var policy = TerminalWorkspaceCachePolicy()
        _ = policy.select(.boss)
        _ = policy.select(.rightWoman)
        XCTAssertEqual(policy.tearDown(), [.boss, .rightWoman])
        XCTAssertTrue(policy.cachedCharacters.isEmpty)
        XCTAssertNil(policy.selectedCharacter)
    }

    // 터미널 화면이 붙어 있는 동안만 하단 입력창·멈춤이 터미널로 간다.
    // 화면이 내려가면 등록이 풀려 대화 모드의 기존 경로로 돌아가야 한다.
    @MainActor
    func testInputSinkStaysRegisteredUntilTearDown() {
        let director = AgentDirector(startBackgroundTasks: false)
        let view = CachedTerminalWorkspacesNSView()
        view.startsProcesses = false

        view.attachInputSink(to: director)
        XCTAssertTrue(director.terminalInputSink === view)

        view.tearDown()
        XCTAssertNil(director.terminalInputSink)
    }

    // 프로세스가 붙지 않은 터미널이나 열린 적 없는 직원에게는 글이 가지 않아야
    // 호출자가 예약을 되돌릴 수 있다.
    @MainActor
    func testInputWithoutProcessIsRejected() async {
        let selectionStore = CharacterSelectionStore()
        let view = CachedTerminalWorkspacesNSView()
        view.startsProcesses = false
        view.configure(
            selectedCharacterID: .boss,
            characterSelectionStore: selectionStore,
            databaseBaseURL: URL(string: "http://127.0.0.1:4317")!,
            sessionRevision: 0,
            restartRequest: nil
        )
        await Task.yield()

        XCTAssertFalse(view.sendText("안녕하세요", to: .boss))
        XCTAssertFalse(view.interrupt(.boss))
        XCTAssertFalse(view.sendText("안녕하세요", to: .leftMan))
        XCTAssertFalse(view.interrupt(.leftMan))
    }

    // CLI가 bracketed paste를 켜 두었으면 여러 줄이 중간에 제출되지 않게 감싸고,
    // 마지막 Enter로 바로 보낸다. 한글은 UTF-8 그대로 들어간다.
    func testSubmissionEncodingWrapsPasteAndEndsWithEnter() {
        let text = "안녕하세요\n두 번째 줄"
        let bracketed = TerminalInputEncoding.submission(
            text,
            bracketedPaste: true
        )
        XCTAssertEqual(
            bracketed,
            Array("\u{1b}[200~".utf8) + Array(text.utf8)
                + Array("\u{1b}[201~".utf8) + [0x0d]
        )

        let plain = TerminalInputEncoding.submission(
            text,
            bracketedPaste: false
        )
        XCTAssertEqual(plain, Array(text.utf8) + [0x0d])
        XCTAssertEqual(TerminalInputEncoding.interrupt, [0x1b])
    }
}
