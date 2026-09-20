import AppKit
import XCTest
@testable import OfficeGame

@MainActor
final class OfficeWindowFramePersistenceTests: XCTestCase {
    func testResizeSurvivesSwiftUIAutosaveNameReplacementAndNewStore() async throws {
        let suite = "OFFICESTRA.WindowFrameTest.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let store = OfficeWindowPlacementStore(defaults: defaults)
        XCTAssertEqual(store.initialContentSize, CGSize(width: 1440, height: 900))
        let screen = try XCTUnwrap(NSScreen.main).visibleFrame
        let initial = CGRect(x: screen.minX + 40, y: screen.minY + 40, width: 640, height: 400)
        let desired = CGRect(x: screen.minX + 60, y: screen.minY + 60, width: 800, height: 520)
        let first = window(frame: initial, store: store)
        await drainMainQueue()
        // WindowGroup can take ownership after the persistence view is mounted.
        let swiftUIName = "SwiftUI.Test.\(UUID().uuidString)"
        defer { NSWindow.removeFrame(usingName: swiftUIName) }
        first.setFrameAutosaveName(swiftUIName)
        first.setFrame(desired, display: false)
        first.close() // Flush even before the resize debounce has fired.
        first.contentView = nil
        first.setFrameAutosaveName("")

        let nextLaunch = OfficeWindowPlacementStore(defaults: defaults)
        let expectedContentSize = first.contentRect(forFrameRect: desired).size
        XCTAssertEqual(nextLaunch.initialContentSize, expectedContentSize,
                       "WindowGroup must start from the saved content size")
        let reopened = window(frame: initial, store: nextLaunch)
        defer { reopened.close(); reopened.contentView = nil }
        await drainMainQueue()
        XCTAssertEqual(reopened.frame.width, desired.width, accuracy: 1)
        XCTAssertEqual(reopened.frame.height, desired.height, accuracy: 1)
        XCTAssertEqual(reopened.frame.origin.x, desired.origin.x, accuracy: 1)
        XCTAssertEqual(reopened.frame.origin.y, desired.origin.y, accuracy: 1)
    }

    func testResizeIsSavedWithoutClosingAndRemountDoesNotResetIt() async throws {
        let suite = "OFFICESTRA.WindowFrameTest.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let store = OfficeWindowPlacementStore(defaults: defaults)
        let first = window(frame: CGRect(x: 40, y: 40, width: 640, height: 400), store: store)
        defer { first.close(); first.contentView = nil }
        await drainMainQueue()
        first.setFrame(CGRect(x: 60, y: 60, width: 800, height: 520), display: false)
        try await Task.sleep(for: .milliseconds(300))
        let current = first.frame
        XCTAssertEqual(store.initialContentSize, first.contentRect(forFrameRect: current).size)
        first.contentView = OfficeWindowFramePersistenceView(store: store)
        await drainMainQueue()
        XCTAssertEqual(first.frame, current)
        XCTAssertNil(first.contentView?.hitTest(.zero))
    }

    func testFullScreenTransitionDoesNotOverwriteNormalPlacement() async throws {
        let suite = "OFFICESTRA.WindowFrameTest.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let store = OfficeWindowPlacementStore(defaults: defaults)
        let first = window(frame: CGRect(x: 40, y: 40, width: 640, height: 400), store: store)
        defer { first.close(); first.contentView = nil }
        await drainMainQueue()
        let normal = first.contentRect(forFrameRect: first.frame).size
        NotificationCenter.default.post(name: NSWindow.willEnterFullScreenNotification, object: first)
        first.setFrame(CGRect(x: 0, y: 0, width: 1600, height: 1000), display: false)
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(store.initialContentSize, normal)
    }

    private func window(frame: CGRect, store: OfficeWindowPlacementStore) -> NSWindow {
        let window = NSWindow(contentRect: frame, styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = OfficeWindowFramePersistenceView(store: store)
        return window
    }

    private func drainMainQueue() async {
        await withCheckedContinuation { continuation in
            DispatchQueue.main.async { continuation.resume() }
        }
    }
}
