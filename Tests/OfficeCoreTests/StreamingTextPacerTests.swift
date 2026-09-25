// 이 파일은 긴 한글과 이모지 응답의 적응형 타자 표시량을 검증한다.

import AppKit
import SwiftUI
import XCTest
@testable import OfficeCore
@testable import OfficeGame

@MainActor
final class StreamingTextPacerTests: XCTestCase {
    func testUnboundedWidthProbePreservesStreamingViewport() {
        let view = IncrementalStreamingTextView(fontSize: 14, lineSpacing: 3)
        view.apply(
            source: String(repeating: "실시간으로 늘어나는 긴 본문입니다. ", count: 40),
            animates: false
        )
        let width: CGFloat = 496
        let height = view.heightThatFits(width: width)
        view.frame = NSRect(x: 0, y: 0, width: width, height: height)
        let textView = view.subviews.compactMap { $0 as? NSTextView }.first
        let containerWidth = textView?.textContainer?.containerSize.width

        for _ in 0..<30 {
            XCTAssertEqual(view.heightThatFits(width: .infinity), height)
            XCTAssertEqual(view.heightThatFits(width: .greatestFiniteMagnitude), height)
            XCTAssertEqual(view.heightThatFits(width: width), height)
        }

        XCTAssertEqual(textView?.textContainer?.containerSize.width, containerWidth)
    }

    func testEmptyBacklogHasNoImmediatePrefix() {
        XCTAssertEqual(
            StreamingTextPacer.immediatelyVisibleCharacterCount(
                remainingCharacterCount: 0
            ),
            0
        )
    }

    func testShortBacklogKeepsOnlyAnimatedTail() {
        XCTAssertEqual(
            StreamingTextPacer.immediatelyVisibleCharacterCount(
                remainingCharacterCount: 180
            ),
            0
        )
        XCTAssertEqual(
            StreamingTextPacer.immediatelyVisibleCharacterCount(
                remainingCharacterCount: 181
            ),
            1
        )
    }

    func testLargeBacklogAppearsImmediatelyExceptForTail() {
        XCTAssertEqual(
            StreamingTextPacer.immediatelyVisibleCharacterCount(
                remainingCharacterCount: 1_920
            ),
            1_740
        )
        XCTAssertEqual(
            StreamingTextPacer.immediatelyVisibleCharacterCount(
                remainingCharacterCount: 20_000
            ),
            19_820
        )
    }

    func testTailAnimationPreservesUnicodeAndMarkdownText() {
        let target = String(
            repeating: "한글🙂 **굵게**\n| 열 | 값 |\n",
            count: 80
        )
        var rendered = ""
        var index = target.startIndex
        var remaining = target.count
        let immediatelyVisibleCount =
            StreamingTextPacer.immediatelyVisibleCharacterCount(
                remainingCharacterCount: remaining
            )

        if immediatelyVisibleCount > 0 {
            let nextIndex = target.index(
                index,
                offsetBy: immediatelyVisibleCount
            )
            rendered.append(contentsOf: target[index ..< nextIndex])
            index = nextIndex
            remaining -= immediatelyVisibleCount
        }

        var animatedFrameCount = 0
        while remaining > 0 {
            let nextIndex = target.index(after: index)
            rendered.append(contentsOf: target[index ..< nextIndex])
            index = nextIndex
            remaining -= 1
            animatedFrameCount += 1
            XCTAssertTrue(target.hasPrefix(rendered))
        }

        XCTAssertEqual(rendered, target)
        XCTAssertLessThanOrEqual(
            animatedFrameCount,
            StreamingTextPacer.animatedTailCharacterCount
        )
    }

    func testUpdatePlanAnimatesOnlyNewUnicodeSuffix() {
        let plan = StreamingTextPacer.updatePlan(
            current: "한글🙂",
            target: "한글🙂 문장이 이어집니다.",
            animates: true
        )

        XCTAssertEqual(plan.immediateText, "한글🙂")
        XCTAssertEqual(
            String(plan.animatedCharacters),
            " 문장이 이어집니다."
        )
    }

