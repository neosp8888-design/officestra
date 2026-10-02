import AppKit
import Combine
import OfficeCore
import SwiftUI
import XCTest
@testable import OfficeGame

@MainActor
final class ConversationWorkProgressTests: XCTestCase {
    func testRecognizesObservedWorkAcrossProvidersWithoutReadingArgumentsAsIntent() throws {
        for (kind, text, expected) in [
            ("command", "/bin/zsh -lc \"rg -n 'swift test' Sources\"", ConversationWorkProgress.Phase.search),
            ("command", "python3 scripts/office-test.py --kind swift -- swift test", .test),
            ("command", "swift build -c release", .build),
            ("tool", "도구 · Read · /tmp/test.py", .read),
            ("tool", "도구 · Edit · /tmp/test.py", .edit),
            ("tool", "도구 · Bash · python3 -m unittest", .test),
            ("tool", "도구 · mcp__tools__apply_patch · file.swift", .edit),
            ("tool", "파일 3개를 편집했습니다\n+5 -2", .edit),
            ("thinking", "계획 · 테스트 수행", .plan),
            ("tool", "Graft · 관련 코드 탐색 중", .search),
            ("tool", "검색 · 시세 API", .web),
            ("command", "echo 'swift test 통과'", .command),
            ("tool", "알 수 없는 도구\nswift test 성공", .tool),
        ] {
            XCTAssertEqual(ConversationWorkProgress.phase(try activity(kind: kind, text: text)), expected, text)
        }
    }

    func testCommandCompletionDoesNotClaimTestsPassedOrTaskFinished() throws {
        let progress = ConversationWorkProgress.make(turn: turn(activities: [
            try activity(kind: "command", text: "swift test", status: "completed")
        ]))
        XCTAssertEqual(progress.state, .running)
        XCTAssertEqual(progress.headline, "다음 작업 진행 중")
        XCTAssertEqual(progress.latestResult?.phase.finishedText, "검증 명령 종료")
        XCTAssertNil(progress.nextStep)
    }

    func testTurnStateOverridesStaleRunningOperationAndFailures() throws {
        let stale = try activity(kind: "tool", text: "Graft · 탐색 중", status: "running")
        for (status, needsInput, expected) in [
            (LiveTurnStatus.completed, false, ConversationWorkProgress.State.completed),
            (.completed, true, .needsInput), (.failed, false, .failed),
            (.interrupted, false, .interrupted), (.pending, false, .waiting)
        ] {
            XCTAssertEqual(ConversationWorkProgress.make(turn: turn(status: status, needsInput: needsInput, activities: [stale])).state, expected)
        }
        let later = try activity(id: "new", kind: "command", text: "swift test", status: "completed", seconds: 2)
        XCTAssertEqual(ConversationWorkProgress.make(turn: turn(activities: [stale, later])).headline, "다음 작업 진행 중")
    }

    func testFailureHistoryDoesNotBecomeCurrentFailureAfterRecovery() throws {
        let failed = try activity(kind: "command", text: "swift test", status: "failed")
        XCTAssertEqual(ConversationWorkProgress.make(turn: turn(activities: [failed])).headline, "직전 작업에 오류가 있습니다")
        let fixed = try activity(id: "fixed", kind: "command", text: "swift test", status: "completed", seconds: 2)
        let progress = ConversationWorkProgress.make(turn: turn(activities: [failed, fixed]))
        XCTAssertEqual(progress.headline, "다음 작업 진행 중")
        XCTAssertEqual(progress.history.map(\.status), [.failed, .completed])
    }

    func testOnlyExplicitPlanProvidesNextStepAndThinkingIsNotQuoted() throws {
        let plan = try activity(kind: "tool", text: "도구 · TodoWrite\n[x] 원인 확인\n[~] 수정\n[ ] 테스트")
        let thought = try activity(id: "thought", kind: "thinking", text: "private reasoning", status: "running", seconds: 2)
        let message = try activity(id: "report", kind: "message", text: "**호출 간격을 수정했습니다.**", seconds: 3)
        let progress = ConversationWorkProgress.make(turn: turn(activities: [plan, thought, message]))
        XCTAssertEqual(progress.nextStep, "테스트")
        XCTAssertEqual(progress.report, "호출 간격을 수정했습니다.")
        XCTAssertFalse(String(describing: progress).contains("private reasoning"))
        XCTAssertNil(ConversationWorkProgress.make(turn: turn(status: .completed, activities: [plan])).nextStep)
        XCTAssertNil(ConversationWorkProgress.make(turn: turn(activities: [try activity(kind: "command", text: "echo '[ ] 가짜 계획'")])).nextStep)
    }

    func testHistoryAndPublicReportAreBounded() throws {
        let events = try (0..<1_000).map { try activity(id: "\($0)", kind: "command", text: $0.isMultiple(of: 2) ? "swift test" : "rg x", seconds: Double($0)) }
        let progress = ConversationWorkProgress.make(turn: turn(activities: events + [try activity(id: "message", kind: "message", text: String(repeating: "설명", count: 1_000), seconds: 1_001)]))
        XCTAssertEqual(progress.history.count, 6)
        XCTAssertEqual(progress.history.last?.id, "999")
        XCTAssertLessThanOrEqual(progress.report?.count ?? 0, 160)
    }

