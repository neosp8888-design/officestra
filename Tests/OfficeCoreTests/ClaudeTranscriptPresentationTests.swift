// 이 파일은 Claude 활동을 도구·편집·계획 항목으로 나누는 규칙을 검증한다.

import Foundation
import XCTest
@testable import OfficeGame

final class ClaudeTranscriptPresentationTests: XCTestCase {
    func testResponseCompletionRevisionChangesWhenRunningCompletes() {
        let running = ClaudeResponseCompletionRevision(
            response: "같은 응답",
            isRunning: true,
            animatesResponse: true
        )
        let completed = ClaudeResponseCompletionRevision(
            response: "같은 응답",
            isRunning: false,
            animatesResponse: true
        )

        XCTAssertNotEqual(running, completed)
    }

    func testToolCallSplitsNameAndDetail() throws {
        let call = ClaudeToolCall.parse(
            try makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Read · /Users/example/office/Package.swift",
                status: "completed"
            )
        )

        XCTAssertEqual(call.name, "Read")
        XCTAssertEqual(call.detail, "/Users/example/office/Package.swift")
        XCTAssertEqual(call.family, .read)
    }

    func testBashDetailKeepsSeparatorsInsideCommand() throws {
        let call = ClaudeToolCall.parse(
            try makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Bash · git log --oneline · head -3",
                status: "running"
            )
        )

        XCTAssertEqual(call.name, "Bash")
        XCTAssertEqual(call.detail, "git log --oneline · head -3")
        XCTAssertEqual(call.family, .shell)
    }

    func testLegacyAndDetaillessToolsStayReadable() throws {
        let legacy = ClaudeToolCall.parse(
            try makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구를 사용해 업무를 처리하는 중...",
                status: "running"
            )
        )
        let pending = ClaudeToolCall.parse(
            try makeActivity(
                id: "tool-2",
                kind: "tool",
                text: "도구 · Bash",
                status: "running"
            )
        )

        XCTAssertEqual(legacy.displayName, "도구")
        XCTAssertEqual(legacy.family, .other)
        XCTAssertEqual(pending.displayName, "Bash")
        XCTAssertEqual(pending.detail, "")
    }

    func testMCPToolBadgeUsesLastNameSegment() throws {
        let call = ClaudeToolCall.parse(
            try makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · mcp__claude_ai_Google_Drive__search_files",
                status: "completed"
            )
        )

        XCTAssertEqual(call.displayName, "search_files")
        XCTAssertEqual(call.family, .other)
    }

    func testTimelineKeepsOrderAndSeparatesEditsFromTools() throws {
        let activities = try [
            makeActivity(
                id: "think-1",
                kind: "thinking",
                text: "먼저 구조를 확인한다.",
                status: "completed"
            ),
            makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Read · Sources/App.swift",
                status: "completed"
            ),
            makeActivity(
                id: "tool-2",
                kind: "tool",
                text: "도구 · Bash · swift build",
                status: "completed"
            ),
            makeActivity(
                id: "tool-3",
                kind: "tool",
                text: "도구 · Edit · Sources/App.swift",
                status: "completed"
            ),
            makeActivity(
                id: "tool-4",
                kind: "tool",
                text: "도구 · Write · Sources/New.swift",
                status: "completed"
            ),
            makeActivity(
                id: "tool-5",
                kind: "tool",
                text: "도구 · Bash · swift test",
                status: "running"
            ),
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        // 코덱스와 같은 규칙이다. 편집 뒤에 온 Bash는 앞의 Bash 칸으로
        // 끌려 올라가지 않고 발생한 자리에 새 칸으로 남는다.
        XCTAssertEqual(presentation.entries.count, 5)
        guard
            case .thoughts(let thoughts) = presentation.entries[0],
            case .tools(let reads) = presentation.entries[1],
            case .tools(let build) = presentation.entries[2],
            case .edits(let edits) = presentation.entries[3],
            case .tools(let test) = presentation.entries[4]
        else {
            return XCTFail("Claude 타임라인 순서가 다릅니다.")
        }
        XCTAssertEqual(
            thoughts.latestThought?.text,
            "먼저 구조를 확인한다."
        )
        XCTAssertEqual(reads.kind, .read)
        XCTAssertEqual(reads.steps.map(\.activityID), ["tool-1"])
        XCTAssertEqual(reads.title, "Read")
        XCTAssertEqual(build.kind, .shell)
        XCTAssertEqual(build.steps.map(\.activityID), ["tool-2"])
        XCTAssertEqual(build.title, "Bash")
        XCTAssertFalse(build.isRunning)
        XCTAssertEqual(edits.steps.map(\.activityID), ["tool-3", "tool-4"])
        XCTAssertEqual(edits.fileCount, 2)
        XCTAssertEqual(edits.title, "파일 2개를 편집했습니다")
        XCTAssertEqual(test.kind, .shell)
        XCTAssertEqual(test.steps.map(\.activityID), ["tool-5"])
        XCTAssertTrue(test.isRunning)
        XCTAssertFalse(presentation.showsWaiting)
    }

    func testToolGroupsSplitByKindAndResetAfterMessage() throws {
        let activities = try [
            makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Grep · struct Claude",
                status: "completed"
            ),
            makeActivity(
                id: "tool-2",
                kind: "tool",
                text: "도구 · WebFetch · https://example.com",
                status: "completed"
            ),
            makeActivity(
                id: "tool-3",
                kind: "tool",
                text: "도구 · Task · 하위 검토 위임",
                status: "completed"
            ),
            makeActivity(
                id: "message-1",
                kind: "message",
                text: "1차 정리를 마쳤습니다.",
                status: "completed"
            ),
            makeActivity(
                id: "tool-4",
                kind: "tool",
                text: "도구 · Grep · func make",
                status: "completed"
            ),
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        XCTAssertEqual(
            presentation.entries.map(\.id),
            [
                "tools:search:tool-1",
                "tools:web:tool-2",
                "tools:delegate:tool-3",
                "activity:message-1",
                "tools:search:tool-4",
            ]
        )
    }

    func testConsecutiveThoughtsCollapseIntoOneRun() throws {
        let activities = try (0..<3).map { index in
            try makeActivity(
                id: "think-\(index)",
                kind: "thinking",
                text: "\(index)번째 추론",
                status: "completed"
            )
        }

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        XCTAssertEqual(presentation.entries.count, 1)
        guard case .thoughts(let run) = presentation.entries[0] else {
            return XCTFail("추론 그룹이 없습니다.")
        }
        XCTAssertEqual(run.thoughts.count, 3)
        XCTAssertEqual(run.latestThought?.text, "2번째 추론")
        XCTAssertEqual(
            run.visibleHistoryThoughts(showsAll: false, limit: 20)
                .map(\.text),
            ["0번째 추론", "1번째 추론"]
        )
        XCTAssertEqual(run.hiddenHistoryThoughtCount(limit: 1), 1)
        XCTAssertEqual(
            run.visibleHistoryThoughts(showsAll: false, limit: 1)
                .map(\.text),
            ["1번째 추론"]
        )
        XCTAssertFalse(run.isRunning)
    }

    func testRepeatedToolsAreCountedInGroupTitle() throws {
        let activities = try (0..<4).map { index in
            try makeActivity(
                id: "tool-\(index)",
                kind: "tool",
                text: index == 3
                    ? "도구 · Grep · struct"
                    : "도구 · Read · Sources/File\(index).swift",
                status: "completed"
            )
        }

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        guard
            case .tools(let reads) = presentation.entries.first,
            case .tools(let searches) = presentation.entries.last
        else {
            return XCTFail("도구 그룹이 없습니다.")
        }
        XCTAssertEqual(reads.kind, .read)
        XCTAssertEqual(reads.title, "Read 3")
        XCTAssertEqual(searches.kind, .search)
        XCTAssertEqual(searches.title, "Grep")
    }

    func testPlanBoardKeepsOnlyTheLatestChecklist() throws {
        let activities = try [
            makeActivity(
                id: "plan-1",
                kind: "tool",
                text: "도구 · TodoWrite · 0/2단계\n[~] 분석\n[ ] 구현",
                status: "completed"
            ),
            makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Read · Sources/App.swift",
                status: "completed"
            ),
            makeActivity(
                id: "plan-2",
                kind: "tool",
                text: "도구 · TodoWrite · 1/2단계\n[x] 분석\n[~] 구현",
                status: "completed"
            ),
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        XCTAssertEqual(presentation.entries.count, 2)
        guard
            case .tools = presentation.entries[0],
            case .plan(let board) = presentation.entries[1]
        else {
            return XCTFail("계획 보드가 최신 위치에 없습니다.")
        }
        XCTAssertEqual(board.activityID, "plan-2")
        XCTAssertEqual(board.doneCount, 1)
        XCTAssertEqual(board.steps.map(\.text), ["분석", "구현"])
        XCTAssertEqual(board.activeStep?.text, "구현")
    }

    func testRunningResponseIsMarkedAsStreamingMessage() throws {
        let activity = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "중간 보고입니다.",
            status: "completed"
        )

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [activity],
            response: "중간 보고입니다.\n\n이어서 작성 중",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        XCTAssertEqual(presentation.streamingMessageID, "response:turn-1")
        XCTAssertFalse(presentation.showsWaiting)
        guard case .message(let last) = presentation.entries.last else {
            return XCTFail("응답 메시지가 없습니다.")
        }
        XCTAssertEqual(last.text, "이어서 작성 중")
    }

    func testSteerSplitsToolGroupsAndKeepsWaitingWhileUndelivered() throws {
        let activities = try [
            makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Bash · swift build",
                status: "completed"
            ),
            makeActivity(
                id: "steer-1",
                kind: "steer",
                text: "테스트도 같이 돌려줘",
                status: "running"
            ),
            makeActivity(
                id: "tool-2",
                kind: "tool",
                text: "도구 · Bash · swift test",
                status: "completed"
            ),
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        guard
            presentation.entries.count == 3,
            case .tools = presentation.entries[0],
            case .steer(let note) = presentation.entries[1],
            case .tools = presentation.entries[2]
        else {
            return XCTFail("끼어들기가 발생 위치에서 작업 묶음을 끊지 않았습니다.")
        }
        XCTAssertEqual(note.text, "테스트도 같이 돌려줘")
        XCTAssertEqual(note.status, .running)
        XCTAssertEqual(note.statusLabel, OfficeLocalization.string("전달 대기"))
        XCTAssertTrue(
            presentation.showsWaiting,
            "전달을 기다리는 끼어들기 때문에 생각 중 표시가 사라지면 안 됩니다."
        )
    }

    func testMessagesStayAtChronologicalPositionsBetweenWorkGroups() throws {
        let activities = try [
            makeActivity(
                id: "message-1",
                kind: "message",
                text: "첫 번째 진행 보고",
                status: "completed"
            ),
            makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Bash · swift test",
                status: "completed"
            ),
            makeActivity(
                id: "message-2",
                kind: "message",
                text: "두 번째 진행 보고",
                status: "completed"
            ),
            makeActivity(
                id: "tool-2",
                kind: "tool",
                text: "도구 · Read · Package.swift",
                status: "completed"
            ),
            makeActivity(
                id: "message-3",
                kind: "message",
                text: "최종 보고",
                status: "completed"
            ),
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "첫 번째 진행 보고\n\n두 번째 진행 보고\n\n최종 보고",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        XCTAssertEqual(presentation.entries.count, 5)
        guard
            case .message(let first) = presentation.entries[0],
            case .tools = presentation.entries[1],
            case .message(let second) = presentation.entries[2],
            case .tools = presentation.entries[3],
            case .message(let latest) = presentation.entries[4]
        else {
            return XCTFail("공개 대화가 실제 발생 위치에 있지 않습니다.")
        }
        XCTAssertEqual(first.text, "첫 번째 진행 보고")
        XCTAssertEqual(second.text, "두 번째 진행 보고")
        XCTAssertEqual(latest.text, "최종 보고")
        XCTAssertEqual(presentation.latestMessage, latest)
    }

    func testStreamingResponseKeepsPromotedMessageAtOriginalPosition() throws {
        let activity = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "완료 전 보고",
            status: "completed"
        )

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [activity],
            response: "완료 전 보고\n\n작성 중인 최신 응답",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        XCTAssertEqual(presentation.entries.count, 2)
        guard
            case .message(let promoted) = presentation.entries[0],
            case .message(let latest) = presentation.entries[1]
        else {
            return XCTFail("진행 메시지와 최신 응답이 분리되지 않았습니다.")
        }
        XCTAssertEqual(promoted.text, "완료 전 보고")
        XCTAssertEqual(latest.text, "작성 중인 최신 응답")
        XCTAssertEqual(presentation.streamingMessageID, latest.id)
    }

    func testCompletedResponseIsNotStreaming() throws {
        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [],
            response: "완료했습니다.",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        XCTAssertNil(presentation.streamingMessageID)
        XCTAssertFalse(presentation.showsWaiting)
    }

    func testWaitingShowsBeforeFirstActivity() {
        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 1_000),
            isRunning: true
        )

        XCTAssertTrue(presentation.entries.isEmpty)
        XCTAssertTrue(presentation.showsWaiting)
    }

    func testContentlessThinkingBecomesWaitingInsteadOfCard() throws {
        let activity = try makeActivity(
            id: "think-1",
            kind: "thinking",
            text: "추론 중",
            status: "running"
        )

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [activity],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 1_000),
            isRunning: true
        )

        XCTAssertTrue(presentation.entries.isEmpty)
        XCTAssertTrue(presentation.showsWaiting)
    }

    /// 원문이 끝내 오지 않은 자리표시자가 카드로 굳어 `추론 · 1개`가
    /// 여러 번 겹쳐 보이던 문제를 고정한다.
    func testStrayThinkingPlaceholdersDoNotStackAsCards() throws {
        let activities = try [
            makeActivity(
                id: "think-1",
                kind: "thinking",
                text: "추론 중",
                status: "running"
            ),
            makeActivity(
                id: "message-1",
                kind: "message",
                text: "확인해 보겠습니다.",
                status: "completed"
            ),
            makeActivity(
                id: "think-2",
                kind: "thinking",
                text: "추론 중",
                status: "running"
            ),
            makeActivity(
                id: "think-3",
                kind: "thinking",
                text: "두 구현의 차이를 먼저 본다.",
                status: "completed"
            ),
            makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Bash · git log",
                status: "running"
            ),
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        XCTAssertEqual(
            presentation.entries.map(\.id),
            [
                "activity:message-1",
                "thoughts:think-3",
                "tools:shell:tool-1",
            ]
        )
        guard case .thoughts(let run) = presentation.entries[1] else {
            return XCTFail("추론 카드가 없습니다.")
        }
        XCTAssertEqual(run.thoughts.count, 1)
        XCTAssertEqual(run.latestThought?.text, "두 구현의 차이를 먼저 본다.")
        XCTAssertFalse(run.isRunning)
        // 실제로 도는 도구가 있으므로 아래쪽 `생각 중`은 뜨지 않는다.
        XCTAssertFalse(presentation.showsWaiting)
    }

    func testRepeatedEditsCollapseIntoOneRowWithSummedStats() throws {
        let activities = try [
            makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Edit · Sources/OfficeGame/AgentDirector.swift\n+12 -4",
                status: "completed"
            ),
            makeActivity(
                id: "tool-2",
                kind: "tool",
                text: "도구 · Edit · Sources/OfficeGame/AgentDirector.swift\n+3 -1",
                status: "completed"
            ),
            makeActivity(
                id: "tool-3",
                kind: "tool",
                text: "도구 · Edit · Tests/OfficeCoreTests/FeedTests.swift\n+20 -0",
                status: "completed"
            ),
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        guard case .edits(let edits) = presentation.entries.last else {
            return XCTFail("편집 항목이 없습니다.")
        }

        XCTAssertEqual(
            edits.files.map(\.path),
            [
                "Sources/OfficeGame/AgentDirector.swift",
                "Tests/OfficeCoreTests/FeedTests.swift",
            ],
            "같은 파일을 두 번 고치면 한 줄로 합쳐 첫 등장 순서를 지켜야 합니다."
        )
        XCTAssertEqual(edits.files.map(\.editCount), [2, 1])
        XCTAssertEqual(edits.files.map(\.additions), [15, 20])
        XCTAssertEqual(edits.files.map(\.deletions), [5, 0])
        XCTAssertEqual(edits.fileCount, 2)
        XCTAssertEqual(edits.title, "파일 2개를 편집했습니다")

        let totals = try XCTUnwrap(edits.totals)
        XCTAssertEqual(totals.additions, 35)
        XCTAssertEqual(totals.deletions, 5)
        XCTAssertTrue(edits.copyText.contains("+35 -5"))
        XCTAssertTrue(
            edits.copyText.contains(
                "Sources/OfficeGame/AgentDirector.swift +15 -5"
            )
        )
    }

    func testEditRunHidesTotalsWhenAnyStatisticIsMissing() throws {
        let activities = try [
            makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Edit · Sources/A.swift\n+12 -4",
                status: "completed"
            ),
            // 옛 기록에는 통계 줄이 없다.
            makeActivity(
                id: "tool-2",
                kind: "tool",
                text: "도구 · Edit · Sources/B.swift",
                status: "completed"
            ),
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        guard case .edits(let edits) = presentation.entries.last else {
            return XCTFail("편집 항목이 없습니다.")
        }

        XCTAssertEqual(edits.files.map(\.path), ["Sources/A.swift", "Sources/B.swift"])
        XCTAssertEqual(edits.files[0].additions, 12)
        XCTAssertNil(edits.files[1].additions)
        XCTAssertNil(
            edits.totals,
            "통계가 하나라도 빠지면 실제보다 작은 합계를 보여주면 안 됩니다."
        )
        XCTAssertFalse(edits.copyText.contains("+12 -4\nSources/A"))
    }

    func testMergedEditRowKeepsFailedAndRunningStatus() throws {
        let activities = try [
            makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "도구 · Edit · Sources/A.swift\n+5 -1",
                status: "completed"
            ),
            makeActivity(
                id: "tool-2",
                kind: "tool",
                text: "도구 · Edit · Sources/A.swift",
                status: "failed"
            ),
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        guard case .edits(let edits) = presentation.entries.last else {
            return XCTFail("편집 항목이 없습니다.")
        }

        XCTAssertEqual(edits.files.count, 1)
        XCTAssertEqual(edits.files[0].editCount, 2)
        XCTAssertEqual(edits.files[0].status, .failed)
        XCTAssertNil(edits.files[0].additions)
    }

    func testResponseInsideShownMessageIsNotRepeated() throws {
        // 백엔드는 응답에서 기계 블록을 떼어 저장하지만 메시지 활동에는
        // 원문이 남는다. 둘을 그대로 두면 같은 답이 두 번 그려진다.
        let visibleAnswer = "정리했습니다.\n\n표는 위와 같습니다."
        let rawMessage = visibleAnswer
            + "\n\n[OFFICE_SOURCES]\n"
            + #"[{"kind":"file","title":"설정","locator":"a.mjs:1"}]"#
        let activities = try [
            makeActivity(
                id: "message-1",
                kind: "message",
                text: rawMessage,
                status: "completed"
            )
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: visibleAnswer,
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        let messages = presentation.entries.compactMap { entry -> String? in
            if case .message(let message) = entry {
                return message.text
            }
            return nil
        }

        XCTAssertEqual(
            messages.count,
            1,
            "같은 답이 활동과 응답으로 두 번 들어가면 안 됩니다."
        )
    }

    func testProgressMessagesStripBeforeFinalAnswerIsDeduplicated() throws {
        // 진행문이 앞에 있으면 응답에서 그만큼 떼어낸 나머지를 최종
        // 메시지와 견줘야 한다. 앞부분만 보면 중복을 놓친다.
        let progress = "확인하겠습니다."
        let finalAnswer = "정리했습니다.\n\n표는 위와 같습니다."
        let rawFinal = finalAnswer
            + "\n\n[OFFICE_SOURCES]\n"
            + #"[{"kind":"file","title":"설정","locator":"a.mjs:1"}]"#
        let activities = try [
            makeActivity(
                id: "message-1",
                kind: "message",
                text: progress,
                status: "completed"
            ),
            makeActivity(
                id: "message-2",
                kind: "message",
                text: rawFinal,
                status: "completed"
            ),
        ]

        let presentation = ClaudeTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: progress + "\n\n" + finalAnswer,
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        let messages = presentation.entries.compactMap { entry -> String? in
            if case .message(let message) = entry {
                return message.text
            }
            return nil
        }

        XCTAssertEqual(messages, [progress, rawFinal])
    }

    private func makeActivity(
        id: String,
        kind: String,
        text: String,
        status: String
    ) throws -> LiveFeedActivity {
        let data = try JSONSerialization.data(withJSONObject: [
            "id": id,
            "kind": kind,
            "text": text,
            "status": status,
            "occurredAt": 1_000,
        ])
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .secondsSince1970
        return try decoder.decode(LiveFeedActivity.self, from: data)
    }
}