    func testUpdatePlanHandlesReplacementAndDisabledAnimation() {
        let replacement = StreamingTextPacer.updatePlan(
            current: "이전 응답",
            target: "새 응답",
            animates: true
        )
        XCTAssertEqual(replacement.immediateText, "")
        XCTAssertEqual(String(replacement.animatedCharacters), "새 응답")

        let immediate = StreamingTextPacer.updatePlan(
            current: "이전 응답",
            target: "새 응답",
            animates: false
        )
        XCTAssertEqual(immediate.immediateText, "새 응답")
        XCTAssertTrue(immediate.animatedCharacters.isEmpty)
    }

    func testFullLinePlanStartsEmptyAndCompletesWithinOneSecond() {
        let target = String(repeating: "가", count: 1_000)
        let plan = StreamingTextPacer.fullLineUpdatePlan(
            current: "",
            target: target,
            animates: true
        )

        XCTAssertEqual(plan.immediateText, "")
        XCTAssertEqual(String(plan.animatedCharacters), target)
        XCTAssertEqual(plan.charactersPerTick, 1)
        XCTAssertEqual(
            plan.animationDuration,
            StreamingTextPacer.maximumLineRevealDuration
        )
    }

    func testFullLinePlanKeepsShortLinesSmooth() {
        let plan = StreamingTextPacer.fullLineUpdatePlan(
            current: "",
            target: "짧은 문장",
            animates: true
        )

        XCTAssertEqual(plan.charactersPerTick, 1)
        XCTAssertEqual(String(plan.animatedCharacters), "짧은 문장")
    }

    func testFullLineDurationIsProportionalThenCapsWithoutSawtooth() {
        var previousDuration = TimeInterval.zero
        for count in 1...2_000 {
            let plan = StreamingTextPacer.fullLineUpdatePlan(
                current: "",
                target: String(repeating: "a", count: count),
                animates: true
            )
            let duration = plan.animationDuration ?? .infinity
            let expected = min(
                StreamingTextPacer.maximumLineRevealDuration,
                TimeInterval(count)
                    * StreamingTextPacer.animationTickInterval
            )
            XCTAssertEqual(duration, expected, accuracy: 0.000_001)
            XCTAssertGreaterThanOrEqual(duration, previousDuration)
            previousDuration = duration
        }

        let fifty = StreamingTextPacer.fullLineUpdatePlan(
            current: "",
            target: String(repeating: "a", count: 50),
            animates: true
        )
        let fiftyOne = StreamingTextPacer.fullLineUpdatePlan(
            current: "",
            target: String(repeating: "a", count: 51),
            animates: true
        )
        XCTAssertEqual(
            fifty.animationDuration ?? .infinity,
            0.8,
            accuracy: 0.000_001
        )
        XCTAssertEqual(
            fiftyOne.animationDuration ?? .infinity,
            0.8,
            accuracy: 0.000_001
        )
    }

    func testElapsedRevealCatchesUpAfterDelayedTick() {
        XCTAssertEqual(
            StreamingTextPacer.elapsedRevealCharacterCount(
                totalCharacterCount: 1_000,
                minimumCharacterCount: 18,
                elapsed: 0.45,
                duration: 0.9
            ),
            500
        )
        XCTAssertEqual(
            StreamingTextPacer.elapsedRevealCharacterCount(
                totalCharacterCount: 1_000,
                minimumCharacterCount: 18,
                elapsed: 1.2,
                duration: 0.9
            ),
            1_000
        )
    }

    func testImmediateFullLineCompletionFiresOncePerRevision() async {
        let view = IncrementalStreamingTextView(
            fontSize: 14,
            lineSpacing: 3
        )
        let completion = expectation(description: "즉시 완료")
        completion.assertForOverFulfill = true
        var completionCount = 0
        view.onFinishedTyping = {
            completionCount += 1
            completion.fulfill()
        }

        view.apply(
            source: "완료",
            animates: false,
            revealMode: .fullLine
        )
        await fulfillment(of: [completion], timeout: 1)
        view.apply(
            source: "완료",
            animates: false,
            revealMode: .fullLine
        )
        try? await Task.sleep(for: .milliseconds(20))

        XCTAssertEqual(completionCount, 1)
    }

