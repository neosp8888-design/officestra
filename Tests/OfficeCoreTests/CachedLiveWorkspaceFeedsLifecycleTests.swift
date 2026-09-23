import AppKit
import OfficeCore
import SwiftUI
import XCTest
@testable import OfficeGame

@MainActor
final class CachedLiveWorkspaceFeedsLifecycleTests: XCTestCase {
    func testMeasureContinuousResizeWorkload() async throws {
        guard let output = ProcessInfo.processInfo.environment["OFFICESTRA_RESIZE_PERF"] else {
            throw XCTSkip("Set OFFICESTRA_RESIZE_PERF to measure the same resize workload before/after")
        }
        let director = AgentDirector(startBackgroundTasks: false)
        director.liveFeedStore.replace(with: (0..<10).map { index in
            makeTurn(id: "resize-perf-\(index)", characterID: OfficeCharacter.boss.rawValue,
                prompt: "질문 \(index)",
                response: (0..<24).map { "## 항목 \(index)-\($0)\n\n긴 대화의 **줄바꿈과 배치** 비용을 측정하는 문장입니다. 내용과 선택 효과를 유지합니다.\n\n| 항목 | 결과 |\n| --- | --- |\n| \($0) | 확인 |\n" }.joined(separator: "\n"),
                status: .completed, startedAt: Date(timeIntervalSinceReferenceDate: Double(400_000 + index)), backend: .codex)
        })
        director.liveFeedStore.finishInitialLoading()
        let container = CachedLiveWorkspaceFeedsNSView(frame: NSRect(x: 0, y: 0, width: 900, height: 700))
        let window = NSWindow(contentRect: container.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = container
        defer { container.tearDown(); window.contentView = nil }
        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        let mounted = try await waitNaturallyUntil(timeout: .seconds(6)) {
            container.activeCharacterIDForTesting == .boss && container.visibleCardCountForTesting > 0
        }
        XCTAssertTrue(mounted)
        let resolvedScroll = try await waitForPrimaryScrollView(in: container)
        let scroll = try XCTUnwrap(resolvedScroll)
        performLiveScroll(scroll, toTop: false)
        try await settle(for: .milliseconds(200))
        func documents(_ view: NSView) -> [SelectableMarkdownDocumentView] {
            (view as? SelectableMarkdownDocumentView).map { [$0] } ?? view.subviews.flatMap(documents)
        }
        var results: [String: [String: Double]] = [:]
        for dimension in ["width", "height", "width-monotonic"] {
            let phaseStart = ProcessInfo.processInfo.systemUptime
            let cpuStart = clock()
            let layoutStart = container.resizeForcedLayoutCountForTesting
            if ProcessInfo.processInfo.environment["OFFICESTRA_RESIZE_LIVE"] == "1" {
                container.viewWillStartLiveResize()
            }
            var durations: [Double] = []
            let texts = documents(container)
            let before = texts.reduce(0) { $0 + $1.textLayoutMeasurementCount }
            for step in 0..<40 {
                let offset = CGFloat(dimension == "width-monotonic" ? step : (step < 20 ? step : 39 - step)) * 3
                let size = NSSize(width: 900 + (dimension.hasPrefix("width") ? offset : 0), height: 700 - (dimension == "height" ? offset : 0))
                let start = ProcessInfo.processInfo.systemUptime
                container.setFrameSize(size)
                container.layoutSubtreeIfNeeded()
                durations.append((ProcessInfo.processInfo.systemUptime - start) * 1000)
                try await settle(for: .milliseconds(8))
            }
            container.viewDidEndLiveResize()
            try await settle(for: .milliseconds(200))
            results[dimension] = ["meanMS": durations.reduce(0,+) / Double(durations.count),
                "p95MS": durations.sorted()[37], "maxMS": durations.max() ?? 0,
                "textMeasurements": Double(texts.reduce(0) { $0 + $1.textLayoutMeasurementCount } - before),
                "viewportUpdates": Double(container.resizeForcedLayoutCountForTesting - layoutStart),
                "phaseElapsedMS": (ProcessInfo.processInfo.systemUptime - phaseStart) * 1000,
                "phaseCPUMS": Double(clock() - cpuStart) / Double(CLOCKS_PER_SEC) * 1000]
            XCTAssertGreaterThan(container.visibleCardCountForTesting, 0)
            XCTAssertEqual(container.activeCharacterIDForTesting, .boss)
        }
        try JSONSerialization.data(withJSONObject: results, options: [.prettyPrinted, .sortedKeys]).write(to: URL(fileURLWithPath: output))
    }

    func testLiveResizeCoalescesBurstAndFlushesFinalViewport() async throws {
        let director = AgentDirector(startBackgroundTasks: false)
        director.liveFeedStore.replace(with: makeTurns(completedResponse:
            String(repeating: "긴 응답과 줄바꿈 확인입니다. ", count: 100)))
        director.liveFeedStore.finishInitialLoading()
        let container = CachedLiveWorkspaceFeedsNSView(frame: NSRect(x: 0, y: 0, width: 900, height: 700))
        let window = NSWindow(contentRect: container.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = container
        defer { container.tearDown(); window.contentView = nil }
        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        let mounted = try await waitNaturallyUntil(timeout: .seconds(6)) {
            container.activeCharacterIDForTesting == .boss && container.visibleCardCountForTesting > 0
        }
        XCTAssertTrue(mounted)
        let resolved = try await waitForPrimaryScrollView(in: container)
        let scroll = try XCTUnwrap(resolved)
        performLiveScroll(scroll, toTop: false)
        try await settle(for: .milliseconds(100))
        let layouts = container.resizeForcedLayoutCountForTesting
        let searches = container.primaryScrollSearchCountForTesting
        container.viewWillStartLiveResize()
        for step in 1...100 {
            container.setFrameSize(NSSize(width: 900 - CGFloat(step), height: 700 - CGFloat(step)))
            container.layoutSubtreeIfNeeded()
        }
        XCTAssertLessThan(container.resizeForcedLayoutCountForTesting - layouts, 10,
            "A queued resize burst must not reflow the transcript for every intermediate size")
        container.viewDidEndLiveResize()
        try await settle(for: .milliseconds(250))
        XCTAssertTrue(container.subviews.allSatisfy { $0.frame == container.bounds })
        XCTAssertLessThan(container.primaryScrollSearchCountForTesting - searches, 5)
        XCTAssertGreaterThan(container.visibleCardCountForTesting, 0)
        let document = try XCTUnwrap(scroll.documentView)
        let snapshot = LiveWorkspaceFeedScrollGeometry.snapshot(documentBounds: document.bounds,
            visibleRect: scroll.documentVisibleRect, isFlipped: document.isFlipped)
        XCTAssertLessThanOrEqual(snapshot.distanceFromBottom, 20)
        let settledLayouts = container.resizeForcedLayoutCountForTesting
        try await settle(for: .milliseconds(150))
        XCTAssertEqual(container.resizeForcedLayoutCountForTesting, settledLayouts,
            "No resize timer should keep laying out an idle conversation")

        performSmallLiveScroll(scroll, delta: -300)
        try await settle(for: .milliseconds(100))
        container.viewWillStartLiveResize()
        for step in 1...40 {
            container.setFrameSize(NSSize(width: 800 - CGFloat(step), height: 600))
            container.layoutSubtreeIfNeeded()
        }
        container.viewDidEndLiveResize()
        try await settle(for: .milliseconds(150))
        let readingSnapshot = LiveWorkspaceFeedScrollGeometry.snapshot(documentBounds: document.bounds,
            visibleRect: scroll.documentVisibleRect, isFlipped: document.isFlipped)
        XCTAssertGreaterThan(readingSnapshot.distanceFromBottom, 20,
            "Resizing while reading history must not resume following the latest message")
        XCTAssertGreaterThan(container.visibleCardCountForTesting, 0)

        // A pending timer must not revive a detached host after closing the pane.
        container.viewWillStartLiveResize()
        container.setFrameSize(NSSize(width: 810, height: 600))
        container.layoutSubtreeIfNeeded()
        container.setFrameSize(NSSize(width: 820, height: 600))
        container.layoutSubtreeIfNeeded()
        container.tearDown()
        try await settle(for: .milliseconds(100))
        XCTAssertEqual(container.liveHostingViewCountForTesting, 0)
    }

    func testScrollObserverCoalescesLogitechStyleBurstPerGesture() async throws {
        let scrollView = NSScrollView(
            frame: NSRect(x: 0, y: 0, width: 700, height: 500)
        )
        let documentView = FlippedScrollDocumentView(
            frame: NSRect(x: 0, y: 0, width: 700, height: 6_000)
        )
        scrollView.documentView = documentView

        var metricsCount = 0
        var userScrollStartedCount = 0
        var userScrollActivityCount = 0
        var userScrollCompletionCount = 0
        var topLoadCount = 0
        var topLoadGate = LiveWorkspaceFeedTopLoadGate()
        let coordinator = LiveWorkspaceFeedScrollObserver.Coordinator(
            onMetrics: { _ in
                metricsCount += 1
            },
            onUserScrollStarted: {
                userScrollStartedCount += 1
            },
            onUserScrollActivity: {
                userScrollActivityCount += 1
            },
            onUserScroll: { snapshot in
                userScrollCompletionCount += 1
                if topLoadGate.shouldLoad(
                    distanceFromTop: snapshot.distanceFromTop,
                    threshold: 120,
                    isProgrammaticScrollInFlight: false
                ) {
                    topLoadCount += 1
                }
            }
        )
        coordinator.attach(to: scrollView)
        defer {
            coordinator.detach()
        }
        try await settle(for: .milliseconds(50))

        metricsCount = 0
        userScrollStartedCount = 0
        userScrollActivityCount = 0
        userScrollCompletionCount = 0
        topLoadCount = 0

        let notificationCenter = NotificationCenter.default
        notificationCenter.post(
            name: NSScrollView.willStartLiveScrollNotification,
            object: scrollView
        )
        for index in 0..<300 {
            let remainingDistance = max(0, 5_500 - CGFloat(index) * 24)
            scrollView.contentView.scroll(
                to: NSPoint(x: 0, y: remainingDistance)
            )
            scrollView.reflectScrolledClipView(scrollView.contentView)
            notificationCenter.post(
                name: NSScrollView.didLiveScrollNotification,
                object: scrollView
            )
        }
        notificationCenter.post(
            name: NSScrollView.didEndLiveScrollNotification,
            object: scrollView
        )
        try await settle(for: .milliseconds(100))

        XCTAssertEqual(userScrollStartedCount, 1)
        XCTAssertEqual(
            userScrollActivityCount,
            1,
            "300회 wheel burst가 user activity callback "
                + "\(userScrollActivityCount)회로 그대로 증폭됐습니다."
        )
        XCTAssertLessThanOrEqual(
            metricsCount,
            2,
            "한 run-loop의 300회 bounds 변경이 metrics callback "
                + "\(metricsCount)회로 증폭됐습니다."
        )
        XCTAssertEqual(userScrollCompletionCount, 1)
        XCTAssertEqual(
            topLoadCount,
            1,
            "한 wheel gesture에서 이전 대화 로딩은 한 번만 허용됩니다."
        )

        let settledCounts = (
            metricsCount,
            userScrollActivityCount,
            userScrollCompletionCount,
            topLoadCount
        )
        try await settle(for: .seconds(2))
        XCTAssertEqual(metricsCount, settledCounts.0)
        XCTAssertEqual(userScrollActivityCount, settledCounts.1)
        XCTAssertEqual(userScrollCompletionCount, settledCounts.2)
        XCTAssertEqual(topLoadCount, settledCounts.3)
    }

    func testHostedLiveScrollStressQuiesces() async throws {
        let director = AgentDirector(startBackgroundTasks: false)
        director.liveFeedStore.replace(with: makeTurns(streamingStep: 1))
        director.liveFeedStore.finishInitialLoading()

        let rootHost = NSHostingView(
            rootView: CachedLiveWorkspaceFeeds(director: director)
                .frame(width: 900, height: 700)
        )
        rootHost.frame = NSRect(x: 0, y: 0, width: 900, height: 700)
        let window = NSWindow(
            contentRect: rootHost.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = rootHost
        rootHost.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(200))
        XCTAssertTrue(
            rootHost.window === window,
            "실제 window가 없으면 scroll observer와 live feed AppKit 뷰가 "
                + "mount되지 않습니다."
        )

        guard let container = allDescendants(of: rootHost)
            .compactMap({ $0 as? CachedLiveWorkspaceFeedsNSView })
            .first
        else {
            XCTFail("SwiftUI representable이 live feed container를 만들지 못했습니다.")
            window.contentView = nil
            return
        }

        defer {
            container.tearDown()
            window.contentView = nil
        }

        // 실제 NSWindow 안에서 representable update, NSScrollView observer,
        // 타자 뷰와 자동 하단 이동을 함께 동작하게 한다.
        for index in 0..<10 {
            let character = OfficeCharacter.allCases[
                index % OfficeCharacter.allCases.count
            ]
            director.selectedCharacterID = character
            rootHost.layoutSubtreeIfNeeded()
            try await settle(for: .milliseconds(80))
        }

        for _ in 0..<8 {
            guard let scrollView = primaryScrollView(in: rootHost) else {
                XCTFail("실제 window 계층에서 live feed NSScrollView를 찾지 못했습니다.")
                return
            }
            performLiveScroll(scrollView, toTop: true)
            try await settle(for: .milliseconds(80))

            guard let refreshedScrollView = primaryScrollView(in: rootHost)
            else {
                XCTFail("상단 로딩 뒤 live feed NSScrollView가 사라졌습니다.")
                return
            }
            performLiveScroll(refreshedScrollView, toTop: false)
            try await settle(for: .milliseconds(80))
        }

        // 실제 실행 중 카드처럼 콘텐츠 높이를 연속 변경해 geometry report
        // -> scrollTo -> layout report 폐루프가 남아 있다면 깨운다.
        for step in 2...12 {
            director.liveFeedStore.replace(
                with: makeTurns(streamingStep: step)
            )
            try await settle(for: .milliseconds(40))
        }

        guard
            let scrollView = primaryScrollView(in: rootHost),
            let documentView = scrollView.documentView
        else {
            XCTFail("스트레스 직후 live feed scroll 계층을 찾지 못했습니다.")
            return
        }

        var geometryNotificationCount = 0
        let notificationCenter = NotificationCenter.default
        let registrations = [
            notificationCenter.addObserver(
                forName: NSView.boundsDidChangeNotification,
                object: scrollView.contentView,
                queue: .main
            ) { _ in
                geometryNotificationCount += 1
            },
            notificationCenter.addObserver(
                forName: NSView.frameDidChangeNotification,
                object: scrollView.contentView,
                queue: .main
            ) { _ in
                geometryNotificationCount += 1
            },
            notificationCenter.addObserver(
                forName: NSView.boundsDidChangeNotification,
                object: documentView,
                queue: .main
            ) { _ in
                geometryNotificationCount += 1
            },
            notificationCenter.addObserver(
                forName: NSView.frameDidChangeNotification,
                object: documentView,
                queue: .main
            ) { _ in
                geometryNotificationCount += 1
            },
        ]
        defer {
            registrations.forEach(notificationCenter.removeObserver)
        }

        // 입력이 멈춘 뒤에도 레이아웃/스크롤이 계속 자기 자신을 깨우는
        // 회귀를 빠르게 검출한다. 정상 상태에서는 2초 동안 0~수회다.
        try await settle(for: .seconds(2))
        XCTAssertLessThanOrEqual(
            geometryNotificationCount,
            20,
            "입력이 끝난 뒤에도 scroll geometry 알림이 "
                + "\(geometryNotificationCount)회 발생했습니다. 자동 스크롤과 "
                + "layout 측정이 서로 재촉발되는 상태입니다."
        )
    }

    func testImmediateScrollAfterSelectionWithCompletedTypingQuiesces()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let response = (0..<12).map { index in
            "### 완료 줄 \(index)\n직원 전환 직후에도 안정적으로 표시합니다."
        }.joined(separator: "\n")
        let turns = makeTurns(completedResponse: response)
        director.liveFeedStore.replace(with: turns)
        director.liveFeedStore.finishInitialLoading()
        for character in OfficeCharacter.allCases {
            director.liveFeedStore.beginResponseAnimation(
                for: "\(character.rawValue)-29"
            )
        }

        let rootHost = NSHostingView(
            rootView: CachedLiveWorkspaceFeeds(director: director)
                .frame(width: 900, height: 700)
        )
        rootHost.frame = NSRect(x: 0, y: 0, width: 900, height: 700)
        let window = NSWindow(
            contentRect: rootHost.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = rootHost
        rootHost.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(120))

        guard let container = allDescendants(of: rootHost)
            .compactMap({ $0 as? CachedLiveWorkspaceFeedsNSView })
            .first
        else {
            XCTFail("완료 타이핑용 live feed container를 만들지 못했습니다.")
            window.contentView = nil
            return
        }
        defer {
            container.tearDown()
            window.contentView = nil
        }

        for index in 0..<15 {
            director.selectedCharacterID = OfficeCharacter.allCases[
                index % OfficeCharacter.allCases.count
            ]
            rootHost.layoutSubtreeIfNeeded()
            try await settle(for: .milliseconds(4))
            guard
                let scrollView = try await waitForPrimaryScrollView(
                    in: rootHost
                )
            else {
                XCTFail("직원 전환 직후 live feed scroll view가 사라졌습니다.")
                return
            }
            performSmallLiveScroll(
                scrollView,
                delta: index.isMultiple(of: 2) ? 8 : -8
            )
        }

        XCTAssertLessThanOrEqual(
            container.liveHostingViewCountForTesting,
            1,
            "빠른 전환 중 live SwiftUI 호스트가 겹쳤습니다."
        )
        let clock = ContinuousClock()
        let startedAt = clock.now
        try await settle(for: .seconds(2))
        XCTAssertLessThan(
            startedAt.duration(to: clock.now),
            .seconds(4),
            "입력 종료 뒤에도 live feed transaction이 "
                + "메인 스레드를 점유합니다."
        )
        XCTAssertEqual(container.liveHostingViewCountForTesting, 1)
        XCTAssertEqual(
            container.activeCharacterIDForTesting,
            director.selectedCharacterID
        )
        XCTAssertFalse(
            container.hasTransitionLoadingGateForTesting,
            "빠른 전환이 끝난 뒤 정적 차폐 이미지가 남았습니다."
        )
    }

    func testReturningToStreamingEmployeeStartsInDocumentAndQuiesces()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let baseDate = Date(timeIntervalSinceReferenceDate: 80_000)
        let leftManTurns = (0..<10).map { index in
            makeTurn(
                id: "left-man-stable-\(index)",
                characterID: OfficeCharacter.leftMan.rawValue,
                prompt: "클대리 대화 \(index)",
                response: "클대리의 완료 응답 \(index)",
                status: .completed,
                startedAt: baseDate.addingTimeInterval(TimeInterval(index))
            )
        }
        var bossTurns = try makeComplexBossTurns(
            updateStep: 0,
            baseDate: baseDate
        )
        director.liveFeedStore.replace(with: bossTurns + leftManTurns)
        director.liveFeedStore.finishInitialLoading()

        let rootHost = NSHostingView(
            rootView: CachedLiveWorkspaceFeeds(director: director)
                .frame(width: 900, height: 700)
        )
        rootHost.frame = NSRect(x: 0, y: 0, width: 900, height: 700)
        let window = NSWindow(
            contentRect: rootHost.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = rootHost
        rootHost.layoutSubtreeIfNeeded()

        guard let container = allDescendants(of: rootHost)
            .compactMap({ $0 as? CachedLiveWorkspaceFeedsNSView })
            .first
        else {
            XCTFail("직원 복귀 회귀 테스트의 live feed container가 없습니다.")
            window.contentView = nil
            return
        }
        defer {
            container.tearDown()
            window.contentView = nil
        }

        let didMountBoss = try await waitUntil(
            in: rootHost,
            timeout: .seconds(2)
        ) {
            container.activeCharacterIDForTesting == .boss
                && self.primaryScrollView(in: container) != nil
        }
        XCTAssertTrue(didMountBoss, "첫 백부장 대화 화면이 준비되지 않았습니다.")

        let bossStore = director.liveFeedStore.characterStore(
            for: OfficeCharacter.boss.rawValue
        )
        var finalScrollView: NSScrollView?
        var finalDocumentView: NSView?

        for cycle in 1...4 {
            var previousBossHost = liveFeedHost(in: container)
            let releasedPreviousBossHost = WeakTestReference(
                previousBossHost
            )
            previousBossHost = nil
            let leftManGateInstallCount =
                container.transitionLoadingGateInstallCountForTesting
            director.selectedCharacterID = .leftMan
            rootHost.layoutSubtreeIfNeeded()
            let didInstallLeftManGate = try await waitUntil(
                in: rootHost,
                timeout: .seconds(1)
            ) {
                container.transitionLoadingGateInstallCountForTesting
                    > leftManGateInstallCount
            }
            XCTAssertTrue(
                didInstallLeftManGate,
                "\(cycle)회차 클대리 전환 준비 중 이전 화면 차폐를 "
                    + "설치하지 않았습니다."
            )
            var maximumLiveHostCount = liveFeedHostCount(in: container)
            let didMountLeftMan = try await waitUntil(
                in: rootHost,
                timeout: .seconds(2)
            ) {
                maximumLiveHostCount = max(
                    maximumLiveHostCount,
                    self.liveFeedHostCount(in: container)
                )
                return container.activeCharacterIDForTesting == .leftMan
                    && self.primaryScrollView(in: container) != nil
            }
            XCTAssertTrue(
                didMountLeftMan,
                "\(cycle)회차 클대리 전환 화면이 준비되지 않았습니다."
            )
            XCTAssertLessThanOrEqual(
                maximumLiveHostCount,
                1,
                "\(cycle)회차 전환 중 live NSHostingView가 겹쳤습니다."
            )
            XCTAssertNil(
                releasedPreviousBossHost.value,
                "\(cycle)회차 비선택 백부장 SwiftUI 그래프가 해제되지 않았습니다."
            )

            // 백부장 호스트가 화면에서 빠진 동안 실제 스트리밍처럼 응답과
            // 활동 수를 여러 번 늘린다. 이 갱신은 숨은 호스트를 살려 두지
            // 않으면서도 복귀 첫 프레임에는 전부 반영돼야 한다.
            for step in 1...5 {
                bossTurns = try makeComplexBossTurns(
                    updateStep: cycle * 10 + step,
                    baseDate: baseDate
                )
                director.liveFeedStore.replace(with: bossTurns + leftManTurns)
                rootHost.layoutSubtreeIfNeeded()
                try await settle(for: .milliseconds(6))
            }

            let expectedStep = cycle * 10 + 5
            var leftManHost = liveFeedHost(in: container)
            let releasedLeftManHost = WeakTestReference(leftManHost)
            leftManHost = nil
            let bossGateInstallCount =
                container.transitionLoadingGateInstallCountForTesting
            director.selectedCharacterID = .boss
            rootHost.layoutSubtreeIfNeeded()
            let didInstallBossGate = try await waitUntil(
                in: rootHost,
                timeout: .seconds(1)
            ) {
                container.transitionLoadingGateInstallCountForTesting
                    > bossGateInstallCount
            }
            XCTAssertTrue(
                didInstallBossGate,
                "\(cycle)회차 백부장 복귀 준비 중 이전 화면 차폐를 "
                    + "설치하지 않았습니다."
            )
            maximumLiveHostCount = liveFeedHostCount(in: container)
            let didReturnToBoss = try await waitUntil(
                in: rootHost,
                timeout: .seconds(2)
            ) {
                maximumLiveHostCount = max(
                    maximumLiveHostCount,
                    self.liveFeedHostCount(in: container)
                )
                return container.activeCharacterIDForTesting == .boss
                    && self.primaryScrollView(in: container) != nil
            }
            XCTAssertTrue(
                didReturnToBoss,
                "\(cycle)회차 백부장 복귀 화면이 준비되지 않았습니다."
            )
            XCTAssertLessThanOrEqual(
                maximumLiveHostCount,
                1,
                "\(cycle)회차 복귀 중 live NSHostingView가 겹쳤습니다."
            )
            XCTAssertNil(
                releasedLeftManHost.value,
                "\(cycle)회차 비선택 클대리 SwiftUI 그래프가 해제되지 않았습니다."
            )

            guard
                let returnedHost = liveFeedHost(in: container),
                let returnedScrollView = primaryScrollView(in: container),
                let returnedDocumentView = returnedScrollView.documentView
            else {
                XCTFail("\(cycle)회차 복귀 첫 유효 프레임의 scroll 계층이 없습니다.")
                return
            }

            XCTAssertTrue(returnedHost.superview === container)

            // activeCharacterIDForTesting은 pending 호스트가 준비돼 실제로
            // 화면에 교체된 순간에만 바뀐다. 따라서 여기의 첫 assertion이
            // 사용자가 보게 되는 복귀 첫 유효 프레임을 검사한다.
            assertViewportIntersectsDocument(
                returnedScrollView,
                step: "\(cycle)회차 복귀 첫 유효 프레임"
            )
            XCTAssertEqual(
                bossStore.turns.first?.id,
                "boss-streaming",
                "숨은 동안 갱신된 최신 running 턴이 복귀 때 보존되지 않았습니다."
            )
            XCTAssertEqual(
                bossStore.turns.first?.activities.count,
                expectedStep,
                "숨은 동안 늘어난 activities가 복귀 첫 프레임에 반영되지 않았습니다."
            )
            XCTAssertTrue(
                bossStore.turns.first?.response.contains(
                    "스트리밍 갱신 \(expectedStep)"
                ) == true,
                "숨은 동안 늘어난 응답이 복귀 첫 프레임에 반영되지 않았습니다."
            )

            try await settle(for: .milliseconds(350))
            rootHost.layoutSubtreeIfNeeded()
            assertViewportIntersectsDocument(
                returnedScrollView,
                step: "\(cycle)회차 복귀 정착 후"
            )
            let snapshot = LiveWorkspaceFeedScrollGeometry.snapshot(
                documentBounds: returnedDocumentView.bounds,
                visibleRect: returnedScrollView.documentVisibleRect,
                isFlipped: returnedDocumentView.isFlipped
            )
            XCTAssertLessThanOrEqual(
                snapshot.distanceFromBottom,
                20,
                "\(cycle)회차 복귀 뒤 최신 running 턴이 보이는 하단에 "
                    + "정착하지 않았습니다."
            )

            finalScrollView = returnedScrollView
            finalDocumentView = returnedDocumentView
        }

        guard
            let finalScrollView,
            let finalDocumentView
        else {
            XCTFail("최종 복귀의 scroll 계층이 없습니다.")
            return
        }
        var geometryNotificationCount = 0
        let notificationCenter = NotificationCenter.default
        let registrations = [
            notificationCenter.addObserver(
                forName: NSView.boundsDidChangeNotification,
                object: finalScrollView.contentView,
                queue: .main
            ) { _ in
                geometryNotificationCount += 1
            },
            notificationCenter.addObserver(
                forName: NSView.frameDidChangeNotification,
                object: finalDocumentView,
                queue: .main
            ) { _ in
                geometryNotificationCount += 1
            },
        ]
        defer {
            registrations.forEach(notificationCenter.removeObserver)
        }

        try await settle(for: .seconds(1))
        XCTAssertLessThanOrEqual(
            geometryNotificationCount,
            12,
            "복귀와 스트리밍 입력이 끝난 뒤에도 geometry가 "
                + "\(geometryNotificationCount)회 변했습니다. 하단 보정이나 "
                + "레이아웃 작업이 정착하지 않았습니다."
        )
    }

    func testRapidSelectionReleasesInactiveHostsAndCreatesFreshHosts()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        director.liveFeedStore.replace(with: makeTurns())
        director.liveFeedStore.finishInitialLoading()
        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        let didMountInitialBoss = try await waitUntil(
            in: container,
            timeout: .seconds(2)
        ) {
            container.activeCharacterIDForTesting == .boss
                && container.liveHostingViewCountForTesting == 1
        }
        XCTAssertTrue(didMountInitialBoss, "첫 백부장 호스트가 준비되지 않았습니다.")

        let selectionSequence = (0..<3).flatMap { _ in
            OfficeCharacter.allCases.filter { $0 != .boss } + [.boss]
        }
        for character in selectionSequence {
            var previousHost = liveFeedHost(in: container)
            let releasedPreviousHost = WeakTestReference(previousHost)
            director.selectedCharacterID = character
            container.configure(
                director: director,
                selectedCharacterID: character
            )
            container.layoutSubtreeIfNeeded()
            previousHost = nil

            XCTAssertTrue(
                container.hasTransitionLoadingGateForTesting,
                "직원 \(character.rawValue) 전환 준비 중 정적 차폐가 없습니다."
            )

            XCTAssertLessThanOrEqual(
                container.liveHostingViewCountForTesting,
                1,
                "직원 전환 시작부터 live NSHostingView는 하나 이하여야 합니다."
            )
            var maximumLiveHostCount =
                container.liveHostingViewCountForTesting
            let didActivate = try await waitUntil(
                in: container,
                timeout: .seconds(2)
            ) {
                maximumLiveHostCount = max(
                    maximumLiveHostCount,
                    container.liveHostingViewCountForTesting
                )
                return container.activeCharacterIDForTesting == character
                    && self.primaryScrollView(in: container) != nil
            }
            XCTAssertTrue(
                didActivate,
                "직원 \(character.rawValue) 호스트가 준비되지 않았습니다."
            )
            XCTAssertLessThanOrEqual(
                maximumLiveHostCount,
                1,
                "직원 \(character.rawValue) 전환 중 live 호스트가 "
                    + "\(maximumLiveHostCount)개 겹쳤습니다."
            )

            guard let attachedHost = liveFeedHost(in: container) else {
                XCTFail("직원 \(character.rawValue)의 live 호스트가 없습니다.")
                return
            }
            XCTAssertTrue(attachedHost.superview === container)
            let didReleasePreviousHost = try await waitUntil(
                timeout: .seconds(1)
            ) {
                releasedPreviousHost.value == nil
            }
            XCTAssertTrue(
                didReleasePreviousHost,
                "비선택 직원의 NSHostingView가 해제되지 않았습니다."
            )
        }

        director.selectedCharacterID = nil
        let gateInstallCountBeforeDeselection =
            container.transitionLoadingGateInstallCountForTesting
        container.configure(
            director: director,
            selectedCharacterID: nil
        )

        XCTAssertEqual(container.liveHostingViewCountForTesting, 0)
        XCTAssertNil(container.activeCharacterIDForTesting)
        XCTAssertNil(container.pendingCharacterIDForTesting)
        XCTAssertFalse(container.hasTransitionLoadingGateForTesting)
        XCTAssertEqual(
            container.transitionLoadingGateInstallCountForTesting,
            gateInstallCountBeforeDeselection,
            "직원 선택 해제에는 전환 차폐를 새로 만들면 안 됩니다."
        )

        director.selectedCharacterID = .boss
        container.configure(
            director: director,
            selectedCharacterID: .boss
        )
        XCTAssertFalse(
            container.hasTransitionLoadingGateForTesting,
            "선택 해제 뒤 첫 직원 진입에는 전환 차폐가 없어야 합니다."
        )
        XCTAssertEqual(
            container.transitionLoadingGateInstallCountForTesting,
            gateInstallCountBeforeDeselection,
            "선택 해제 뒤 첫 직원 진입이 전환 차폐를 만들었습니다."
        )
        let didRestoreBoss = try await waitUntil(
            in: container,
            timeout: .seconds(2)
        ) {
            container.activeCharacterIDForTesting == .boss
                && container.liveHostingViewCountForTesting == 1
        }
        XCTAssertTrue(didRestoreBoss)
    }

