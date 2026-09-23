// 빠른 마우스 이동마다 중첩 hosting/scroll/selection 트리가 hit-test를
// 반복하지 않도록 대화 영역의 hover 갱신만 60Hz로 합친다.
// 휠은 별도 coalescer로 이동량을 합치며, 클릭·드래그·키 입력은 즉시 전달한다.

import AppKit
import SwiftUI

// 로컬 이벤트 모니터는 실제 앱의 AppKit tracking/hover 탐색을 막지
// 못했다. 기본 sendEvent로 들어가기 전에 합쳐 재전달의 이중 탐색도 막는다.
@objc(OfficeApplication)
final class OfficeApplication: NSApplication {
    private lazy var scrollWheels = ConversationScrollWheelCoalescer { [weak self] event in
        guard let self, ConversationPointerMoveBoundaryView.containsScroll(event) else { return }
        self.forwardEvent(event)
    }
    private lazy var pointerMoves = ConversationPointerMoveCoalescer { [weak self] event in
        guard let self, ConversationPointerMoveBoundaryView.contains(event),
              NSEvent.pressedMouseButtons == 0 else { return }
        self.forwardEvent(event)
    }

    override func sendEvent(_ event: NSEvent) {
        if event.type == .scrollWheel {
            pointerMoves.cancel()
            if ConversationScrollEdgeGate.shouldIgnore(event) {
                // A queued inward movement must be delivered before deciding
                // whether this new outward movement is still at the edge.
                scrollWheels.flush()
                if ConversationScrollEdgeGate.shouldIgnore(event) { return }
            }
            if let event = scrollWheels.process(
                event, isInsideConversation: ConversationPointerMoveBoundaryView.containsScroll(event)
            ) {
                super.sendEvent(event)
            }
            return
        }
        // Deliver accumulated movement before a click, direction/region change,
        // or key event can replace the feed or its scroll target.
        scrollWheels.flush()
        scrollWheels.cancel()
        guard event.type == .mouseMoved else {
            pointerMoves.cancel()
            super.sendEvent(event)
            return
        }
        if let event = pointerMoves.process(
            event,
            isInsideConversation: ConversationPointerMoveBoundaryView.contains(event),
            isDragging: NSEvent.pressedMouseButtons != 0
        ) {
            super.sendEvent(event)
        }
    }

    func cancelPendingPointerMove() {
        pointerMoves.cancel()
        scrollWheels.cancel()
    }

    private func forwardEvent(_ event: NSEvent) {
        // The viewport may have reached the edge while a wheel batch waited.
        guard !ConversationScrollEdgeGate.shouldIgnore(event) else { return }
        super.sendEvent(event)
    }
}

struct ConversationPointerMoveBoundary: NSViewRepresentable {
    func makeNSView(context: Context) -> ConversationPointerMoveBoundaryView {
        ConversationPointerMoveBoundaryView()
    }

    func updateNSView(_ nsView: ConversationPointerMoveBoundaryView, context: Context) {}

    static func dismantleNSView(_ nsView: ConversationPointerMoveBoundaryView, coordinator: ()) {
        nsView.unregister()
    }
}

@MainActor
final class ConversationPointerMoveBoundaryView: NSView {
    private static let regions = NSHashTable<ConversationPointerMoveBoundaryView>.weakObjects()
    var isRegistered: Bool { Self.regions.contains(self) }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        unregister()
        guard window != nil else { return }
        Self.regions.add(self)
    }

    func unregister() {
        Self.regions.remove(self)
        (NSApp as? OfficeApplication)?.cancelPendingPointerMove()
    }

    static func contains(_ event: NSEvent) -> Bool {
        guard event.type == .mouseMoved else { return false }
        return regions.allObjects.contains { $0.containsEvent(event) }
    }

    static func containsScroll(_ event: NSEvent) -> Bool {
        guard event.type == .scrollWheel else { return false }
        return regions.allObjects.contains { $0.containsEvent(event) }
    }

    private func containsEvent(_ event: NSEvent) -> Bool {
        guard let window, event.window === window, window.attachedSheet == nil,
              !isHiddenOrHasHiddenAncestor else { return false }
        // 하위 hit-test를 호출하지 않고 현재 보이는 사각형만 확인한다.
        return visibleRect.contains(convert(event.locationInWindow, from: nil))
    }
}

@MainActor
final class ConversationPointerMoveCoalescer {
    static let frameInterval: TimeInterval = 1.0 / 60.0

    private let now: () -> TimeInterval
    private let deliver: (NSEvent) -> Void
    private var lastDeliveryTime: TimeInterval?
    private var pendingEvent: NSEvent?
    private var timer: Timer?
    private var isDelivering = false

    init(
        now: @escaping () -> TimeInterval = { ProcessInfo.processInfo.systemUptime },
        deliver: @escaping (NSEvent) -> Void
    ) {
        self.now = now
        self.deliver = deliver
    }

    // 중간 mouseMoved만 합치고 마지막 좌표는 timer로 전달한다.
    // 멈춘 위치의 커서·hover·도움말도 마지막 이동을 받는다.
    func process(
        _ event: NSEvent,
        isInsideConversation: Bool,
        isDragging: Bool = false
    ) -> NSEvent? {
        if isDelivering { return event }
        guard event.type == .mouseMoved, isInsideConversation, !isDragging else {
            cancel()
            return event
        }
        let time = now()
        let delay = lastDeliveryTime.map { Self.frameInterval - (time - $0) } ?? 0
        guard delay > 0 else {
            clearPending()
            lastDeliveryTime = time
            return event
        }
        pendingEvent = event
        if timer == nil {
            let timer = Timer(timeInterval: delay, repeats: false) { [weak self] _ in
                MainActor.assumeIsolated { self?.flush() }
            }
            RunLoop.main.add(timer, forMode: .common)
            self.timer = timer
        }
        return nil
    }

    func cancel() {
        clearPending()
        lastDeliveryTime = nil
    }

    func flush() {
        guard let event = pendingEvent else { return }
        clearPending()
        lastDeliveryTime = now()
        isDelivering = true
        defer { isDelivering = false }
        deliver(event)
    }

    private func clearPending() {
        timer?.invalidate()
        timer = nil
        pendingEvent = nil
    }
}
