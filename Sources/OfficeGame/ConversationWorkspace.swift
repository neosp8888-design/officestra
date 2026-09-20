import AppKit
import OfficeCore
import SwiftUI

enum ConversationPane: String { case left, right }

@MainActor
final class ConversationLayoutStore: ObservableObject {
    struct State: Equatable {
        var isSplit = false
        var left: OfficeCharacter? = .boss
        var right: OfficeCharacter?
        var active: ConversationPane = .left
        var fraction: Double = 0.5
    }
    @Published private(set) var state = State()
    private let defaults: UserDefaults?
    private var lastRight: OfficeCharacter?

    init(defaults: UserDefaults? = .standard) {
        self.defaults = defaults
        lastRight = defaults?.string(forKey: "officeConversationRightEmployee").flatMap(OfficeCharacter.init(rawValue:))
        if let value = defaults?.object(forKey: "officeConversationSplitFraction") as? Double {
            state.fraction = min(0.8, max(0.2, value))
        }
    }

    var selected: OfficeCharacter? { state.active == .left ? state.left : state.right }
    var visible: Set<OfficeCharacter> {
        Set(state.isSplit ? [state.left, state.right].compactMap { $0 } : [selected].compactMap { $0 })
    }

    func toggle(current: OfficeCharacter?) {
        var next = state
        if next.isSplit {
            lastRight = next.right
            defaults?.set(lastRight?.rawValue, forKey: "officeConversationRightEmployee")
            next.left = selected ?? current
            next.right = nil
            next.active = .left
            next.isSplit = false
        } else {
            next.left = current
            next.right = lastRight == current ? nil : lastRight
            next.active = .left
            next.isSplit = true
        }
        state = next
    }

    func select(_ character: OfficeCharacter?) {
        var next = state
        if !next.isSplit { next.left = character; next.active = .left }
        else if next.left == character { next.active = .left }
        else if next.right == character, character != nil { next.active = .right }
        else if next.active == .left { next.left = character }
        else { next.right = character }
        if state != next { state = next; rememberRight() }
    }

    @discardableResult
    func choose(_ character: OfficeCharacter, in pane: ConversationPane) -> OfficeCharacter {
        var next = state
        if next.left == character { next.active = .left }
        else if next.right == character { next.active = .right }
        else {
            if pane == .left { next.left = character } else { next.right = character }
            next.active = pane
        }
        if state != next { state = next; rememberRight() }
        return character
    }

    func resize(to fraction: Double) {
        guard fraction.isFinite else { return }
        state.fraction = min(0.8, max(0.2, fraction))
    }
    func saveSize() { defaults?.set(state.fraction, forKey: "officeConversationSplitFraction") }
    private func rememberRight() {
        if state.isSplit, let right = state.right {
            lastRight = right
            defaults?.set(right.rawValue, forKey: "officeConversationRightEmployee")
        }
    }

    static let divider: CGFloat = 9
    static func leftWidth(in width: CGFloat, fraction: Double) -> CGFloat {
        let usable = max(0, width - divider)
        let minimum = min(220, usable / 2)
        return min(max(usable * fraction, minimum), usable - minimum)
    }
    func frames(in bounds: CGRect) -> [OfficeCharacter: CGRect] {
        guard state.isSplit else { return selected.map { [$0: bounds] } ?? [:] }
        let leftWidth = Self.leftWidth(in: bounds.width, fraction: state.fraction)
        let height = max(0, bounds.height)
        var frames: [OfficeCharacter: CGRect] = [:]
        if let left = state.left { frames[left] = CGRect(x: 0, y: 0, width: leftWidth, height: height) }
        if let right = state.right { frames[right] = CGRect(x: leftWidth + Self.divider, y: 0, width: max(0, bounds.width-leftWidth-Self.divider), height: height) }
        return frames
    }
}

@MainActor
final class EmployeeDraftStore: ObservableObject {
    @Published var drafts: [OfficeCharacter: CommandEntryDraft] = [:]
}