    func testInitialMountSkipsGateButEmployeeSwitchWaitsForFeed()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        // 첫 snapshot이 오기 전의 기본 백부장 mount를 시작한 직후
        // 코과장으로 선택이 바뀌는 실제 재시작 경합을 재현한다.
        container.configure(
            director: director,
            selectedCharacterID: .boss
        )
        container.layoutSubtreeIfNeeded()
        XCTAssertFalse(
            container.hasTransitionLoadingGateForTesting,
            "최초 직원 진입에 전환 차폐가 나타났습니다."
        )
        XCTAssertEqual(container.transitionLoadingGateInstallCountForTesting, 0)
        XCTAssertTrue(director.characterSelectionStore.isConversationLoading)
        XCTAssertLessThanOrEqual(container.liveHostingViewCountForTesting, 1)
        XCTAssertNil(container.activeCharacterIDForTesting)

        director.selectedCharacterID = .rightMan
        container.configure(
            director: director,
            selectedCharacterID: .rightMan
        )
        container.layoutSubtreeIfNeeded()

        XCTAssertTrue(
            container.hasTransitionLoadingGateForTesting,
            "첫 피드를 기다리는 코과장 전환에서 로딩 차폐가 사라졌습니다."
        )
        XCTAssertTrue(director.characterSelectionStore.isConversationLoading)
        XCTAssertNil(
            container.pendingCharacterIDForTesting,
            "새 host는 로딩 차폐가 먼저 그려진 다음 main-queue 차례에 만들어야 합니다."
        )
        XCTAssertNil(container.activeCharacterIDForTesting)
        XCTAssertEqual(container.liveHostingViewCountForTesting, 0)
        XCTAssertEqual(
            container.hitTest(
                NSPoint(x: container.bounds.midX, y: container.bounds.midY)
            )?.identifier?.rawValue,
            "live-workspace-feed-loading-gate",
            "로딩 차폐가 전환 중 hit-test를 받아 스크롤·클릭을 막아야 합니다."
        )

