import AppKit
import SwiftUI

/// SwiftUI owns frameAutosaveName and can replace it after mounting.
/// Keep placement outside that mechanism, including the scene's initial size.
@MainActor
final class OfficeWindowPlacementStore {
    static let shared = OfficeWindowPlacementStore()
    static let preferenceKey = "officeMainWindowPlacementV1"

    private struct Placement: Codable, Equatable {
        let frame: String
        let contentSize: CGSize
    }

    private let defaults: UserDefaults
    private let restoredWindows = NSHashTable<NSWindow>.weakObjects()

    init(defaults: UserDefaults = .standard) { self.defaults = defaults }

    private var placement: Placement? {
        guard let data = defaults.data(forKey: Self.preferenceKey),
              let value = try? JSONDecoder().decode(Placement.self, from: data),
              value.contentSize.width.isFinite, value.contentSize.height.isFinite,
              value.contentSize.width > 0, value.contentSize.height > 0 else { return nil }
        return value
    }

    var initialContentSize: CGSize {
        placement?.contentSize ?? CGSize(width: 1440, height: 900)
    }

    func restoreOnce(_ window: NSWindow) {
        guard !restoredWindows.contains(window) else { return }
        restoredWindows.add(window)
        if let placement { window.setFrame(from: placement.frame) }
    }

    func save(_ window: NSWindow) {
        guard !window.styleMask.contains(.fullScreen) else { return }
        let size = window.contentRect(forFrameRect: window.frame).size
        guard size.width.isFinite, size.height.isFinite, size.width > 0, size.height > 0 else { return }
        let value = Placement(frame: window.frameDescriptor, contentSize: size)
        guard value != placement, let data = try? JSONEncoder().encode(value) else { return }
        defaults.set(data, forKey: Self.preferenceKey)
    }
}

struct OfficeWindowFramePersistence: NSViewRepresentable {
    var store: OfficeWindowPlacementStore = .shared

    func makeNSView(context: Context) -> OfficeWindowFramePersistenceView {
        OfficeWindowFramePersistenceView(store: store)
    }

    func updateNSView(_ view: OfficeWindowFramePersistenceView, context: Context) {}

    static func dismantleNSView(_ view: OfficeWindowFramePersistenceView, coordinator: ()) {
        view.stopObserving()
    }
}

final class OfficeWindowFramePersistenceView: NSView {
    private let store: OfficeWindowPlacementStore
    private weak var observedWindow: NSWindow?
    private var observers: [NSObjectProtocol] = []
    private var pendingSave: DispatchWorkItem?
    private var ready = false
    private var changingFullScreen = false

    init(store: OfficeWindowPlacementStore) {
        self.store = store
        super.init(frame: .zero)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        stopObserving()
        guard let window else { return }
        observedWindow = window
        for name in [NSWindow.didResizeNotification, NSWindow.didMoveNotification] {
            observe(name, object: window) { $0.scheduleSave() }
        }
        observe(NSWindow.willCloseNotification, object: window) { $0.saveNow() }
        observe(NSApplication.willTerminateNotification, object: nil) { $0.saveNow() }
        for name in [NSWindow.willEnterFullScreenNotification, NSWindow.willExitFullScreenNotification] {
            observe(name, object: window) { view in
                view.saveNow()
                view.changingFullScreen = true
            }
        }
        for name in [NSWindow.didEnterFullScreenNotification, NSWindow.didExitFullScreenNotification] {
            observe(name, object: window) { view in
                view.changingFullScreen = false
                view.scheduleSave()
            }
        }
        DispatchQueue.main.async { [weak self, weak window] in
            guard let self, let window, self.window === window else { return }
            self.store.restoreOnce(window)
            self.ready = true
        }
    }

    private func observe(_ name: Notification.Name, object: AnyObject?, action: @escaping (OfficeWindowFramePersistenceView) -> Void) {
        observers.append(NotificationCenter.default.addObserver(forName: name, object: object, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { if let self { action(self) } }
        })
    }

    private func scheduleSave() {
        guard ready, !changingFullScreen else { return }
        pendingSave?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.saveNow() }
        pendingSave = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2, execute: work)
    }

    private func saveNow() {
        pendingSave?.cancel()
        pendingSave = nil
        guard ready, !changingFullScreen, let observedWindow else { return }
        store.save(observedWindow)
    }

    func stopObserving() {
        saveNow()
        for observer in observers { NotificationCenter.default.removeObserver(observer) }
        observers.removeAll()
        observedWindow = nil
        ready = false
        changingFullScreen = false
    }
}
