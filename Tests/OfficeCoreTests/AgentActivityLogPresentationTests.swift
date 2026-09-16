// 이 파일은 Codex 전사 타임라인의 순서와 그룹 펼침 정책을 검증한다.

import Foundation
import XCTest
@testable import OfficeGame

final class AgentActivityLogPresentationTests: XCTestCase {
    func testCodexOperationExpansionNeverOpensFromStatusChanges() {
        XCTAssertFalse(
            transcriptGroupExpansionState(
                current: false,
                isRunning: true
            )
        )
        XCTAssertTrue(
            transcriptGroupExpansionState(
                current: true,
                isRunning: true
            )
        )
        XCTAssertFalse(
            transcriptGroupExpansionState(
                current: true,
                isRunning: false
            )
        )
    }

    func testLargeActivityGroupKeepsLatestVisibleAndCapsHistory() throws {
        let activities = try (0..<50).map { index in
            try makeActivity(
                id: "command-\(index)",
                kind: "command",
                text: "command \(index)",
                status: "completed"
            )
        }
        let group = CodexActivityGroup(
            kind: .command,
            items: activities.map(CodexActivityGroupItem.activity)
        )

        XCTAssertEqual(group.latestItem?.id, "command-49")
        XCTAssertEqual(group.hiddenHistoryItemCount(limit: 20), 29)
        XCTAssertEqual(
            group.visibleHistoryItems(
                showsAll: false,
                limit: 20
            ).map(\.id),
            activities.dropLast().suffix(20).map(\.id)
        )
        XCTAssertEqual(
            group.visibleHistoryItems(showsAll: true, limit: 20).count,
            49
        )
    }