    /// 코드 펜스 안의 빈 줄은 원문이 ""이라 초기 targetText와 같다.
    /// 이때도 완료가 통보돼야 줄 단위 타자가 다음 줄로 넘어간다.
    func testEmptySourceStillReportsCompletion() async {
        let view = IncrementalStreamingTextView(
            fontSize: 14,
            lineSpacing: 3
        )
        let completion = expectation(description: "빈 줄 완료")
        view.onFinishedTyping = {
            completion.fulfill()
        }

        view.apply(
            source: "",
            animates: false,
            revealMode: .fullLine
        )

        await fulfillment(of: [completion], timeout: 1)
    }

    func testReplacedImmediateRevisionIgnoresStaleCompletion() async {
        let view = IncrementalStreamingTextView(
            fontSize: 14,
            lineSpacing: 3
        )
        let completion = expectation(description: "최신 revision만 완료")
        completion.assertForOverFulfill = true
        var completionCount = 0
        view.onFinishedTyping = {
            completionCount += 1
            completion.fulfill()
        }

        view.apply(
            source: "교체 전",
            animates: false,
            revealMode: .fullLine
        )
        view.apply(
            source: "교체 후",
            animates: false,
            revealMode: .fullLine
        )
        await fulfillment(of: [completion], timeout: 1)

        XCTAssertEqual(completionCount, 1)
    }

    func testMountedLongLineCompletesWithinWallClockBudget() async {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 200),
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        let view = IncrementalStreamingTextView(
            fontSize: 14,
            lineSpacing: 3
        )
        view.frame = window.contentView?.bounds ?? .zero
        window.contentView = view
        let completion = expectation(description: "긴 한 줄 타이핑 완료")
        let startedAt = ProcessInfo.processInfo.systemUptime
        var completedAt: TimeInterval?
        view.onFinishedTyping = {
            completedAt = ProcessInfo.processInfo.systemUptime
            completion.fulfill()
        }

        view.apply(
            source: String(repeating: "가", count: 1_000),
            animates: true,
            revealMode: .fullLine
        )
        await fulfillment(of: [completion], timeout: 1.2)

