import AppKit
import OfficeCore
import SwiftTerm
import SwiftUI

struct TerminalAPIDispatch: Equatable, Sendable {
    let id: String
    let sessionID: String
}

enum TerminalAPIInputPolicy {
    // Fail closed on drafts, multiline continuations, menus and unknown CLI
    // layouts. This is checked again after the network claim, on the main actor.
    static func isEmptyPrompt(prefix: String, suffix: String, bracketedPaste: Bool, atBottom: Bool, dimPlaceholder: Bool = false) -> Bool {
        guard bracketedPaste, atBottom,
              [">", "❯", "›"].contains(prefix.trimmingCharacters(in: .whitespaces)) else { return false }
        let tail = suffix.trimmingCharacters(in: .whitespacesAndNewlines)
        if tail.isEmpty { return true }
        // Both installed CLIs render empty-field suggestions with SGR 2.
        // Identical words typed by the user are not a placeholder.
        return dimPlaceholder && (tail == "Ask Codex to do anything" ||
            (tail.hasPrefix("Try \"") && tail.hasSuffix("\"")))
    }
}

@MainActor
final class APIProcessTerminalView: LocalProcessTerminalView {
    var inputRevision = 0
    private(set) var lastInputAt = -TimeInterval.infinity
    func recordInput() {
        inputRevision &+= 1
        lastInputAt = ProcessInfo.processInfo.systemUptime
    }
    override func send(source: TerminalView, data: ArraySlice<UInt8>) {
        recordInput()
        super.send(source: source, data: data)
    }
    var hasEmptyInput: Bool {
        // A just-submitted manual turn can precede the hook/watcher's running
        // event. Reject during that short gap; never queue behind it.
        guard !hasMarkedText(), ProcessInfo.processInfo.systemUptime - lastInputAt >= 2 else { return false }
        let model = getTerminal()
        let cursor = model.getCursorLocation()
        guard let line = model.getLine(row: cursor.y) else { return false }
        let suffix = line.translateToString(trimRight: true, startCol: cursor.x)
        let endColumn = min(model.cols, cursor.x + suffix.utf16.count)
        let dimPlaceholder = cursor.x < endColumn && (cursor.x..<endColumn).allSatisfy { column in
            guard let cell = model.getCharData(col: column, row: cursor.y) else { return false }
            return cell.attribute.style.contains(.dim)
        }
        return TerminalAPIInputPolicy.isEmptyPrompt(
            prefix: line.translateToString(startCol: 0, endCol: cursor.x),
            suffix: suffix,
            bracketedPaste: model.bracketedPasteMode, atBottom: !canScroll || scrollPosition >= 0.999,
            dimPlaceholder: dimPlaceholder
        )
    }
}

enum TerminalWorkspaceAppearance {
    static let foregroundColor = NSColor.white
    static let backgroundColor = NSColor.black
    static let font = NSFont.monospacedSystemFont(
        ofSize: 13,
        weight: .regular
    )
}

/// 하단 입력창의 글을 CLI에 넣을 때 쓰는 바이트열이다. CLI가 bracketed paste를
/// 켜 두었으면 붙여넣기로 감싸 여러 줄이 중간에 제출되지 않게 하고, 마지막에
/// Enter를 붙여 바로 보낸다. 멈춤은 세 CLI 모두 Esc가 진행 중인 응답을 끊는다.
enum TerminalInputEncoding {
    static let bracketedPasteStart: [UInt8] = Array("\u{1b}[200~".utf8)
    static let bracketedPasteEnd: [UInt8] = Array("\u{1b}[201~".utf8)
    static let enter: [UInt8] = [0x0d]
    static let interrupt: [UInt8] = [0x1b]

    static func submission(
        _ text: String,
        bracketedPaste: Bool
    ) -> [UInt8] {
        var bytes: [UInt8] = []
        if bracketedPaste {
            bytes += bracketedPasteStart
        }
        bytes += Array(text.utf8)
        if bracketedPaste {
            bytes += bracketedPasteEnd
        }
        bytes += enter
        return bytes
    }
}