        let baseDate = Date(timeIntervalSinceReferenceDate: 120_000)
        let rightManTurns = (0..<12).map { index in
            makeTurn(
                id: "initial-right-man-\(index)",
                characterID: OfficeCharacter.rightMan.rawValue,
                prompt: "코과장 초기 대화 \(index)",
                response: String(
                    repeating: "코과장 검증 결과 \(index)입니다.\n",
                    count: 8
                ),
                status: .completed,
                startedAt: baseDate.addingTimeInterval(TimeInterval(index))
            )
        }
        director.liveFeedStore.replace(with: rightManTurns)
        director.liveFeedStore.finishInitialLoading()

        var maximumLiveHostCount = container.liveHostingViewCountForTesting
        let didActivateRightMan = try await waitNaturallyUntil(
            timeout: .seconds(4)
        ) {
            maximumLiveHostCount = max(
                maximumLiveHostCount,
                container.liveHostingViewCountForTesting
            )
            return container.activeCharacterIDForTesting == .rightMan
                && !container.hasTransitionLoadingGateForTesting
                && !director.characterSelectionStore.isConversationLoading
        }
        XCTAssertTrue(
            didActivateRightMan,
            "피드 반영 뒤 코과장 화면이 자연 run-loop에서 준비되지 않았습니다."
        )
        XCTAssertLessThanOrEqual(
            maximumLiveHostCount,
            1,
            "초기 피드 전환 중 live host가 겹쳤습니다."
        )
        XCTAssertNil(container.pendingCharacterIDForTesting)
        XCTAssertFalse(container.hasTransitionLoadingGateForTesting)
        XCTAssertFalse(director.characterSelectionStore.isConversationLoading)

