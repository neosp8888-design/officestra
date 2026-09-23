import AppKit

// Main-thread registry of the actual live-feed scroll views. Inspecting their
// viewport rectangles avoids walking SwiftUI's transcript hit-test tree.
enum ConversationScrollEdgeGate {
    private static let targets = NSHashTable<Target>.weakObjects()

    final class Target: NSObject {
        weak var scrollView: NSScrollView?
        var canLoadOlderTurns = false

        func attach(to scrollView: NSScrollView) {
            self.scrollView = scrollView
            ConversationScrollEdgeGate.targets.add(self)
        }

        func detach() {
            ConversationScrollEdgeGate.targets.remove(self)
            scrollView = nil
        }
    }

    static func shouldIgnore(_ event: NSEvent) -> Bool {
        guard event.type == .scrollWheel,
              event.scrollingDeltaX == 0, event.scrollingDeltaY != 0,
              event.modifierFlags.intersection([.shift, .control, .option, .command]).isEmpty,
              event.phase.intersection([.ended, .cancelled]).isEmpty,
              event.momentumPhase.intersection([.ended, .cancelled]).isEmpty else { return false }
        for target in targets.allObjects {
            guard let scroll = target.scrollView, let window = scroll.window,
                  event.window === window, window.attachedSheet == nil,
                  !scroll.isHiddenOrHasHiddenAncestor,
                  let document = scroll.documentView else { continue }
            let clip = scroll.contentView
            guard clip.visibleRect.contains(clip.convert(event.locationInWindow, from: nil)) else { continue }
            let visible = scroll.documentVisibleRect
            let range = max(0, document.bounds.height - visible.height)
            let fromTop = document.isFlipped
                ? visible.minY - document.bounds.minY
                : document.bounds.maxY - visible.maxY
            if event.scrollingDeltaY < 0 {
                return fromTop >= range - 0.5
            }
            return fromTop <= 0.5 && !target.canLoadOlderTurns
        }
        return false
    }
}