struct TerminalWorkspaceCachePolicy: Equatable {
    private(set) var cachedCharacters: Set<OfficeCharacter> = []
    private(set) var selectedCharacter: OfficeCharacter?
    private(set) var restartRevisions: [OfficeCharacter: Int] = [:]

    mutating func select(_ character: OfficeCharacter?) -> Bool {
        selectedCharacter = character
        guard let character else { return false }
        return cachedCharacters.insert(character).inserted
    }

    mutating func shouldRestart(
        character: OfficeCharacter,
        revision: Int
    ) -> Bool {
        guard selectedCharacter == character else { return false }
        let previous = restartRevisions[character] ?? 0
        guard revision > previous else { return false }
        restartRevisions[character] = revision
        return true
    }

    mutating func tearDown() -> Set<OfficeCharacter> {
        let removed = cachedCharacters
        cachedCharacters.removeAll()
        selectedCharacter = nil
        return removed
    }
}

struct CachedTerminalWorkspaces: NSViewRepresentable {
    @ObservedObject private var director: AgentDirector
    @ObservedObject private var characterSelectionStore:
        CharacterSelectionStore

    init(director: AgentDirector) {
        self.director = director
        _characterSelectionStore = ObservedObject(
            wrappedValue: director.characterSelectionStore
        )
    }

    func makeNSView(context: Context) -> CachedTerminalWorkspacesNSView {
        let view = CachedTerminalWorkspacesNSView()
        view.attachInputSink(to: director)
        configure(view)
        return view
    }

    func updateNSView(
        _ nsView: CachedTerminalWorkspacesNSView,
        context: Context
    ) {
        configure(nsView)
    }

    static func dismantleNSView(
        _ nsView: CachedTerminalWorkspacesNSView,
        coordinator: ()
    ) {
        nsView.tearDown()
    }

    private func configure(_ view: CachedTerminalWorkspacesNSView) {
        view.configure(
            selectedCharacterID:
                characterSelectionStore.selectedCharacterID,
            characterSelectionStore: characterSelectionStore,
            databaseBaseURL: director.databaseBaseURL,
            sessionRevision: director.terminalSessionRevision,
            restartRequest: director.terminalRestartRequest
        )
    }
}

@MainActor
final class CachedTerminalWorkspacesNSView: NSView {
    private var entries: [OfficeCharacter: TerminalProcessHostView] = [:]
    private var cachePolicy = TerminalWorkspaceCachePolicy()
    private var selectedCharacterID: OfficeCharacter?
    private var databaseBaseURL: URL?
    private var sessionRevision = 0
    private weak var characterSelectionStore: CharacterSelectionStore?
    private var lastFocusRequest: OfficeCharacter?
    private weak var inputSinkOwner: AgentDirector?

    var startsProcesses = true

    var cachedCharacterIDsForTesting: Set<OfficeCharacter> {
        Set(entries.keys)
    }

    var activeCharacterIDForTesting: OfficeCharacter? {
        selectedCharacterID
    }