        XCTAssertLessThan(
            (completedAt ?? .infinity) - startedAt,
            1.0
        )
        window.contentView = nil
    }

    func testFullLineTypingDoesNotAdvanceDuringEventTracking() {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 240, height: 160),
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        let scrollView = NSScrollView(frame: window.contentView?.bounds ?? .zero)
        let documentView = NSView(
            frame: NSRect(x: 0, y: 0, width: 240, height: 160)
        )
        let view = IncrementalStreamingTextView(
            fontSize: 14,
            lineSpacing: 3
        )
        view.frame = documentView.bounds
        documentView.addSubview(view)
        scrollView.documentView = documentView
        window.contentView = scrollView
        view.apply(
            source: String(repeating: "스크롤 중에는 멈춥니다. ", count: 80),
            animates: true,
            revealMode: .fullLine
        )
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.05))

        guard let textView = view.subviews.first as? NSTextView else {
            XCTFail("타이핑 NSTextView를 찾지 못했습니다.")
            window.contentView = nil
            return
        }
        let beforeEventTracking = textView.string

        NotificationCenter.default.post(
            name: NSScrollView.willStartLiveScrollNotification,
            object: scrollView
        )
        let deadline = Date(timeIntervalSinceNow: 0.95)
        while Date() < deadline {
            _ = RunLoop.main.run(
                mode: .eventTracking,
                before: deadline
            )
        }

        XCTAssertEqual(
            textView.string,
            beforeEventTracking,
            "live-scroll event tracking 중에도 타이핑 timer가 실행됐습니다."
        )
        NotificationCenter.default.post(
            name: NSScrollView.didEndLiveScrollNotification,
            object: scrollView
        )
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.05))
        XCTAssertNotEqual(
            textView.string,
            beforeEventTracking,
            "event tracking이 끝난 뒤 타이핑이 재개되지 않았습니다."
        )
        XCTAssertLessThan(
            textView.string.count,
            String(repeating: "스크롤 중에는 멈춥니다. ", count: 80).count,
            "긴 live-scroll 뒤 현재 줄이 한 번에 완료됐습니다."
        )
        window.contentView = nil
    }

    func testFullLineTypingKeepsIntrinsicHeightStableAcrossTicks() async {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 180, height: 240),
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        let view = IncrementalStreamingTextView(
            fontSize: 14,
            lineSpacing: 3
        )
        view.frame = window.contentView?.bounds ?? .zero
        window.contentView = view
        view.apply(
            source: String(repeating: "높이가 바뀌지 않는 긴 문장 ", count: 100),
            animates: true,
            revealMode: .fullLine
        )
        let initialHeight = view.intrinsicContentSize.height

        try? await Task.sleep(for: .milliseconds(450))
        guard let textView = view.subviews.first as? NSTextView else {
            XCTFail("타이핑 NSTextView를 찾지 못했습니다.")
            window.contentView = nil
            return
        }
        XCTAssertGreaterThan(textView.string.count, 100)
        XCTAssertEqual(
            view.intrinsicContentSize.height,
            initialHeight,
            accuracy: 0.5,
            "fullLine tick이 intrinsic height를 다시 변경했습니다."
        )
        window.contentView = nil
    }

    func testCompletedLineTypingReservesFinalMarkdownHeight() {
        let source = "## " + String(
            repeating: "완성된 Markdown 높이를 먼저 확보합니다. ",
            count: 18
        )
        let width = CGFloat(220)
        let typingController = NSHostingController(
            rootView: CompletedResponseLineTypingView(
                typingIdentity: "height-reservation",
                source: source,
                fontSize: 14,
                fileBaseDirectory: nil,
                animates: true,
                animatesInitialSource: true,
                presentsTyping: true,
                onFinishedTyping: {}
            )
            .environment(\._accessibilityReduceMotion, false)
            .frame(width: width)
        )
        let markdownController = NSHostingController(
            rootView: ConversationMarkdownView(
                source: source,
                fontSize: 14,
                fileBaseDirectory: nil
            )
            .frame(width: width)
        )
        let proposal = NSSize(width: width, height: 10_000)

        let typingHeight = typingController.sizeThatFits(in: proposal).height
        let markdownHeight = markdownController.sizeThatFits(
            in: proposal
        ).height
        XCTAssertGreaterThanOrEqual(
            typingHeight,
            markdownHeight,
            "타이핑 시작 전 최종 Markdown 높이가 예약되지 않았습니다."
        )
        XCTAssertLessThanOrEqual(
            typingHeight - markdownHeight,
            5.5,
            "높이 예약이 줄 사이 간격보다 크게 문서 높이를 늘렸습니다."
        )
    }

    func testCompletedLineTypingReservesLongMarkdownLinkSourceHeight() {
        let source = "[짧은 제목]("
            + String(
                repeating: "https://example.com/very-long-path/",
                count: 20
            )
            + ")"
        let width = CGFloat(220)
        let typingController = NSHostingController(
            rootView: CompletedResponseLineTypingView(
                typingIdentity: "long-link-height-reservation",
                source: source,
                fontSize: 14,
                fileBaseDirectory: nil,
                animates: true,
                animatesInitialSource: true,
                presentsTyping: true,
                onFinishedTyping: {}
            )
            .environment(\._accessibilityReduceMotion, false)
            .frame(width: width)
        )
        let rawTextController = NSHostingController(
            rootView: Text(source)
                .font(.system(size: 14))
                .lineSpacing(3)
                .fixedSize(horizontal: false, vertical: true)
                .frame(width: width, alignment: .leading)
        )
        let proposal = NSSize(width: width, height: 10_000)

        XCTAssertGreaterThanOrEqual(
            typingController.sizeThatFits(in: proposal).height,
            rawTextController.sizeThatFits(in: proposal).height,
            "긴 Markdown 링크 원문이 타이핑 중 잘릴 수 있습니다."
        )
    }

    func testCompletedTypingInsideLazyScrollKeepsDocumentHeightStable() {
        let source = String(
            repeating: "스크롤과 동시에 표시되는 긴 완성 응답입니다. ",
            count: 70
        )
        let rootView = ScrollView {
            LazyVStack(alignment: .leading, spacing: 8) {
                CompletedResponseLineTypingView(
                    typingIdentity: "lazy-scroll-height",
                    source: source,
                    fontSize: 14,
                    fileBaseDirectory: nil,
                    animates: true,
                    animatesInitialSource: true,
                    presentsTyping: true,
                    onFinishedTyping: {}
                )
                Color.clear.frame(height: 600)
            }
            .frame(width: 240)
        }
        .environment(\._accessibilityReduceMotion, false)
        .frame(width: 260, height: 180)
        let hostingView = NSHostingView(rootView: rootView)
        hostingView.frame = NSRect(x: 0, y: 0, width: 260, height: 180)
        let window = NSWindow(
            contentRect: hostingView.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = hostingView
        hostingView.layoutSubtreeIfNeeded()
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.2))
        hostingView.layoutSubtreeIfNeeded()

        guard
            let scrollView = firstDescendant(
                of: hostingView,
                as: NSScrollView.self
            ),
            let documentView = scrollView.documentView,
            let typingView = firstDescendant(
                of: hostingView,
                as: IncrementalStreamingTextView.self
            ),
            let textView = typingView.subviews.first as? NSTextView
        else {
            XCTFail("LazyVStack 안의 실제 타이핑 NSView를 찾지 못했습니다.")
            window.contentView = nil
            return
        }
        XCTAssertTrue(typingView.window === window)
        XCTAssertGreaterThan(textView.string.count, 0)
        XCTAssertLessThan(textView.string.count, source.count)

        documentView.postsFrameChangedNotifications = true
        var documentFrameChanges = 0
        let registration = NotificationCenter.default.addObserver(
            forName: NSView.frameDidChangeNotification,
            object: documentView,
            queue: .main
        ) { _ in
            documentFrameChanges += 1
        }
        defer {
            NotificationCenter.default.removeObserver(registration)
            window.contentView = nil
        }

        let textBeforeLiveScroll = textView.string
        var minimumDocumentHeight = documentView.frame.height
        var maximumDocumentHeight = documentView.frame.height
        NotificationCenter.default.post(
            name: NSScrollView.willStartLiveScrollNotification,
            object: scrollView
        )
        for step in 0..<120 {
            let scrollableY = max(
                0,
                documentView.frame.height - scrollView.contentView.bounds.height
            )
            let offset = min(scrollableY, CGFloat(step % 30) * 2)
            scrollView.contentView.scroll(
                to: NSPoint(x: 0, y: offset)
            )
            _ = RunLoop.main.run(
                mode: .eventTracking,
                before: Date(timeIntervalSinceNow: 0.003)
            )
            hostingView.layoutSubtreeIfNeeded()
            minimumDocumentHeight = min(
                minimumDocumentHeight,
                documentView.frame.height
            )
            maximumDocumentHeight = max(
                maximumDocumentHeight,
                documentView.frame.height
            )
        }
        XCTAssertTrue(typingView.window === window)
        XCTAssertEqual(
            textView.string,
            textBeforeLiveScroll,
            "실제 LazyVStack 안에서 live-scroll 중 타이핑이 진행됐습니다."
        )
        XCTAssertLessThanOrEqual(
            maximumDocumentHeight - minimumDocumentHeight,
            0.5,
            "타이핑 활성 구간의 live-scroll 중 문서 높이가 변했습니다."
        )
        XCTAssertLessThanOrEqual(
            documentFrameChanges,
            5,
            "live-scroll 중 문서 프레임이 반복해서 무효화됐습니다."
        )

        NotificationCenter.default.post(
            name: NSScrollView.didEndLiveScrollNotification,
            object: scrollView
        )
        let textCountBeforeResume = textView.string.count
        minimumDocumentHeight = documentView.frame.height
        maximumDocumentHeight = documentView.frame.height
        let frameChangesBeforeResume = documentFrameChanges
        let resumeDeadline = Date(timeIntervalSinceNow: 0.25)
        while Date() < resumeDeadline {
            RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.016))
            hostingView.layoutSubtreeIfNeeded()
            minimumDocumentHeight = min(
                minimumDocumentHeight,
                documentView.frame.height
            )
            maximumDocumentHeight = max(
                maximumDocumentHeight,
                documentView.frame.height
            )
        }

        XCTAssertGreaterThan(
            textView.string.count,
            textCountBeforeResume,
            "live-scroll 종료 뒤 실제 타이핑이 재개되지 않았습니다."
        )
        XCTAssertLessThan(
            textView.string.count,
            source.count,
            "긴 live-scroll 뒤 줄 전체가 한 번에 나타났습니다."
        )
        XCTAssertLessThanOrEqual(
            maximumDocumentHeight - minimumDocumentHeight,
            0.5,
            "타이핑이 진행되는 동안 문서 높이가 tick마다 변했습니다."
        )
        XCTAssertLessThanOrEqual(
            documentFrameChanges - frameChangesBeforeResume,
            5,
            "타이핑이 진행되는 동안 문서 프레임이 반복해서 무효화됐습니다."
        )
    }

    func testTypingMountedAfterScrollStartPausesOnFirstLiveUpdate() {
        let scrollView = NSScrollView(
            frame: NSRect(x: 0, y: 0, width: 260, height: 180)
        )
        let documentView = NSView(
            frame: NSRect(x: 0, y: 0, width: 260, height: 800)
        )
        scrollView.documentView = documentView
        let window = NSWindow(
            contentRect: scrollView.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = scrollView

        let typingView = IncrementalStreamingTextView(
            fontSize: 14,
            lineSpacing: 3
        )
        typingView.frame = NSRect(x: 0, y: 0, width: 240, height: 120)

        // 실제 회귀 순서: 기존 피드에서 gesture가 시작된 뒤 직원 전환으로
        // 새 타이핑 뷰가 mount된다. 새 뷰는 willStart를 받을 수 없다.
        NotificationCenter.default.post(
            name: NSScrollView.willStartLiveScrollNotification,
            object: scrollView
        )
        documentView.addSubview(typingView)
        let source = String(
            repeating: "전환 직후 스크롤 중에는 타자를 멈춥니다. ",
            count: 120
        )
        typingView.apply(
            source: source,
            animates: true,
            revealMode: .fullLine
        )
        NotificationCenter.default.post(
            name: NSScrollView.didLiveScrollNotification,
            object: scrollView
        )

        guard let textView = typingView.subviews.first as? NSTextView else {
            XCTFail("타이핑 NSTextView를 찾지 못했습니다.")
            window.contentView = nil
            return
        }
        let pausedText = textView.string
        RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.12))
        XCTAssertEqual(
            textView.string,
            pausedText,
            "willStart 뒤 mount된 뷰가 didLiveScroll을 놓쳐 타이핑했습니다."
        )

        NotificationCenter.default.post(
            name: NSScrollView.didEndLiveScrollNotification,
            object: scrollView
        )
        let pausedCount = textView.string.count
        XCTAssertTrue(
            waitUntil(timeout: 0.5) {
                textView.string.count > pausedCount
            },
            "live-scroll 종료 뒤 늦게 mount된 타이핑이 재개되지 않았습니다."
        )
        window.contentView = nil
    }

    private func waitUntil(
        timeout: TimeInterval,
        pollInterval: TimeInterval = 0.016,
        condition: () -> Bool
    ) -> Bool {
        let deadline = ProcessInfo.processInfo.systemUptime + timeout
        repeat {
            if condition() {
                return true
            }
            let remaining = max(
                0,
                deadline - ProcessInfo.processInfo.systemUptime
            )
            RunLoop.main.run(
                until: Date(
                    timeIntervalSinceNow: min(pollInterval, remaining)
                )
            )
        } while ProcessInfo.processInfo.systemUptime < deadline
        return condition()
    }

    private func firstDescendant<ViewType: NSView>(
        of root: NSView,
        as type: ViewType.Type
    ) -> ViewType? {
        for subview in root.subviews {
            if let match = subview as? ViewType {
                return match
            }
            if let match = firstDescendant(of: subview, as: type) {
                return match
            }
        }
        return nil
    }
}
