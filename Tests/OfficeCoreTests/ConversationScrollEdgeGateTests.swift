import AppKit
import XCTest
@testable import OfficeGame

@MainActor
final class ConversationScrollEdgeGateTests: XCTestCase {
    func testBottomIgnoresDownwardBurstButAllowsUpwardMovement() throws {
        let fixture = Fixture()
        fixture.scroll(to: 700)
        let down = try fixture.wheel(y: -4)
        for _ in 0..<1_000 { XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(down)) }
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: 4)))
    }

    func testTopPassesUpwardInputOnlyWhenOlderTurnsCanBeLoaded() throws {
        let fixture = Fixture()
        fixture.scroll(to: 0)
        let up = try fixture.wheel(y: 4)
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(up))
        fixture.target.canLoadOlderTurns = true
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(up))
        fixture.target.canLoadOlderTurns = false
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(up))
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4)))
    }

    func testMiddleAndNewlyGrownDocumentRemainScrollable() throws {
        let fixture = Fixture()
        fixture.scroll(to: 350)
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: 4)))
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4)))
        fixture.scroll(to: 700)
        let down = try fixture.wheel(y: -4)
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(down))
        fixture.document.frame.size.height = 1_400
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(down))
    }

    func testNonFlippedDocumentsUseTheirVisualTopAndBottom() throws {
        let fixture = Fixture()
        fixture.view.documentView = NSView(frame: fixture.document.frame)
        fixture.scroll(to: 0)
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4)))
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: 4)))
        fixture.scroll(to: 700)
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: 4)))
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4)))
    }

    func testFeedObserverOwnsGateRegistrationAndPagingChanges() throws {
        let fixture = Fixture()
        fixture.target.detach()
        let coordinator = LiveWorkspaceFeedScrollObserver.Coordinator(
            onMetrics: { _ in }, onUserScrollStarted: {}, onUserScrollActivity: {},
            onUserScroll: { _ in }, canLoadOlderTurns: true)
        defer { coordinator.detach() }
        coordinator.attach(to: fixture.view)
        fixture.scroll(to: 0)
        let up = try fixture.wheel(y: 4)
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(up))
        coordinator.scrollEdgeTarget.canLoadOlderTurns = false
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(up))
        coordinator.detach()
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(up))
    }

    func testHorizontalModifiedAndGestureBoundaryEventsArePreserved() throws {
        let fixture = Fixture()
        fixture.scroll(to: 700)
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4, x: 2)))
        for flag: CGEventFlags in [.maskShift, .maskControl, .maskCommand, .maskAlternate] {
            XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4, flags: flag)))
        }
        for phase: Int64 in [4, 8] {
            XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4, phase: phase)))
        }
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4, phase: 1)))
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4, phase: 2)))
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4, momentum: 3)))
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4, momentum: 2)))
    }

    func testSplitPanesUseTheirOwnViewportAndPagingState() throws {
        let fixture = Fixture()
        let right = NSScrollView(frame: NSRect(x: 450, y: 0, width: 400, height: 300))
        let document = FlippedDocument(frame: NSRect(x: 0, y: 0, width: 400, height: 1_000))
        right.documentView = document
        fixture.window.contentView?.addSubview(right)
        let rightTarget = ConversationScrollEdgeGate.Target()
        rightTarget.attach(to: right)
        defer { rightTarget.detach() }
        fixture.scroll(to: 700)
        right.contentView.scroll(to: NSPoint(x: 0, y: 0))
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4)))
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4, point: NSPoint(x: 500, y: 150))))
        let rightUp = try fixture.wheel(y: 4, point: NSPoint(x: 500, y: 150))
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(rightUp))
        rightTarget.canLoadOlderTurns = true
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(rightUp))
    }

    func testHiddenDetachedAndOutsideViewsDoNotConsumeInput() throws {
        let fixture = Fixture()
        fixture.scroll(to: 700)
        let down = try fixture.wheel(y: -4)
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4, point: NSPoint(x: 900, y: 150))))
        fixture.view.isHidden = true
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(down))
        fixture.view.isHidden = false
        fixture.target.detach()
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(down))
    }

    func testShortDocumentStopsBothDirectionsUnlessOlderTurnsExist() throws {
        let fixture = Fixture()
        fixture.document.frame.size.height = 100
        fixture.scroll(to: 0)
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: -4)))
        XCTAssertTrue(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: 4)))
        fixture.target.canLoadOlderTurns = true
        XCTAssertFalse(ConversationScrollEdgeGate.shouldIgnore(try fixture.wheel(y: 4)))
    }

    private final class FlippedDocument: NSView { override var isFlipped: Bool { true } }

    @MainActor
    private final class Fixture {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1_000, height: 400),
                              styleMask: [.borderless], backing: .buffered, defer: false)
        let view = NSScrollView(frame: NSRect(x: 0, y: 0, width: 400, height: 300))
        let document = FlippedDocument(frame: NSRect(x: 0, y: 0, width: 400, height: 1_000))
        let target = ConversationScrollEdgeGate.Target()
        init() {
            window.contentView?.addSubview(view)
            view.documentView = document
            target.attach(to: view)
        }
        deinit { target.detach() }
        func scroll(to y: CGFloat) { view.contentView.scroll(to: NSPoint(x: 0, y: y)) }
        func wheel(y: Int64, x: Int64 = 0, point: NSPoint = NSPoint(x: 200, y: 150),
                   flags: CGEventFlags = [], phase: Int64 = 0, momentum: Int64 = 0) throws -> NSEvent {
            let seed = try XCTUnwrap(NSEvent.mouseEvent(with: .mouseMoved, location: point,
                modifierFlags: [], timestamp: 1, windowNumber: window.windowNumber,
                context: nil, eventNumber: 0, clickCount: 0, pressure: 0))
            let cg = try XCTUnwrap(seed.cgEvent?.copy())
            cg.type = .scrollWheel
            cg.location = CGPoint(x: point.x, y: window.frame.height - point.y)
            cg.flags = flags
            cg.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
            cg.setIntegerValueField(.scrollWheelEventPointDeltaAxis1, value: y)
            cg.setIntegerValueField(.scrollWheelEventPointDeltaAxis2, value: x)
            cg.setIntegerValueField(.scrollWheelEventScrollPhase, value: phase)
            cg.setIntegerValueField(.scrollWheelEventMomentumPhase, value: momentum)
            return try XCTUnwrap(NSEvent(cgEvent: cg))
        }
    }
}