    // 전환할 때 어느 직원의 터미널로 커서를 옮겨 달라고 요청했는지 기록한다.
    var lastFocusRequestCharacterForTesting: OfficeCharacter? {
        lastFocusRequest
    }

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.backgroundColor = NSColor.black.cgColor
        clipsToBounds = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }

    // 터미널 화면이 붙어 있는 동안 하단 입력창·예약·멈춤이 이리로 오도록
    // director에 등록한다. 화면이 내려가면 tearDown이 등록을 푼다.
    func attachInputSink(to director: AgentDirector) {
        inputSinkOwner = director
        director.terminalInputSink = self
    }

    func configure(
        selectedCharacterID: OfficeCharacter?,
        characterSelectionStore: CharacterSelectionStore,
        databaseBaseURL: URL,
        sessionRevision: Int,
        restartRequest: TerminalRestartRequest?
    ) {
        self.characterSelectionStore = characterSelectionStore
        self.databaseBaseURL = databaseBaseURL
        self.sessionRevision = sessionRevision
        if self.selectedCharacterID != selectedCharacterID {
            selectionWillChange(to: selectedCharacterID)
        }
        if
            let restartRequest,
            cachePolicy.shouldRestart(
                character: restartRequest.character,
                revision: restartRequest.revision
            )
        {
            entries[restartRequest.character]?.restart()
        }
    }

    private func selectionWillChange(to character: OfficeCharacter?) {
        selectedCharacterID = character
        _ = cachePolicy.select(character)
        for (id, entry) in entries {
            entry.isHidden = id != character
        }
        guard let character else { return }
        if entries[character] == nil {
            guard let databaseBaseURL else { return }
            let entry = TerminalProcessHostView(
                character: character,
                databaseBaseURL: databaseBaseURL
            )
            entry.translatesAutoresizingMaskIntoConstraints = false
            addSubview(entry)
            NSLayoutConstraint.activate([
                entry.leadingAnchor.constraint(equalTo: leadingAnchor),
                entry.trailingAnchor.constraint(equalTo: trailingAnchor),
                entry.topAnchor.constraint(equalTo: topAnchor),
                entry.bottomAnchor.constraint(equalTo: bottomAnchor),
            ])
            entries[character] = entry
            if startsProcesses {
                entry.start()
            }
        }
        focusSelectedTerminal(character)
        completeSelectionLoading(for: character)
    }

    // 직원을 바꾸면 그 직원의 터미널로 키보드 포커스를 옮겨, 곧바로 커서에
    // 입력할 수 있게 한다. 처음 뜨는 터미널은 프로세스가 붙는 시점에
    // installAndStart가 포커스를 잡고, 이미 떠 있는 터미널은 여기서 잡는다.
    private func focusSelectedTerminal(_ character: OfficeCharacter) {
        DispatchQueue.main.async { [weak self] in
            guard
                let self,
                self.selectedCharacterID == character
            else {
                return
            }
            self.lastFocusRequest = character
            self.entries[character]?.focusTerminal()
        }
    }

    private func completeSelectionLoading(for character: OfficeCharacter) {
        // SwiftUI의 updateNSView 안에서 @Published 상태를 즉시 바꾸지 않고,
        // 실제 터미널 호스트가 붙은 다음 main-queue 차례에 선택 잠금을 푼다.
        DispatchQueue.main.async { [weak self] in
            guard
                let self,
                self.selectedCharacterID == character
            else {
                return
            }
            self.characterSelectionStore?.completeConversationLoading(
                for: character
            )
        }
    }

    func tearDown() {
        if inputSinkOwner?.terminalInputSink === self {
            inputSinkOwner?.terminalInputSink = nil
        }
        inputSinkOwner = nil
        _ = cachePolicy.tearDown()
        for entry in entries.values {
            entry.terminateAndUnlock()
            entry.removeFromSuperview()
        }
        entries.removeAll()
        selectedCharacterID = nil
    }
}

extension CachedTerminalWorkspacesNSView: TerminalInputSink {
    func dispatch(_ request: TerminalAPIDispatch, to character: OfficeCharacter,
                  canSend: @escaping @MainActor () -> Bool) {
        entries[character]?.dispatch(request, canSend: canSend)
    }
    func sendText(_ text: String, to character: OfficeCharacter) -> Bool {
        entries[character]?.sendText(text) ?? false
    }

    func interrupt(_ character: OfficeCharacter) -> Bool {
        entries[character]?.sendInterrupt() ?? false
    }
}

