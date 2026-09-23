import AppKit
import XCTest
@testable import OfficeGame

@MainActor
final class ConversationScrollWheelCoalescerTests: XCTestCase {
    func testWindowAndConversationBoundarySurviveMerging() throws {
        let boundary = ConversationPointerMoveBoundaryView(frame: NSRect(x: 0, y: 0, width: 400, height: 400))
        let window = NSWindow(contentRect: boundary.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = boundary
        defer { window.contentView = nil; boundary.unregister() }
        func event(at point: NSPoint) throws -> NSEvent {
            let seed = try XCTUnwrap(NSEvent.mouseEvent(with: .mouseMoved, location: point,
                modifierFlags: [], timestamp: 1, windowNumber: window.windowNumber, context: nil,
                eventNumber: 1, clickCount: 0, pressure: 0))
            let cg = try XCTUnwrap(seed.cgEvent?.copy())
            cg.type = .scrollWheel
            cg.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
            cg.setIntegerValueField(.scrollWheelEventPointDeltaAxis1, value: 4)
            // The AppKit-created seed carries window-relative CG coordinates.
            cg.location = CGPoint(x: point.x, y: window.frame.height - point.y)
            return try XCTUnwrap(NSEvent(cgEvent: cg))
        }
        let inside = try event(at: NSPoint(x: 20, y: 20))
        XCTAssertEqual(inside.windowNumber, window.windowNumber)
        XCTAssertTrue(ConversationPointerMoveBoundaryView.containsScroll(inside), "point=\(inside.locationInWindow), window=\(String(describing: inside.window)), bounds=\(boundary.visibleRect)")
        XCTAssertFalse(ConversationPointerMoveBoundaryView.containsScroll(try event(at: NSPoint(x: 500, y: 20))))
        var delivered: [NSEvent] = []
        let coalescer = ConversationScrollWheelCoalescer(now: { 0 }) { delivered.append($0) }
        defer { coalescer.cancel() }
        _ = coalescer.process(inside, isInsideConversation: true)
        XCTAssertNil(coalescer.process(inside, isInsideConversation: true))
        XCTAssertNil(coalescer.process(inside, isInsideConversation: true))
        coalescer.flush()
        let merged = try XCTUnwrap(delivered.first)
        XCTAssertEqual(merged.windowNumber, window.windowNumber)
        XCTAssertTrue(ConversationPointerMoveBoundaryView.containsScroll(merged))
        boundary.isHidden = true
        XCTAssertFalse(ConversationPointerMoveBoundaryView.containsScroll(merged))
    }

    func testNativeScrollViewEndsAtSamePositionAfterCoalescing() async throws {
        for precise in [true, false] {
            func scrollView() -> NSScrollView {
                let view = NSScrollView(frame: NSRect(x: 0, y: 0, width: 400, height: 400))
                view.hasVerticalScroller = true
                view.hasHorizontalScroller = true
                view.documentView = NSView(frame: NSRect(x: 0, y: 0, width: 20_000, height: 20_000))
                view.contentView.scroll(to: NSPoint(x: 10_000, y: 10_000))
                return view
            }
            let direct = scrollView()
            let batched = scrollView()
            let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 800, height: 400),
                                  styleMask: [.borderless], backing: .buffered, defer: false)
            window.contentView?.addSubview(direct)
            batched.frame.origin.x = 400
            window.contentView?.addSubview(batched)
            defer { window.contentView = nil }
            let start = direct.contentView.bounds.origin
            var time = 0.0
            let coalescer = ConversationScrollWheelCoalescer(now: { time }) { batched.scrollWheel(with: $0) }
            defer { coalescer.cancel() }
            for index in 0..<100 {
                time = Double(index) / 1_000
                let event = try wheel(y: -4, x: -2, precise: precise)
                direct.scrollWheel(with: event)
                if let event = coalescer.process(event, isInsideConversation: true) {
                    batched.scrollWheel(with: event)
                }
            }
            coalescer.flush()
            try await Task.sleep(for: .milliseconds(300))
            XCTAssertNotEqual(direct.contentView.bounds.origin, start)
            XCTAssertEqual(batched.contentView.bounds.minY, direct.contentView.bounds.minY, accuracy: 0.01)
            XCTAssertEqual(batched.contentView.bounds.minX, direct.contentView.bounds.minX, accuracy: 0.01)
        }
    }

    func testFastWheelPreservesBothAxesForPixelAndLineDevices() throws {
        for precise in [true, false] {
            var time = 0.0
            var delivered: [NSEvent] = []
            let coalescer = ConversationScrollWheelCoalescer(now: { time }) { delivered.append($0) }
            defer { coalescer.cancel() }
            var expectedX = 0.0
            var expectedY = 0.0
            for index in 0..<1_000 {
                time = Double(index) / 1_000
                let event = try wheel(y: 4, x: 2, precise: precise)
                expectedX += event.scrollingDeltaX
                expectedY += event.scrollingDeltaY
                if let event = coalescer.process(event, isInsideConversation: true) { delivered.append(event) }
            }
            coalescer.flush()
            XCTAssertLessThanOrEqual(delivered.count, 61)
            XCTAssertGreaterThanOrEqual(delivered.count, 58)
            XCTAssertEqual(delivered.reduce(0) { $0 + $1.scrollingDeltaX }, expectedX, accuracy: 0.001)
            XCTAssertEqual(delivered.reduce(0) { $0 + $1.scrollingDeltaY }, expectedY, accuracy: 0.001)
            XCTAssertTrue(delivered.allSatisfy { $0.hasPreciseScrollingDeltas == precise })
            print("[wheel-coalescing] precise=\(precise): 1000 inputs -> \(delivered.count) deliveries; X/Y conserved")
        }
    }

    func testFractionalAndDeviceSpecificDeltasArePreserved() throws {
        var delivered: [NSEvent] = []
        let coalescer = ConversationScrollWheelCoalescer(now: { 0 }) { delivered.append($0) }
        defer { coalescer.cancel() }
        let event = try wheel()
        let cg = try XCTUnwrap(event.cgEvent?.copy())
        let fields: [CGEventField] = [.scrollWheelEventFixedPtDeltaAxis1,
            .scrollWheelEventFixedPtDeltaAxis2, .scrollWheelEventFixedPtDeltaAxis3,
            .scrollWheelEventAcceleratedDeltaAxis1, .scrollWheelEventAcceleratedDeltaAxis2,
            .scrollWheelEventRawDeltaAxis1, .scrollWheelEventRawDeltaAxis2]
        for field in fields { cg.setDoubleValueField(field, value: 0.25) }
        let fractional = try XCTUnwrap(NSEvent(cgEvent: cg))
        _ = coalescer.process(fractional, isInsideConversation: true)
        XCTAssertNil(coalescer.process(fractional, isInsideConversation: true))
        XCTAssertNil(coalescer.process(fractional, isInsideConversation: true))
        coalescer.flush()
        let result = try XCTUnwrap(delivered.first?.cgEvent)
        for field in fields { XCTAssertEqual(result.getDoubleValueField(field), 0.5, accuracy: 0.00001) }
    }

    func testReversalAndTargetChangesFlushRatherThanCancelMotion() throws {
        let changes = [try wheel(y: -4), try wheel(x: -2),
                       try wheel(location: CGPoint(x: 80, y: 80)),
                       try wheel(flags: .maskShift), try wheel(precise: false)]
        for changed in changes {
            var delivered: [NSEvent] = []
            let coalescer = ConversationScrollWheelCoalescer(now: { 0 }) { delivered.append($0) }
            defer { coalescer.cancel() }
            _ = coalescer.process(try wheel(x: 2), isInsideConversation: true)
            let pending = try wheel(x: 2)
            XCTAssertNil(coalescer.process(pending, isInsideConversation: true))
            XCTAssertTrue(coalescer.process(changed, isInsideConversation: true) === changed)
            XCTAssertEqual(delivered.count, 1)
            XCTAssertTrue(delivered.first === pending)
            coalescer.flush()
            XCTAssertEqual(delivered.count, 1)
        }
    }

    func testGestureBoundariesAndMomentumRetainOrder() throws {
        var delivered: [NSEvent] = []
        let coalescer = ConversationScrollWheelCoalescer(now: { 0 }) { delivered.append($0) }
        defer { coalescer.cancel() }
        let sequence = [try wheel(phase: 1), try wheel(phase: 2), try wheel(phase: 2),
                        try wheel(phase: 2), try wheel(phase: 4), try wheel(momentum: 1),
                        try wheel(momentum: 2), try wheel(momentum: 2), try wheel(momentum: 3)]
        for event in sequence {
            if let event = coalescer.process(event, isInsideConversation: true) { delivered.append(event) }
        }
        XCTAssertEqual(delivered.map(\.phase), [.began, .changed, .changed, .ended, [], [], [], []])
        XCTAssertEqual(delivered.map(\.momentumPhase), [[], [], [], [], .began, .changed, .changed, .ended])
        XCTAssertEqual(delivered.reduce(0) { $0 + $1.scrollingDeltaY }, sequence.reduce(0) { $0 + $1.scrollingDeltaY })
    }

    func testOutsideRegionAndClickFlushPendingAndStayImmediate() throws {
        let click = try XCTUnwrap(NSEvent.mouseEvent(with: .leftMouseDown, location: .zero,
            modifierFlags: [], timestamp: 0, windowNumber: 0, context: nil,
            eventNumber: 0, clickCount: 1, pressure: 1))
        for next in [try wheel(), click] {
            var delivered: [NSEvent] = []
            let coalescer = ConversationScrollWheelCoalescer(now: { 0 }) { delivered.append($0) }
            defer { coalescer.cancel() }
            _ = coalescer.process(try wheel(), isInsideConversation: true)
            XCTAssertNil(coalescer.process(try wheel(), isInsideConversation: true))
            XCTAssertTrue(coalescer.process(next, isInsideConversation: false) === next)
            XCTAssertEqual(delivered.count, 1)
            coalescer.flush()
            XCTAssertEqual(delivered.count, 1)
        }
    }

    func testTrailingTimerDeliversOnceAndCancelPreventsLateScroll() async throws {
        var time = 0.0
        var delivered: [NSEvent] = []
        let replayed = expectation(description: "Trailing wheel delivered")
        let coalescer = ConversationScrollWheelCoalescer(now: { time }) {
            delivered.append($0)
            replayed.fulfill()
        }
        defer { coalescer.cancel() }
        _ = coalescer.process(try wheel(), isInsideConversation: true)
        time = 0.001
        XCTAssertNil(coalescer.process(try wheel(), isInsideConversation: true))
        XCTAssertNil(coalescer.process(try wheel(), isInsideConversation: true))
        await fulfillment(of: [replayed], timeout: 1)
        XCTAssertEqual(delivered.count, 1)
        XCTAssertEqual(delivered.first?.scrollingDeltaY, 8)
        time = 0.002
        XCTAssertNil(coalescer.process(try wheel(), isInsideConversation: true))
        coalescer.cancel()
        try await Task.sleep(for: .milliseconds(40))
        coalescer.flush()
        XCTAssertEqual(delivered.count, 1)
    }

    private func wheel(y: Int32 = 4, x: Int32 = 0, precise: Bool = true,
                       location: CGPoint = CGPoint(x: 20, y: 20), flags: CGEventFlags = [],
                       phase: Int64 = 0, momentum: Int64 = 0) throws -> NSEvent {
        let cg = try XCTUnwrap(CGEvent(scrollWheelEvent2Source: nil,
            units: precise ? .pixel : .line, wheelCount: 2, wheel1: y, wheel2: x, wheel3: 0))
        cg.location = location
        cg.flags = flags
        cg.setIntegerValueField(.scrollWheelEventIsContinuous, value: precise ? 1 : 0)
        cg.setIntegerValueField(.scrollWheelEventScrollPhase, value: phase)
        cg.setIntegerValueField(.scrollWheelEventMomentumPhase, value: momentum)
        return try XCTUnwrap(NSEvent(cgEvent: cg))
    }
}