    func testCodexTranscriptShowsWaitingImmediatelyWithoutActivities() {
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 1_000),
            isRunning: true
        )

        XCTAssertTrue(presentation.entries.isEmpty)
        XCTAssertTrue(presentation.showsWaiting)
    }

    func testCodexTranscriptKeepsWaitingBelowReasoning() throws {
        let activity = try makeActivity(
            id: "reasoning-1",
            kind: "thinking",
            text: "원인을 분석하고 있습니다.",
            status: "running"
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [activity],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 1_000),
            isRunning: true
        )

        XCTAssertTrue(presentation.showsWaiting)
        XCTAssertEqual(presentation.entries.count, 1)
        guard case .activityGroup(let group) = presentation.entries[0] else {
            return XCTFail("추론 카드가 없습니다.")
        }
        XCTAssertEqual(group.kind, .reasoning)
        XCTAssertEqual(group.latestItem?.id, activity.id)
    }

    func testCodexTranscriptShowsLatestMessageBeforeLaterWork() throws {
        let first = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "첫 응답",
            status: "completed"
        )
        let command = try makeActivity(
            id: "command-1",
            kind: "command",
            text: "swift test",
            status: "completed"
        )
        let latest = try makeActivity(
            id: "message-2",
            kind: "message",
            text: "최신 응답",
            status: "completed"
        )
        let tool = try makeActivity(
            id: "tool-1",
            kind: "tool",
            text: "검증 완료",
            status: "completed"
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [first, command, latest, tool],
            response: first.text,
            responseUpdatedAt: Date(timeIntervalSince1970: 1_000),
            isRunning: true
        )

        XCTAssertEqual(
            presentation.latestMessage?.id,
            "activity:message-2"
        )
        XCTAssertEqual(presentation.latestMessage?.text, "최신 응답")
        XCTAssertNil(presentation.deferredResponseMessageID)
        guard
            presentation.entries.count == 4,
            case .message = presentation.entries[0],
            case .activityGroup = presentation.entries[1],
            case .message(let visibleLatest) = presentation.entries[2],
            case .activityGroup = presentation.entries[3]
        else {
            return XCTFail("후속 작업 앞의 공개 메시지가 숨겨졌습니다.")
        }
        XCTAssertEqual(visibleLatest.text, latest.text)
    }

    func testVisibleMessageStaysInPlaceWhenItBecomesConclusion() throws {
        let message = try makeActivity(
            id: "message-final",
            kind: "message",
            text: "완료했습니다.",
            status: "completed"
        )
        let laterTool = try makeActivity(
            id: "tool-after",
            kind: "tool",
            text: "후처리 검증",
            status: "completed"
        )
        let running = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [message, laterTool],
            response: message.text,
            responseUpdatedAt: Date(timeIntervalSince1970: 1_000),
            isRunning: true
        )
        let completed = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [message, laterTool],
            response: message.text,
            responseUpdatedAt: Date(timeIntervalSince1970: 1_001),
            isRunning: false
        )

        XCTAssertNil(running.deferredResponseMessageID)
        XCTAssertNil(completed.deferredResponseMessageID)
        XCTAssertEqual(
            running.conclusionMessageID,
            "activity:message-final"
        )
        XCTAssertEqual(
            completed.conclusionMessageID,
            "activity:message-final"
        )
        XCTAssertEqual(running.entries.map(\.id), completed.entries.map(\.id))
        XCTAssertEqual(
            completed.entries.map(\.id),
            [
                "activity:message-final",
                "activity-group:work:tool-after",
            ]
        )
        XCTAssertTrue(running.showsWaiting)
        XCTAssertFalse(completed.showsWaiting)
    }

    func testCodexTranscriptDefersMessageAtTimelineEnd() throws {
        let command = try makeActivity(
            id: "command-1",
            kind: "command",
            text: "swift test",
            status: "completed"
        )
        let message = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "작성 중인 응답",
            status: "completed"
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [command, message],
            response: "작성 중인 응답",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        XCTAssertEqual(
            presentation.deferredResponseMessageID,
            "activity:message-1"
        )
        XCTAssertTrue(presentation.showsWaiting)
    }

    func testCodexTranscriptDefersActivityBeforeResponseDraftArrives() throws {
        let message = try makeActivity(
            id: "message-early",
            kind: "message",
            text: "먼저 도착한 공개 메시지",
            status: "completed"
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [message],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        XCTAssertEqual(
            presentation.deferredResponseMessageID,
            "activity:message-early"
        )
    }

    func testRunningPublicMessagesStayVisibleBetweenStableWorkGroups() throws {
        let reasoning = try makeActivity(
            id: "reason-1",
            kind: "thinking",
            text: "구조를 확인합니다.",
            status: "completed"
        )
        let firstMessage = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "첫 진행 상황입니다.",
            status: "completed"
        )
        let command = try makeActivity(
            id: "command-1",
            kind: "command",
            text: "swift test",
            status: "completed"
        )
        let secondMessage = try makeActivity(
            id: "message-2",
            kind: "message",
            text: "두 번째 진행 상황입니다.",
            status: "completed"
        )

        let firstSnapshot = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [reasoning, firstMessage],
            response: firstMessage.text,
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )
        let secondSnapshot = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [
                reasoning,
                firstMessage,
                command,
                secondMessage,
            ],
            response: firstMessage.text + "\n\n" + secondMessage.text,
            responseUpdatedAt: Date(timeIntervalSince1970: 2_001),
            isRunning: true
        )

        guard
            case .activityGroup(let firstWorkGroup) =
                firstSnapshot.entries[0],
            case .activityGroup(let secondWorkGroup) =
                secondSnapshot.entries[0]
        else {
            return XCTFail("실행 중 작업 내역 카드가 없습니다.")
        }
        XCTAssertEqual(firstWorkGroup.id, secondWorkGroup.id)
        XCTAssertEqual(
            firstWorkGroup.items.map(\.id),
            ["reason-1"]
        )
        XCTAssertEqual(
            secondWorkGroup.items.map(\.id),
            ["reason-1"]
        )
        guard
            case .message(let visibleProgress) = secondSnapshot.entries[1],
            case .activityGroup(let laterWorkGroup) =
                secondSnapshot.entries[2],
            case .message(let deferredMessage) = secondSnapshot.entries[3]
        else {
            return XCTFail("공개 메시지와 후속 작업 순서가 다릅니다.")
        }
        XCTAssertEqual(visibleProgress.text, firstMessage.text)
        XCTAssertEqual(laterWorkGroup.items.map(\.id), ["command-1"])
        XCTAssertNotEqual(secondWorkGroup.id, laterWorkGroup.id)
        XCTAssertEqual(deferredMessage.text, secondMessage.text)
        XCTAssertEqual(
            secondSnapshot.deferredResponseMessageID,
            "activity:message-2"
        )
        XCTAssertEqual(secondSnapshot.entries.count, 4)
        XCTAssertTrue(secondSnapshot.showsWaiting)
    }

    func testNewestRunningMessageWaitsForLaggingResponseDraft() throws {
        let firstMessage = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "첫 진행 상황입니다.",
            status: "completed"
        )
        let command = try makeActivity(
            id: "command-1",
            kind: "command",
            text: "swift test",
            status: "completed"
        )
        let newestMessage = try makeActivity(
            id: "message-2",
            kind: "message",
            text: "새 진행 상황입니다.",
            status: "completed"
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [firstMessage, command, newestMessage],
            response: firstMessage.text,
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        XCTAssertEqual(
            presentation.deferredResponseMessageID,
            "activity:message-2"
        )
        guard
            presentation.entries.count == 3,
            case .message(let visibleProgress) = presentation.entries[0],
            case .activityGroup(let workGroup) = presentation.entries[1],
            case .message = presentation.entries[2]
        else {
            return XCTFail("확정된 진행 기록이 없습니다.")
        }
        XCTAssertEqual(visibleProgress.text, firstMessage.text)
        XCTAssertEqual(
            workGroup.items.map(\.id),
            ["command-1"]
        )
    }

    func testCodexStreamingCandidateSurvivesTerminalSnapshot() throws {
        let message = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "완료 응답",
            status: "completed"
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [message],
            response: "완료 응답",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        XCTAssertEqual(
            presentation.deferredResponseMessageID,
            "activity:message-1"
        )
        XCTAssertFalse(presentation.showsWaiting)
    }

    func testFailedTurnKeepsLastPublicMessageVisible() throws {
        let message = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "검증 도중 실행이 중단됐습니다.",
            status: "completed"
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [message],
            response: message.text,
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false,
            isCompleted: false
        )

        XCTAssertNil(presentation.deferredResponseMessageID)
        XCTAssertEqual(presentation.entries.count, 1)
        guard case .message(let visibleMessage) = presentation.entries[0]
        else {
            return XCTFail("중단된 진행문이 대화에 없습니다.")
        }
        XCTAssertEqual(visibleMessage.text, message.text)
    }

    func testInterruptedTurnKeepsResponseMissingFromActivities() {
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [],
            response: "중단 직전까지 작성된 부분 응답",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false,
            isCompleted: false
        )

        XCTAssertEqual(
            presentation.deferredResponseMessageID,
            "response:turn-1"
        )
        XCTAssertEqual(
            presentation.deferredResponseMessage?.text,
            "중단 직전까지 작성된 부분 응답"
        )
        XCTAssertEqual(presentation.entries.count, 1)
    }

    func testCodexRunningCandidateIsDeferredUntilComplete() {
        XCTAssertEqual(
            CodexResponseDisplayPolicy.mode(
                isDeferredResponseCandidate: true,
                isRunning: true,
                isCurrentRevisionPresented: false,
                animatesResponse: true
            ),
            .deferred
        )
    }

    func testCodexTypingFinishesUnseenTerminalCandidateOnce() {
        XCTAssertEqual(
            CodexResponseDisplayPolicy.mode(
                isDeferredResponseCandidate: true,
                isRunning: false,
                isCurrentRevisionPresented: false,
                animatesResponse: true
            ),
            .typing
        )
        XCTAssertEqual(
            CodexResponseDisplayPolicy.mode(
                isDeferredResponseCandidate: true,
                isRunning: false,
                isCurrentRevisionPresented: true,
                animatesResponse: true
            ),
            .committed
        )
    }

    func testCodexTypingSkipsNoncandidateAndInactiveMessages() {
        XCTAssertEqual(
            CodexResponseDisplayPolicy.mode(
                isDeferredResponseCandidate: false,
                isRunning: true,
                isCurrentRevisionPresented: false,
                animatesResponse: true
            ),
            .static
        )
        XCTAssertEqual(
            CodexResponseDisplayPolicy.mode(
                isDeferredResponseCandidate: true,
                isRunning: true,
                isCurrentRevisionPresented: false,
                animatesResponse: false
            ),
            .deferred
        )
    }

    func testCodexQuestionComposerWaitsForTypedResponse() {
        XCTAssertFalse(
            CodexResponseDisplayPolicy.showsInlineQuestionAnswer(
                needsInput: true,
                backend: .codex,
                animatesResponse: true
            )
        )
        XCTAssertTrue(
            CodexResponseDisplayPolicy.showsInlineQuestionAnswer(
                needsInput: true,
                backend: .codex,
                animatesResponse: false
            )
        )
        XCTAssertTrue(
            CodexResponseDisplayPolicy.showsInlineQuestionAnswer(
                needsInput: true,
                backend: .claude,
                animatesResponse: true
            )
        )
    }

    func testResponseSourcesWaitForStreamingAndTypingToFinish() {
        XCTAssertFalse(
            ResponseSourceDisplayPolicy.showsSources(
                hasSources: true,
                isRunning: true,
                animatesResponse: true
            )
        )
        XCTAssertFalse(
            ResponseSourceDisplayPolicy.showsSources(
                hasSources: true,
                isRunning: false,
                animatesResponse: true
            )
        )
        XCTAssertTrue(
            ResponseSourceDisplayPolicy.showsSources(
                hasSources: true,
                isRunning: false,
                animatesResponse: false
            )
        )
        XCTAssertFalse(
            ResponseSourceDisplayPolicy.showsSources(
                hasSources: false,
                isRunning: false,
                animatesResponse: false
            )
        )
    }

    func testCompletedResponseLineSequencePreservesBlankLines() {
        let sequence = CompletedResponseLineSequence(
            source: "첫 줄\n\n세 번째 줄\n"
        )

        XCTAssertEqual(
            sequence.lines,
            [
                CompletedResponseLine(
                    index: 0,
                    source: "첫 줄",
                    renderKind: .markdown
                ),
                CompletedResponseLine(
                    index: 1,
                    source: "",
                    renderKind: .blank
                ),
                CompletedResponseLine(
                    index: 2,
                    source: "세 번째 줄",
                    renderKind: .markdown
                ),
                CompletedResponseLine(
                    index: 3,
                    source: "",
                    renderKind: .blank
                ),
            ]
        )
        XCTAssertFalse(sequence.isLastLine(2))
        XCTAssertTrue(sequence.isLastLine(3))
    }

    func testCompletedResponseChoosesStableRendererForEveryLine() {
        let sequence = CompletedResponseLineSequence(
            source: "**서문**\n\n| 항목 | 값 |\n|---|---|\n| 하나 | 1 |\n```swift\nlet value = 1\n```"
        )

        XCTAssertEqual(
            sequence.lines.map(\.renderKind),
            [
                .markdown,
                .blank,
                .table,
                .table,
                .table,
                .codeFence,
                .code,
                .codeFence,
            ]
        )
    }

    /// 표 각 행이 회색 고정폭 블록으로 굳어 보이던 문제를 고정한다.
    /// 줄 단위 렌더는 타자 중에만 쓰고, 끝나면 원문 전체를 다시 그려야 한다.
    func testFinishedTypingRendersWholeSourceMarkdown() {
        XCTAssertTrue(
            CompletedResponseRenderPlan.rendersWholeSourceMarkdown(
                playsSequence: true,
                reduceMotion: false,
                hasLines: true,
                didFinishTyping: true
            )
        )
        XCTAssertFalse(
            CompletedResponseRenderPlan.rendersWholeSourceMarkdown(
                playsSequence: true,
                reduceMotion: false,
                hasLines: true,
                didFinishTyping: false
            )
        )
    }

    func testNonTypingResponseSkipsLineByLineRendering() {
        // 과거 턴을 다시 열 때는 타자 없이 곧바로 표가 보여야 한다.
        XCTAssertTrue(
            CompletedResponseRenderPlan.rendersWholeSourceMarkdown(
                playsSequence: false,
                reduceMotion: false,
                hasLines: true,
                didFinishTyping: false
            )
        )
        XCTAssertTrue(
            CompletedResponseRenderPlan.rendersWholeSourceMarkdown(
                playsSequence: true,
                reduceMotion: true,
                hasLines: true,
                didFinishTyping: false
            )
        )
        XCTAssertTrue(
            CompletedResponseRenderPlan.rendersWholeSourceMarkdown(
                playsSequence: true,
                reduceMotion: false,
                hasLines: false,
                didFinishTyping: false
            )
        )
    }

    func testConclusionStaysBeforeLaterPublicMessage() throws {
        let earlierMessage = try makeActivity(
            id: "message-earlier",
            kind: "message",
            text: "이전 공개 메시지",
            status: "completed"
        )
        let finalMessage = try makeActivity(
            id: "message-final",
            kind: "message",
            text: "완료 응답",
            status: "completed"
        )
        let tool = try makeActivity(
            id: "tool-after",
            kind: "tool",
            text: "후처리",
            status: "completed"
        )
        let laterMessage = try makeActivity(
            id: "message-after",
            kind: "message",
            text: "후처리 알림",
            status: "completed"
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [
                earlierMessage,
                finalMessage,
                tool,
                laterMessage,
            ],
            response: "완료 응답",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        XCTAssertNil(presentation.deferredResponseMessageID)
        XCTAssertEqual(
            presentation.conclusionMessageID,
            "activity:message-final"
        )
        guard
            presentation.entries.count == 4,
            case .message(let earlierVisible) = presentation.entries[0],
            case .message(let conclusion) = presentation.entries[1],
            case .activityGroup(let workGroup) = presentation.entries[2],
            case .message(let laterVisible) = presentation.entries[3]
        else {
            return XCTFail("중간 공개 메시지의 타임라인 순서가 다릅니다.")
        }
        XCTAssertEqual(earlierVisible.text, earlierMessage.text)
        XCTAssertEqual(conclusion.text, finalMessage.text)
        XCTAssertEqual(workGroup.kind, .work)
        XCTAssertEqual(workGroup.items.map(\.id), ["tool-after"])
        XCTAssertEqual(laterVisible.text, laterMessage.text)
    }

    func testCompactEntriesAlwaysIncludeDeferredCandidate() throws {
        var activities = [
            try makeActivity(
                id: "message-final",
                kind: "message",
                text: "완료 응답",
                status: "completed"
            ),
        ]
        activities += try (0..<20).map { index in
            try makeActivity(
                id: "message-after-\(index)",
                kind: "message",
                text: "후속 \(index)",
                status: "completed"
            )
        }
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "완료 응답",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )
        let visible = presentation.visibleEntries(
            showsAll: false,
            compactLimit: 18
        )

        XCTAssertEqual(visible.count, 18)
        XCTAssertTrue(
            visible.contains { $0.id == "activity:message-final" }
        )
        XCTAssertEqual(visible.first?.id, "activity:message-final")
        XCTAssertEqual(visible[1].id, "activity:message-after-3")
        XCTAssertEqual(visible.last?.id, "activity:message-after-19")
    }

    func testCodexTranscriptKeepsMessagesBetweenChronologicalWorkGroups()
        throws
    {
        let activities = try [
            makeActivity(
                id: "reason-1",
                kind: "thinking",
                text: "구조를 확인합니다.",
                status: "completed"
            ),
            makeActivity(
                id: "command-1",
                kind: "command",
                text: "swift test",
                status: "completed"
            ),
            makeActivity(
                id: "tool-1",
                kind: "tool",
                text: "검색 · Feed",
                status: "completed"
            ),
            makeActivity(
                id: "reason-2",
                kind: "thinking",
                text: "검증 범위를 확정합니다.",
                status: "completed"
            ),
            makeActivity(
                id: "message-1",
                kind: "message",
                text: "첫 확인을 마쳤습니다.",
                status: "completed"
            ),
            makeActivity(
                id: "command-2",
                kind: "command",
                text: "swift build",
                status: "completed"
            ),
        ]

        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "첫 확인을 마쳤습니다.\n\n최종 검증도 통과했습니다.",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        XCTAssertEqual(presentation.entries.count, 6)
        guard
            case .activityGroup(let firstReasoning) =
                presentation.entries[0],
            case .activityGroup(let firstWorkGroup) =
                presentation.entries[1],
            case .activityGroup(let secondReasoning) =
                presentation.entries[2],
            case .message(let progressMessage) = presentation.entries[3],
            case .activityGroup(let secondWorkGroup) =
                presentation.entries[4],
            case .message(let finalMessage) = presentation.entries[5]
        else {
            return XCTFail("Codex 타임라인 순서가 다릅니다.")
        }
        XCTAssertEqual(firstWorkGroup.kind, .work)
        XCTAssertEqual(firstReasoning.kind, .reasoning)
        XCTAssertEqual(secondReasoning.kind, .reasoning)
        XCTAssertEqual(firstReasoning.items.map(\.id), ["reason-1"])
        XCTAssertEqual(secondReasoning.items.map(\.id), ["reason-2"])
        XCTAssertEqual(
            firstWorkGroup.items.map(\.id),
            [
                "command-1",
                "tool-1",
            ]
        )
        XCTAssertEqual(progressMessage.text, "첫 확인을 마쳤습니다.")
        XCTAssertEqual(secondWorkGroup.items.map(\.id), ["command-2"])
        XCTAssertNotEqual(firstWorkGroup.id, secondWorkGroup.id)
        XCTAssertEqual(finalMessage.text, "최종 검증도 통과했습니다.")
        XCTAssertFalse(presentation.showsWaiting)
    }

    func testCodexTranscriptGroupsCollaborationAndChangesOutsideWorkLog()
        throws
    {
        let activities = try [
            makeActivity(
                id: "message-before",
                kind: "message",
                text: "검토 준비를 마쳤습니다.",
                status: "completed"
            ),
            makeActivity(
                id: "files-1",
                kind: "tool",
                text: "파일 1개를 편집했습니다\n수정 A.swift",
                status: "completed"
            ),
            makeActivity(
                id: "collab-spawn",
                kind: "collaboration",
                text: "스크롤 정책을 검토해 주세요.",
                status: "running",
                collaboration: [
                    "action": "spawn",
                    "agentThreadId": "reviewer-1",
                    "prompt": "스크롤 정책을 검토해 주세요.",
                    "agentStatus": "running",
                ]
            ),
            makeActivity(
                id: "legacy-wait",
                kind: "tool",
                text: "협업 · wait",
                status: "completed"
            ),
            makeActivity(
                id: "message-middle",
                kind: "message",
                text: "첫 검토를 요청했습니다.",
                status: "completed"
            ),
            makeActivity(
                id: "command-1",
                kind: "command",
                text: "swift test",
                status: "completed"
            ),
            makeActivity(
                id: "collab-result",
                kind: "collaboration",
                text: "회귀 위험이 없습니다.",
                status: "completed",
                collaboration: [
                    "action": "result",
                    "agentThreadId": "reviewer-1",
                    "message": "회귀 위험이 없습니다.",
                    "agentStatus": "completed",
                ]
            ),
            makeActivity(
                id: "message-final",
                kind: "message",
                text: "완료했습니다.",
                status: "completed"
            ),
        ]

        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "검토 준비를 마쳤습니다.\n\n첫 검토를 요청했습니다.\n\n완료했습니다.",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        XCTAssertEqual(presentation.entries.count, 6)
        guard
            case .message(let firstMessage) = presentation.entries[0],
            case .activityGroup(let changeGroup) =
                presentation.entries[1],
            case .activityGroup(let collaborationGroup) =
                presentation.entries[2],
            case .message(let middleMessage) = presentation.entries[3],
            case .activityGroup(let workGroup) =
                presentation.entries[4],
            case .message(let finalMessage) = presentation.entries[5]
        else {
            return XCTFail("공개 메시지와 전용 그룹의 순서가 다릅니다.")
        }
        XCTAssertEqual(firstMessage.text, "검토 준비를 마쳤습니다.")
        XCTAssertEqual(changeGroup.kind, .changes)
        XCTAssertEqual(changeGroup.items.map(\.id), ["files-1"])
        XCTAssertEqual(collaborationGroup.kind, .collaboration)
        XCTAssertEqual(
            collaborationGroup.items.map(\.id),
            ["collab-spawn"]
        )
        XCTAssertEqual(
            collaborationGroup.items[0]
                .collaborationSummary?.activityIDs,
            ["collab-spawn", "collab-result"]
        )
        XCTAssertEqual(middleMessage.text, "첫 검토를 요청했습니다.")
        XCTAssertEqual(workGroup.kind, .work)
        XCTAssertEqual(workGroup.items.map(\.id), ["command-1"])
        XCTAssertEqual(
            collaborationGroup.items[0]
                .collaborationSummary?.completedCount,
            1
        )
        XCTAssertEqual(finalMessage.text, "완료했습니다.")
    }

    func testNewCollaborationAfterMessageStartsNewDedicatedGroup() throws {
        let first = try makeActivity(
            id: "collab-1",
            kind: "collaboration",
            text: "첫 검토를 요청합니다.",
            status: "running",
            collaboration: [
                "action": "spawn",
                "agentThreadId": "reviewer-1",
                "prompt": "첫 검토를 요청합니다.",
                "agentStatus": "running",
            ]
        )
        let message = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "첫 검토를 요청했습니다.",
            status: "completed"
        )
        let second = try makeActivity(
            id: "collab-2",
            kind: "collaboration",
            text: "새 검토를 요청합니다.",
            status: "running",
            collaboration: [
                "action": "spawn",
                "agentThreadId": "reviewer-2",
                "prompt": "새 검토를 요청합니다.",
                "agentStatus": "running",
            ]
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [first, message, second],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        guard
            presentation.entries.count == 3,
            case .activityGroup(let firstGroup) = presentation.entries[0],
            case .message = presentation.entries[1],
            case .activityGroup(let secondGroup) = presentation.entries[2]
        else {
            return XCTFail("메시지 뒤 새 협업 그룹이 없습니다.")
        }
        XCTAssertEqual(firstGroup.kind, .collaboration)
        XCTAssertEqual(secondGroup.kind, .collaboration)
        XCTAssertNotEqual(firstGroup.id, secondGroup.id)
        XCTAssertEqual(
            firstGroup.items[0].collaborationSummary?.activityIDs,
            ["collab-1"]
        )
        XCTAssertEqual(
            secondGroup.items[0].collaborationSummary?.activityIDs,
            ["collab-2"]
        )
        XCTAssertTrue(presentation.showsWaiting)
    }

    func testCollaborationGroupKeepsIdentityWhenStatusUpdates() throws {
        let running = try makeActivity(
            id: "collab-1",
            kind: "collaboration",
            text: "UI 구조를 검토해 주세요.",
            status: "running",
            collaboration: [
                "action": "spawn",
                "agentThreadId": "reviewer-1",
                "prompt": "UI 구조를 검토해 주세요.",
                "agentStatus": "running",
            ]
        )
        let completed = try makeActivity(
            id: "collab-1",
            kind: "collaboration",
            text: "UI 구조 검토 완료",
            status: "completed",
            collaboration: [
                "action": "result",
                "agentThreadId": "reviewer-1",
                "message": "UI 구조 검토 완료",
                "agentStatus": "completed",
            ]
        )
        let runningPresentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [running],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )
        let completedPresentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [completed],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_001),
            isRunning: false
        )

        guard
            case .activityGroup(let runningGroup) =
                runningPresentation.entries[0],
            case .activityGroup(let completedGroup) =
                completedPresentation.entries[0]
        else {
            return XCTFail("협업 전용 그룹이 없습니다.")
        }
        XCTAssertEqual(runningGroup.kind, .collaboration)
        XCTAssertEqual(completedGroup.kind, .collaboration)
        XCTAssertEqual(runningGroup.id, completedGroup.id)
        XCTAssertTrue(runningPresentation.showsWaiting)
        XCTAssertFalse(completedPresentation.showsWaiting)
        XCTAssertEqual(
            completedGroup.items[0].collaborationSummary?.completedCount,
            1
        )
    }

    func testCollaborationResultAfterMessageUpdatesOriginalGroup() throws {
        let spawn = try makeActivity(
            id: "collab-spawn",
            kind: "collaboration",
            text: "UI 구조를 검토해 주세요.",
            status: "running",
            collaboration: [
                "action": "spawn",
                "agentThreadId": "reviewer-1",
                "agentLabel": "UI 검토자",
                "prompt": "UI 구조를 검토해 주세요.",
                "agentStatus": "running",
            ]
        )
        let message = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "검토를 요청했습니다.",
            status: "completed"
        )
        let result = try makeActivity(
            id: "collab-result",
            kind: "collaboration",
            text: "UI 구조 검토 완료",
            status: "completed",
            collaboration: [
                "action": "result",
                "agentThreadId": "reviewer-1",
                "message": "UI 구조 검토 완료",
                "agentStatus": "completed",
            ]
        )
        let beforeResult = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [spawn, message],
            response: message.text,
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )
        let afterResult = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [spawn, message, result],
            response: message.text,
            responseUpdatedAt: Date(timeIntervalSince1970: 2_001),
            isRunning: true
        )

        guard
            case .activityGroup(let runningGroup) = beforeResult.entries[0],
            case .activityGroup(let updatedGroup) = afterResult.entries[0]
        else {
            return XCTFail("원래 협업 그룹이 유지되지 않았습니다.")
        }
        let runningSummary = try XCTUnwrap(
            runningGroup.items[0].collaborationSummary
        )
        let updatedSummary = try XCTUnwrap(
            updatedGroup.items[0].collaborationSummary
        )
        XCTAssertEqual(runningGroup.id, updatedGroup.id)
        XCTAssertEqual(runningSummary.runningCount, 1)
        XCTAssertEqual(updatedSummary.completedCount, 1)
        XCTAssertEqual(updatedSummary.agents[0].label, "UI 검토자")
        XCTAssertEqual(
            updatedSummary.agents[0].prompt,
            "UI 구조를 검토해 주세요."
        )
        XCTAssertEqual(updatedSummary.agents[0].result, "UI 구조 검토 완료")
        XCTAssertEqual(
            updatedSummary.activityIDs,
            ["collab-spawn", "collab-result"]
        )
        XCTAssertEqual(
            afterResult.entries.filter {
                if case .activityGroup(let group) = $0 {
                    return group.kind == .collaboration
                }
                return false
            }.count,
            1
        )
    }

    func testCollaborationSummaryCombinesRequestsAndLatestResultsByReviewer()
        throws
    {
        let activities = try [
            makeActivity(
                id: "spawn-1",
                kind: "collaboration",
                text: "UI 구조를 검토해 주세요.",
                status: "running",
                collaboration: [
                    "action": "spawn",
                    "agentThreadId": "reviewer-1",
                    "prompt": "UI 구조를 검토해 주세요.",
                    "agentStatus": "running",
                ]
            ),
            makeActivity(
                id: "spawn-2",
                kind: "collaboration",
                text: "CPU 부하를 검토해 주세요.",
                status: "running",
                collaboration: [
                    "action": "spawn",
                    "agentThreadId": "reviewer-2",
                    "prompt": "CPU 부하를 검토해 주세요.",
                    "agentStatus": "running",
                ]
            ),
            makeActivity(
                id: "result-1",
                kind: "collaboration",
                text: "접힘 UI가 적절합니다.",
                status: "completed",
                collaboration: [
                    "action": "result",
                    "agentThreadId": "reviewer-1",
                    "message": "접힘 UI가 적절합니다.",
                    "agentStatus": "completed",
                ]
            ),
            makeActivity(
                id: "follow-up-2",
                kind: "collaboration",
                text: "Markdown 비용도 확인해 주세요.",
                status: "running",
                collaboration: [
                    "action": "follow_up",
                    "agentThreadId": "reviewer-2",
                    "prompt": "Markdown 비용도 확인해 주세요.",
                    "agentStatus": "running",
                ]
            ),
            makeActivity(
                id: "result-2",
                kind: "collaboration",
                text: "렌더링 회귀가 발견됐습니다.",
                status: "failed",
                collaboration: [
                    "action": "result",
                    "agentThreadId": "reviewer-2",
                    "message": "렌더링 회귀가 발견됐습니다.",
                    "agentStatus": "errored",
                ]
            ),
        ]
        let group = CodexActivityGroup(
            kind: .collaboration,
            items: activities.map(CodexActivityGroupItem.activity)
        )
        let summary = try XCTUnwrap(
            CodexCollaborationSummary.make(from: group.items)
        )

        XCTAssertEqual(summary.agents.count, 2)
        XCTAssertEqual(summary.completedCount, 1)
        XCTAssertEqual(summary.failedCount, 1)
        XCTAssertEqual(summary.runningCount, 0)
        XCTAssertEqual(summary.statusText, "2명 · 1명 완료 · 1명 오류")
        XCTAssertEqual(summary.agents[0].prompt, "UI 구조를 검토해 주세요.")
        XCTAssertEqual(summary.agents[0].result, "접힘 UI가 적절합니다.")
        XCTAssertEqual(
            summary.agents[1].followUps,
            ["Markdown 비용도 확인해 주세요."]
        )
        XCTAssertEqual(
            summary.latestEvent?.text,
            "렌더링 회귀가 발견됐습니다."
        )
        XCTAssertEqual(
            summary.displayLabel(for: "reviewer-2"),
            "검토자 2"
        )
    }

    func testCollaborationSummaryReadsPromptAndResultFromOneUpdatedRow()
        throws
    {
        let activity = try makeActivity(
            id: "collaboration-1",
            kind: "collaboration",
            text: "현재 이벤트 형식 검토 완료",
            status: "completed",
            collaboration: [
                "action": "result",
                "agentThreadId": "reviewer-1",
                "agentLabel": "event schema review",
                "prompt": "event schema review",
                "message": "현재 이벤트 형식 검토 완료",
                "agentStatus": "completed",
            ]
        )
        let summary = try XCTUnwrap(
            CodexCollaborationSummary.make(
                from: [CodexActivityGroupItem.activity(activity)]
            )
        )

        XCTAssertEqual(summary.agents.count, 1)
        XCTAssertEqual(summary.completedCount, 1)
        XCTAssertEqual(summary.runningCount, 0)
        XCTAssertEqual(summary.agents[0].label, "event schema review")
        XCTAssertEqual(summary.agents[0].prompt, "event schema review")
        XCTAssertEqual(
            summary.agents[0].result,
            "현재 이벤트 형식 검토 완료"
        )
    }

    func testLegacyCollaborationToolUsesDedicatedGroup() throws {
        let activity = try makeActivity(
            id: "legacy-collaboration",
            kind: "tool",
            text: "협업 · 검토 결과를 받았습니다.",
            status: "completed"
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [activity],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        guard case .activityGroup(let collaborationGroup) =
            presentation.entries[0]
        else {
            return XCTFail("과거 협업 기록의 전용 그룹이 없습니다.")
        }
        XCTAssertEqual(collaborationGroup.kind, .collaboration)
        XCTAssertEqual(
            collaborationGroup.items.map(\.id),
            ["legacy-collaboration"]
        )
        XCTAssertEqual(
            collaborationGroup.items[0].collaborationSummary?.activityIDs,
            ["legacy-collaboration"]
        )
    }

    func testCodexTranscriptPreservesRepeatedMessageAfterPrefixRemoval() throws {
        let activity = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "같은 문장",
            status: "completed"
        )

        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [activity],
            response: "같은 문장\n\n같은 문장",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        let messages = presentation.entries.compactMap { entry -> String? in
            guard case .message(let message) = entry else {
                return nil
            }
            return message.text
        }
        XCTAssertEqual(messages, ["같은 문장", "같은 문장"])
        XCTAssertEqual(
            presentation.entries.map(\.id),
            ["activity:message-1", "response:turn-1"]
        )
    }

    func testCodexTranscriptDoesNotGuessAtMismatchedResponsePrefix() throws {
        let activity = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "이전 형식 메시지",
            status: "completed"
        )

        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [activity],
            response: "서로 다른 최종 응답",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        let messages = presentation.entries.compactMap { entry -> String? in
            guard case .message(let message) = entry else {
                return nil
            }
            return message.text
        }
        XCTAssertEqual(
            messages,
            ["이전 형식 메시지", "서로 다른 최종 응답"]
        )
    }

    func testCodexTranscriptRemovesKnownLegacyMessageSuffix() throws {
        let activities = try [
            makeActivity(
                id: "message-1",
                kind: "message",
                text: "첫 메시지",
                status: "completed"
            ),
            makeActivity(
                id: "message-2",
                kind: "message",
                text: "두 번째 메시지",
                status: "completed"
            ),
        ]

        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: activities,
            response: "두 번째 메시지\n\n최종 메시지",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        let messages = presentation.entries.compactMap { entry -> String? in
            guard case .message(let message) = entry else {
                return nil
            }
            return message.text
        }
        XCTAssertEqual(
            messages,
            ["첫 메시지", "두 번째 메시지", "최종 메시지"]
        )
    }

    func testCodexTranscriptCollectsFileChangeResult() throws {
        let activity = try makeActivity(
            id: "files-1",
            kind: "tool",
            text: [
                "파일 2개를 편집했습니다",
                "+3 -1",
                "수정 Sources/OfficeGame/Feed.swift",
                "추가 Tests/FeedTests.swift",
            ].joined(separator: "\n"),
            status: "completed"
        )

        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [activity],
            response: "완료했습니다.",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        guard
            presentation.entries.count == 2,
            case .activityGroup(let changeGroup) = presentation.entries[0],
            case .message = presentation.entries[1]
        else {
            return XCTFail("파일 변경 카드가 실제 위치에 없습니다.")
        }
        XCTAssertEqual(changeGroup.kind, .changes)
        let changes = try XCTUnwrap(
            changeGroup.latestItem?.changeSummary
        )
        XCTAssertEqual(
            changes.files,
            [
                "수정 Sources/OfficeGame/Feed.swift",
                "추가 Tests/FeedTests.swift",
            ]
        )
        XCTAssertEqual(changes.additions, 3)
        XCTAssertEqual(changes.deletions, 1)
        XCTAssertEqual(changes.reportedFileCount, 2)
    }

    func testFileChangeDescriptionExtractsFinderPath() {
        XCTAssertEqual(
            CodexFileChangeSummary.filePath(
                from: "수정 Sources/OfficeGame/Feed.swift"
            ),
            "Sources/OfficeGame/Feed.swift"
        )
        XCTAssertEqual(
            CodexFileChangeSummary.filePath(
                from: "이동 Sources/OfficeGame/NewFeed.swift"
            ),
            "Sources/OfficeGame/NewFeed.swift"
        )
        XCTAssertNil(CodexFileChangeSummary.filePath(from: "삭제 "))
    }

    func testFinderTargetResolvesRelativeAndAbsoluteFiles() throws {
        let fileManager = FileManager.default
        let workspace = fileManager.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let file = workspace.appendingPathComponent("Sources/Feed.swift")
        try fileManager.createDirectory(
            at: file.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        XCTAssertTrue(fileManager.createFile(atPath: file.path, contents: Data()))
        defer { try? fileManager.removeItem(at: workspace) }

        let relative = try XCTUnwrap(
            WorkspaceFileRevealTarget.resolve(
                path: "Sources/Feed.swift",
                workspaceDirectory: workspace.path,
                fileManager: fileManager
            )
        )
        XCTAssertEqual(relative.url, file.standardizedFileURL)
        XCTAssertTrue(relative.selectsItem)

        let absolute = try XCTUnwrap(
            WorkspaceFileRevealTarget.resolve(
                path: file.path,
                workspaceDirectory: "/unused",
                fileManager: fileManager
            )
        )
        XCTAssertEqual(absolute.url, file.standardizedFileURL)
        XCTAssertTrue(absolute.selectsItem)
    }

    func testFinderTargetUsesClosestFolderForDeletedFile() throws {
        let fileManager = FileManager.default
        let workspace = fileManager.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let existingFolder = workspace
            .appendingPathComponent("Sources", isDirectory: true)
        try fileManager.createDirectory(
            at: existingFolder,
            withIntermediateDirectories: true
        )
        defer { try? fileManager.removeItem(at: workspace) }

        let target = try XCTUnwrap(
            WorkspaceFileRevealTarget.resolve(
                path: "Sources/Deleted.swift",
                workspaceDirectory: workspace.path,
                fileManager: fileManager
            )
        )
        XCTAssertEqual(target.url, existingFolder.standardizedFileURL)
        XCTAssertFalse(target.selectsItem)
    }

    func testFinderTargetResolvesCompactedLegacyPath() throws {
        let fileManager = FileManager.default
        let workspace = fileManager.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let file = workspace.appendingPathComponent("Sources/Feed.swift")
        try fileManager.createDirectory(
            at: file.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        XCTAssertTrue(fileManager.createFile(atPath: file.path, contents: Data()))
        defer { try? fileManager.removeItem(at: workspace) }

        let target = try XCTUnwrap(
            WorkspaceFileRevealTarget.resolve(
                path: "…/Sources/Feed.swift",
                workspaceDirectory: workspace.path,
                fileManager: fileManager
            )
        )
        XCTAssertEqual(target.url, file.standardizedFileURL)
        XCTAssertTrue(target.selectsItem)
    }

    func testFinderTargetRecoversLegacyWorktreeRootWithoutGuessing() throws {
        let fileManager = FileManager.default
        let workspace = fileManager.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let rootFile = workspace.appendingPathComponent("Package.swift")
        let deepFile = workspace.appendingPathComponent(
            "packages/app/Sources/Feed.swift"
        )
        try fileManager.createDirectory(
            at: deepFile.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        XCTAssertTrue(
            fileManager.createFile(atPath: rootFile.path, contents: Data())
        )
        XCTAssertTrue(
            fileManager.createFile(atPath: deepFile.path, contents: Data())
        )
        defer { try? fileManager.removeItem(at: workspace) }

        let rootTarget = try XCTUnwrap(
            WorkspaceFileRevealTarget.resolve(
                path: "…/03ffd78858a8/right-woman-70d95def-eae4-441f-b0ec-e5cd2e230dee/Package.swift",
                workspaceDirectory: workspace.path,
                fileManager: fileManager
            )
        )
        XCTAssertEqual(rootTarget.url, rootFile.standardizedFileURL)
        XCTAssertTrue(rootTarget.selectsItem)

        let deepTarget = try XCTUnwrap(
            WorkspaceFileRevealTarget.resolve(
                path: "…/Sources/Feed.swift",
                workspaceDirectory: workspace.path,
                fileManager: fileManager
            )
        )
        XCTAssertEqual(deepTarget.url, workspace.standardizedFileURL)
        XCTAssertFalse(deepTarget.selectsItem)
    }

    func testFinderTargetDoesNotSelectAmbiguousCompactedPath() throws {
        let fileManager = FileManager.default
        let workspace = fileManager.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        for prefix in ["packages/one", "vendor/packages/two"] {
            let file = workspace.appendingPathComponent(
                "\(prefix)/Sources/Feed.swift"
            )
            try fileManager.createDirectory(
                at: file.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            XCTAssertTrue(
                fileManager.createFile(atPath: file.path, contents: Data())
            )
        }
        defer { try? fileManager.removeItem(at: workspace) }

        let target = try XCTUnwrap(
            WorkspaceFileRevealTarget.resolve(
                path: "…/Sources/Feed.swift",
                workspaceDirectory: workspace.path,
                fileManager: fileManager
            )
        )
        XCTAssertEqual(target.url, workspace.standardizedFileURL)
        XCTAssertFalse(target.selectsItem)
    }

    func testFinderTargetKeepsCompactedDeletedPathInsideWorkspace() throws {
        let fileManager = FileManager.default
        let workspace = fileManager.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let existingFolder = workspace.appendingPathComponent(
            "Sources/OfficeGame",
            isDirectory: true
        )
        try fileManager.createDirectory(
            at: existingFolder,
            withIntermediateDirectories: true
        )
        defer { try? fileManager.removeItem(at: workspace) }

        let target = try XCTUnwrap(
            WorkspaceFileRevealTarget.resolve(
                path: "…/Sources/OfficeGame/Deleted.swift",
                workspaceDirectory: workspace.path,
                fileManager: fileManager
            )
        )
        XCTAssertEqual(target.url, existingFolder.standardizedFileURL)
        XCTAssertFalse(target.selectsItem)
    }

    func testFinderTargetDoesNotEscapeMissingCompactedWorkspace() {
        let missingWorkspace = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        XCTAssertNil(
            WorkspaceFileRevealTarget.resolve(
                path: "…/Sources/Deleted.swift",
                workspaceDirectory: missingWorkspace.path
            )
        )
    }

    func testFinderTargetRejectsUnsafeCompactedPath() {
        XCTAssertNil(
            WorkspaceFileRevealTarget.fileURL(
                path: "…/../outside.txt",
                workspaceDirectory: "/Users/example/office"
            )
        )
    }

    func testCodexTranscriptMergesGeneratedImagesIntoConclusion() throws {
        let message = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "이미지를 만들었습니다.",
            status: "completed"
        )
        let laterMessage = try makeActivity(
            id: "message-later",
            kind: "message",
            text: "후속 알림",
            status: "completed"
        )
        let preview = "[![생성 이미지 1](<file:///tmp/result.png>)](<file:///tmp/result.png>)"

        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [message, laterMessage],
            response: "이미지를 만들었습니다.\n\n\(preview)",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        let messages = presentation.entries.compactMap { entry -> String? in
            guard case .message(let message) = entry else {
                return nil
            }
            return message.text
        }
        XCTAssertEqual(
            messages,
            [
                "후속 알림",
                "이미지를 만들었습니다.\n\n\(preview)",
            ]
        )
        XCTAssertEqual(
            presentation.deferredResponseMessageID,
            "activity:message-1"
        )
        XCTAssertEqual(presentation.entries[0].id, "activity:message-later")
    }

    func testFileSummaryCountsRepeatedPathOnce() throws {
        let first = try makeActivity(
            id: "files-1",
            kind: "tool",
            text: "파일 1개를 편집했습니다\n수정 A.swift",
            status: "completed"
        )
        let second = try makeActivity(
            id: "files-2",
            kind: "tool",
            text: "파일 1개를 편집했습니다\n수정 A.swift",
            status: "completed"
        )

        let summary = try XCTUnwrap(
            CodexFileChangeSummary.make(from: [first, second])
        )

        XCTAssertEqual(summary.files, ["수정 A.swift"])
        XCTAssertEqual(summary.reportedFileCount, 1)
        XCTAssertEqual(summary.title, "파일 1개를 편집했습니다")
    }

    func testRunningFileChangeUpdatesInPlaceBetweenMessages() throws {
        let before = try makeActivity(
            id: "message-1",
            kind: "message",
            text: "수정을 시작합니다.",
            status: "completed"
        )
        let running = try makeActivity(
            id: "files-1",
            kind: "tool",
            text: "파일 변경을 적용하는 중 · 2개",
            status: "running"
        )
        let after = try makeActivity(
            id: "message-2",
            kind: "message",
            text: "검증을 이어갑니다.",
            status: "completed"
        )
        let response = "수정을 시작합니다.\n\n검증을 이어갑니다."

        let runningPresentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [before, running, after],
            response: response,
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        guard
            runningPresentation.entries.count == 3,
            case .message(let visibleBefore) =
                runningPresentation.entries[0],
            case .activityGroup(let runningChangeGroup) =
                runningPresentation.entries[1],
            case .message = runningPresentation.entries[2]
        else {
            return XCTFail("실행 중 파일 변경의 타임라인 위치가 다릅니다.")
        }
        XCTAssertEqual(visibleBefore.text, before.text)
        XCTAssertEqual(runningChangeGroup.kind, .changes)
        XCTAssertEqual(
            runningChangeGroup.items.map(\.id),
            ["files-1"]
        )
        let runningChangeItem = try XCTUnwrap(
            runningChangeGroup.items.first { $0.id == "files-1" }
        )
        let runningChanges = try XCTUnwrap(
            runningChangeItem.changeSummary
        )
        XCTAssertEqual(
            runningChanges.title,
            "파일 2개를 편집하는 중"
        )
        XCTAssertTrue(runningChanges.files.isEmpty)
        XCTAssertEqual(runningChanges.status, .running)

        let completed = try makeActivity(
            id: "files-1",
            kind: "tool",
            text: "파일 2개를 편집했습니다\n+3 -1\n수정 A.swift\n수정 B.swift",
            status: "completed"
        )
        let completedPresentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [before, completed, after],
            response: response,
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: true
        )

        XCTAssertEqual(
            runningPresentation.entries.map(\.id),
            completedPresentation.entries.map(\.id)
        )
        guard
            case .activityGroup(let completedChangeGroup) =
                completedPresentation.entries[1]
        else {
            return XCTFail("완료 파일 변경 카드가 기존 위치를 잃었습니다.")
        }
        let completedChangeItem = try XCTUnwrap(
            completedChangeGroup.items.first { $0.id == "files-1" }
        )
        let completedChanges = try XCTUnwrap(
            completedChangeItem.changeSummary
        )
        XCTAssertEqual(completedChanges.status, .completed)
        XCTAssertEqual(completedChanges.files, ["수정 A.swift", "수정 B.swift"])
        XCTAssertEqual(completedChanges.additions, 3)
        XCTAssertEqual(completedChanges.deletions, 1)
    }

    func testFileChangesStayOutsideWorkWithoutReordering() throws {
        let first = try makeActivity(
            id: "files-1",
            kind: "tool",
            text: "파일 1개를 편집했습니다\n수정 A.swift",
            status: "completed"
        )
        let command = try makeActivity(
            id: "command-1",
            kind: "command",
            text: "swift test",
            status: "completed"
        )
        let second = try makeActivity(
            id: "files-2",
            kind: "tool",
            text: "파일 1개를 편집했습니다\n수정 B.swift",
            status: "completed"
        )

        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [first, command, second],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        guard
            presentation.entries.count == 3,
            case .activityGroup(let firstChangeGroup) =
                presentation.entries[0],
            case .activityGroup(let workGroup) = presentation.entries[1],
            case .activityGroup(let secondChangeGroup) =
                presentation.entries[2]
        else {
            return XCTFail("파일 변경과 일반 작업의 시간순 그룹이 다릅니다.")
        }
        XCTAssertEqual(firstChangeGroup.kind, .changes)
        XCTAssertEqual(firstChangeGroup.items.map(\.id), ["files-1"])
        XCTAssertEqual(workGroup.kind, .work)
        XCTAssertEqual(workGroup.items.map(\.id), ["command-1"])
        XCTAssertEqual(secondChangeGroup.kind, .changes)
        XCTAssertEqual(secondChangeGroup.items.map(\.id), ["files-2"])
        XCTAssertNotEqual(firstChangeGroup.id, secondChangeGroup.id)
        let firstChangeItem = try XCTUnwrap(firstChangeGroup.latestItem)
        let secondChangeItem = try XCTUnwrap(secondChangeGroup.latestItem)
        let firstChanges = try XCTUnwrap(firstChangeItem.changeSummary)
        let secondChanges = try XCTUnwrap(secondChangeItem.changeSummary)
        XCTAssertEqual(
            firstChanges.files,
            ["수정 A.swift"]
        )
        XCTAssertEqual(secondChanges.files, ["수정 B.swift"])
    }

    func testAdjacentFileChangesShareDedicatedGroup() throws {
        let first = try makeActivity(
            id: "files-1",
            kind: "tool",
            text: "파일 1개를 편집했습니다\n수정 A.swift",
            status: "completed"
        )
        let second = try makeActivity(
            id: "files-2",
            kind: "tool",
            text: "파일 1개를 편집했습니다\n수정 B.swift",
            status: "completed"
        )
        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [first, second],
            response: "",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        guard
            presentation.entries.count == 1,
            case .activityGroup(let changeGroup) = presentation.entries[0]
        else {
            return XCTFail("인접 파일 변경이 하나로 묶이지 않았습니다.")
        }
        XCTAssertEqual(changeGroup.kind, .changes)
        XCTAssertEqual(changeGroup.items.map(\.id), ["files-1", "files-2"])
    }

    func testFileSummaryOmitsPartialStatistics() throws {
        let measured = try makeActivity(
            id: "files-1",
            kind: "tool",
            text: "파일 1개를 편집했습니다\n+3 -1\n수정 A.swift",
            status: "completed"
        )
        let unmeasured = try makeActivity(
            id: "files-2",
            kind: "tool",
            text: "파일 1개를 편집했습니다\n수정 Binary.dat",
            status: "completed"
        )

        let summary = try XCTUnwrap(
            CodexFileChangeSummary.make(from: [measured, unmeasured])
        )

        XCTAssertNil(summary.additions)
        XCTAssertNil(summary.deletions)
    }

    func testFailedFileChangeUsesFailureTitle() throws {
        let activity = try makeActivity(
            id: "files-failed",
            kind: "tool",
            text: "파일 2개를 편집했습니다\n수정 A.swift\n수정 B.swift",
            status: "failed"
        )

        let summary = try XCTUnwrap(
            CodexFileChangeSummary.make(from: [activity])
        )

        XCTAssertEqual(summary.title, "파일 2개 변경에 실패했습니다")
        XCTAssertEqual(summary.status, .failed)
    }

    func testSingleTruncatedFileSummaryKeepsDeclaredTotal() throws {
        let files = (0..<40).map { "수정 File\($0).swift" }
        let activity = try makeActivity(
            id: "files-1",
            kind: "tool",
            text: (["파일 50개를 편집했습니다"] + files + ["외 10개"])
                .joined(separator: "\n"),
            status: "completed"
        )

        let summary = try XCTUnwrap(
            CodexFileChangeSummary.make(from: [activity])
        )

        XCTAssertEqual(summary.files.count, 40)
        XCTAssertEqual(summary.reportedFileCount, 50)
        XCTAssertEqual(summary.title, "파일 50개를 편집했습니다")
        XCTAssertFalse(summary.files.contains("외 10개"))
    }

    func testLegacyFileChangeSentenceBecomesResultCard() throws {
        let activity = try makeActivity(
            id: "files-legacy",
            kind: "tool",
            text: "파일 변경을 반영했습니다.",
            status: "completed"
        )

        let presentation = CodexTranscriptPresentation.make(
            turnID: "turn-1",
            activities: [activity],
            response: "완료했습니다.",
            responseUpdatedAt: Date(timeIntervalSince1970: 2_000),
            isRunning: false
        )

        guard
            presentation.entries.count == 2,
            case .activityGroup(let changeGroup) = presentation.entries[0]
        else {
            return XCTFail("이전 파일 변경 기록의 위치가 다릅니다.")
        }
        let changes = try XCTUnwrap(
            changeGroup.latestItem?.changeSummary
        )
        XCTAssertEqual(changes.files, [])
        XCTAssertEqual(changes.title, "파일 변경을 반영했습니다")
    }

    func testLegacyTruncatedFileSummarySeparatesRemainder() throws {
        let activity = try makeActivity(
            id: "files-legacy",
            kind: "tool",
            text: "파일 · 수정 A.swift, 수정 B.swift, 수정 C.swift 외 4개",
            status: "completed"
        )

        let summary = try XCTUnwrap(
            CodexFileChangeSummary.make(from: [activity])
        )

        XCTAssertEqual(
            summary.files,
            ["수정 A.swift", "수정 B.swift", "수정 C.swift"]
        )
        XCTAssertEqual(summary.reportedFileCount, 7)
        XCTAssertEqual(summary.title, "파일 7개를 편집했습니다")
    }

    private func makeActivity(
        id: String,
        kind: String,
        text: String,
        status: String,
        collaboration: [String: Any]? = nil
    ) throws -> LiveFeedActivity {
        var object: [String: Any] = [
            "id": id,
            "kind": kind,
            "text": text,
            "status": status,
            "occurredAt": 1_000,
        ]
        if let collaboration {
            object["collaboration"] = collaboration
        }
        let data = try JSONSerialization.data(withJSONObject: object)
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .secondsSince1970
        return try decoder.decode(LiveFeedActivity.self, from: data)
    }
}