@MainActor
private final class TerminalProcessHostView:
    NSView,
    @preconcurrency LocalProcessTerminalViewDelegate
{
    private let character: OfficeCharacter
    private let database: OfficeDatabaseClient
    private var terminal: APIProcessTerminalView?
    private var launchSpecification: TerminalLaunchSpecification?
    private var launchTask: Task<Void, Never>?
    private var generation = 0
    private var isTerminating = false
    private var applicationTerminationObserver: NSObjectProtocol?

    private lazy var statusLabel: NSTextField = {
        let label = NSTextField(
            labelWithString: OfficeLocalization.string("터미널을 시작하는 중입니다…")
        )
        label.textColor = .secondaryLabelColor
        label.alignment = .center
        label.font = .systemFont(ofSize: 13, weight: .medium)
        label.translatesAutoresizingMaskIntoConstraints = false
        return label
    }()

    private lazy var restartButton: NSButton = {
        let button = NSButton(
            title: OfficeLocalization.string("다시 시작"),
            target: self,
            action: #selector(restartPressed)
        )
        button.bezelStyle = .rounded
        button.isHidden = true
        button.translatesAutoresizingMaskIntoConstraints = false
        return button
    }()

    init(character: OfficeCharacter, databaseBaseURL: URL) {
        self.character = character
        database = OfficeDatabaseClient(baseURL: databaseBaseURL)
        super.init(frame: .zero)
        wantsLayer = true
        layer?.backgroundColor = NSColor.black.cgColor
        addSubview(statusLabel)
        addSubview(restartButton)
        NSLayoutConstraint.activate([
            statusLabel.centerXAnchor.constraint(equalTo: centerXAnchor),
            statusLabel.centerYAnchor.constraint(equalTo: centerYAnchor, constant: -16),
            statusLabel.leadingAnchor.constraint(greaterThanOrEqualTo: leadingAnchor, constant: 24),
            statusLabel.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -24),
            restartButton.centerXAnchor.constraint(equalTo: centerXAnchor),
            restartButton.topAnchor.constraint(equalTo: statusLabel.bottomAnchor, constant: 12),
        ])
        applicationTerminationObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.willTerminateNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.terminateAndUnlock()
            }
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }

    deinit {
        if let applicationTerminationObserver {
            NotificationCenter.default.removeObserver(applicationTerminationObserver)
        }
    }

    func start() {
        generation &+= 1
        let requestedGeneration = generation
        launchTask?.cancel()
        statusLabel.stringValue = OfficeLocalization.string("터미널을 시작하는 중입니다…")
        statusLabel.isHidden = false
        restartButton.isHidden = true
        launchTask = Task { [weak self] in
            guard let self else { return }
            do {
                let specification = try await database.openTerminalSession(
                    character: character
                )
                guard
                    !Task.isCancelled,
                    requestedGeneration == generation
                else {
                    try? await database.closeTerminalSession(
                        character: character
                    )
                    return
                }
                installAndStart(specification)
            } catch {
                guard requestedGeneration == generation else { return }
                showStopped(
                    OfficeLocalization.format(
                        "터미널을 열지 못했습니다.\n%@",
                        error.localizedDescription
                    )
                )
            }
        }
    }

    @objc private func restartPressed() {
        restart()
    }

    func restart() {
        generation &+= 1
        let pendingLaunch = launchTask
        launchTask = nil
        pendingLaunch?.cancel()
        let oldTerminal = terminal
        terminal = nil
        oldTerminal?.processDelegate = nil
        oldTerminal?.terminate()
        oldTerminal?.removeFromSuperview()
        statusLabel.stringValue = OfficeLocalization.string("터미널을 다시 시작하는 중입니다…")
        statusLabel.isHidden = false
        restartButton.isHidden = true
        Task { [weak self] in
            guard let self else { return }
            await pendingLaunch?.value
            try? await database.closeTerminalSession(character: character)
            guard !Task.isCancelled else { return }
            start()
        }
    }

    func terminateAndUnlock() {
        guard !isTerminating else { return }
        isTerminating = true
        generation &+= 1
        launchTask?.cancel()
        terminal?.processDelegate = nil
        terminal?.terminate()
        terminal = nil
        Task { [database, character] in
            try? await database.closeTerminalSession(character: character)
        }
    }

    private func installAndStart(
        _ specification: TerminalLaunchSpecification
    ) {
        terminal?.processDelegate = nil
        terminal?.removeFromSuperview()
        launchSpecification = specification
        let terminal = APIProcessTerminalView(frame: bounds)
        terminal.processDelegate = self
        terminal.translatesAutoresizingMaskIntoConstraints = false
        terminal.font = TerminalWorkspaceAppearance.font
        terminal.nativeForegroundColor =
            TerminalWorkspaceAppearance.foregroundColor
        terminal.nativeBackgroundColor =
            TerminalWorkspaceAppearance.backgroundColor
        terminal.caretColor = TerminalWorkspaceAppearance.foregroundColor
        terminal.caretTextColor = TerminalWorkspaceAppearance.backgroundColor
        terminal.selectedTextBackgroundColor =
            NSColor.systemBlue.withAlphaComponent(0.45)
        addSubview(terminal, positioned: .below, relativeTo: statusLabel)
        NSLayoutConstraint.activate([
            terminal.leadingAnchor.constraint(equalTo: leadingAnchor),
            terminal.trailingAnchor.constraint(equalTo: trailingAnchor),
            terminal.topAnchor.constraint(equalTo: topAnchor),
            terminal.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
        self.terminal = terminal
        statusLabel.isHidden = true
        restartButton.isHidden = true
        let environment = specification.env
            .sorted(by: { $0.key < $1.key })
            .map { "\($0.key)=\($0.value)" }
        terminal.startProcess(
            executable: specification.executable,
            args: specification.args,
            environment: environment,
            currentDirectory: specification.cwd
        )
        // 시작이 끝났을 때 이 호스트가 숨어 있으면(그 사이 다른 직원으로
        // 넘어갔다면) 포커스를 뺏지 않는다.
        if !isHidden {
            window?.makeFirstResponder(terminal)
        }
    }

    // 이미 프로세스가 붙어 있는 터미널로 키보드 포커스를 옮긴다.
    func focusTerminal() {
        guard let terminal else { return }
        window?.makeFirstResponder(terminal)
    }

    // 하단 입력창의 글을 붙여넣기처럼 넣고 Enter를 친다. 프로세스가 없으면
    // 받지 않아 호출자가 예약을 되돌릴 수 있다.
    func sendText(_ text: String) -> Bool {
        guard let terminal else { return false }
        let bytes = TerminalInputEncoding.submission(
            text,
            bracketedPaste: terminal.getTerminal().bracketedPasteMode
        )
        terminal.send(data: bytes[...])
        return true
    }

    func dispatch(_ request: TerminalAPIDispatch, canSend: @escaping @MainActor () -> Bool) {
        guard let spec = launchSpecification, spec.terminalSessionId == request.sessionID,
              let ownerToken = spec.ownerToken, let originalTerminal = terminal,
              !isTerminating else { return }
        let revision = originalTerminal.inputRevision
        let originalGeneration = generation
        Task { [weak self] in
            guard let self else { return }
            do {
                let claim = try await database.terminalDispatchControl(
                    character: character, dispatchID: request.id, sessionID: request.sessionID,
                    ownerToken: ownerToken, action: "claim"
                )
                guard let claim, Date().timeIntervalSince1970 * 1_000 < claim.expiresAt,
                      generation == originalGeneration,
                      terminal === originalTerminal, !isTerminating,
                      originalTerminal.inputRevision == revision,
                      canSend(), originalTerminal.hasEmptyInput else {
                    _ = try? await database.terminalDispatchControl(
                        character: character, dispatchID: request.id, sessionID: request.sessionID,
                        ownerToken: ownerToken, action: "reject"
                    )
                    return
                }
                // No await between the last input check and the PTY write.
                // The backend waits for the real CLI hook; never resend here.
                _ = sendText(claim.prompt)
            } catch {
                // Lost claims/closed sessions must not cause a second write.
            }
        }
    }

    func sendInterrupt() -> Bool {
        guard let terminal else { return false }
        terminal.send(data: TerminalInputEncoding.interrupt[...])
        return true
    }

    private func showStopped(_ message: String) {
        statusLabel.stringValue = message
        statusLabel.isHidden = false
        restartButton.isHidden = false
    }

    func processTerminated(source: TerminalView, exitCode: Int32?) {
        guard source === terminal else { return }
        terminal?.processDelegate = nil
        terminal = nil
        source.removeFromSuperview()
        let message: String
        if let exitCode {
            message = OfficeLocalization.format("터미널이 종료됐습니다 (종료 코드 %d).", exitCode)
        } else {
            message = OfficeLocalization.string("터미널이 종료됐습니다.")
        }
        showStopped(message)
        Task { [database, character] in
            try? await database.closeTerminalSession(character: character)
        }
    }

    func sizeChanged(
        source: LocalProcessTerminalView,
        newCols: Int,
        newRows: Int
    ) {}

    func setTerminalTitle(
        source: LocalProcessTerminalView,
        title: String
    ) {}

    func hostCurrentDirectoryUpdate(
        source: TerminalView,
        directory: String?
    ) {}
}
