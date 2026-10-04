import AppKit
import Combine
import OfficeCore
import SwiftUI
import XCTest
@testable import OfficeGame

@MainActor
final class OfficeWorkStatusTests: XCTestCase {
    func testOnlyKnownOperationNamesDetermineTheShortLabel() {
        XCTAssertEqual(OfficeWorkPhase.current(kind: "command", text: "swift test", status: "running"), .test)
        XCTAssertEqual(OfficeWorkPhase.current(kind: "command", text: "/bin/zsh -lc \"rg test file\"", status: "running"), .search)
        XCTAssertEqual(OfficeWorkPhase.current(kind: "command", text: "echo 'swift test passed'", status: "running"), .command)
        XCTAssertEqual(OfficeWorkPhase.current(kind: "tool", text: "도구 · Read · tests.swift", status: "running"), .read)
        XCTAssertEqual(OfficeWorkPhase.current(kind: "tool", text: "도구 · functions.apply_patch", status: "running"), .edit)
        XCTAssertEqual(OfficeWorkPhase.current(kind: "thinking", text: "private reasoning", status: "completed"), .thinking)
        XCTAssertEqual(OfficeWorkPhase.current(kind: "message", text: "a long report", status: "completed"), .reply)
    }

    func testFinishedOperationDoesNotClaimOngoingWorkOrSuccess() {
        for status in ["completed", "failed"] {
            XCTAssertEqual(OfficeWorkPhase.current(kind: "command", text: "swift test", status: status), .working)
        }
    }

    func testHiddenTranscriptStillUpdatesOnlyTheChangedEmployeeBubble() throws {
        let feed = LiveFeedStore()
        let first = feed.characterStore(for: "boss")
        let second = feed.characterStore(for: "left-man")
        var transcriptChanges = 0, statusChanges = 0
        let a = first.objectWillChange.sink { transcriptChanges += 1 }
        let b = first.workStatusStore.objectWillChange.sink { statusChanges += 1 }
        let activity = try activity("command", "swift test")
        feed.replace(with: [turn(activities: [activity])])
        XCTAssertEqual(first.workStatusStore.phase, .test)
        XCTAssertNil(second.workStatusStore.phase)
        feed.replace(with: [turn(response: "streamed text", activities: [activity])])
        XCTAssertEqual(statusChanges, 1)
        XCTAssertEqual(transcriptChanges, 0)
        feed.presentCharacterFeeds(["boss", "left-man"], selected: "left-man")
        feed.presentCharacterFeeds([], selected: "boss")
        XCTAssertEqual(first.workStatusStore.phase, .test)
        feed.replace(with: [])
        XCTAssertNil(first.workStatusStore.phase)
        withExtendedLifetime((a,b)) {}
    }

    func testTerminalEventSurvivesFeedRefreshAndRejectsLateOtherTurns() {
        let store = OfficeWorkStatusStore()
        store.update(turns: [turn()])
        store.receive(turnID: "turn", activity: .init(kind: "command", text: "swift test", status: "running"))
        XCTAssertEqual(store.phase, .test)
        store.update(turns: [turn(response: "new text")])
        XCTAssertEqual(store.phase, .test)
        store.receive(turnID: "old", activity: .init(kind: "command", text: "rg x", status: "running"))
        XCTAssertEqual(store.phase, .test)
        store.update(turns: [turn(status: .completed)])
        XCTAssertNil(store.phase)
        store.receive(turnID: "turn", activity: .init(kind: "command", text: "rg x", status: "running"))
        XCTAssertNil(store.phase)
    }

    func testEventBeforeTurnFeedIsBufferedWithoutShowingOldStatus() {
        let store = OfficeWorkStatusStore()
        store.receive(turnID: "turn", activity: .init(kind: "tool", text: "도구 · Edit", status: "running"))
        XCTAssertNil(store.phase)
        store.update(turns: [turn()])
        XCTAssertEqual(store.phase, .edit)
        store.update(turns: [turn(id: "new")])
        XCTAssertEqual(store.phase, .working)
        for status: LiveTurnStatus in [.completed, .failed, .interrupted] {
            store.update(turns: [turn(status: status)])
            XCTAssertNil(store.phase)
        }
    }