        guard let scrollView = primaryScrollView(in: container) else {
            XCTFail("준비된 코과장 대화의 NSScrollView가 없습니다.")
            return
        }
        assertViewportIntersectsDocument(
            scrollView,
            step: "초기 피드 뒤 코과장 첫 표시"
        )
    }

    func testInitialFeedCompletionAfterThreeSecondsRearmsMountChecks()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        XCTAssertFalse(
            container.hasTransitionLoadingGateForTesting,
            "최초 직원 진입에 전환 차폐가 나타났습니다."
        )
        XCTAssertEqual(container.transitionLoadingGateInstallCountForTesting, 0)
        try await settle(for: .milliseconds(3_200))
        // 최초 피드가 늦어도 차폐는 처음부터 만들지 않는다. 준비 확인은
        // 차폐와 독립적으로 계속되어 뒤늦게 온 피드를 활성화해야 한다.
        XCTAssertFalse(
            container.hasTransitionLoadingGateForTesting,
            "최초 피드를 기다리는 동안 전환 차폐가 생겼습니다."
        )
        XCTAssertEqual(container.transitionLoadingGateInstallCountForTesting, 0)
        XCTAssertTrue(director.characterSelectionStore.isConversationLoading)

        director.liveFeedStore.replace(
            with: Array(
                makeTurns()
                    .filter {
                        $0.characterId == OfficeCharacter.boss.rawValue
                    }
                    .prefix(10)
            )
        )
        director.liveFeedStore.finishInitialLoading()

        let didActivate = try await waitNaturallyUntil(
            timeout: .seconds(4)
        ) {
            container.activeCharacterIDForTesting == .boss
                && !container.hasTransitionLoadingGateForTesting
                && !director.characterSelectionStore.isConversationLoading
        }
        XCTAssertTrue(
            didActivate,
            "3초 자동 확인 예산 뒤 피드가 도착해도 준비 판정이 재무장되어야 합니다."
        )
    }

    func testSelectionGatePrecedesAsynchronousHostReplacement()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        director.liveFeedStore.replace(with: makeTurns())
        director.liveFeedStore.finishInitialLoading()
        let rootHost = NSHostingView(
            rootView: CachedLiveWorkspaceFeeds(director: director)
                .frame(width: 900, height: 700)
        )
        rootHost.frame = NSRect(x: 0, y: 0, width: 900, height: 700)
        let window = NSWindow(
            contentRect: rootHost.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = rootHost
        rootHost.layoutSubtreeIfNeeded()

        guard let container = allDescendants(of: rootHost)
            .compactMap({ $0 as? CachedLiveWorkspaceFeedsNSView })
            .first
        else {
            XCTFail("SwiftUI representable이 live feed container를 만들지 못했습니다.")
            window.contentView = nil
            return
        }
        defer {
            container.tearDown()
            window.contentView = nil
        }

        let didMountBoss = try await waitNaturallyUntil(
            timeout: .seconds(3)
        ) {
            container.activeCharacterIDForTesting == .boss
                && !container.hasTransitionLoadingGateForTesting
        }
        XCTAssertTrue(didMountBoss)
        let bossHost = liveFeedHost(in: container)

        director.selectedCharacterID = .leftWoman
        XCTAssertTrue(container.hasTransitionLoadingGateForTesting)
        XCTAssertTrue(liveFeedHost(in: container) === bossHost)
        XCTAssertEqual(container.activeCharacterIDForTesting, .boss)
        XCTAssertNil(container.pendingCharacterIDForTesting)

        let didMountLeftWoman = try await waitNaturallyUntil(
            timeout: .seconds(4)
        ) {
            container.activeCharacterIDForTesting == .leftWoman
                && !container.hasTransitionLoadingGateForTesting
        }
        XCTAssertTrue(didMountLeftWoman)
    }

    func testPendingViewportReclampsAfterDocumentGrowAndShrink()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let baseDate = Date(timeIntervalSinceReferenceDate: 140_000)
        func turns(lineCount: Int) -> [LiveFeedTurn] {
            (0..<10).map { index in
                let startedAt = baseDate.addingTimeInterval(
                    TimeInterval(index)
                )
                return makeTurn(
                    id: "reclamp-\(index)",
                    characterID: OfficeCharacter.leftWoman.rawValue,
                    prompt: "높이 변경 \(index)",
                    response: String(
                        repeating: "Markdown 높이 검증 줄입니다.\n",
                        count: lineCount
                    ),
                    status: .completed,
                    startedAt: startedAt,
                    updatedAt: startedAt.addingTimeInterval(
                        TimeInterval(lineCount)
                    ),
                    backend: .claude
                )
            }
        }

        director.liveFeedStore.replace(with: turns(lineCount: 8))
        director.liveFeedStore.finishInitialLoading()
        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        // 이 테스트는 전환 차폐 아래의 pending viewport 보정을 검증한다.
        // 먼저 실제 백부장 화면을 활성화한 뒤 직원 간 전환을 만든다.
        container.configure(
            director: director,
            selectedCharacterID: .boss
        )
        let didMountBoss = try await waitNaturallyUntil(
            timeout: .seconds(3)
        ) {
            container.activeCharacterIDForTesting == .boss
                && !container.hasTransitionLoadingGateForTesting
        }
        XCTAssertTrue(didMountBoss, "전환 전 백부장 화면이 준비되지 않았습니다.")

        director.selectedCharacterID = .leftWoman
        container.configure(
            director: director,
            selectedCharacterID: .leftWoman
        )
        let didClampInitialHeight = try await waitNaturallyUntil(
            timeout: .seconds(3),
            pollInterval: .milliseconds(10)
        ) {
            container.pendingCharacterIDForTesting == .leftWoman
                && container.viewportClampCountForTesting >= 1
                && container.hasTransitionLoadingGateForTesting
        }
        XCTAssertTrue(didClampInitialHeight)

        director.liveFeedStore.replace(with: turns(lineCount: 80))
        let didReclampGrownHeight = try await waitNaturallyUntil(
            timeout: .seconds(3),
            pollInterval: .milliseconds(10)
        ) {
            container.viewportClampCountForTesting >= 2
        }
        XCTAssertTrue(
            didReclampGrownHeight,
            "문서가 커진 뒤 새 안정 높이에서 viewport를 다시 보정하지 않았습니다."
        )

        director.liveFeedStore.replace(with: turns(lineCount: 2))
        let didReclampShrunkHeight = try await waitNaturallyUntil(
            timeout: .seconds(3),
            pollInterval: .milliseconds(10)
        ) {
            container.viewportClampCountForTesting >= 3
        }
        XCTAssertTrue(
            didReclampShrunkHeight,
            "문서가 줄어든 뒤 문서 밖 origin을 다시 보정하지 않았습니다."
        )

        let didActivate = try await waitNaturallyUntil(
            timeout: .seconds(3)
        ) {
            container.activeCharacterIDForTesting == .leftWoman
                && !container.hasTransitionLoadingGateForTesting
        }
        XCTAssertTrue(didActivate)
        guard let scrollView = primaryScrollView(in: container) else {
            XCTFail("grow/shrink 뒤 NSScrollView가 없습니다.")
            return
        }
        assertViewportIntersectsDocument(
            scrollView,
            step: "grow/shrink 재보정 뒤 첫 표시"
        )
    }

    func testGrowingCommandComposerKeepsConversationVisible() async throws {
        let director = AgentDirector(startBackgroundTasks: false)
        let characterID = OfficeCharacter.boss
        let turns = Array(
            makeTurns()
                .filter { $0.characterId == characterID.rawValue }
                .prefix(10)
        )
        director.liveFeedStore.replace(with: turns)
        director.liveFeedStore.finishInitialLoading()

        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        container.configure(
            director: director,
            selectedCharacterID: characterID
        )
        container.layoutSubtreeIfNeeded()
        let didMount = try await waitNaturallyUntil(timeout: .seconds(4)) {
            container.activeCharacterIDForTesting == characterID
                && !container.hasTransitionLoadingGateForTesting
        }
        XCTAssertTrue(didMount, "대화 화면이 준비되지 않았습니다.")

        guard
            let scrollView = try await waitForPrimaryScrollView(in: container),
            let documentView = scrollView.documentView
        else {
            XCTFail("대화 NSScrollView를 찾지 못했습니다.")
            return
        }

        func assertViewportInsideDocument(_ step: String) {
            guard let currentDocument = scrollView.documentView else {
                XCTFail("\(step): 대화 문서가 사라졌습니다.")
                return
            }
            let visible = scrollView.documentVisibleRect
            let overlap = visible.intersection(currentDocument.bounds).height
            let expected = min(visible.height, currentDocument.bounds.height)
            XCTAssertGreaterThan(
                overlap,
                0,
                "\(step): viewport가 문서 밖으로 나가 대화가 사라졌습니다."
            )
            XCTAssertGreaterThanOrEqual(
                overlap,
                expected - 1,
                "\(step): viewport(\(visible))가 문서"
                    + "(\(currentDocument.bounds))를 부분적으로만 덮어 "
                    + "빈 영역이 노출됩니다."
            )
        }

        func distanceFromBottom() -> CGFloat {
            guard let currentDocument = scrollView.documentView else {
                return .greatestFiniteMagnitude
            }
            return LiveWorkspaceFeedScrollGeometry.snapshot(
                documentBounds: currentDocument.bounds,
                visibleRect: scrollView.documentVisibleRect,
                isFlipped: currentDocument.isFlipped
            ).distanceFromBottom
        }

        assertViewportInsideDocument("입력창 확대 전")
        XCTAssertGreaterThan(
            documentView.bounds.height,
            scrollView.contentView.bounds.height,
            "재현하려면 대화가 화면보다 길어야 합니다."
        )
        XCTAssertLessThanOrEqual(
            distanceFromBottom(),
            20,
            "재현하려면 최신 대화를 보고 있는 상태여야 합니다."
        )

        // 입력창이 여러 줄로 커지면서 대화 영역 높이가 단계적으로 줄어드는
        // 상황을 그대로 만든다.
        var height = container.bounds.height
        for _ in 0..<6 {
            height -= 48
            container.setFrameSize(
                NSSize(width: container.bounds.width, height: height)
            )
            container.layoutSubtreeIfNeeded()
            try await settle(for: .milliseconds(40))
            assertViewportInsideDocument("입력창 확대 중(높이 \(Int(height)))")
            // 입력창이 커져 대화 영역이 줄어도 보고 있던 최신 대화가
            // 화면 밖으로 밀려나면 안 된다.
            XCTAssertLessThanOrEqual(
                distanceFromBottom(),
                20,
                "입력창 확대(높이 \(Int(height)))로 최신 대화가 "
                    + "\(Int(distanceFromBottom()))pt 위로 밀려 사라졌습니다."
            )
        }

        // 입력을 지워 입력창이 다시 줄어들 때도 대화가 유지돼야 한다.
        for _ in 0..<6 {
            height += 48
            container.setFrameSize(
                NSSize(width: container.bounds.width, height: height)
            )
            container.layoutSubtreeIfNeeded()
            try await settle(for: .milliseconds(40))
            assertViewportInsideDocument("입력창 축소 중(높이 \(Int(height)))")
        }

        try await settle(for: .milliseconds(200))
        assertViewportInsideDocument("정착 후")
        XCTAssertEqual(
            container.activeCharacterIDForTesting,
            characterID,
            "입력창 크기 변화가 대화 화면을 다시 만들면 안 됩니다."
        )
        XCTAssertFalse(
            container.hasTransitionLoadingGateForTesting,
            "입력창 크기 변화로 로딩 차폐가 다시 나타나면 안 됩니다."
        )
    }

    func testInitialLoadingPlaceholderAppearsOnlyBeforeFirstFeedLoad() {
        let store = LiveFeedStore()
        XCTAssertFalse(
            store.didCompleteFirstFeedLoad,
            "첫 적재 전에는 로딩 자리표시자를 쓸 수 있어야 합니다."
        )

        store.finishInitialLoading()
        XCTAssertTrue(store.didCompleteFirstFeedLoad)

        // 이후 갱신이나 직원 전환이 이어져도 첫 적재 완료 기록은
        // 되돌아가지 않는다. 로딩 자리표시자로 대화 영역을 통째로
        // 바꾸는 일이 다시 생기면 화면이 비어 보인다.
        store.replace(with: [
            makeTurn(
                id: "after-first-load",
                characterID: OfficeCharacter.boss.rawValue,
                prompt: "이후 업무",
                status: .completed,
                startedAt: Date(timeIntervalSinceReferenceDate: 200_000)
            ),
        ])
        store.selectCharacterFeed(OfficeCharacter.leftWoman.rawValue)
        XCTAssertTrue(
            store.didCompleteFirstFeedLoad,
            "갱신과 직원 전환은 최초 로딩 표시 조건을 되살리면 안 됩니다."
        )
    }

    func testDocumentShrinkAfterResizeKeepsViewportInsideDocument()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let characterID = OfficeCharacter.boss
        let tall = Array(
            makeTurns(completedResponse: String(
                repeating: "긴 응답 줄입니다.\n",
                count: 60
            ))
            .filter { $0.characterId == characterID.rawValue }
            .prefix(10)
        )
        director.liveFeedStore.replace(with: tall)
        director.liveFeedStore.finishInitialLoading()

        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        container.configure(
            director: director,
            selectedCharacterID: characterID
        )
        container.layoutSubtreeIfNeeded()
        _ = try await waitNaturallyUntil(timeout: .seconds(4)) {
            container.activeCharacterIDForTesting == characterID
                && !container.hasTransitionLoadingGateForTesting
        }
        guard
            let scrollView = try await waitForPrimaryScrollView(in: container)
        else {
            XCTFail("대화 NSScrollView를 찾지 못했습니다.")
            return
        }
        performLiveScroll(scrollView, toTop: false)
        try await settle(for: .milliseconds(80))

        // 창 크기를 바꾼 뒤, SwiftUI가 카드를 다시 실체화하며 문서
        // 높이를 늦게 줄이는 실제 순서를 재현한다. 이 급락이 흰 화면의
        // 방아쇠였다.
        container.setFrameSize(NSSize(width: 760, height: 520))
        container.layoutSubtreeIfNeeded()
        director.liveFeedStore.replace(
            with: Array(
                makeTurns(completedResponse: "짧은 응답")
                    .filter { $0.characterId == characterID.rawValue }
                    .prefix(10)
            )
        )
        container.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(250))

        guard let documentView = scrollView.documentView else {
            XCTFail("문서가 사라졌습니다.")
            return
        }
        let visible = scrollView.documentVisibleRect
        let overlap = visible.intersection(documentView.bounds).height
        XCTAssertGreaterThanOrEqual(
            overlap,
            min(visible.height, documentView.bounds.height) - 1,
            "리사이즈 뒤 문서가 줄자 viewport(\(visible))가 문서"
                + "(\(documentView.bounds)) 밖에 남아 흰 화면이 됐습니다."
        )

        // 기하가 멀쩡해도 그려진 것이 없으면 사용자 눈에는 흰 화면이다.
        // 계수기가 컨테이너를 세면 항상 1이 나와 신호가 되지 않는다.
        let drawn = leafViewCount(
            in: documentView,
            intersecting: scrollView.documentVisibleRect
        )
        XCTAssertGreaterThan(
            drawn,
            1,
            "리사이즈 정착 뒤 보이는 영역에 그려진 뷰가 없습니다."
        )
    }

    private func leafViewCount(
        in documentView: NSView,
        intersecting rect: NSRect
    ) -> Int {
        var counted = 0
        var stack = documentView.subviews
        while let view = stack.popLast() {
            let frame = documentView.convert(view.bounds, from: view)
            guard frame.intersects(rect) else {
                continue
            }
            if view.subviews.isEmpty {
                if frame.height >= 8, frame.width >= 8 {
                    counted += 1
                }
            } else {
                stack.append(contentsOf: view.subviews)
            }
        }
        return counted
    }

    func testLoadingGateNeverPaintsOutsideConversationArea() async throws {
        let director = AgentDirector(startBackgroundTasks: false)
        director.liveFeedStore.replace(with: makeTurns())
        director.liveFeedStore.finishInitialLoading()

        // 실제 배치처럼 대화 영역을 창보다 작게 잡는다. 좌측 사무실
        // 화면과 아래 입력·직원 선택 영역에 해당하는 여백을 남긴다.
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1_200, height: 800),
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        let root = NSView(frame: window.contentLayoutRect)
        let conversationFrame = NSRect(x: 420, y: 120, width: 700, height: 560)
        let container = CachedLiveWorkspaceFeedsNSView(
            frame: conversationFrame
        )
        root.addSubview(container)
        window.contentView = root
        defer {
            container.tearDown()
            window.contentView = nil
        }

        XCTAssertTrue(
            container.clipsToBounds,
            "대화 영역 밖으로 그려지지 않도록 경계를 잘라야 합니다."
        )

        func assertSubviewsStayInsideConversationArea(_ step: String) {
            for subview in container.subviews {
                let inRoot = container.convert(subview.frame, to: root)
                XCTAssertTrue(
                    conversationFrame.contains(inRoot)
                        || conversationFrame.intersection(inRoot) == inRoot,
                    "\(step): \(subview.identifier?.rawValue ?? "하위 뷰")가 "
                        + "대화 영역(\(conversationFrame)) 밖 \(inRoot)까지 "
                        + "그려집니다."
                )
            }
        }

        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        assertSubviewsStayInsideConversationArea("최초 mount")
        _ = try await waitNaturallyUntil(timeout: .seconds(4)) {
            container.activeCharacterIDForTesting == .boss
                && !container.hasTransitionLoadingGateForTesting
        }

        // 전환 차폐가 떠 있는 동안이 가장 위험한 구간이다.
        director.selectedCharacterID = .leftWoman
        container.configure(
            director: director,
            selectedCharacterID: .leftWoman
        )
        container.layoutSubtreeIfNeeded()
        XCTAssertTrue(
            container.hasTransitionLoadingGateForTesting,
            "전환 직후 차폐가 없으면 이 회귀를 검증할 수 없습니다."
        )
        assertSubviewsStayInsideConversationArea("전환 차폐 중")

        // 창 크기가 바뀌어 대화 영역이 줄어드는 동안에도 유지돼야 한다.
        container.setFrameSize(NSSize(width: 700, height: 420))
        container.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(60))
        for subview in container.subviews {
            XCTAssertTrue(
                container.bounds.contains(subview.frame)
                    || container.bounds.intersection(subview.frame)
                        == subview.frame,
                "대화 영역 축소 후 \(subview.identifier?.rawValue ?? "하위 뷰")가 "
                    + "\(subview.frame)로 경계를 넘습니다."
            )
        }
    }

    func testLoadingGateShowsCenteredSystemSpinner() async throws {
        let director = AgentDirector(startBackgroundTasks: false)
        director.liveFeedStore.replace(with: makeTurns())
        director.liveFeedStore.finishInitialLoading()

        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 700, height: 560)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        _ = try await waitNaturallyUntil(timeout: .seconds(4)) {
            container.activeCharacterIDForTesting == .boss
                && !container.hasTransitionLoadingGateForTesting
        }

        director.selectedCharacterID = .leftWoman
        container.configure(
            director: director,
            selectedCharacterID: .leftWoman
        )
        container.layoutSubtreeIfNeeded()

        guard
            let gate = container.subviews.first(where: {
                $0.identifier?.rawValue == "live-workspace-feed-loading-gate"
            })
        else {
            XCTFail("전환 차폐 뷰를 찾지 못했습니다.")
            return
        }

        XCTAssertTrue(
            gate.subviews.allSatisfy { !($0 is NSTextField) },
            "전환 차폐에는 안내 문구를 두지 않는다."
        )
        XCTAssertFalse(
            gate.accessibilityLabel()?.isEmpty ?? true,
            "화면에 문구가 없어도 접근성 레이블은 남겨야 합니다."
        )

        guard
            let spinner = gate.subviews
                .compactMap({ $0 as? NSProgressIndicator })
                .first
        else {
            XCTFail("전환 스피너가 없습니다.")
            return
        }
        XCTAssertGreaterThanOrEqual(
            min(spinner.frame.width, spinner.frame.height),
            28,
            "전환 스피너가 눈에 띄게 커야 합니다."
        )
        XCTAssertEqual(
            spinner.frame.midX,
            gate.bounds.midX,
            accuracy: 0.01,
            "전환 스피너는 대화 영역 한가운데에 있어야 합니다."
        )
        XCTAssertEqual(
            spinner.frame.midY,
            gate.bounds.midY,
            accuracy: 0.01,
            "전환 스피너는 대화 영역 한가운데에 있어야 합니다."
        )
        XCTAssertEqual(spinner.style, .spinning)
        XCTAssertTrue(spinner.isIndeterminate)
        XCTAssertEqual(spinner.controlSize, .large)
        XCTAssertFalse(spinner.isDisplayedWhenStopped)
    }

    func testLoadingGateDoesNotBleedBackgroundOutsideItsBounds() async throws {
        let director = AgentDirector(startBackgroundTasks: false)
        director.liveFeedStore.replace(with: makeTurns())
        director.liveFeedStore.finishInitialLoading()

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1_200, height: 800),
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        let root = NSView(frame: window.contentLayoutRect)
        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 420, y: 120, width: 700, height: 560)
        )
        root.addSubview(container)
        window.contentView = root
        defer {
            container.tearDown()
            window.contentView = nil
        }

        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        _ = try await waitNaturallyUntil(timeout: .seconds(4)) {
            container.activeCharacterIDForTesting == .boss
                && !container.hasTransitionLoadingGateForTesting
        }

        director.selectedCharacterID = .leftWoman
        container.configure(
            director: director,
            selectedCharacterID: .leftWoman
        )
        container.layoutSubtreeIfNeeded()

        guard
            let gate = container.subviews.first(where: {
                $0.identifier?.rawValue == "live-workspace-feed-loading-gate"
            })
        else {
            XCTFail("전환 차폐 뷰를 찾지 못했습니다.")
            return
        }

        // isOpaque를 선언하면 AppKit이 뒤쪽 그리기를 건너뛴다. 차폐 뷰가
        // 상위 레이어를 공유한 채 이 최적화를 쓰면 배경 채움이 대화
        // 영역 밖 창 전체로 번진다.
        XCTAssertFalse(
            gate.isOpaque,
            "차폐 뷰가 불투명을 선언하면 배경이 창 전체로 번집니다."
        )
        XCTAssertTrue(
            gate.wantsLayer,
            "차폐 뷰는 자기 레이어에만 배경을 그려야 합니다."
        )
        XCTAssertNotNil(
            gate.layer?.backgroundColor,
            "차폐 배경은 레이어 배경색으로 그려야 합니다."
        )
        XCTAssertEqual(
            gate.frame,
            container.bounds,
            "차폐 뷰는 대화 영역과 정확히 같은 범위여야 합니다."
        )
        XCTAssertTrue(
            container.clipsToBounds,
            "대화 영역 밖으로 그려지지 않도록 경계를 잘라야 합니다."
        )
    }

    func testClaudeTranscriptTransitionsStayCoveredUntilViewportIsReady()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let baseDate = Date(timeIntervalSinceReferenceDate: 130_000)
        let bossTurns = Array(
            makeTurns()
                .filter { $0.characterId == OfficeCharacter.boss.rawValue }
                .prefix(10)
        )
        let leftWomanTurns = try makeClaudeTurns(
            characterID: .leftWoman,
            label: "로과장",
            baseDate: baseDate
        )
        let leftManTurns = try makeClaudeTurns(
            characterID: .leftMan,
            label: "클대리",
            baseDate: baseDate.addingTimeInterval(1_000)
        )
        director.liveFeedStore.replace(
            with: bossTurns + leftWomanTurns + leftManTurns
        )
        director.liveFeedStore.finishInitialLoading()

        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        let didMountBoss = try await waitNaturallyUntil(
            timeout: .seconds(4)
        ) {
            container.activeCharacterIDForTesting == .boss
                && !container.hasTransitionLoadingGateForTesting
        }
        XCTAssertTrue(didMountBoss, "Claude 전환 전 백부장 화면이 준비되지 않았습니다.")

        for character in [OfficeCharacter.leftWoman, .leftMan] {
            XCTAssertTrue(
                director.liveFeedStore.turns(for: character.rawValue)
                    .allSatisfy {
                        $0.characterBackend == .claude
                            && $0.backend == .claude
                    },
                "\(character.rawValue) 회귀 데이터가 실제 Claude backend가 아닙니다."
            )
            var previousHost = liveFeedHost(in: container)
            let releasedPreviousHost = WeakTestReference(previousHost)
            director.selectedCharacterID = character
            container.configure(
                director: director,
                selectedCharacterID: character
            )
            container.layoutSubtreeIfNeeded()
            previousHost = nil

            XCTAssertTrue(
                container.hasTransitionLoadingGateForTesting,
                "긴 Claude transcript 전환 직후 로딩 차폐가 없습니다."
            )
            XCTAssertTrue(director.characterSelectionStore.isConversationLoading)
            XCTAssertLessThanOrEqual(container.liveHostingViewCountForTesting, 1)

            var maximumLiveHostCount = container.liveHostingViewCountForTesting
            let didActivate = try await waitNaturallyUntil(
                timeout: .seconds(6)
            ) {
                maximumLiveHostCount = max(
                    maximumLiveHostCount,
                    container.liveHostingViewCountForTesting
                )
                return container.activeCharacterIDForTesting == character
                    && !container.hasTransitionLoadingGateForTesting
                    && !director.characterSelectionStore.isConversationLoading
            }
            XCTAssertTrue(
                didActivate,
                "긴 Claude transcript가 준비된 뒤에도 차폐가 해제되지 않았습니다."
            )
            XCTAssertLessThanOrEqual(
                maximumLiveHostCount,
                1,
                "Claude 화면 전환 중 live host가 겹쳤습니다."
            )
            XCTAssertNil(
                releasedPreviousHost.value,
                "Claude 전환 중 비선택 직원의 SwiftUI host가 남았습니다."
            )

            guard let scrollView = primaryScrollView(in: container) else {
                XCTFail("준비된 Claude transcript의 NSScrollView가 없습니다.")
                return
            }
            assertViewportIntersectsDocument(
                scrollView,
                step: "\(character.rawValue) Claude 첫 표시"
            )
            guard let documentView = scrollView.documentView else {
                XCTFail("준비된 Claude transcript documentView가 없습니다.")
                return
            }
            let snapshot = LiveWorkspaceFeedScrollGeometry.snapshot(
                documentBounds: documentView.bounds,
                visibleRect: scrollView.documentVisibleRect,
                isFlipped: documentView.isFlipped
            )
            XCTAssertLessThanOrEqual(
                snapshot.distanceFromBottom,
                20,
                "Claude transcript가 첫 표시에서 하단에 정착하지 않았습니다."
            )
        }
    }

    func testSameEmployeeSubmissionKeepsEntireScrollHierarchyIdentity()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let baseDate = Date(timeIntervalSinceReferenceDate: 70_000)
        var persistedTurns = Array(
            makeTurns()
                .filter { $0.characterId == OfficeCharacter.boss.rawValue }
                .prefix(10)
        )
        director.liveFeedStore.replace(with: persistedTurns)
        director.liveFeedStore.finishInitialLoading()
        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        container.configure(
            director: director,
            selectedCharacterID: .boss
        )
        container.layoutSubtreeIfNeeded()
        let didMount = try await waitUntil(
            in: container,
            timeout: .seconds(2)
        ) {
            container.activeCharacterIDForTesting == .boss
                && self.primaryScrollView(in: container) != nil
        }
        XCTAssertTrue(
            didMount,
            "동일 직원 제출 전 대화 계층이 준비되지 않았습니다."
        )
        XCTAssertFalse(container.hasTransitionLoadingGateForTesting)
        XCTAssertFalse(director.characterSelectionStore.isConversationLoading)

        guard
            let mountedHost = liveFeedHost(in: container),
            let mountedScrollView = primaryScrollView(in: container),
            let mountedDocumentView = mountedScrollView.documentView
        else {
            XCTFail("제출 전 NSHostingView/NSScrollView/documentView가 없습니다.")
            return
        }

        let localID = "local-same-employee-identity"
        let serverID = "server-same-employee-identity"
        let optimistic = makeTurn(
            id: localID,
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "같은 직원에게 새 업무",
            status: .running,
            startedAt: baseDate
        )
        director.liveFeedStore.insertOptimisticTurn(optimistic)
        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(20))
        XCTAssertFalse(
            container.hasTransitionLoadingGateForTesting,
            "같은 직원 optimistic 삽입을 직원 전환으로 오인했습니다."
        )
        XCTAssertFalse(director.characterSelectionStore.isConversationLoading)

        director.liveFeedStore.reconcileOptimisticTurn(
            id: localID,
            with: serverID
        )
        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(20))
        XCTAssertFalse(
            container.hasTransitionLoadingGateForTesting,
            "같은 직원 server ID 전환에서 로딩 차폐가 생겼습니다."
        )
        XCTAssertFalse(director.characterSelectionStore.isConversationLoading)

        persistedTurns.insert(optimistic.replacingID(with: serverID), at: 0)
        director.liveFeedStore.replace(with: persistedTurns)
        container.configure(director: director, selectedCharacterID: .boss)
        container.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(100))
        XCTAssertFalse(
            container.hasTransitionLoadingGateForTesting,
            "같은 직원 persisted 반영에서 로딩 차폐가 생겼습니다."
        )
        XCTAssertFalse(director.characterSelectionStore.isConversationLoading)

        XCTAssertTrue(
            liveFeedHost(in: container) === mountedHost,
            "같은 직원 제출이 NSHostingView를 직원 전환처럼 교체했습니다."
        )
        XCTAssertTrue(
            primaryScrollView(in: container) === mountedScrollView,
            "같은 직원 제출이 NSScrollView를 교체했습니다."
        )
        XCTAssertTrue(
            mountedScrollView.documentView === mountedDocumentView,
            "같은 직원 제출이 대화 documentView를 교체했습니다."
        )
        assertViewportIntersectsDocument(
            mountedScrollView,
            step: "같은 직원 제출 정착 후"
        )
    }

    func testPostMountRefreshWaitsUntilSelectedHostIsInWindow()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        director.liveFeedStore.replace(with: makeTurns())
        director.liveFeedStore.finishInitialLoading()
        let characterStore = director.liveFeedStore.characterStore(
            for: OfficeCharacter.boss.rawValue
        )
        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )

        defer {
            container.tearDown()
        }

        container.configure(
            director: director,
            selectedCharacterID: .boss
        )
        container.layoutSubtreeIfNeeded()

        XCTAssertEqual(
            characterStore.presentationRevision,
            0,
            "window에 연결되기 전의 갱신은 새 SwiftUI 그래프가 구독하기 "
                + "전에 유실될 수 있습니다."
        )

        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        container.layoutSubtreeIfNeeded()
        let didPublishAfterWindowAttach = try await waitUntil(in: container) {
            characterStore.presentationRevision > 0
        }
        XCTAssertTrue(
            didPublishAfterWindowAttach,
            "창 연결 뒤 목록 재발행이 제한 시간 안에 일어나지 않았습니다."
        )

        XCTAssertEqual(characterStore.presentationRevision, 1)
        XCTAssertTrue(container.window === window)
        let scrollView = try await waitForPrimaryScrollView(in: container)
        XCTAssertNotNil(
            scrollView,
            "창에 연결된 뒤 선택 직원 대화 목록이 실제로 mount되어야 합니다."
        )
        window.contentView = nil
    }

    func testSameEmployeeTurnIDTransitionKeepsConversationMountedAtBottom()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let questionID = "existing-inline-question"
        let question = """
        다음 작업 방향을 선택해 주세요.

        1. 첫 번째 선택지
        2. 두 번째 선택지
        3. 세 번째 선택지
        4. 네 번째 선택지
        5. 다섯 번째 선택지
        6. 여섯 번째 선택지
        7. 일곱 번째 선택지
        8. 여덟 번째 선택지
        """
        var existingTurns = Array(
            makeTurns()
                .filter {
                    $0.characterId == OfficeCharacter.boss.rawValue
                }
                .prefix(9)
        )
        existingTurns.insert(
            makeTurn(
                id: questionID,
                characterID: OfficeCharacter.boss.rawValue,
                prompt: "기존 확인 질문",
                response: question,
                status: .completed,
                needsInput: true,
                startedAt: Date(timeIntervalSinceReferenceDate: 25_000)
            ),
            at: 0
        )
        director.liveFeedStore.replace(with: existingTurns)
        director.liveFeedStore.finishInitialLoading()

        let rootHost = NSHostingView(
            rootView: CachedLiveWorkspaceFeeds(director: director)
                .frame(width: 900, height: 700)
        )
        rootHost.frame = NSRect(x: 0, y: 0, width: 900, height: 700)
        let window = NSWindow(
            contentRect: rootHost.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = rootHost
        rootHost.layoutSubtreeIfNeeded()

        let didMountBoss = try await waitNaturallyUntil(
            timeout: .seconds(3)
        ) {
            allDescendants(of: rootHost)
                .compactMap { $0 as? CachedLiveWorkspaceFeedsNSView }
                .first?.activeCharacterIDForTesting == .boss
        }
        XCTAssertTrue(didMountBoss)

        guard
            let container = allDescendants(of: rootHost)
                .compactMap({ $0 as? CachedLiveWorkspaceFeedsNSView })
                .first,
            let mountedHost = liveFeedHost(in: container),
            let mountedScrollView = try await waitForPrimaryScrollView(
                in: rootHost
            ),
            let mountedDocumentView = mountedScrollView.documentView
        else {
            XCTFail("기존 대화의 실제 NSHostingView/NSScrollView 계층이 없습니다.")
            window.contentView = nil
            return
        }
        defer {
            container.tearDown()
            window.contentView = nil
        }

        performLiveScroll(mountedScrollView, toTop: false)
        try await settle(for: .milliseconds(100))

        var persistedTurns = existingTurns
        let submittedAt = Date(timeIntervalSinceReferenceDate: 30_000)
        for index in 0..<8 {
            let localID = "local-same-employee-\(index)"
            let serverID = "server-same-employee-\(index)"
            let prompt = "동일 직원 새 업무 \(index)"
            let startedAt = submittedAt.addingTimeInterval(
                TimeInterval(index)
            )
            let optimistic = makeTurn(
                id: localID,
                characterID: OfficeCharacter.boss.rawValue,
                prompt: prompt,
                status: .running,
                startedAt: startedAt
            )

            director.liveFeedStore.insertOptimisticTurn(optimistic)
            let presentationID = director.liveFeedStore.presentationID(
                forTurnID: localID
            )
            rootHost.layoutSubtreeIfNeeded()
            try await settle(for: .milliseconds(8))
            director.liveFeedStore.reconcileOptimisticTurn(
                id: localID,
                with: serverID
            )
            XCTAssertEqual(
                director.liveFeedStore.presentationID(forTurnID: serverID),
                presentationID,
                "서버 turn ID 전환이 같은 카드의 SwiftUI 정체성을 "
                    + "삭제 후 재삽입으로 바꿨습니다."
            )
            rootHost.layoutSubtreeIfNeeded()
            try await settle(for: .milliseconds(8))

            let persisted = optimistic.replacingID(with: serverID)
            persistedTurns.insert(persisted, at: 0)
            director.liveFeedStore.replace(with: persistedTurns)
            rootHost.layoutSubtreeIfNeeded()
            try await settle(for: .milliseconds(16))

            XCTAssertTrue(
                container.subviews.first === mountedHost,
                "동일 직원 제출 중 NSHostingView가 교체됐습니다."
            )
            XCTAssertTrue(
                primaryScrollView(in: rootHost) === mountedScrollView,
                "optimistic/server ID 전환 중 NSScrollView가 교체됐습니다."
            )
            XCTAssertTrue(
                mountedScrollView.documentView === mountedDocumentView,
                "optimistic/server ID 전환 중 대화 문서가 교체됐습니다."
            )
        }

        try await settle(for: .milliseconds(350))
        rootHost.layoutSubtreeIfNeeded()

        guard let documentView = mountedScrollView.documentView else {
            XCTFail("새 업무 제출 뒤 대화 문서가 사라졌습니다.")
            return
        }
        let snapshot = LiveWorkspaceFeedScrollGeometry.snapshot(
            documentBounds: documentView.bounds,
            visibleRect: mountedScrollView.documentVisibleRect,
            isFlipped: documentView.isFlipped
        )

        XCTAssertEqual(director.selectedCharacterID, .boss)
        XCTAssertEqual(container.liveHostingViewCountForTesting, 1)
        XCTAssertFalse(container.hasTransitionLoadingGateForTesting)
        XCTAssertLessThanOrEqual(
            snapshot.distanceFromBottom,
            20,
            "기존 대화가 있는 동일 직원에게 새 업무를 제출한 뒤 "
                + "대화 화면이 문서 위쪽/빈 영역으로 이동했습니다."
        )
        XCTAssertGreaterThan(
            mountedScrollView.documentVisibleRect.intersection(
                documentView.bounds
            ).height,
            0,
            "대화 viewport가 문서 밖의 흰 영역에 남았습니다."
        )
    }

    func testSecondSubmissionKeepsTallCompletedCardsAndViewportInBounds()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let characterID = OfficeCharacter.boss.rawValue
        let tallResponse = (0..<70)
            .map { "완료된 응답 본문 \($0)번째 줄입니다. 내용이 길어 카드가 화면보다 큽니다." }
            .joined(separator: "\n")
        let tallerResponse = (0..<200)
            .map { "첫 카드 본문 \($0)번째 줄입니다. 카드가 매우 큽니다." }
            .joined(separator: "\n")
        let baseDate = Date(timeIntervalSinceReferenceDate: 50_000)
        let completedTurns = [
            makeTurn(
                id: "tall-first",
                characterID: characterID,
                prompt: "첫 번째 질문",
                response: tallerResponse,
                status: .completed,
                startedAt: baseDate
            ),
            makeTurn(
                id: "tall-second",
                characterID: characterID,
                prompt: "두 번째 질문",
                response: tallResponse,
                status: .completed,
                startedAt: baseDate.addingTimeInterval(60)
            ),
        ]
        director.liveFeedStore.replace(with: completedTurns)
        director.liveFeedStore.finishInitialLoading()

        let rootHost = NSHostingView(
            rootView: CachedLiveWorkspaceFeeds(director: director)
                .frame(width: 900, height: 700)
        )
        rootHost.frame = NSRect(x: 0, y: 0, width: 900, height: 700)
        let window = NSWindow(
            contentRect: rootHost.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = rootHost
        rootHost.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(300))

        guard
            let container = allDescendants(of: rootHost)
                .compactMap({ $0 as? CachedLiveWorkspaceFeedsNSView })
                .first,
            let scrollView = try await waitForPrimaryScrollView(
                in: rootHost
            ),
            let documentView = scrollView.documentView
        else {
            XCTFail("긴 완료 카드 2개의 NSScrollView 계층이 없습니다.")
            window.contentView = nil
            return
        }
        defer {
            container.tearDown()
            window.contentView = nil
        }

        let mountedHeight = documentView.bounds.height
        XCTAssertGreaterThan(
            mountedHeight,
            scrollView.contentView.bounds.height * 2,
            "긴 완료 카드 2개가 화면보다 충분히 커야 재현 조건이 됩니다."
        )

        func assertViewportInBounds(_ step: String) {
            guard let currentDocument = scrollView.documentView else {
                XCTFail("\(step): 대화 문서가 사라졌습니다.")
                return
            }
            XCTAssertGreaterThan(
                scrollView.documentVisibleRect.intersection(
                    currentDocument.bounds
                ).height,
                0,
                "\(step): viewport가 문서 밖 흰 영역에 남았습니다."
            )
        }

        // 1) 같은 직원에게 두 번째 질문 제출: 빈 optimistic 턴 삽입.
        let localID = "local-third"
        let serverID = "server-third"
        let submittedAt = baseDate.addingTimeInterval(120)
        director.liveFeedStore.insertOptimisticTurn(
            makeTurn(
                id: localID,
                characterID: characterID,
                prompt: "세 번째 질문",
                status: .running,
                startedAt: submittedAt
            )
        )
        let presentationID = director.liveFeedStore.presentationID(
            forTurnID: localID
        )
        rootHost.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(60))

        let heightAfterOptimistic =
            scrollView.documentView?.bounds.height ?? 0
        XCTAssertGreaterThanOrEqual(
            heightAfterOptimistic,
            mountedHeight - 1,
            "빈 optimistic 턴 삽입이 기존 긴 카드를 제거해 문서 높이가 "
                + "\(Int(mountedHeight)) → \(Int(heightAfterOptimistic))로 "
                + "급락했습니다. 이 급락이 흰 화면의 원인입니다."
        )
        assertViewportInBounds("optimistic 삽입 직후")

        // 2) local → server ID 전환.
        director.liveFeedStore.reconcileOptimisticTurn(
            id: localID,
            with: serverID
        )
        rootHost.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(60))
        XCTAssertEqual(
            director.liveFeedStore.presentationID(forTurnID: serverID),
            presentationID,
            "server ID 전환이 카드 정체성을 바꿨습니다."
        )
        XCTAssertGreaterThanOrEqual(
            scrollView.documentView?.bounds.height ?? 0,
            mountedHeight - 1,
            "server ID 전환 중 기존 카드가 제거돼 문서가 줄었습니다."
        )
        assertViewportInBounds("server ID 전환 직후")

        // 3) 활동·응답 증가를 반영한 서버 스냅샷 반영.
        var persisted = Array(completedTurns.reversed())
        persisted.insert(
            makeTurn(
                id: serverID,
                characterID: characterID,
                prompt: "세 번째 질문",
                response: String(
                    repeating: "생성 중 응답 줄입니다.\n",
                    count: 12
                ),
                status: .running,
                startedAt: submittedAt
            ),
            at: 0
        )
        director.liveFeedStore.replace(with: persisted)
        rootHost.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(60))
        XCTAssertGreaterThanOrEqual(
            scrollView.documentView?.bounds.height ?? 0,
            mountedHeight - 1,
            "응답 증가 반영 중 기존 카드가 제거돼 문서가 줄었습니다."
        )
        assertViewportInBounds("응답 증가 반영 직후")

        // 4) 위로 스크롤해 카드를 실체화한 상태에서 또 한 번 제출한다.
        //    과거 기록을 읽는 중의 새 제출에서 기존 카드 identity와
        //    문서 높이가 유지되고 viewport가 문서 안에 남는지 본다.
        //    표시 창 재슬라이스 제거 자체의 판별 회귀는
        //    LiveWorkspaceFeedDisplayAnchor 단위 테스트가 담당한다.
        //    (흰 화면의 화면 픽셀 상태는 윈도서버 밖에서 관찰할 수
        //    없어 in-process 테스트로는 직접 판정하지 못한다.)
        performLiveScroll(scrollView, toTop: true)
        rootHost.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(150))
        assertViewportInBounds("위로 스크롤 직후")
        let materializedHeight =
            scrollView.documentView?.bounds.height ?? 0
        XCTAssertGreaterThan(
            materializedHeight,
            mountedHeight - 1,
            "위로 스크롤 뒤에는 모든 카드가 실체화돼 있어야 합니다."
        )

        director.liveFeedStore.insertOptimisticTurn(
            makeTurn(
                id: "local-fourth",
                characterID: characterID,
                prompt: "네 번째 질문",
                status: .running,
                startedAt: baseDate.addingTimeInterval(180)
            )
        )
        rootHost.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(80))

        let heightAfterFourth =
            scrollView.documentView?.bounds.height ?? 0
        XCTAssertGreaterThanOrEqual(
            heightAfterFourth,
            mountedHeight - 1,
            "과거 기록을 읽는 중의 새 제출이 최초 안정 문서보다 "
                + "작아졌습니다. 문서가 \(Int(mountedHeight)) → "
                + "\(Int(heightAfterFourth))로 붕괴하면 흰 화면과 "
                + "스크롤바 튐이 재발합니다."
        )
        assertViewportInBounds("실체화 상태 제출 직후")

        try await settle(for: .milliseconds(250))
        assertViewportInBounds("정착 후")
    }

    func testScrolledConversationDoesNotJumpOrLoopOnComposerResizeAndSubmit()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let characterID = OfficeCharacter.boss.rawValue
        let response = (0..<80)
            .map { "완료 응답 \($0)번째 줄입니다. 스크롤 상태를 재현합니다." }
            .joined(separator: "\n")
        let baseDate = Date(timeIntervalSinceReferenceDate: 240_000)
        director.liveFeedStore.replace(with: (0..<4).reversed().map { index in
            makeTurn(
                id: "resize-submit-\(index)",
                characterID: characterID,
                prompt: "기존 질문 \(index)",
                response: response,
                status: .completed,
                startedAt: baseDate.addingTimeInterval(TimeInterval(index))
            )
        })
        director.liveFeedStore.finishInitialLoading()

        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        container.configure(
            director: director,
            selectedCharacterID: .boss
        )
        container.layoutSubtreeIfNeeded()
        let didMount = try await waitNaturallyUntil(timeout: .seconds(4)) {
            container.activeCharacterIDForTesting == .boss
                && !container.hasTransitionLoadingGateForTesting
        }
        XCTAssertTrue(didMount)

        guard
            let scrollView = try await waitForPrimaryScrollView(in: container),
            let documentView = scrollView.documentView
        else {
            XCTFail("실제 대화 NSScrollView 계층이 없습니다.")
            return
        }
        XCTAssertGreaterThan(
            documentView.bounds.height,
            scrollView.contentView.bounds.height * 2
        )

        let clipView = scrollView.contentView
        let scrollableHeight = max(
            0,
            documentView.bounds.height - clipView.bounds.height
        )
        NotificationCenter.default.post(
            name: NSScrollView.willStartLiveScrollNotification,
            object: scrollView
        )
        clipView.scroll(
            to: NSPoint(
                x: clipView.bounds.minX,
                y: documentView.isFlipped
                    ? scrollableHeight / 2
                    : documentView.bounds.minY + scrollableHeight / 2
            )
        )
        scrollView.reflectScrolledClipView(clipView)
        NotificationCenter.default.post(
            name: NSScrollView.didLiveScrollNotification,
            object: scrollView
        )
        NotificationCenter.default.post(
            name: NSScrollView.didEndLiveScrollNotification,
            object: scrollView
        )
        try await settle(for: .milliseconds(120))

        let readingOrigin = clipView.bounds.origin
        let readingSnapshot = LiveWorkspaceFeedScrollGeometry.snapshot(
            documentBounds: documentView.bounds,
            visibleRect: scrollView.documentVisibleRect,
            isFlipped: documentView.isFlipped
        )
        XCTAssertGreaterThan(readingSnapshot.distanceFromBottom, 20)

        // Shift+Enter로 입력창이 커졌다 줄어드는 것과 같은 대화 영역
        // 높이 변화를 만든다. 읽던 위치가 문서 밖이나 하단으로 튀면 안 된다.
        container.setFrameSize(NSSize(width: 900, height: 580))
        container.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(80))
        container.setFrameSize(NSSize(width: 900, height: 700))
        container.layoutSubtreeIfNeeded()
        try await settle(for: .milliseconds(80))

        XCTAssertEqual(
            clipView.bounds.origin.y,
            readingOrigin.y,
            accuracy: 2,
            "입력창 높이 변경만으로 읽던 스크롤 위치가 바뀌었습니다."
        )
        XCTAssertGreaterThan(
            scrollView.documentVisibleRect.intersection(documentView.bounds).height,
            0,
            "입력창 높이 변경 뒤 viewport가 문서 밖 흰 영역을 봅니다."
        )

        // 실제 submit 순서: optimistic 턴을 넣고 command metadata를 발행한다.
        director.liveFeedStore.insertOptimisticTurn(
            makeTurn(
                id: "local-short-submit",
                characterID: characterID,
                prompt: "짧게 답변바람",
                status: .running,
                startedAt: baseDate.addingTimeInterval(10)
            )
        )
        container.updateMetadataForTesting(
            LiveWorkspaceFeedMetadata(
                latestTerminalTurnID: nil,
                latestSubmittedCommandID: UUID(
                    uuidString: "88888888-8888-8888-8888-888888888888"
                ),
                latestStartedCommandID: nil
            )
        )
        try await settle(for: .milliseconds(300))

        let afterSubmitSnapshot = LiveWorkspaceFeedScrollGeometry.snapshot(
            documentBounds: documentView.bounds,
            visibleRect: scrollView.documentVisibleRect,
            isFlipped: documentView.isFlipped
        )
        XCTAssertGreaterThan(
            afterSubmitSnapshot.distanceFromBottom,
            20,
            "과거 대화를 읽는 중의 짧은 제출이 viewport를 강제로 하단으로 "
                + "이동했습니다."
        )
        XCTAssertEqual(
            clipView.bounds.origin.x,
            readingOrigin.x,
            accuracy: 1
        )
        XCTAssertEqual(
            clipView.bounds.origin.y,
            readingOrigin.y,
            accuracy: 2,
            "입력창 높이 변경과 짧은 제출 뒤 읽던 스크롤 위치가 바뀌었습니다."
        )

        var geometryNotificationCount = 0
        let notificationCenter = NotificationCenter.default
        let registrations = [
            notificationCenter.addObserver(
                forName: NSView.boundsDidChangeNotification,
                object: clipView,
                queue: .main
            ) { _ in
                geometryNotificationCount += 1
            },
            notificationCenter.addObserver(
                forName: NSView.frameDidChangeNotification,
                object: documentView,
                queue: .main
            ) { _ in
                geometryNotificationCount += 1
            },
        ]
        defer {
            registrations.forEach(notificationCenter.removeObserver)
        }
        try await settle(for: .seconds(1))
        XCTAssertLessThanOrEqual(
            geometryNotificationCount,
            12,
            "제출이 끝난 뒤에도 scroll/layout이 "
                + "\(geometryNotificationCount)회 계속 반복됩니다."
        )
    }

    func testResizeKeepsEveryBoundedConversationCardMaterialized()
        async throws
    {
        let director = AgentDirector(startBackgroundTasks: false)
        let characterID = OfficeCharacter.leftWoman.rawValue
        let baseDate = Date(timeIntervalSinceReferenceDate: 260_000)
        let response = (0..<45)
            .map { "리사이즈 실체화 검증용 응답 \($0)번째 줄입니다." }
            .joined(separator: "\n")
        director.liveFeedStore.replace(with: (0..<10).reversed().map { index in
            makeTurn(
                id: "resize-materialization-\(index)",
                characterID: characterID,
                prompt: "기존 질문 \(index)",
                response: response,
                status: .completed,
                startedAt: baseDate.addingTimeInterval(TimeInterval(index)),
                backend: .claude
            )
        })
        director.liveFeedStore.finishInitialLoading()

        let container = CachedLiveWorkspaceFeedsNSView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 700)
        )
        let window = NSWindow(
            contentRect: container.bounds,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.contentView = container
        defer {
            container.tearDown()
            window.contentView = nil
        }

        container.configure(
            director: director,
            selectedCharacterID: .leftWoman
        )
        container.layoutSubtreeIfNeeded()
        let didMaterializeInitialWindow = try await waitNaturallyUntil(
            timeout: .seconds(4)
        ) {
            container.activeCharacterIDForTesting == .leftWoman
                && container.mountedCardCountForTesting == 10
                && container.visibleCardCountForTesting > 0
        }
        XCTAssertTrue(
            didMaterializeInitialWindow,
            "표시 창 10건이 모두 실체화되지 않았습니다: mounted="
                + "\(container.mountedCardCountForTesting), visible="
                + "\(container.visibleCardCountForTesting)"
        )

        guard let scrollView = try await waitForPrimaryScrollView(in: container)
        else {
            XCTFail("리사이즈 검증용 NSScrollView가 없습니다.")
            return
        }
        performLiveScroll(scrollView, toTop: false)
        try await settle(for: .milliseconds(80))

        // 실제 함정 기록에서 문제가 난 1,100~1,170pt 폭을 왕복한다.
        // LazyVStack은 이때 추정 문서 높이만 남기고 보이는 카드를 0개로
        // 만들었지만, 제한된 eager 창은 모든 카드 표식을 계속 보유해야 한다.
        let sizes = [
            NSSize(width: 1_102, height: 609),
            NSSize(width: 1_107, height: 613),
            NSSize(width: 1_121, height: 620),
            NSSize(width: 1_146, height: 628),
            NSSize(width: 1_163.5, height: 633),
            NSSize(width: 1_151.5, height: 643),
            NSSize(width: 900, height: 700),
        ]
        for size in sizes {
            container.setFrameSize(size)
            container.layoutSubtreeIfNeeded()
            let stayedMaterialized = try await waitNaturallyUntil(
                timeout: .milliseconds(500)
            ) {
                container.mountedCardCountForTesting == 10
                    && container.visibleCardCountForTesting > 0
            }
            XCTAssertTrue(
                stayedMaterialized,
                "\(Int(size.width))x\(Int(size.height)) 리사이즈 뒤 카드가 "
                    + "미실체화됐습니다: mounted="
                    + "\(container.mountedCardCountForTesting), visible="
                    + "\(container.visibleCardCountForTesting)"
            )
            guard let documentView = scrollView.documentView else {
                XCTFail("리사이즈 중 대화 문서가 사라졌습니다.")
                return
            }
            XCTAssertGreaterThan(
                scrollView.documentVisibleRect.intersection(documentView.bounds)
                    .height,
                0,
                "리사이즈 뒤 viewport가 대화 문서를 벗어났습니다."
            )
        }
    }

    func testScrollGeometryReportsFlippedTopAndBottomWithoutPreferenceKeys() {
        let documentBounds = CGRect(x: 0, y: 0, width: 500, height: 1_000)

        let top = LiveWorkspaceFeedScrollGeometry.snapshot(
            documentBounds: documentBounds,
            visibleRect: CGRect(x: 0, y: 0, width: 500, height: 300),
            isFlipped: true
        )
        XCTAssertEqual(top.distanceFromTop, 0)
        XCTAssertEqual(top.distanceFromBottom, 700)
        XCTAssertEqual(top.viewportHeight, 300)
        XCTAssertEqual(top.contentHeight, 1_000)

        let bottom = LiveWorkspaceFeedScrollGeometry.snapshot(
            documentBounds: documentBounds,
            visibleRect: CGRect(x: 0, y: 700, width: 500, height: 300),
            isFlipped: true
        )
        XCTAssertEqual(bottom.distanceFromTop, 700)
        XCTAssertEqual(bottom.distanceFromBottom, 0)
    }

    func testScrollGeometryHandlesNonFlippedAndShortDocuments() {
        let nonFlippedTop = LiveWorkspaceFeedScrollGeometry.snapshot(
            documentBounds: CGRect(x: 0, y: 0, width: 500, height: 1_000),
            visibleRect: CGRect(x: 0, y: 700, width: 500, height: 300),
            isFlipped: false
        )
        XCTAssertEqual(nonFlippedTop.distanceFromTop, 0)
        XCTAssertEqual(nonFlippedTop.distanceFromBottom, 700)

        let shortDocument = LiveWorkspaceFeedScrollGeometry.snapshot(
            documentBounds: CGRect(x: 0, y: 0, width: 500, height: 200),
            visibleRect: CGRect(x: 0, y: 0, width: 500, height: 300),
            isFlipped: true
        )
        XCTAssertEqual(shortDocument.distanceFromTop, 0)
        XCTAssertEqual(shortDocument.distanceFromBottom, 0)
    }

    func testTopLoadGateLoadsOnlyOnceUntilUserLeavesTopThreshold() {
        var gate = LiveWorkspaceFeedTopLoadGate()

        XCTAssertTrue(
            gate.shouldLoad(
                distanceFromTop: 40,
                threshold: 120,
                isProgrammaticScrollInFlight: false
            )
        )
        XCTAssertFalse(
            gate.shouldLoad(
                distanceFromTop: 20,
                threshold: 120,
                isProgrammaticScrollInFlight: false
            )
        )
        XCTAssertFalse(
            gate.shouldLoad(
                distanceFromTop: 250,
                threshold: 120,
                isProgrammaticScrollInFlight: false
            )
        )
        XCTAssertTrue(
            gate.shouldLoad(
                distanceFromTop: 30,
                threshold: 120,
                isProgrammaticScrollInFlight: false
            )
        )
    }

    func testTopLoadGateDoesNotConsumeProgrammaticTopPosition() {
        var gate = LiveWorkspaceFeedTopLoadGate()

        XCTAssertFalse(
            gate.shouldLoad(
                distanceFromTop: 0,
                threshold: 120,
                isProgrammaticScrollInFlight: true
            )
        )
        XCTAssertTrue(
            gate.shouldLoad(
                distanceFromTop: 0,
                threshold: 120,
                isProgrammaticScrollInFlight: false
            )
        )
        // 로드 뒤 상단 임계 안에 머무는 동안에는 새 휠 세션이
        // 시작돼도 다음 페이지를 연쇄 로드하지 않는다. 문서가 짧아진
        // 비정상 상태에서 페이지가 계속 쌓여 CPU가 치솟는 것을 막는다.
        XCTAssertFalse(
            gate.shouldLoad(
                distanceFromTop: 0,
                threshold: 120,
                isProgrammaticScrollInFlight: false
            )
        )
        XCTAssertFalse(
            gate.shouldLoad(
                distanceFromTop: 60,
                threshold: 120,
                isProgrammaticScrollInFlight: false
            )
        )
        // 임계 밖으로 나가면 재장전되고, 다시 상단에 닿으면 다음
        // 페이지를 한 번 로드한다.
        XCTAssertFalse(
            gate.shouldLoad(
                distanceFromTop: 300,
                threshold: 120,
                isProgrammaticScrollInFlight: false
            )
        )
        XCTAssertTrue(
            gate.shouldLoad(
                distanceFromTop: 10,
                threshold: 120,
                isProgrammaticScrollInFlight: false
            )
        )
    }

    private func makeTurns(
        streamingStep: Int? = nil,
        completedResponse: String? = nil
    ) -> [LiveFeedTurn] {
        let origin = Date(timeIntervalSinceReferenceDate: 10_000)
        return OfficeCharacter.allCases.flatMap { character in
            (0..<30).map { index in
                let timestamp = origin.addingTimeInterval(
                    TimeInterval(index)
                )
                let isStreaming = streamingStep != nil && index == 29
                return LiveFeedTurn(
                    id: "\(character.rawValue)-\(index)",
                    characterId: character.rawValue,
                    characterName: character.rawValue,
                    characterBackend: .codex,
                    backend: .codex,
                    model: "gpt-5.6-sol",
                    effort: "high",
                    fastMode: false,
                    externalSessionId: nil,
                    conversationWorkdir: "/repo",
                    prompt: "업무 \(index)",
                    response: isStreaming
                        ? String(
                            repeating: "진행 중인 응답 줄입니다.\n",
                            count: streamingStep ?? 1
                        )
                        : completedResponse ?? "완료 \(index)",
                    feedback: nil,
                    status: isStreaming ? .running : .completed,
                    needsInput: false,
                    errorMessage: nil,
                    responseSourceWarning: nil,
                    wikiProposalWarning: nil,
                    startedAt: timestamp,
                    endedAt: isStreaming ? nil : timestamp,
                    updatedAt: timestamp.addingTimeInterval(
                        TimeInterval(streamingStep ?? 0)
                    ),
                    estimatedCostUsd: nil,
                    sessionContext: nil,
                    activities: [],
                    sources: nil,
                    workspace: nil
                )
            }
        }
    }

    private func makeTurn(
        id: String,
        characterID: String,
        prompt: String,
        response: String = "",
        status: LiveTurnStatus,
        needsInput: Bool = false,
        startedAt: Date,
        updatedAt: Date? = nil,
        activities: [LiveFeedActivity] = [],
        backend: AgentBackend = .codex,
        model: String? = nil
    ) -> LiveFeedTurn {
        LiveFeedTurn(
            id: id,
            characterId: characterID,
            characterName: characterID,
            characterBackend: backend,
            backend: backend,
            model: model ?? (backend == .claude
                ? "claude-sonnet-5"
                : "gpt-5.6-sol"),
            effort: "high",
            fastMode: false,
            externalSessionId: nil,
            conversationWorkdir: "/repo",
            prompt: prompt,
            response: response,
            feedback: nil,
            status: status,
            needsInput: needsInput,
            errorMessage: nil,
            responseSourceWarning: nil,
            wikiProposalWarning: nil,
            startedAt: startedAt,
            endedAt: status.isRunning ? nil : startedAt,
            updatedAt: updatedAt ?? startedAt,
            estimatedCostUsd: nil,
            sessionContext: nil,
            activities: activities,
            sources: nil,
            workspace: nil
        )
    }

    private func makeClaudeTurns(
        characterID: OfficeCharacter,
        label: String,
        baseDate: Date
    ) throws -> [LiveFeedTurn] {
        try (0..<10).map { turnIndex in
            let occurredAt = baseDate.addingTimeInterval(
                TimeInterval(turnIndex * 100)
            )
            let activities = try (0..<24).map { activityIndex in
                let kind: String
                switch activityIndex % 3 {
                case 0:
                    kind = "thinking"
                case 1:
                    kind = "tool"
                default:
                    kind = "message"
                }
                return try makeActivity(
                    id: "\(characterID.rawValue)-claude-\(turnIndex)-\(activityIndex)",
                    kind: kind,
                    text: "\(label) Claude 활동 \(activityIndex): "
                        + String(
                            repeating: "긴 transcript 본문과 도구 결과 ",
                            count: 5
                        ),
                    status: "completed",
                    occurredAt: occurredAt.addingTimeInterval(
                        TimeInterval(activityIndex)
                    )
                )
            }
            let response = """
            ### \(label) Claude 완료 응답 \(turnIndex)

            | 검증 항목 | 결과 |
            |---|---|
            | backend | Claude |
            | transcript | 긴 활동 24개 |

            \((0..<45).map {
                "- \(label) 장문 Markdown \($0)번째 줄: 화면 전환 중 문서 높이와 viewport가 안정되어야 합니다."
            }.joined(separator: "\n"))

            ```text
            \(String(repeating: "Claude 도구 출력과 추론 내용\n", count: 18))
            ```
            """
            return makeTurn(
                id: "\(characterID.rawValue)-claude-turn-\(turnIndex)",
                characterID: characterID.rawValue,
                prompt: "\(label) Claude 업무 \(turnIndex)",
                response: response,
                status: .completed,
                startedAt: occurredAt,
                updatedAt: occurredAt.addingTimeInterval(30),
                activities: activities,
                backend: .claude
            )
        }
    }

    private func makeComplexBossTurns(
        updateStep: Int,
        baseDate: Date
    ) throws -> [LiveFeedTurn] {
        let completedTurns = (0..<9).map { index in
            let response = """
            ### 완료 카드 \(index)

            | 항목 | 값 |
            |---|---|
            | 카드 | \(index) |
            | 상태 | 완료 |

            - 복귀할 때도 이 Markdown 카드가 유지되어야 합니다.
            - 문서 높이가 충분히 커야 viewport 이탈을 검출할 수 있습니다.

            ```text
            완료 카드 \(index)의 여러 줄 코드 블록
            첫 번째 줄
            두 번째 줄
            세 번째 줄
            네 번째 줄
            ```
            """
            return makeTurn(
                id: "boss-completed-\(index)",
                characterID: OfficeCharacter.boss.rawValue,
                prompt: "백부장 완료 업무 \(index)",
                response: response,
                status: .completed,
                startedAt: baseDate.addingTimeInterval(TimeInterval(index))
            )
        }
        let activities = try (0..<updateStep).map { index in
            try makeActivity(
                id: "boss-stream-activity-\(index)",
                kind: index.isMultiple(of: 2) ? "reasoning" : "command",
                text: "숨은 동안 추가된 활동 \(index)",
                status: index == updateStep - 1 ? "running" : "completed",
                occurredAt: baseDate.addingTimeInterval(
                    101 + TimeInterval(index)
                )
            )
        }
        let running = makeTurn(
            id: "boss-streaming",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "백부장 실행 중 업무",
            response: String(
                repeating: "스트리밍 갱신 \(updateStep) 본문입니다.\n",
                count: max(1, updateStep)
            ),
            status: .running,
            startedAt: baseDate.addingTimeInterval(100),
            updatedAt: baseDate.addingTimeInterval(100 + TimeInterval(updateStep)),
            activities: activities
        )
        return [running] + Array(completedTurns.reversed())
    }

    private func makeActivity(
        id: String,
        kind: String,
        text: String,
        status: String,
        occurredAt: Date
    ) throws -> LiveFeedActivity {
        let data = try JSONSerialization.data(withJSONObject: [
            "id": id,
            "kind": kind,
            "text": text,
            "status": status,
            "occurredAt": occurredAt.timeIntervalSince1970,
        ])
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .secondsSince1970
        return try decoder.decode(LiveFeedActivity.self, from: data)
    }

    private func assertViewportIntersectsDocument(
        _ scrollView: NSScrollView,
        step: String,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        guard let documentView = scrollView.documentView else {
            XCTFail("\(step): 대화 문서가 없습니다.", file: file, line: line)
            return
        }
        let intersection = scrollView.documentVisibleRect.intersection(
            documentView.bounds
        )
        XCTAssertGreaterThan(
            intersection.width,
            0,
            "\(step): viewport 가로 영역이 문서와 겹치지 않습니다.",
            file: file,
            line: line
        )
        XCTAssertGreaterThan(
            intersection.height,
            0,
            "\(step): viewport가 문서 밖 흰 영역을 보고 있습니다.",
            file: file,
            line: line
        )
    }

    private func settle(for duration: Duration) async throws {
        try await Task.sleep(for: duration)
        await Task.yield()
    }

    private func waitUntil(
        in root: NSView? = nil,
        timeout: Duration = .seconds(1),
        pollInterval: Duration = .milliseconds(4),
        condition: () -> Bool
    ) async throws -> Bool {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        repeat {
            root?.layoutSubtreeIfNeeded()
            if condition() {
                return true
            }
            try await settle(for: pollInterval)
        } while clock.now < deadline
        root?.layoutSubtreeIfNeeded()
        return condition()
    }

    /// AppKit/SwiftUI의 실제 layout 이벤트와 mount reporter만으로 준비되는지
    /// 확인한다. 기존 waitUntil처럼 4ms마다 layoutSubtreeIfNeeded를 강제로
    /// 호출하지 않아, 테스트가 준비 신호 결함을 가리지 못하게 한다.
    private func waitNaturallyUntil(
        timeout: Duration = .seconds(1),
        pollInterval: Duration = .milliseconds(20),
        condition: () -> Bool
    ) async throws -> Bool {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        repeat {
            if condition() {
                return true
            }
            try await settle(for: pollInterval)
        } while clock.now < deadline
        return condition()
    }

    private func primaryScrollView(in root: NSView) -> NSScrollView? {
        allDescendants(of: root)
            .compactMap { $0 as? NSScrollView }
            .max { lhs, rhs in
                lhs.bounds.width * lhs.bounds.height
                    < rhs.bounds.width * rhs.bounds.height
            }
    }

    private func liveFeedHost(in container: NSView) -> NSView? {
        container.subviews.first { subview in
            primaryScrollView(in: subview) != nil
        }
    }

    private func liveFeedHostCount(in container: NSView) -> Int {
        container.subviews.count { subview in
            primaryScrollView(in: subview) != nil
        }
    }

    private func waitForPrimaryScrollView(
        in root: NSView,
        timeout: Duration = .milliseconds(500)
    ) async throws -> NSScrollView? {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        repeat {
            root.layoutSubtreeIfNeeded()
            if let scrollView = primaryScrollView(in: root) {
                return scrollView
            }
            try await settle(for: .milliseconds(4))
        } while clock.now < deadline
        root.layoutSubtreeIfNeeded()
        return primaryScrollView(in: root)
    }

    private func allDescendants(of view: NSView) -> [NSView] {
        view.subviews.flatMap { subview in
            [subview] + allDescendants(of: subview)
        }
    }

    private func performLiveScroll(
        _ scrollView: NSScrollView,
        toTop: Bool
    ) {
        guard let documentView = scrollView.documentView else {
            return
        }
        let clipView = scrollView.contentView
        let scrollableHeight = max(
            0,
            documentView.bounds.height - clipView.bounds.height
        )
        let targetY: CGFloat
        if documentView.isFlipped {
            targetY = toTop ? documentView.bounds.minY : scrollableHeight
        } else {
            targetY = toTop ? scrollableHeight : documentView.bounds.minY
        }

        NotificationCenter.default.post(
            name: NSScrollView.willStartLiveScrollNotification,
            object: scrollView
        )
        clipView.scroll(to: NSPoint(x: clipView.bounds.minX, y: targetY))
        scrollView.reflectScrolledClipView(clipView)
        NotificationCenter.default.post(
            name: NSScrollView.didLiveScrollNotification,
            object: scrollView
        )
        NotificationCenter.default.post(
            name: NSScrollView.didEndLiveScrollNotification,
            object: scrollView
        )
    }

    private func performSmallLiveScroll(
        _ scrollView: NSScrollView,
        delta: CGFloat
    ) {
        guard let documentView = scrollView.documentView else {
            return
        }
        let clipView = scrollView.contentView
        let maximumY = max(
            documentView.bounds.minY,
            documentView.bounds.height - clipView.bounds.height
        )
        let targetY = min(
            maximumY,
            max(documentView.bounds.minY, clipView.bounds.minY + delta)
        )
        NotificationCenter.default.post(
            name: NSScrollView.willStartLiveScrollNotification,
            object: scrollView
        )
        clipView.scroll(to: NSPoint(x: clipView.bounds.minX, y: targetY))
        scrollView.reflectScrolledClipView(clipView)
        NotificationCenter.default.post(
            name: NSScrollView.didLiveScrollNotification,
            object: scrollView
        )
        NotificationCenter.default.post(
            name: NSScrollView.didEndLiveScrollNotification,
            object: scrollView
        )
    }
}

@MainActor
private final class FlippedScrollDocumentView: NSView {
    override var isFlipped: Bool {
        true
    }
}

private final class WeakTestReference<Object: AnyObject> {
    weak var value: Object?

    init(_ value: Object?) {
        self.value = value
    }
}
