import AppKit
import XCTest
@testable import OfficeGame

@MainActor
final class ConversationPointerMoveCoalescerTests: XCTestCase {
    func testBundleDeclaresTheEarlyEventDispatcherClass() throws {
        let repository = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let data = try Data(contentsOf: repository.appendingPathComponent("Resources/Info.plist"))
        let info = try XCTUnwrap(PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any])
        XCTAssertEqual(info["NSPrincipalClass"] as? String, "OfficeApplication")
        XCTAssertTrue(NSClassFromString("OfficeApplication") === OfficeApplication.self)
    }

    func testScopeRejectsOtherWindowsAndPointsOutsideTheFeed() throws {
        let boundary = ConversationPointerMoveBoundaryView(frame: NSRect(x: 0, y: 0, width: 500, height: 500))
        let window = NSWindow(contentRect: boundary.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = boundary
        defer { window.contentView = nil; boundary.unregister() }
        func event(x: CGFloat, windowNumber: Int) throws -> NSEvent {
            try XCTUnwrap(NSEvent.mouseEvent(
                with: .mouseMoved, location: NSPoint(x: x, y: 20), modifierFlags: [],
                timestamp: 0, windowNumber: windowNumber, context: nil,
                eventNumber: 0, clickCount: 0, pressure: 0
            ))
        }
        XCTAssertTrue(ConversationPointerMoveBoundaryView.contains(try event(x: 20, windowNumber: window.windowNumber)))
        XCTAssertFalse(ConversationPointerMoveBoundaryView.contains(try event(x: 700, windowNumber: window.windowNumber)))
        XCTAssertFalse(ConversationPointerMoveBoundaryView.contains(try event(x: 20, windowNumber: 0)))
        boundary.isHidden = true
        XCTAssertFalse(ConversationPointerMoveBoundaryView.contains(try event(x: 20, windowNumber: window.windowNumber)))
    }

    func testFastMovementIsBoundedAndDeliversTheLastPosition() throws {
        var time = 0.0
        var delivered: [NSEvent] = []
        let coalescer = ConversationPointerMoveCoalescer(now: { time }) { delivered.append($0) }
        defer { coalescer.cancel() }
        for index in 0..<1_000 {
            time = Double(index) / 1_000
            if let event = coalescer.process(try mouseEvent(index: index), isInsideConversation: true) {
                delivered.append(event)
            }
        }
        time += ConversationPointerMoveCoalescer.frameInterval
        coalescer.flush()
        XCTAssertLessThanOrEqual(delivered.count, 61)
        XCTAssertGreaterThanOrEqual(delivered.count, 58)
        XCTAssertEqual(delivered.first?.locationInWindow.x, 0)
        XCTAssertEqual(delivered.last?.locationInWindow.x, 999)
        print("[pointer-coalescing] 1000 moves / 1 second → \(delivered.count) deliveries; last position preserved")
    }

    func testClickDragScrollAndKeysStayImmediateAndCancelOldHover() throws {
        for type: NSEvent.EventType in [.leftMouseDown, .leftMouseUp, .leftMouseDragged,
                                        .rightMouseDown, .otherMouseDown, .scrollWheel,
                                        .keyDown, .keyUp, .flagsChanged, .mouseExited] {
            var time = 0.0
            var delivered: [NSEvent] = []
            let coalescer = ConversationPointerMoveCoalescer(now: { time }) { delivered.append($0) }
            defer { coalescer.cancel() }
            _ = coalescer.process(try mouseEvent(index: 0), isInsideConversation: true)
            time = 0.001
            XCTAssertNil(coalescer.process(try mouseEvent(index: 1), isInsideConversation: true))
            let event = try event(of: type)
            XCTAssertTrue(coalescer.process(event, isInsideConversation: true) === event, "\(type)")
            coalescer.flush()
            XCTAssertTrue(delivered.isEmpty, "Stale hover after \(type)")
        }
    }

    func testOtherRegionsAndDraggingAreNotLimited() throws {
        let coalescer = ConversationPointerMoveCoalescer(now: { 0 }) { _ in XCTFail("Unexpected replay") }
        defer { coalescer.cancel() }
        for index in 0..<100 {
            let event = try mouseEvent(index: index)
            XCTAssertTrue(coalescer.process(event, isInsideConversation: false) === event)
            XCTAssertTrue(coalescer.process(event, isInsideConversation: true, isDragging: true) === event)
        }
    }

    func testTrailingTimerReplaysOnceWithoutRecapturingItself() async throws {
        var delivered: [NSEvent] = []
        let replayed = expectation(description: "Final pointer position is delivered")
        var coalescer: ConversationPointerMoveCoalescer!
        coalescer = ConversationPointerMoveCoalescer { event in
            delivered.append(event)
            XCTAssertTrue(coalescer.process(event, isInsideConversation: true) === event)
            replayed.fulfill()
        }
        defer { coalescer.cancel(); coalescer = nil }
        let first = try mouseEvent(index: 0)
        XCTAssertTrue(coalescer.process(first, isInsideConversation: true) === first)
        XCTAssertNil(coalescer.process(try mouseEvent(index: 1), isInsideConversation: true))
        XCTAssertNil(coalescer.process(try mouseEvent(index: 2), isInsideConversation: true))
        await fulfillment(of: [replayed], timeout: 1)
        XCTAssertEqual(delivered.map(\.locationInWindow.x), [2])
        try await Task.sleep(for: .milliseconds(40))
        XCTAssertEqual(delivered.count, 1)
    }

    func testUnmountCancelsPendingMoveAndBoundaryDoesNotInterceptClicks() throws {
        var time = 0.0
        let coalescer = ConversationPointerMoveCoalescer(now: { time }) { _ in XCTFail("Replay after cancel") }
        _ = coalescer.process(try mouseEvent(index: 0), isInsideConversation: true)
        time = 0.001
        _ = coalescer.process(try mouseEvent(index: 1), isInsideConversation: true)
        coalescer.cancel()
        coalescer.flush()
        let boundary = ConversationPointerMoveBoundaryView(frame: NSRect(x: 0, y: 0, width: 500, height: 500))
        XCTAssertNil(boundary.hitTest(NSPoint(x: 50, y: 50)))
        let window = NSWindow(contentRect: boundary.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = boundary
        XCTAssertTrue(boundary.isRegistered)
        window.contentView = nil
        XCTAssertFalse(boundary.isRegistered)
        boundary.unregister()
    }

    private func mouseEvent(index: Int) throws -> NSEvent {
        try XCTUnwrap(NSEvent.mouseEvent(
            with: .mouseMoved, location: NSPoint(x: index, y: 20), modifierFlags: [],
            timestamp: 0, windowNumber: 0, context: nil,
            eventNumber: index, clickCount: 0, pressure: 0
        ))
    }

    private func event(of type: NSEvent.EventType) throws -> NSEvent {
        if type == .keyDown || type == .keyUp || type == .flagsChanged {
            return try XCTUnwrap(NSEvent.keyEvent(
                with: type, location: .zero, modifierFlags: [], timestamp: 0,
                windowNumber: 0, context: nil, characters: "c", charactersIgnoringModifiers: "c",
                isARepeat: false, keyCode: 8
            ))
        }
        if type == .scrollWheel {
            return try XCTUnwrap(NSEvent(cgEvent: try XCTUnwrap(CGEvent(
                scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 1,
                wheel1: 4, wheel2: 0, wheel3: 0
            ))))
        }
        if type == .mouseExited {
            return try XCTUnwrap(NSEvent.enterExitEvent(
                with: type, location: .zero, modifierFlags: [], timestamp: 0,
                windowNumber: 0, context: nil, eventNumber: 0, trackingNumber: 0, userData: nil
            ))
        }
        return try XCTUnwrap(NSEvent.mouseEvent(
            with: type, location: .zero, modifierFlags: [], timestamp: 0,
            windowNumber: 0, context: nil, eventNumber: 0, clickCount: 1, pressure: 1
        ))
    }
}
