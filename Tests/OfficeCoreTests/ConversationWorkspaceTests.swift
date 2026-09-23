import AppKit
import Combine
import Darwin
import OfficeCore
import SwiftUI
import XCTest
@testable import OfficeGame

@MainActor
final class ConversationWorkspaceTests: XCTestCase {
    func testMeasureNativeComposerLatency() async throws {
        guard let output = ProcessInfo.processInfo.environment["OFFICESTRA_COMPOSER_PERF"] else {
            throw XCTSkip("Set OFFICESTRA_COMPOSER_PERF for the opt-in native input diagnostic")
        }
        var results: [String: [String: Double]] = [:]
        func descendants(_ view: NSView) -> [NSView] { [view] + view.subviews.flatMap(descendants) }
        for split in [false, true] {
            let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
            director.selectedCharacterID = .rightMan
            director.employeeComposerStore.focusesComposerOnSelection = false
            if split {
                director.toggleConversationSplit()
                chooseReady(director, .rightWoman, in: .right)
                let markdown = String(repeating: "### 내용\n\n긴 대화의 **배치 비용**과 입력 반응을 측정합니다.\n\n| 항목 | 결과 |\n| --- | --- |\n| 대화 | 유지 |\n\n", count: 20)
                director.liveFeedStore.replace(with: [turn(.rightMan, text: markdown), turn(.rightWoman, text: markdown)])
                director.liveFeedStore.finishInitialLoading()
            }
            let content: AnyView = split
                ? AnyView(SplitPerformanceWorkspace(director: director, selection: director.characterSelectionStore))
                : AnyView(CommandEntryRow(director: director, placeholder: "입력", attachmentCount: 0,
                    isPreparingAttachments: false, onChooseAttachments: {}, onSubmit: { _ in false }))
            let root = NSHostingView(rootView: content.frame(width: 1000, height: 720))
            root.frame = CGRect(x: 0, y: 0, width: 1000, height: 720)
            let window = NSWindow(contentRect: root.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
            window.contentView = root
            defer {
                descendants(root).compactMap { $0 as? ConversationFeedHostsNSView }.forEach { $0.tearDown() }
                window.contentView = nil
            }
            try await settle(root)
            let input = try XCTUnwrap(descendants(root).compactMap { $0 as? CommandComposerTextView }.first)
            for (label, character) in [("english", "a"), ("korean", "가"), ("newlines", "\n")] {
                input.string = ""
                input.didChangeText()
                try await settle(root)
                let documents = descendants(root).compactMap { $0 as? SelectableMarkdownDocumentView }
                let before = documents.reduce(0) { $0 + $1.layoutPassCount }
                var editMS: [Double] = [], layoutMS: [Double] = []
                for _ in 0..<40 {
                    // This offline fixture deliberately never connects a backend.
                    // Re-enable the native editor after each availability refresh.
                    input.isEditable = true
                    let start = ProcessInfo.processInfo.systemUptime
                    input.insertText(character, replacementRange: NSRange(location: (input.string as NSString).length, length: 0))
                    editMS.append((ProcessInfo.processInfo.systemUptime - start) * 1000)
                    let renderStart = ProcessInfo.processInfo.systemUptime
                    root.layoutSubtreeIfNeeded()
                    root.displayIfNeeded()
                    layoutMS.append((ProcessInfo.processInfo.systemUptime - renderStart) * 1000)
                    try await Task.sleep(for: .milliseconds(20))
                }
                func p95(_ values: [Double]) -> Double { values.sorted()[Int(Double(values.count - 1) * 0.95)] }
                results["\(split ? "split" : "inputOnly")_\(label)"] = [
                    "editP95MS": p95(editMS), "editMaxMS": editMS.max() ?? 0,
                    "layoutP95MS": p95(layoutMS), "layoutMaxMS": layoutMS.max() ?? 0,
                    "markdownLayouts": Double(documents.reduce(0) { $0 + $1.layoutPassCount } - before)
                ]
                XCTAssertEqual(input.string, String(repeating: character, count: 40))
            }
        }
        try JSONSerialization.data(withJSONObject: results, options: [.prettyPrinted, .sortedKeys]).write(to: URL(fileURLWithPath: output))
    }

    func testFocusDoesNotInvalidateWholeOfficeButStillUpdatesSelectionAndBubble() throws {
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        director.selectedCharacterID = .rightMan
        director.toggleConversationSplit()
        chooseReady(director, .rightWoman, in: .right)
        var officeUpdates = 0
        let observation = director.objectWillChange.sink { officeUpdates += 1 }
        defer { observation.cancel() }
        for _ in 0..<20 {
            chooseReady(director, .rightMan, in: .left)
            XCTAssertEqual(director.selectedCharacterID, .rightMan)
            XCTAssertNotNil(director.speechBubbleStore.bubbles[.rightMan])
            chooseReady(director, .rightWoman, in: .right)
            XCTAssertEqual(director.selectedCharacterID, .rightWoman)
            XCTAssertNotNil(director.speechBubbleStore.bubbles[.rightWoman])
        }
        XCTAssertEqual(officeUpdates, 0, "Pane focus belongs to the selection store, not the whole office")
    }

    func testIdenticalLocalModelPollDoesNotInvalidateOfficeAndChangesStillArrive() throws {
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        let catalog = try JSONDecoder().decode(LocalProviderList.self, from: Data(#"{"statuses":[{"id":"local","state":"ready"}],"profiles":[],"assignments":[{"characterId":"right-man","profileId":"local","reasoning":"high","address":"192.168.1.2"}]}"#.utf8))
        director.applyLocalProviderCatalog(catalog)
        var updates = 0
        let observation = director.objectWillChange.sink { updates += 1 }
        defer { observation.cancel() }
        for _ in 0..<100 { director.applyLocalProviderCatalog(catalog) }
        XCTAssertEqual(updates, 0)
        let stopped = LocalProviderList(statuses: [LocalProviderStatus(id: "local", state: "stopped", error: nil)], profiles: [], assignments: [])
        director.applyLocalProviderCatalog(stopped)
        XCTAssertGreaterThan(updates, 0)
        XCTAssertEqual(director.localProviderStatuses.first?.state, "stopped")
        XCTAssertTrue(director.localProfileAssignments.isEmpty)
        XCTAssertTrue(director.localReasoningSelections.isEmpty)
        XCTAssertTrue(director.localHostAddresses.isEmpty)
    }

    func testMeasureSplitInteractionWorkload() async throws {
        guard let output = ProcessInfo.processInfo.environment["OFFICESTRA_SPLIT_PERF"] else {
            throw XCTSkip("Set OFFICESTRA_SPLIT_PERF to a JSON output path for the native workload")
        }
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        director.selectedCharacterID = .rightMan
        director.toggleConversationSplit()
        chooseReady(director, .rightWoman, in: .right)
        let markdown = String(repeating: "### 검토 내용\n\n본문을 유지하면서 **포커스 이동**, 입력, 스크롤을 함께 검증합니다. 긴 문장의 줄바꿈과 레이아웃 비용을 측정합니다.\n\n| 항목 | 결과 |\n| --- | --- |\n| 대화 | 계속 유지 |\n| 선택 | 독립 보존 |\n\n", count: 12)
        director.liveFeedStore.replace(with: [turn(.rightMan, text: markdown), turn(.rightWoman, text: markdown)])
        director.liveFeedStore.finishInitialLoading()
        let root = NSHostingView(rootView: SplitPerformanceWorkspace(director: director, selection: director.characterSelectionStore)
            .frame(width: 1000, height: 720))
        root.frame = CGRect(x: 0, y: 0, width: 1000, height: 720)
        let window = NSWindow(contentRect: root.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = root
        if ProcessInfo.processInfo.environment["OFFICESTRA_SPLIT_PERF_VISIBLE"] == "1" { window.orderFront(nil) }
        defer { window.orderOut(nil); window.contentView = nil }
        try await settle(root)
        func descendants(_ view: NSView) -> [NSView] { [view] + view.subviews.flatMap(descendants) }
        let feeds = try XCTUnwrap(descendants(root).compactMap { $0 as? ConversationFeedHostsNSView }.first)
        defer { feeds.tearDown() }
        let documents = descendants(root).compactMap { $0 as? SelectableMarkdownDocumentView }
        XCTAssertFalse(documents.isEmpty)
        let scrolls = [OfficeCharacter.rightMan, .rightWoman].compactMap { feeds.hostForTesting($0) }.compactMap { scrollView(in: $0) }
        XCTAssertEqual(scrolls.count, 2)
        func cpu() -> Double {
            var usage = rusage()
            getrusage(RUSAGE_SELF, &usage)
            return Double(usage.ru_utime.tv_sec + usage.ru_stime.tv_sec)
                + Double(usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) / 1_000_000
        }
        var results: [String: [String: Double]] = [:]
        func measure(_ name: String, _ action: () async throws -> Void) async throws {
            let started = Date(), used = cpu()
            let currentDocuments = descendants(root).compactMap { $0 as? SelectableMarkdownDocumentView }
            let requests = currentDocuments.reduce(0) { $0 + $1.heightRequestCount }
            let layouts = currentDocuments.reduce(0) { $0 + $1.layoutPassCount }
            let created = SelectableMarkdownDocumentView.createdCount
            try await action()
            results[name] = ["cpuPercent": (cpu() - used) / Date().timeIntervalSince(started) * 100,
                "heightRequests": Double(currentDocuments.reduce(0) { $0 + $1.heightRequestCount } - requests),
                "layoutPasses": Double(currentDocuments.reduce(0) { $0 + $1.layoutPassCount } - layouts),
                "documents": Double(currentDocuments.count),
                "created": Double(SelectableMarkdownDocumentView.createdCount - created)]
        }
        func focus(_ i: Int) { director.chooseConversationCharacter(i.isMultiple(of: 2) ? .rightMan : .rightWoman,
            in: i.isMultiple(of: 2) ? .left : .right, focusesComposer: false) }
        func type(_ i: Int) {
            director.employeeComposerStore.draftStore.drafts[director.selectedCharacterID ?? .rightMan] = CommandEntryDraft(text: "입력 중 \(i)")
        }
        func scroll(_ i: Int) {
            for scroll in scrolls {
                let origin = scroll.contentView.bounds.origin
                scroll.contentView.scroll(to: CGPoint(x: origin.x, y: max(0, origin.y + (i % 10 < 5 ? -14 : 14))))
                scroll.reflectScrolledClipView(scroll.contentView)
            }
        }
        try await measure("idle") { try await Task.sleep(for: .seconds(2)) }
        try await measure("focus") { for i in 0..<30 { focus(i); try await Task.sleep(for: .milliseconds(40)) } }
        try await measure("typing") { for i in 0..<40 { type(i); try await Task.sleep(for: .milliseconds(30)) } }
        try await measure("scroll") { for i in 0..<40 { scroll(i); try await Task.sleep(for: .milliseconds(30)) } }
        try await measure("combined") { for i in 0..<40 { focus(i); type(i); scroll(i); try await Task.sleep(for: .milliseconds(30)) } }
        try await Task.sleep(for: .milliseconds(500))
        try await measure("idleAfter") { try await Task.sleep(for: .seconds(2)) }
        try JSONSerialization.data(withJSONObject: results, options: [.prettyPrinted, .sortedKeys]).write(to: URL(fileURLWithPath: output))
    }

    func testFocusLineAnimationSurvivesOrdinaryUpdatesAndStopsWhenDetached() {
        let view = CoreAnimationFocusLineNSView(frame: CGRect(x: 0, y: 0, width: 460, height: 2))
        let window = NSWindow(contentRect: view.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = view
        defer { window.contentView = nil }
        view.layoutSubtreeIfNeeded()
        XCTAssertTrue(view.isShimmering)
        let starts = view.animationStarts
        for _ in 0..<100 {
            view.setAnimated(true)
            view.needsLayout = true
            view.layoutSubtreeIfNeeded()
        }
        XCTAssertEqual(view.animationStarts, starts, "Feed updates must not restart the compositor animation")
        XCTAssertNil(view.hitTest(.zero))
        view.setAnimated(false)
        XCTAssertFalse(view.isShimmering, "Reduce Motion must stop the moving highlight")
        view.setAnimated(true)
        XCTAssertTrue(view.isShimmering)
        window.contentView = nil
        XCTAssertFalse(view.isShimmering, "An unmounted indicator must not keep animating")
    }

    func testLoadingRejectsRepeatedSelectionsBeforeChangingPaneOrEmployee() {
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        director.selectedCharacterID = .rightMan
        director.toggleConversationSplit()
        let initial = director.conversationLayout.state
        for _ in 0..<100 {
            director.chooseConversationCharacter(.rightWoman, in: .right)
            director.chooseConversationCharacter(.leftMan, in: .left)
        }
        XCTAssertEqual(director.conversationLayout.state, initial)
        XCTAssertEqual(director.selectedCharacterID, .rightMan)
        finishSelection(director)
        director.chooseConversationCharacter(.rightWoman, in: .right)
        let pending = director.conversationLayout.state
        for _ in 0..<100 { director.chooseConversationCharacter(.rightMan, in: .left) }
        XCTAssertEqual(director.conversationLayout.state, pending)
        XCTAssertEqual(director.selectedCharacterID, .rightWoman)
        finishSelection(director)
        director.chooseConversationCharacter(.rightMan, in: .left)
        XCTAssertEqual(director.selectedCharacterID, .rightMan)
    }

    func testReplacingPaneReleasesHiddenChatHostAndShowsLoadingGate() async throws {
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        director.selectedCharacterID = .rightMan
        director.toggleConversationSplit()
        chooseReady(director, .rightWoman, in: .right)
        let view = ConversationFeedHostsNSView(frame: CGRect(x: 0, y: 0, width: 920, height: 500))
        defer { view.tearDown() }
        view.configure(director: director, layout: director.conversationLayout)
        weak let old = view.hostForTesting(.rightWoman)
        XCTAssertTrue(try XCTUnwrap(old).hasTransitionLoadingGateForTesting)
        for character in [OfficeCharacter.leftMan, .leftWoman, .boss] {
            chooseReady(director, character, in: .right)
            view.configure(director: director, layout: director.conversationLayout)
            XCTAssertEqual(view.visibleCharactersForTesting, [.rightMan, character])
            XCTAssertTrue(try XCTUnwrap(view.hostForTesting(character)).hasTransitionLoadingGateForTesting)
        }
        // AppKit releases removed views after its deferred display cycle.
        for _ in 0..<50 {
            if old == nil { break }
            try await Task.sleep(for: .milliseconds(40))
        }
        XCTAssertNil(old, "Hidden chat trees must be released instead of accumulating across employees")
        XCTAssertNil(view.hostForTesting(.rightWoman))
        XCTAssertNil(view.hostForTesting(.leftMan))
        XCTAssertNil(view.hostForTesting(.leftWoman))
    }

    func testPaneClicksSynchronizeEmployeeWithoutTakingTextFocusOrReactingToHover() throws {
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        director.selectedCharacterID = .rightMan
        director.toggleConversationSplit()
        chooseReady(director, .rightWoman, in: .right)
        let root = NSView(frame: CGRect(x: 0, y: 0, width: 1000, height: 600))
        let observer = ConversationPaneFocusNSView(frame: CGRect(x: 30, y: 50, width: 920, height: 500))
        observer.director = director
        root.addSubview(observer)
        let text = NSTextView(frame: CGRect(x: 50, y: 100, width: 200, height: 100))
        text.string = "본문 선택 유지"
        root.addSubview(text)
        let window = NSWindow(contentRect: root.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = root
        defer { observer.stopObserving(); window.contentView = nil }
        window.makeFirstResponder(text)
        text.setSelectedRange(NSRange(location: 0, length: 2))
        func event(_ type: NSEvent.EventType, x: CGFloat, y: CGFloat = 100) throws -> NSEvent {
            try XCTUnwrap(NSEvent.mouseEvent(with: type, location: observer.convert(CGPoint(x: x, y: y), to: nil),
                modifierFlags: [], timestamp: 0, windowNumber: window.windowNumber,
                context: nil, eventNumber: 1, clickCount: 1, pressure: 1))
        }

        observer.observeFocusEvent(try event(.mouseMoved, x: 100))
        XCTAssertEqual(director.selectedCharacterID, .rightWoman)
        finishSelection(director)
        observer.observeFocusEvent(try event(.leftMouseDown, x: 100))
        XCTAssertEqual(director.selectedCharacterID, .rightMan)
        XCTAssertEqual(director.conversationLayout.state.active, .left)
        XCTAssertFalse(director.employeeComposerStore.focusesComposerOnSelection)
        XCTAssertTrue(window.firstResponder === text)
        XCTAssertEqual(text.selectedRange(), NSRange(location: 0, length: 2))
        XCTAssertNil(observer.hitTest(CGPoint(x: 100, y: 100)))

        let divider = ConversationLayoutStore.leftWidth(in: 920, fraction: 0.5)
        observer.observeFocusEvent(try event(.leftMouseDown, x: divider + 4))
        observer.observeFocusEvent(try event(.leftMouseDown, x: 800, y: 510))
        XCTAssertEqual(director.selectedCharacterID, .rightMan)
        finishSelection(director)
        observer.observeFocusEvent(try event(.rightMouseDown, x: 800))
        XCTAssertEqual(director.selectedCharacterID, .rightWoman)
        XCTAssertEqual(director.conversationLayout.state.active, .right)
        chooseReady(director, .rightMan, in: .left)
        XCTAssertTrue(director.employeeComposerStore.focusesComposerOnSelection, "Explicit name selection can focus the composer again")
        director.toggleConversationSplit()
        observer.observeFocusEvent(try event(.leftMouseDown, x: 800))
        XCTAssertEqual(director.selectedCharacterID, .rightMan)
    }

    func testSplitKeepsCurrentConversationAndDoesNotGuessTheFirstRightEmployee() {
        let layout = ConversationLayoutStore(defaults: nil)
        layout.select(.rightMan)
        layout.toggle(current: .rightMan)
        XCTAssertEqual(layout.state.left, .rightMan)
        XCTAssertNil(layout.state.right)
        XCTAssertEqual(layout.selected, .rightMan)
        layout.choose(.rightWoman, in: .right)
        XCTAssertEqual(layout.visible, [.rightMan, .rightWoman])
        layout.select(.rightMan)
        XCTAssertEqual(layout.state.active, .left)
        XCTAssertEqual(layout.state.right, .rightWoman)
        layout.choose(.rightWoman, in: .left)
        XCTAssertEqual(layout.state.active, .right, "Selecting an already displayed employee focuses that pane")
        XCTAssertEqual(layout.state.left, .rightMan)
        layout.toggle(current: layout.selected)
        XCTAssertEqual(layout.selected, .rightWoman)
        XCTAssertEqual(layout.visible, [.rightWoman])
        layout.toggle(current: .rightMan)
        XCTAssertEqual(layout.state.right, .rightWoman)
    }

    func testLastRightAndInnerRatioPersistWithoutChangingTheOuterOfficeWidth() throws {
        let suite = "ConversationWorkspaceTests-\(UUID())"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        defaults.set(0.42, forKey: "officeLeftColumnFraction")
        let first = ConversationLayoutStore(defaults: defaults)
        first.toggle(current: .rightMan)
        first.choose(.rightWoman, in: .right)
        first.resize(to: 0.6)
        first.saveSize()
        let restored = ConversationLayoutStore(defaults: defaults)
        restored.toggle(current: .boss)
        XCTAssertEqual(restored.state.right, .rightWoman)
        XCTAssertEqual(restored.state.fraction, 0.6)
        XCTAssertEqual(defaults.double(forKey: "officeLeftColumnFraction"), 0.42)
        restored.resize(to: .nan)
        XCTAssertEqual(restored.state.fraction, 0.6)
        for width in [0.0, 200, 569, 900] {
            let frames = restored.frames(in: CGRect(x: 0, y: 0, width: width, height: 500))
            XCTAssertTrue(frames.values.allSatisfy { $0.width >= 0 && $0.height >= 0 })
        }
    }

    func testBothFeedsKeepPublishingWhileOnlyOneEmployeeIsTheInputTarget() {
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        director.selectedCharacterID = .rightMan
        director.toggleConversationSplit()
        chooseReady(director, .rightWoman, in: .right)
        director.liveFeedStore.replace(with: [turn(.rightMan, text: "왼쪽 첫 답변"), turn(.rightWoman, text: "오른쪽 첫 답변")])
        director.liveFeedStore.replace(with: [turn(.rightMan, text: "왼쪽 계속 갱신"), turn(.rightWoman, text: "오른쪽 계속 갱신")])
        XCTAssertEqual(director.liveFeedStore.characterStore(for: "right-man").turns.first?.response, "왼쪽 계속 갱신")
        XCTAssertEqual(director.liveFeedStore.characterStore(for: "right-woman").turns.first?.response, "오른쪽 계속 갱신")
        XCTAssertEqual(director.selectedCharacterID, .rightWoman)
        chooseReady(director, .rightMan, in: .left)
        XCTAssertEqual(director.liveFeedStore.visibleCharacterFeedIDs, ["right-man", "right-woman"])
        director.toggleConversationSplit()
        XCTAssertEqual(director.liveFeedStore.visibleCharacterFeedIDs, ["right-man"])
    }

    func testDraftsAndAttachmentsStayWithTheirEmployeeAndTypingDoesNotPublishTheWholeComposer() {
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        let store = director.employeeComposerStore
        let attachment = PendingAttachment(sourceURL: URL(fileURLWithPath: "/tmp/user.txt"), stagedURL: URL(fileURLWithPath: "/tmp/staged.txt"))
        store.drafts[.rightMan] = CommandEntryDraft(text: "작성 중인 안건")
        store.attachments[.rightMan] = [attachment]
        director.selectedCharacterID = .rightMan
        director.toggleConversationSplit()
        chooseReady(director, .rightWoman, in: .right)
        XCTAssertNil(store.drafts[.rightWoman])
        XCTAssertNil(store.attachments[.rightWoman])
        var broadUpdates = 0
        let subscription = store.objectWillChange.sink { broadUpdates += 1 }
        store.drafts[.rightWoman] = CommandEntryDraft(text: "별도 초안")
        XCTAssertEqual(broadUpdates, 0, "Keystrokes must update only the text row")
        chooseReady(director, .rightMan, in: .left)
        XCTAssertEqual(store.drafts[.rightMan]?.text, "작성 중인 안건")
        XCTAssertEqual(store.attachments[.rightMan], [attachment])
        XCTAssertEqual(store.drafts[.rightWoman]?.text, "별도 초안")
        subscription.cancel()
    }

    func testFeedHostsSurviveFocusChangesAndMergingWithoutSharingScrollViews() async throws {
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        director.selectedCharacterID = .rightMan
        director.liveFeedStore.replace(with: [turn(.rightMan, text: String(repeating: "왼쪽 대화 내용입니다.\n\n", count: 100)), turn(.rightWoman, text: String(repeating: "오른쪽 대화 내용입니다.\n\n", count: 100))])
        director.liveFeedStore.finishInitialLoading()
        let view = ConversationFeedHostsNSView(frame: CGRect(x: 0, y: 0, width: 920, height: 500))
        let window = NSWindow(contentRect: view.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = view
        defer { view.tearDown(); window.contentView = nil }
        view.configure(director: director, layout: director.conversationLayout)
        try await settle(view)
        let left = try XCTUnwrap(view.hostForTesting(.rightMan))
        XCTAssertEqual(left.activeCharacterIDForTesting, .rightMan)
        director.toggleConversationSplit()
        director.chooseConversationCharacter(.rightWoman, in: .right)
        view.configure(director: director, layout: director.conversationLayout)
        try await settle(view)
        let right = try XCTUnwrap(view.hostForTesting(.rightWoman))
        XCTAssertEqual(view.visibleCharactersForTesting, [.rightMan, .rightWoman])
        XCTAssertEqual(right.activeCharacterIDForTesting, .rightWoman)
        let leftScroll = try XCTUnwrap(scrollView(in: left))
        let rightScroll = try XCTUnwrap(scrollView(in: right))
        XCTAssertFalse(leftScroll === rightScroll)
        leftScroll.contentView.scroll(to: CGPoint(x: 0, y: 100))
        rightScroll.contentView.scroll(to: CGPoint(x: 0, y: 200))
        let before = leftScroll.contentView.bounds.origin
        director.chooseConversationCharacter(.rightMan, in: .left)
        view.configure(director: director, layout: director.conversationLayout)
        await Task.yield()
        XCTAssertTrue(view.hostForTesting(.rightMan) === left)
        XCTAssertTrue(view.hostForTesting(.rightWoman) === right)
        XCTAssertEqual(leftScroll.contentView.bounds.origin, before)
        director.chooseConversationCharacter(.rightWoman, in: .right)
        director.toggleConversationSplit()
        view.configure(director: director, layout: director.conversationLayout)
        XCTAssertEqual(view.visibleCharactersForTesting, [.rightWoman])
        XCTAssertTrue(view.hostForTesting(.rightWoman) === right)
        XCTAssertTrue(scrollView(in: right) === rightScroll, "Merging retains the active conversation's native scroll view")
        await Task.yield()
        XCTAssertFalse(director.characterSelectionStore.isConversationLoading)
    }

    func testTerminalSplitUsesOneInputSinkAndMergeDoesNotCloseCachedSessions() async {
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        director.selectedCharacterID = .rightMan
        director.toggleConversationSplit()
        chooseReady(director, .rightWoman, in: .right)
        let view = CachedTerminalWorkspacesNSView(frame: CGRect(x: 0, y: 0, width: 920, height: 500))
        view.startsProcesses = false
        view.attachInputSink(to: director)
        defer { view.tearDown() }
        func configure() {
            view.configure(selectedCharacterID: director.selectedCharacterID, characterSelectionStore: director.characterSelectionStore,
                databaseBaseURL: director.databaseBaseURL, sessionRevision: 0, restartRequest: nil, workspaceLayout: director.conversationLayout)
            view.layoutSubtreeIfNeeded()
        }
        configure()
        XCTAssertEqual(view.visibleCharactersForTesting, [.rightMan, .rightWoman])
        XCTAssertTrue(director.terminalInputSink === view)
        director.toggleConversationSplit()
        configure()
        XCTAssertEqual(view.visibleCharactersForTesting, [.rightWoman])
        XCTAssertEqual(view.cachedCharacterIDsForTesting, [.rightMan, .rightWoman])
        await Task.yield()
        XCTAssertEqual(view.lastFocusRequestCharacterForTesting, .rightWoman)
    }

    func testRenderSplitAndEmptyPickerWithoutCallingAgents() async throws {
        guard let directory = ProcessInfo.processInfo.environment["OFFICESTRA_SPLIT_PREVIEW_DIR"] else { return }
        for dark in [false, true] {
            let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
            director.selectedCharacterID = .rightMan
            director.liveFeedStore.replace(with: [turn(.rightMan, text: "현재 대화는 왼쪽에 그대로 유지됩니다.\n\n대화창을 클릭한 뒤 하단에서 직원을 선택할 수 있습니다."), turn(.rightWoman, text: "오른쪽 대화도 계속 갱신됩니다.\n\n포커스된 창의 하단에서 빛이 흘러갑니다.")])
            director.liveFeedStore.finishInitialLoading()
            director.toggleConversationSplit()
            if dark { chooseReady(director, .rightWoman, in: .right) }
            let host = NSHostingView(rootView: ConversationWorkspaceView(director: director, layout: director.conversationLayout, mode: .chat)
                .environment(\.colorScheme, dark ? .dark : .light).frame(width: 920, height: 440))
            host.frame = CGRect(x: 0, y: 0, width: 920, height: 440)
            let window = NSWindow(contentRect: host.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
            window.contentView = host
            host.appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
            try await settle(host)
            let bitmap = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
            host.cacheDisplay(in: host.bounds, to: bitmap)
            try XCTUnwrap(bitmap.representation(using: .png, properties: [:])).write(to: URL(fileURLWithPath: directory).appendingPathComponent("conversation-split-\(dark ? "dark" : "light").png"))
            window.contentView = nil
        }
    }

    private func scrollView(in view: NSView) -> NSScrollView? {
        if let scroll = view as? NSScrollView { return scroll }
        return view.subviews.lazy.compactMap { self.scrollView(in: $0) }.first
    }
    // Pure state fixtures have no mounted feed to signal readiness.
    private func finishSelection(_ director: AgentDirector) {
        if let character = director.selectedCharacterID {
            director.characterSelectionStore.completeConversationLoading(for: character)
        }
    }
    private func chooseReady(_ director: AgentDirector, _ character: OfficeCharacter, in pane: ConversationPane) {
        finishSelection(director)
        director.chooseConversationCharacter(character, in: pane)
    }
    private func settle(_ view: NSView) async throws {
        for _ in 0..<30 { view.layoutSubtreeIfNeeded(); try await Task.sleep(for: .milliseconds(40)) }
    }
    private func turn(_ character: OfficeCharacter, text: String) -> LiveFeedTurn {
        let date = Date(timeIntervalSinceReferenceDate: 10000)
        return LiveFeedTurn(id: character.rawValue, characterId: character.rawValue, characterName: character.rawValue,
            characterBackend: .codex, backend: .codex, model: "gpt-5.6-sol", effort: "high", fastMode: false,
            externalSessionId: nil, conversationWorkdir: "/repo", prompt: "대화 분할 확인", response: text,
            feedback: nil, status: .completed, needsInput: false, errorMessage: nil, responseSourceWarning: nil,
            wikiProposalWarning: nil, startedAt: date, endedAt: date, updatedAt: date, estimatedCostUsd: nil,
            sessionContext: nil, activities: [], sources: nil, workspace: nil)
    }
}

private struct SplitPerformanceWorkspace: View {
    let director: AgentDirector
    @ObservedObject var selection: CharacterSelectionStore
    var body: some View {
        VStack(spacing: 0) {
            ConversationWorkspaceView(director: director, layout: director.conversationLayout, mode: .chat)
            CommandEntryRow(director: director, placeholder: selection.selectedCharacterID?.rawValue ?? "",
                attachmentCount: 0, isPreparingAttachments: false, onChooseAttachments: {}, onSubmit: { _ in false })
        }
    }
}