    func testTerminalSummaryUpdatesWhileTranscriptHiddenAndDoesNotRepublishUnchangedText() throws {
        let feed = LiveFeedStore()
        let character = feed.characterStore(for: "boss")
        var transcriptPublications = 0
        var progressPublications = 0
        let a = character.objectWillChange.sink { transcriptPublications += 1 }
        let b = character.workProgressStore.objectWillChange.sink { progressPublications += 1 }
        let operation = try activity(kind: "command", text: "swift test", status: "running")
        feed.replace(with: [turn(activities: [operation])])
        XCTAssertEqual(character.workProgressStore.progress?.headline, "수정한 내용 검증 중")
        XCTAssertEqual(transcriptPublications, 0)
        XCTAssertEqual(progressPublications, 1)
        feed.replace(with: [turn(response: "stream chunk", activities: [operation])])
        XCTAssertEqual(progressPublications, 1, "Response-only streaming must not redraw the status strip")
        feed.replace(with: [])
        XCTAssertNil(character.workProgressStore.progress)
        withExtendedLifetime((a, b)) {}
    }

    func testCharacterStoresRemainIndependentAcrossFocusAndModeChanges() throws {
        let feed = LiveFeedStore()
        let first = feed.characterStore(for: "boss").workProgressStore
        let second = feed.characterStore(for: "left-man").workProgressStore
        feed.replace(with: [turn(id: "a", activities: [try activity(kind: "command", text: "swift test", status: "running")]),
                            turn(id: "b", characterID: "left-man", activities: [try activity(kind: "tool", text: "도구 · Read · a.py", status: "running")])])
        feed.presentCharacterFeeds(["boss", "left-man"], selected: "boss")
        XCTAssertEqual(first.progress?.headline, "수정한 내용 검증 중")
        feed.presentCharacterFeeds([], selected: "left-man") // Terminal hides GUI feeds.
        XCTAssertEqual(second.progress?.headline, "내용 확인 중")
        XCTAssertEqual(first.progress?.turnID, "a")
        XCTAssertEqual(second.progress?.turnID, "b")
    }

    func testNativeProgressCardsRenderInBothThemesAndNarrowWidths() throws {
        let progress = ConversationWorkProgress.make(turn: turn(activities: [
            try activity(kind: "message", text: "오류가 반복되는 원인을 확인하고 호출 간격을 수정했습니다."),
            try activity(id: "test", kind: "command", text: "swift test", status: "running", seconds: 2)
        ]))
        let directory = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("officestra-work-progress-preview", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        for (name, scheme) in [("light", ColorScheme.light), ("dark", .dark)] {
            for width in [320.0, 720.0] {
                let view = ConversationWorkProgressCard(progress: progress, characterName: "백부장")
                    .frame(width: width).environment(\.colorScheme, scheme)
                    .environment(\.officeHoverEffectsEnabled, false)
                let renderer = ImageRenderer(content: view)
                renderer.scale = 2
                let image = try XCTUnwrap(renderer.cgImage)
                XCTAssertEqual(image.width, Int(width * 2))
                XCTAssertLessThanOrEqual(image.height, 160, "Progress must not consume the conversation viewport")
                let png = try XCTUnwrap(NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:]))
                try png.write(to: directory.appendingPathComponent("\(name)-\(Int(width)).png"))
            }
        }
        print("Progress UI previews: \(directory.path)")
    }

    private func activity(id: String = "op", kind: String, text: String, status: String = "completed", seconds: Double = 1) throws -> LiveFeedActivity {
        let decoder = JSONDecoder(); decoder.dateDecodingStrategy = .secondsSince1970
        return try decoder.decode(LiveFeedActivity.self, from: JSONSerialization.data(withJSONObject: [
            "id": id, "kind": kind, "text": text, "status": status, "occurredAt": seconds
        ]))
    }

    private func turn(id: String = "turn", characterID: String = "boss", response: String = "", status: LiveTurnStatus = .running, needsInput: Bool = false, activities: [LiveFeedActivity]) -> LiveFeedTurn {
        LiveFeedTurn(id: id, characterId: characterID, characterName: "직원", characterBackend: .codex,
                     backend: nil, model: nil, effort: nil, fastMode: nil, externalSessionId: nil,
                     conversationWorkdir: nil, prompt: "오류 수정", response: response, feedback: nil,
                     status: status, needsInput: needsInput, errorMessage: nil,
                     responseSourceWarning: nil, wikiProposalWarning: nil,
                     startedAt: Date(timeIntervalSince1970: 0), endedAt: status.isRunning ? nil : Date(timeIntervalSince1970: 10),
                     updatedAt: Date(timeIntervalSince1970: 10), estimatedCostUsd: nil, sessionContext: nil,
                     activities: activities, sources: nil, workspace: nil)
    }
}
