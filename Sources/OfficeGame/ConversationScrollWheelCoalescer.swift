// Preserve wheel movement and gesture boundaries while avoiding a complete
// SwiftUI hit-test for every high-frequency mouse report between display frames.
import AppKit

@MainActor
final class ConversationScrollWheelCoalescer {
    private let now: () -> TimeInterval
    private let deliver: (NSEvent) -> Void
    private var lastDeliveryTime: TimeInterval?
    private var pendingEvent: NSEvent?
    private var timer: Timer?

    init(
        now: @escaping () -> TimeInterval = { ProcessInfo.processInfo.systemUptime },
        deliver: @escaping (NSEvent) -> Void
    ) {
        self.now = now
        self.deliver = deliver
    }

    func process(_ event: NSEvent, isInsideConversation: Bool) -> NSEvent? {
        guard event.type == .scrollWheel, isInsideConversation,
              NSEvent.pressedMouseButtons == 0,
              event.phase.isEmpty || event.phase == .changed,
              event.momentumPhase.isEmpty || event.momentumPhase == .changed,
              event.cgEvent != nil else {
            // Began/ended/cancelled, clicks, and region changes retain ordering.
            flush()
            lastDeliveryTime = nil
            return event
        }
        if let pendingEvent, !Self.canMerge(pendingEvent, event) {
            flush()
            lastDeliveryTime = now()
            return event
        }
        let rate = max(60, event.window?.screen?.maximumFramesPerSecond ?? 60)
        let delay = lastDeliveryTime.map { 1.0 / Double(rate) - (now() - $0) } ?? 0
        if delay <= 0 {
            let result: NSEvent
            if let pendingEvent {
                guard let merged = Self.merging(pendingEvent, event) else {
                    flush()
                    lastDeliveryTime = now()
                    return event
                }
                result = merged
            } else {
                result = event
            }
            clearPending()
            lastDeliveryTime = now()
            return result
        }
        if let pendingEvent {
            guard let merged = Self.merging(pendingEvent, event) else {
                flush()
                lastDeliveryTime = now()
                return event
            }
            self.pendingEvent = merged
        } else {
            pendingEvent = event
        }
        if timer == nil {
            let timer = Timer(timeInterval: delay, repeats: false) { [weak self] _ in
                MainActor.assumeIsolated { self?.flush() }
            }
            RunLoop.main.add(timer, forMode: .common)
            self.timer = timer
        }
        return nil
    }

    func flush() {
        guard let event = pendingEvent else { return }
        clearPending()
        lastDeliveryTime = now()
        deliver(event)
    }

    func cancel() {
        clearPending()
        lastDeliveryTime = nil
    }

    private func clearPending() {
        timer?.invalidate()
        timer = nil
        pendingEvent = nil
    }

    private static func canMerge(_ first: NSEvent, _ next: NSEvent) -> Bool {
        first.windowNumber == next.windowNumber
            && first.locationInWindow == next.locationInWindow
            && first.modifierFlags == next.modifierFlags
            && first.phase == next.phase
            && first.momentumPhase == next.momentumPhase
            && first.hasPreciseScrollingDeltas == next.hasPreciseScrollingDeltas
            && first.isDirectionInvertedFromDevice == next.isDirectionInvertedFromDevice
            && first.scrollingDeltaX * next.scrollingDeltaX >= 0
            && first.scrollingDeltaY * next.scrollingDeltaY >= 0
    }

    private static func merging(_ first: NSEvent, _ next: NSEvent) -> NSEvent? {
        guard canMerge(first, next), let previous = first.cgEvent,
              let incoming = next.cgEvent, let combined = incoming.copy() else { return nil }
        // AppKit reads different fields for line, precise-pixel and accelerated
        // devices. Adding only deltaY would silently lose movement on MX mice.
        let integerFields: [CGEventField] = [
            .scrollWheelEventDeltaAxis1, .scrollWheelEventDeltaAxis2, .scrollWheelEventDeltaAxis3,
            .scrollWheelEventPointDeltaAxis1, .scrollWheelEventPointDeltaAxis2, .scrollWheelEventPointDeltaAxis3,
        ]
        for field in integerFields {
            let (sum, overflow) = previous.getIntegerValueField(field).addingReportingOverflow(
                incoming.getIntegerValueField(field)
            )
            guard !overflow else { return nil }
            combined.setIntegerValueField(field, value: sum)
        }
        // Some setters also update related fields. Always read both addends
        // from the untouched source events, never from the partially built copy.
        let doubleFields: [CGEventField] = [
            .scrollWheelEventFixedPtDeltaAxis1, .scrollWheelEventFixedPtDeltaAxis2, .scrollWheelEventFixedPtDeltaAxis3,
            .scrollWheelEventAcceleratedDeltaAxis1, .scrollWheelEventAcceleratedDeltaAxis2,
            .scrollWheelEventRawDeltaAxis1, .scrollWheelEventRawDeltaAxis2,
        ]
        for field in doubleFields {
            let sum = previous.getDoubleValueField(field) + incoming.getDoubleValueField(field)
            guard sum.isFinite else { return nil }
            combined.setDoubleValueField(field, value: sum)
        }
        guard let result = NSEvent(cgEvent: combined), result.windowNumber == next.windowNumber,
              result.locationInWindow == next.locationInWindow,
              result.modifierFlags == next.modifierFlags,
              result.phase == next.phase, result.momentumPhase == next.momentumPhase,
              result.hasPreciseScrollingDeltas == next.hasPreciseScrollingDeltas,
              result.isDirectionInvertedFromDevice == next.isDirectionInvertedFromDevice,
              abs(result.scrollingDeltaX - (first.scrollingDeltaX + next.scrollingDeltaX)) < 0.0001,
              abs(result.scrollingDeltaY - (first.scrollingDeltaY + next.scrollingDeltaY)) < 0.0001 else { return nil }
        return result
    }
}