@MainActor
final class EmployeeComposerStore: ObservableObject {
    let draftStore = EmployeeDraftStore()
    var focusesComposerOnSelection = true
    var drafts: [OfficeCharacter: CommandEntryDraft] {
        get { draftStore.drafts }
        set { draftStore.drafts = newValue }
    }
    @Published var attachments: [OfficeCharacter: [PendingAttachment]] = [:]
    @Published var attachmentErrors: [OfficeCharacter: String] = [:]
    @Published var preparing: Set<OfficeCharacter> = []
    var allAttachments: [PendingAttachment] { attachments.values.flatMap { $0 } }
}

struct ConversationWorkspaceView: View {
    @ObservedObject var director: AgentDirector
    @ObservedObject var layout: ConversationLayoutStore
    let mode: OfficeConversationMode
    @State private var dragStart: CGFloat?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        GeometryReader { geometry in
            let leftWidth = ConversationLayoutStore.leftWidth(in: geometry.size.width, fraction: layout.state.fraction)
            ZStack(alignment: .topLeading) {
                if mode == .terminal {
                    CachedTerminalWorkspaces(director: director)
                } else {
                    ConversationFeedHosts(director: director, layout: layout)
                }
                if layout.state.isSplit {
                    let focusedCharacter = layout.selected
                    if let focusedCharacter, let frame = layout.frames(in: CGRect(origin: .zero, size: geometry.size))[focusedCharacter] {
                        CoreAnimationFocusLine(isAnimated: !reduceMotion)
                            .frame(width: frame.width, height: 2)
                            .offset(x: frame.minX, y: max(0, geometry.size.height - 2))
                            .allowsHitTesting(false)
                            .accessibilityHidden(true)
                    }
                    Rectangle().fill(Color.primary.opacity(0.06))
                        .overlay { Rectangle().fill(Color.primary.opacity(0.18)).frame(width: 1) }
                        .frame(width: ConversationLayoutStore.divider, height: geometry.size.height)
                        .contentShape(Rectangle())
                        .onHover { inside in if inside { NSCursor.resizeLeftRight.push() } else { NSCursor.pop() } }
                        .gesture(DragGesture(minimumDistance: 0).onChanged { value in
                            if dragStart == nil { dragStart = leftWidth }
                            let usable = max(1, geometry.size.width-ConversationLayoutStore.divider)
                            layout.resize(to: Double(((dragStart ?? leftWidth)+value.translation.width)/usable))
                        }.onEnded { _ in dragStart = nil; layout.saveSize() })
                        .accessibilityLabel(OfficeLocalization.string("대화 분할 너비 조절"))
                        .accessibilityAdjustableAction { direction in
                            layout.resize(to: layout.state.fraction + (direction == .increment ? 0.05 : -0.05))
                            layout.saveSize()
                        }
                        .offset(x: leftWidth)
                    if layout.state.right == nil {
                        VStack(spacing: 12) {
                            Text(OfficeLocalization.string("오른쪽에서 볼 직원 선택")).font(.headline)
                            ForEach(director.characters.filter { $0.id != layout.state.left }) { character in
                                Button { director.chooseConversationCharacter(character.id, in: .right) } label: {
                                    Label(director.displayName(for: character.id), systemImage: "person.crop.circle")
                                        .frame(maxWidth: .infinity).padding(.vertical, 5)
                                }
                                .buttonStyle(.bordered)
                            }
                        }
                        .padding(18)
                        .frame(width: max(0, geometry.size.width-leftWidth-ConversationLayoutStore.divider), height: geometry.size.height)
                        .offset(x: leftWidth+ConversationLayoutStore.divider)
                    }
                }
            }
        }
        .background { ConversationPaneFocusObserver(director: director) }
        .background(Color(nsColor: .windowBackgroundColor))
    }

}

/// Observe clicks without covering the feed or consuming text-selection events.
struct ConversationPaneFocusObserver: NSViewRepresentable {
    let director: AgentDirector
    func makeNSView(context: Context) -> ConversationPaneFocusNSView {
        let view = ConversationPaneFocusNSView()
        view.director = director
        return view
    }
    func updateNSView(_ view: ConversationPaneFocusNSView, context: Context) { view.director = director }
    static func dismantleNSView(_ view: ConversationPaneFocusNSView, coordinator: ()) { view.stopObserving() }
}

