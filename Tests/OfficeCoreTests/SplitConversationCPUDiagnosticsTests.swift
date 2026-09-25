// 단일·분할 대화 화면의 유휴·한쪽 응답·양쪽 응답 부하를 창별 배치·TextKit 조판 횟수와 CPU 시간으로 비교하는 선택형 진단이다.

import AppKit
import Darwin
import ObjectiveC
import OfficeCore
import SwiftUI
import XCTest
@testable import OfficeGame

/// 창(직원)별로 NSHostingView 배치 횟수·시간과 TextKit 조판 완료·컨테이너 크기 변경을 센다.
@MainActor
private final class PaneProbe: NSObject, @preconcurrency NSLayoutManagerDelegate {
    let name: String
    var hostingLayouts = 0
    var hostingLayoutSeconds = 0.0
    var textKitLayoutCompletions = 0
    var textContainerGeometryChanges = 0
    var streamingGeometryChanges = 0
    var oversizedWidthGeometryChanges = 0
    private(set) var documents: [ObjectIdentifier: (view: SelectableMarkdownDocumentView, base: [Int])] = [:]
    private(set) var streamingViews = Set<ObjectIdentifier>()
    private var attachedManagers = Set<ObjectIdentifier>()

    init(name: String) { self.name = name }

    func layoutManager(_ layoutManager: NSLayoutManager, didCompleteLayoutFor textContainer: NSTextContainer?, atEnd layoutFinishedFlag: Bool) {
        textKitLayoutCompletions += 1
    }

    func layoutManager(_ layoutManager: NSLayoutManager, textContainer: NSTextContainer, didChangeGeometryFrom oldSize: NSSize) {
        textContainerGeometryChanges += 1
        let owner = Self.ownerKind(of: layoutManager)
        if owner == "streaming" { streamingGeometryChanges += 1 }
        // 무한·과대 폭(SwiftUI 이상 크기 탐색)과 실제 폭 사이를 오가는지 본다.
        geometryByOwner[owner, default: 0] += 1
        if max(oldSize.width, textContainer.size.width) > 100_000 {
            oversizedWidthGeometryChanges += 1
            oversizedByOwner[owner, default: 0] += 1
            // 어느 본문(과거 응답·확정 앞부분·작성 중 블록)이 무한 폭 탐색을 받는지 첫 글자로 구분한다.
            let head = String((textContainer.textView?.string ?? "").prefix(18))
            oversizedByDocument[head, default: 0] += 1
        }
    }

    var oversizedByDocument: [String: Int] = [:]

    var geometryByOwner: [String: Int] = [:]
    var oversizedByOwner: [String: Int] = [:]

    /// 조판기를 쓰는 텍스트 뷰의 상위 뷰를 거슬러 올라가 스트리밍·Markdown 문서·기타로 나눈다.
    static func ownerKind(of manager: NSLayoutManager) -> String {
        var view: NSView? = manager.textContainers.first?.textView ?? manager.firstTextView
        while let current = view {
            if current is IncrementalStreamingTextView { return "streaming" }
            if current is SelectableMarkdownDocumentView { return "markdown" }
            view = current.superview
        }
        return manager.textContainers.first?.textView == nil ? "detached" : "other"
    }

    /// 새로 생긴 뷰까지 붙잡는다. 처음 본 문서 뷰는 측정 시작 전이면 현재 값, 도중이면 0을 기준으로 삼는다.
    func scan(_ root: NSView, duringScenario: Bool) {
        for view in SplitCPUDiagnostics.descendants(root) {
            if let document = view as? SelectableMarkdownDocumentView, documents[ObjectIdentifier(document)] == nil {
                documents[ObjectIdentifier(document)] = (document, duringScenario ? [0, 0, 0, 0] : Self.counters(document))
            }
            if view is IncrementalStreamingTextView { streamingViews.insert(ObjectIdentifier(view)) }
            // 선택 전환 풀이 넘겨준 텍스트 스택에는 이전 측정의 probe가 남아 있을 수 있어 제품 코드가 쓰지 않는 위임자만 바꾼다.
            if let text = view as? NSTextView, let manager = text.layoutManager,
               !attachedManagers.contains(ObjectIdentifier(manager)), manager.delegate == nil || manager.delegate is PaneProbe {
                manager.delegate = self
                attachedManagers.insert(ObjectIdentifier(manager))
            }
            if NSStringFromClass(type(of: view)).contains("NSHostingView") {
                SplitCPUDiagnostics.register(view, probe: self)
            }
        }
    }

    static func counters(_ view: SelectableMarkdownDocumentView) -> [Int] {
        [view.layoutPassCount, view.heightRequestCount, view.textLayoutMeasurementCount, view.textLayoutCacheHitCount]
    }