    func testDirectorRoutesTransientEventWithoutWritingTranscript() throws {
        let director = AgentDirector(startBackgroundTasks: false)
        director.liveFeedStore.replace(with: [turn()])
        let data = Data(#"{"type":"terminal.work-status","characterId":"boss","turnId":"turn","workActivity":{"kind":"command","text":"swift test","status":"running"}}"#.utf8)
        let event = try JSONDecoder().decode(RealtimeFeedEvent.self, from: data)
        XCTAssertTrue(director.applyRealtimeEvent(event, schedulesFeedRefresh: false))
        XCTAssertEqual(director.liveFeedStore.characterStore(for: "boss").workStatusStore.phase, .test)
        XCTAssertTrue(director.liveFeedStore.turns[0].activities.isEmpty)
    }

    func testNativeBubblesRenderInBothThemesWithoutExpandingWidth() throws {
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("officestra-status-bubble-preview")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        for (name, scheme) in [("light", ColorScheme.light), ("dark", .dark)] {
            let stores = ["rg source", "swift test", "swift build"].map { command in
                let store = OfficeWorkStatusStore()
                store.update(turns: [turn()])
                store.receive(turnID: "turn", activity: .init(kind: "command", text: command, status: "running"))
                return store
            }
            let view = HStack(spacing: 20) {
                ForEach(stores.indices, id: \.self) { index in
                    OfficeStatusBubble(name: "직원", status: .working, store: stores[index], accent: .teal, tailEdge: .bottom)
                        .frame(width: 80, height: 58)
                }
                OfficeStatusBubble(name: "직원", status: .completed, store: stores[0], accent: .orange, tailEdge: .bottom)
                    .frame(width: 80, height: 58)
            }
            .padding(16)
            .officeGameSurface(accent: .teal, emphasis: .panel, cornerRadius: 12)
            .environment(\.colorScheme, scheme)
            let host = NSHostingView(rootView: view)
            host.frame = NSRect(x: 0, y: 0, width: 412, height: 90)
            host.appearance = NSAppearance(named: scheme == .dark ? .darkAqua : .aqua)
            host.layoutSubtreeIfNeeded()
            RunLoop.main.run(until: Date().addingTimeInterval(0.1))
            let bitmap = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
            host.cacheDisplay(in: host.bounds, to: bitmap)
            let png = try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
            XCTAssertGreaterThan(png.count, 1000)
            try png.write(to: directory.appendingPathComponent("\(name).png"))
        }
        print("Status bubble previews: \(directory.path)")
    }

    private func activity(_ kind: String, _ text: String) throws -> LiveFeedActivity {
        let decoder = JSONDecoder(); decoder.dateDecodingStrategy = .secondsSince1970
        return try decoder.decode(LiveFeedActivity.self, from: JSONSerialization.data(withJSONObject: [
            "id": "op", "kind": kind, "text": text, "status": "running", "occurredAt": 1
        ]))
    }

    private func turn(id: String = "turn", response: String = "", status: LiveTurnStatus = .running, activities: [LiveFeedActivity] = []) -> LiveFeedTurn {
        LiveFeedTurn(id: id, characterId: "boss", characterName: "직원", characterBackend: .codex,
                     backend: nil, model: nil, effort: nil, fastMode: nil, externalSessionId: nil,
                     conversationWorkdir: nil, prompt: "작업", response: response, feedback: nil,
                     status: status, needsInput: false, errorMessage: nil,
                     responseSourceWarning: nil, wikiProposalWarning: nil,
                     startedAt: Date(timeIntervalSince1970: 0), endedAt: status.isRunning ? nil : Date(timeIntervalSince1970: 10),
                     updatedAt: Date(timeIntervalSince1970: 10), estimatedCostUsd: nil, sessionContext: nil,
                     activities: activities, sources: nil, workspace: nil)
    }
}