@MainActor
final class ConversationPaneFocusNSView: NSView {
    weak var director: AgentDirector?
    private var monitor: Any?
    override var isFlipped: Bool { true }
    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        stopObserving()
        guard window != nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] event in
            MainActor.assumeIsolated { self?.observeFocusEvent(event) }
            return event
        }
    }

    func observeFocusEvent(_ event: NSEvent) {
        guard event.type == .leftMouseDown || event.type == .rightMouseDown,
              let window, event.window === window, !isHiddenOrHasHiddenAncestor,
              let director, director.conversationLayout.state.isSplit else { return }
        let point = convert(event.locationInWindow, from: nil)
        guard visibleRect.contains(point) else { return }
        let layout = director.conversationLayout
        for (character, frame) in layout.frames(in: bounds) {
            // Leave the divider and empty picker alone.
            let paneFrame = CGRect(x: frame.minX, y: 0, width: frame.width, height: bounds.height)
            guard paneFrame.contains(point), director.selectedCharacterID != character else { continue }
            director.chooseConversationCharacter(character,
                in: layout.state.left == character ? .left : .right,
                focusesComposer: false)
            break
        }
    }

    func stopObserving() {
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
    }
}

struct ConversationFeedHosts: NSViewRepresentable {
    let director: AgentDirector
    @ObservedObject var layout: ConversationLayoutStore
    func makeNSView(context: Context) -> ConversationFeedHostsNSView { ConversationFeedHostsNSView() }
    func updateNSView(_ view: ConversationFeedHostsNSView, context: Context) { view.configure(director: director, layout: layout) }
    // The viewport is sized by GeometryReader, never by the transcripts inside it.
    // Default AppKit fitting-size traversal probes both hosted feeds repeatedly.
    func sizeThatFits(_ proposal: ProposedViewSize, nsView: ConversationFeedHostsNSView, context: Context) -> CGSize? {
        let width = proposal.width ?? nsView.bounds.width
        let height = proposal.height ?? nsView.bounds.height
        guard width.isFinite, height.isFinite, width > 0, height > 0 else { return nil }
        return CGSize(width: width, height: height)
    }
    static func dismantleNSView(_ view: ConversationFeedHostsNSView, coordinator: ()) { view.tearDown() }
}

@MainActor
final class ConversationFeedHostsNSView: NSView {
    private var hosts: [OfficeCharacter: CachedLiveWorkspaceFeedsNSView] = [:]
    private weak var director: AgentDirector?
    private weak var workspaceLayout: ConversationLayoutStore?
    override var isFlipped: Bool { true }
    var visibleCharactersForTesting: Set<OfficeCharacter> { Set(hosts.filter { $0.value.superview === self }.map(\.key)) }
    func hostForTesting(_ character: OfficeCharacter) -> CachedLiveWorkspaceFeedsNSView? { hosts[character] }

    func configure(director: AgentDirector, layout: ConversationLayoutStore) {
        self.director = director
        workspaceLayout = layout
        for character in hosts.keys.filter({ !layout.visible.contains($0) }) {
            guard let host = hosts.removeValue(forKey: character) else { continue }
            host.setWorkspaceVisible(false)
            host.tearDown()
            host.removeFromSuperview()
        }
        let frames = layout.frames(in: bounds)
        for character in layout.visible {
            let host = hosts[character] ?? CachedLiveWorkspaceFeedsNSView()
            hosts[character] = host
            if host.superview !== self { addSubview(host) }
            let frame = frames[character] ?? .zero
            if host.frame != frame { host.frame = frame }
            host.configure(director: director, selectedCharacterID: character,
                followsGlobalSelection: false, showsInitialLoadingGate: true)
            host.setWorkspaceVisible(true)
        }
    }
    override func layout() {
        super.layout()
        guard let workspaceLayout else { return }
        for (character, frame) in workspaceLayout.frames(in: bounds) {
            if let host = hosts[character], host.frame != frame { host.frame = frame }
        }
    }
    func tearDown() {
        for host in hosts.values { host.tearDown(); host.removeFromSuperview() }
        hosts.removeAll()
    }
}