    func documentDeltas() -> [Int] {
        documents.values.reduce([0, 0, 0, 0]) { total, entry in
            zip(total, zip(Self.counters(entry.view), entry.base)).map { $0 + $1.0 - $1.1 }
        }
    }

    func reset(root: NSView) {
        hostingLayouts = 0
        hostingLayoutSeconds = 0
        textKitLayoutCompletions = 0
        textContainerGeometryChanges = 0
        streamingGeometryChanges = 0
        oversizedWidthGeometryChanges = 0
        geometryByOwner = [:]
        oversizedByOwner = [:]
        oversizedByDocument = [:]
        documents = [:]
        streamingViews = []
        scan(root, duringScenario: false)
    }
}

@MainActor
private enum SplitCPUDiagnostics {
    static var owners: [ObjectIdentifier: PaneProbe] = [:]
    static var swizzled = Set<ObjectIdentifier>()
    static var depth = 0

    static func descendants(_ view: NSView) -> [NSView] { [view] + view.subviews.flatMap(descendants) }

    /// NSHostingView 특수화 클래스의 layout을 한 번만 감싼다. 바깥 호출만 시간에 넣어 중첩 배치를 두 번 세지 않는다.
    static func register(_ view: NSView, probe: PaneProbe) {
        owners[ObjectIdentifier(view)] = probe
        let cls: AnyClass = type(of: view)
        guard !swizzled.contains(ObjectIdentifier(cls)), let method = class_getInstanceMethod(cls, #selector(NSView.layout)) else { return }
        swizzled.insert(ObjectIdentifier(cls))
        typealias Layout = @convention(c) (AnyObject, Selector) -> Void
        let original = unsafeBitCast(method_getImplementation(method), to: Layout.self)
        let block: @convention(block) (AnyObject) -> Void = { object in
            MainActor.assumeIsolated {
                let probe = owners[ObjectIdentifier(object)]
                let outermost = depth == 0
                depth += 1
                let start = ProcessInfo.processInfo.systemUptime
                original(object, #selector(NSView.layout))
                depth -= 1
                if let probe {
                    probe.hostingLayouts += 1
                    if outermost { probe.hostingLayoutSeconds += ProcessInfo.processInfo.systemUptime - start }
                }
            }
        }
        let replacement = imp_implementationWithBlock(block)
        if !class_addMethod(cls, #selector(NSView.layout), replacement, method_getTypeEncoding(method)) {
            method_setImplementation(method, replacement)
        }
    }

    static func cpuSeconds() -> Double {
        var usage = rusage()
        getrusage(RUSAGE_SELF, &usage)
        return Double(usage.ru_utime.tv_sec + usage.ru_stime.tv_sec)
            + Double(usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) / 1_000_000
    }
}

@MainActor
final class SplitConversationCPUDiagnosticsTests: XCTestCase {
    private enum Stream { case none, noneStill, left, both }
    private let left = OfficeCharacter.rightMan
    private let right = OfficeCharacter.rightWoman

    func testMeasureSingleVersusSplitConversationLoad() async throws {
        guard let output = ProcessInfo.processInfo.environment["OFFICESTRA_SPLIT_CPU_DIAG"] else {
            throw XCTSkip("Set OFFICESTRA_SPLIT_CPU_DIAG to a JSON output path for the single/split CPU diagnostic")
        }
        let seconds = Double(ProcessInfo.processInfo.environment["OFFICESTRA_SPLIT_CPU_SECONDS"] ?? "") ?? 6
        // 분할 1000pt 창의 한쪽 본문 폭 = 단일 495pt 창 본문 폭. 단일 1000pt는 사용자 실제 비교 대상이다.
        let configurations: [(name: String, split: Bool, width: CGFloat)] = [
            ("singleWide1000", false, 1000), ("singleNarrow495", false, 495), ("split1000", true, 1000),
        ]
        let repeats = Int(ProcessInfo.processInfo.environment["OFFICESTRA_SPLIT_CPU_REPEATS"] ?? "") ?? 1
        var results: [String: Any] = [:]
        for round in 0..<repeats {
            for configuration in configurations {
                for stream in configuration.split ? [Stream.none, .noneStill, .left, .both] : [Stream.none, .left, .both] {
                    let key = "\(configuration.name)_\(stream)_r\(round)"
                    // 외부 표본기(sample)로 한 조건만 길게 볼 때 쓴다.
                    if let only = ProcessInfo.processInfo.environment["OFFICESTRA_SPLIT_CPU_ONLY"],
                       !key.hasPrefix(only) { continue }
                    results[key] = try await measure(split: configuration.split, width: configuration.width,
                                                     stream: stream, seconds: stream == .none || stream == .noneStill ? 5 : seconds)
                }
            }
        }
        results["conditions"] = ["streamSeconds": seconds, "chunkIntervalMS": 250, "height": 720,
                                 "note": "offscreen borderless window, synthetic codex/claude history, no backend"]
        try JSONSerialization.data(withJSONObject: results, options: [.prettyPrinted, .sortedKeys])
            .write(to: URL(fileURLWithPath: output))
    }

    /// 확정 본문 문서 하나에 매 tick 무한 폭 크기 탐색이 끼어들 때와 실제 폭만 받을 때의 비용을 따로 잰다.
    func testMeasureInfiniteWidthProbeCostOnMarkdownDocument() throws {
        guard let output = ProcessInfo.processInfo.environment["OFFICESTRA_SPLIT_CPU_PROBE_DIAG"] else {
            throw XCTSkip("Set OFFICESTRA_SPLIT_CPU_PROBE_DIAG to a JSON output path for the infinite-width probe diagnostic")
        }
        let width: CGFloat = 496
        let ticks = 300
        var results: [String: Any] = [:]
        for paragraphs in [6, 40] {
            let source = (0..<paragraphs).map { "\($0 + 1)번째 단락입니다. 분할 화면 CPU 비교를 위해 이미 확정된 앞부분 본문을 충분히 길게 둡니다. 줄바꿈 폭에 따라 줄 수가 달라집니다.\n\n" }.joined()
            for probesInfinity in [false, true] {
                SelectableMarkdownLayoutMeasurementCache.shared.removeAll()
                let document = SelectableMarkdownDocumentView(fontSize: 14)
                document.apply(source: source, fallbackDirectory: nil, isDark: false)
                let window = NSWindow(contentRect: CGRect(x: 0, y: 0, width: width, height: 2_000), styleMask: [.borderless],
                                      backing: .buffered, defer: false)
                window.contentView = document
                document.frame = CGRect(x: 0, y: 0, width: width, height: document.heightThatFits(width: width))
                document.layoutSubtreeIfNeeded()
                document.display()
                let measuredBefore = document.textLayoutMeasurementCount
                // 화면 밖 창이라 display()만으로는 글리프를 그리지 않을 수 있어 비트맵에 강제로 그린다.
                let bitmap = try XCTUnwrap(document.bitmapImageRepForCachingDisplay(in: document.bounds))
                var invalidatedTicks = 0
                let startedWall = ProcessInfo.processInfo.systemUptime, startedCPU = SplitCPUDiagnostics.cpuSeconds()
                for _ in 0..<ticks {
                    // 스트리밍 중 형제 뷰 높이가 바뀔 때 SwiftUI 스택이 하는 크기 탐색을 흉내 낸다.
                    if probesInfinity { _ = document.heightThatFits(width: .infinity) }
                    _ = document.heightThatFits(width: width)
                    document.needsLayout = true
                    document.layoutSubtreeIfNeeded()
                    if let manager = document.textView.layoutManager,
                       manager.firstUnlaidCharacterIndex() < document.textView.string.utf16.count { invalidatedTicks += 1 }
                    document.cacheDisplay(in: document.bounds, to: bitmap)
                }
                let cpu = SplitCPUDiagnostics.cpuSeconds() - startedCPU
                let wall = ProcessInfo.processInfo.systemUptime - startedWall
                results["p\(paragraphs)_\(probesInfinity ? "infinityProbe" : "widthOnly")"] = [
                    "characters": source.count, "ticks": ticks,
                    "cpuMSPerTick": (cpu * 1000 / Double(ticks) * 1000).rounded() / 1000,
                    "wallMS": (wall * 1000).rounded(),
                    "textKitMeasurements": document.textLayoutMeasurementCount - measuredBefore,
                    "ticksWithInvalidatedLayoutBeforeDraw": invalidatedTicks,
                ]
                window.contentView = nil
            }
        }
        try JSONSerialization.data(withJSONObject: results, options: [.prettyPrinted, .sortedKeys])
            .write(to: URL(fileURLWithPath: output))
    }

    private func measure(split: Bool, width: CGFloat, stream: Stream, seconds: Double) async throws -> [String: Any] {
        SelectableMarkdownLayoutMeasurementCache.shared.removeAll()
        let director = AgentDirector(startBackgroundTasks: false, conversationDefaults: nil)
        director.selectedCharacterID = left
        if split {
            director.toggleConversationSplit()
            director.characterSelectionStore.completeConversationLoading(for: left)
            director.chooseConversationCharacter(right, in: .right)
        }
        let history = completedHistory(for: left) + completedHistory(for: right)
        director.liveFeedStore.replace(with: history)
        director.liveFeedStore.finishInitialLoading()
        let root = NSHostingView(rootView: ConversationWorkspaceView(director: director, layout: director.conversationLayout, mode: .chat)            .frame(width: width, height: 720))
        root.frame = CGRect(x: 0, y: 0, width: width, height: 720)
        let window = NSWindow(contentRect: root.bounds, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = root
        defer {
            SplitCPUDiagnostics.descendants(root).compactMap { $0 as? ConversationFeedHostsNSView }.forEach { $0.tearDown() }
            window.contentView = nil
        }
        for _ in 0..<40 { root.layoutSubtreeIfNeeded(); root.displayIfNeeded(); try await Task.sleep(for: .milliseconds(40)) }
        let feeds = try XCTUnwrap(SplitCPUDiagnostics.descendants(root).compactMap { $0 as? ConversationFeedHostsNSView }.first)
        let panes = [left, right].compactMap { character in feeds.hostForTesting(character).map { (character, $0) } }
        XCTAssertEqual(panes.count, split ? 2 : 1, "보이는 창 수")
        let probes = Dictionary(uniqueKeysWithValues: panes.map { ($0.0, PaneProbe(name: $0.0.rawValue)) })
        for (character, host) in panes { probes[character]?.reset(root: host) }
        if stream == .noneStill {
            // 하단 포커스 선의 반복 애니메이션만 떼어 유휴 부하에서 차지하는 몫을 본다.
            for line in SplitCPUDiagnostics.descendants(root).compactMap({ $0 as? CoreAnimationFocusLineNSView }) {
                line.layer?.sublayers?.forEach { $0.removeAllAnimations() }
            }
        }

        var turns = history
        var responses: [OfficeCharacter: String] = [:]
        let streamers: [OfficeCharacter] = switch stream {
        case .none, .noneStill: []
        case .left: [left]
        case .both: [left, right]
        }
        let startedWall = ProcessInfo.processInfo.systemUptime, startedCPU = SplitCPUDiagnostics.cpuSeconds()
        var chunk = 0
        var nextChunk = startedWall
        var nextScan = startedWall + 0.25
        while ProcessInfo.processInfo.systemUptime - startedWall < seconds {
            let now = ProcessInfo.processInfo.systemUptime
            if !streamers.isEmpty, now >= nextChunk {
                // 서버 턴 갱신 합치기(250ms)와 같은 간격으로 응답 본문이 늘어난다.
                // 직원마다 본문을 달리해 두 창이 같은 측정 캐시를 나눠 쓰는 착시를 막는다.
                for character in streamers {
                    responses[character, default: Self.streamPrefix(character)] += Self.streamChunk(chunk, character)
                }
                turns = history + streamers.map { runningTurn(for: $0, response: responses[$0] ?? "", step: chunk) }
                director.liveFeedStore.replace(with: turns)
                chunk += 1
                nextChunk += 0.25
            }
            // 화면 갱신 주기 대신 짧게 양보하며 RunLoop의 타이머(타자 16ms tick)와 배치를 돌린다.
            root.layoutSubtreeIfNeeded()
            root.displayIfNeeded()
            try await Task.sleep(for: .milliseconds(16))
            // 뷰 트리 순회도 CPU에 잡히므로 새 뷰 수집은 본문 갱신 주기에 맞춰 250ms마다만 한다.
            if ProcessInfo.processInfo.systemUptime >= nextScan {
                for (character, host) in panes { probes[character]?.scan(host, duringScenario: true) }
                nextScan += 0.25
            }
        }
        for (character, host) in panes { probes[character]?.scan(host, duringScenario: true) }
        let wall = ProcessInfo.processInfo.systemUptime - startedWall
        let cpu = SplitCPUDiagnostics.cpuSeconds() - startedCPU
        var paneResults: [String: Any] = [:]
        for (character, host) in panes {
            guard let probe = probes[character] else { continue }
            let documents = probe.documentDeltas()
            paneResults[character.rawValue] = [
                "paneWidth": Double(host.bounds.width),
                "streams": streamers.contains(character),
                "hostingLayouts": probe.hostingLayouts,
                "hostingLayoutMS": (probe.hostingLayoutSeconds * 1000).rounded(),
                "textKitLayoutCompletions": probe.textKitLayoutCompletions,
                "textContainerGeometryChanges": probe.textContainerGeometryChanges,
                "streamingGeometryChanges": probe.streamingGeometryChanges,
                "oversizedWidthGeometryChanges": probe.oversizedWidthGeometryChanges,
                "geometryByOwner": probe.geometryByOwner,
                "oversizedByOwner": probe.oversizedByOwner,
                "oversizedByDocument": probe.oversizedByDocument,
                "markdownDocuments": probe.documents.count,
                "markdownLayoutPasses": documents[0],
                "markdownHeightRequests": documents[1],
                "markdownTextKitMeasurements": documents[2],
                "markdownMeasurementCacheHits": documents[3],
                "streamingTextViewsSeen": probe.streamingViews.count,
            ]
        }
        let focusLines = SplitCPUDiagnostics.descendants(root).compactMap { $0 as? CoreAnimationFocusLineNSView }
        return ["cpuPercent": (cpu / wall * 100).rounded(), "cpuMS": (cpu * 1000).rounded(), "wallMS": (wall * 1000).rounded(),
                "chunks": chunk, "panes": paneResults,
                "focusLines": focusLines.count, "focusLinesShimmering": focusLines.filter(\.isShimmering).count]
    }

    private static func streamPrefix(_ character: OfficeCharacter) -> String {
        // 긴 응답에서 확정 본문 재조판 비용이 커지는지 보려면 단락 수를 늘린다.
        let paragraphs = Int(ProcessInfo.processInfo.environment["OFFICESTRA_SPLIT_CPU_PREFIX_PARAGRAPHS"] ?? "") ?? 6
        return (0..<paragraphs).map { "\(character.rawValue) \($0 + 1)번째 단락입니다. 분할 화면 CPU 비교를 위해 이미 확정된 앞부분 본문을 충분히 길게 둡니다. 줄바꿈 폭에 따라 줄 수가 달라집니다.\n\n" }.joined()
    }

    /// 약 40자씩 늘고 두 조각마다 줄이 끝나며 여덟 조각마다 단락이 끝난다(목록 줄 포함).
    private static func streamChunk(_ index: Int, _ character: OfficeCharacter) -> String {
        let body = index % 8 == 5 ? "- \(character.rawValue) 목록 항목 \(index): 스트리밍 중 확정되는 목록 줄입니다" : "\(character.rawValue) 응답 조각 \(index)은 한글과 English words를 섞어 줄바꿈을 만든다"
        if index % 8 == 7 { return body + ".\n\n" }
        return index.isMultiple(of: 2) ? body + " " : body + ".\n"
    }

    private func completedHistory(for character: OfficeCharacter) -> [LiveFeedTurn] {
        let origin = Date(timeIntervalSinceReferenceDate: 50_000)
        return (0..<6).map { index in
            let markdown = """
            ### \(character.rawValue) 완료 응답 \(index)

            분할 화면에서 **본문 폭**이 좁아지면 같은 문단의 줄 수가 늘어납니다. 이 문장은 줄바꿈 효과를 드러내도록 일부러 길게 적었습니다.

            | 항목 | 결과 |
            | --- | --- |
            | 대화 | 유지 |
            | 폭 | 비교 |

            - 첫 번째 목록 줄
            - 두 번째 목록 줄에 `inline code`
            """
            let at = origin.addingTimeInterval(TimeInterval(index * 10))
            return turn(id: "\(character.rawValue)-done-\(index)", character: character, response: markdown,
                        status: .completed, backend: .claude, startedAt: at, updatedAt: at)
        }
    }

    private func runningTurn(for character: OfficeCharacter, response: String, step: Int) -> LiveFeedTurn {
        let at = Date(timeIntervalSinceReferenceDate: 50_100)
        return turn(id: "\(character.rawValue)-running", character: character, response: response, status: .running,
                    backend: .claude, startedAt: at, updatedAt: at.addingTimeInterval(TimeInterval(step)))
    }

    private func turn(id: String, character: OfficeCharacter, response: String, status: LiveTurnStatus,
                      backend: AgentBackend, startedAt: Date, updatedAt: Date) -> LiveFeedTurn {
        LiveFeedTurn(id: id, characterId: character.rawValue, characterName: character.rawValue,
            characterBackend: backend, backend: backend, model: "claude-sonnet-5", effort: "high", fastMode: false,
            externalSessionId: nil, conversationWorkdir: "/repo", prompt: "분할 CPU 비교", response: response,
            feedback: nil, status: status, needsInput: false, errorMessage: nil, responseSourceWarning: nil,
            wikiProposalWarning: nil, startedAt: startedAt, endedAt: status.isRunning ? nil : startedAt,
            updatedAt: updatedAt, estimatedCostUsd: nil, sessionContext: nil, activities: [], sources: nil, workspace: nil)
    }
}
