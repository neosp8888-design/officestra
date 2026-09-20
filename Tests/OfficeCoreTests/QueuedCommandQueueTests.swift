// 이 파일은 다음 턴 예약의 개수 제한과 취소·즉시 적용 규칙을 검증한다.

import Foundation
import XCTest
@testable import OfficeGame

@MainActor
final class QueuedCommandQueueTests: XCTestCase {
    func testReplyRecipientIsKeptWithItsQueuedPrompt() {
        var queue = QueuedCommandQueue()
        let command = QueuedCommand(prompt: "대화 이어가기", replyRecipient: .rightWoman)
        XCTAssertTrue(queue.enqueue(command))
        let taken = queue.removeFirst()!
        XCTAssertEqual(taken.replyRecipient, .rightWoman)
        queue.restoreToFront(taken)
        XCTAssertEqual(queue.removeFirst()?.replyRecipient, .rightWoman)
        XCTAssertNil(QueuedCommand(prompt: "내게만 답변").replyRecipient)
    }

    func testQueueAcceptsAtMostThreeReservations() {
        var queue = QueuedCommandQueue()

        XCTAssertEqual(QueuedCommandQueue.maximumCount, 3)
        for index in 0..<3 {
            XCTAssertTrue(
                queue.enqueue(QueuedCommand(prompt: "업무 \(index)")),
                "3개까지는 예약을 받아야 합니다."
            )
        }

        XCTAssertTrue(queue.isFull)
        XCTAssertFalse(
            queue.enqueue(QueuedCommand(prompt: "업무 4")),
            "4번째 예약은 거절해야 합니다."
        )
        XCTAssertEqual(
            queue.commands.map(\.prompt),
            ["업무 0", "업무 1", "업무 2"],
            "가득 찬 큐가 기존 예약을 조용히 밀어내면 안 됩니다."
        )
    }

    func testCancelRemovesOnlyTargetReservation() {
        var queue = QueuedCommandQueue()
        let first = QueuedCommand(prompt: "첫 업무")
        let second = QueuedCommand(prompt: "둘째 업무")
        _ = queue.enqueue(first)
        _ = queue.enqueue(second)

        XCTAssertEqual(queue.remove(id: second.id)?.prompt, "둘째 업무")
        XCTAssertEqual(queue.commands.map(\.id), [first.id])
        XCTAssertNil(
            queue.remove(id: second.id),
            "이미 취소된 예약을 다시 취소해도 안전해야 합니다."
        )
        XCTAssertFalse(queue.isFull)
    }

    func testApplyNowMovesReservationToFrontKeepingOthers() {
        var queue = QueuedCommandQueue()
        let first = QueuedCommand(prompt: "첫 업무")
        let second = QueuedCommand(prompt: "둘째 업무")
        let third = QueuedCommand(prompt: "셋째 업무")
        for command in [first, second, third] {
            _ = queue.enqueue(command)
        }

        XCTAssertTrue(queue.moveToFront(id: third.id))
        XCTAssertEqual(
            queue.commands.map(\.id),
            [third.id, first.id, second.id],
            "바로 적용은 순서만 바꾸고 나머지 예약을 남겨야 합니다."
        )
        XCTAssertFalse(queue.moveToFront(id: UUID()))
    }

    func testDrainTakesReservationsInOrder() {
        var queue = QueuedCommandQueue()
        _ = queue.enqueue(QueuedCommand(prompt: "첫 업무"))
        _ = queue.enqueue(QueuedCommand(prompt: "둘째 업무"))

        XCTAssertEqual(queue.removeFirst()?.prompt, "첫 업무")
        XCTAssertEqual(queue.removeFirst()?.prompt, "둘째 업무")
        XCTAssertTrue(queue.isEmpty)
        XCTAssertNil(queue.removeFirst())
    }

    func testAutomaticDrainRunsOnlyAfterCompletedTurn() {
        XCTAssertTrue(
            QueuedCommandDrainPolicy.shouldDrain(
                status: .completed,
                isImmediateRequest: false
            ),
            "응답이 끝나면 예약이 자동으로 이어져야 합니다."
        )

        for status in [LiveTurnStatus.failed, .interrupted] {
            XCTAssertFalse(
                QueuedCommandDrainPolicy.shouldDrain(
                    status: status,
                    isImmediateRequest: false
                ),
                "\(status)에서 자동으로 이어 가면 중단·실패를 무시하게 됩니다."
            )
        }
    }

    func testImmediateRequestDrainsEvenAfterInterruption() {
        // 바로 적용은 사용자가 직접 중단시킨 것이므로 interrupted에서도
        // 곧바로 다음 예약으로 다시 질문해야 한다.
        for status in [
            LiveTurnStatus.interrupted, .failed, .completed,
        ] {
            XCTAssertTrue(
                QueuedCommandDrainPolicy.shouldDrain(
                    status: status,
                    isImmediateRequest: true
                )
            )
        }
    }

    func testFailedSubmissionRestoresReservationToFront() {
        var queue = QueuedCommandQueue()
        XCTAssertTrue(queue.enqueue(QueuedCommand(prompt: "첫 예약")))
        XCTAssertTrue(queue.enqueue(QueuedCommand(prompt: "둘째 예약")))

        guard let drained = queue.removeFirst() else {
            XCTFail("배출할 예약이 없습니다.")
            return
        }
        XCTAssertEqual(queue.commands.map(\.prompt), ["둘째 예약"])

        queue.restoreToFront(drained)
        XCTAssertEqual(
            queue.commands.map(\.prompt),
            ["첫 예약", "둘째 예약"],
            "제출이 실패한 예약은 원래 순서대로 되돌아와야 합니다."
        )

        // 같은 예약을 두 번 되돌려도 중복되지 않는다.
        queue.restoreToFront(drained)
        XCTAssertEqual(queue.commands.count, 2)

        // 큐가 가득 찬 상태에서도 원래 자기 자리는 되찾는다.
        var fullQueue = QueuedCommandQueue()
        for index in 0..<QueuedCommandQueue.maximumCount {
            XCTAssertTrue(
                fullQueue.enqueue(QueuedCommand(prompt: "예약 \(index)"))
            )
        }
        guard let head = fullQueue.removeFirst() else {
            XCTFail("배출할 예약이 없습니다.")
            return
        }
        XCTAssertTrue(fullQueue.enqueue(QueuedCommand(prompt: "새 예약")))
        XCTAssertTrue(fullQueue.isFull)
        fullQueue.restoreToFront(head)
        XCTAssertEqual(
            fullQueue.commands.first?.prompt,
            head.prompt,
            "가득 찼다는 이유로 실패한 예약을 버리면 안 됩니다."
        )
        XCTAssertEqual(
            fullQueue.commands.count,
            QueuedCommandQueue.maximumCount + 1
        )
    }

    func testDirectorRejectsReservationForIdleCharacter() {
        let director = AgentDirector(startBackgroundTasks: false)

        XCTAssertFalse(
            director.enqueueCommand("업무", for: .boss),
            "실행 중이 아닌 직원에게는 예약 대신 바로 제출해야 합니다."
        )
        XCTAssertTrue(director.queuedCommands(for: .boss).isEmpty)
        XCTAssertFalse(director.canQueueForSelectedCharacter)
        XCTAssertTrue(director.queuedAttachments.isEmpty)
    }

    func testSummaryCondensesMultilinePrompt() {
        let command = QueuedCommand(
            prompt: "  첫 줄입니다.\n둘째 줄입니다.  "
        )

        XCTAssertEqual(command.summary, "첫 줄입니다. 둘째 줄입니다.")

        let long = QueuedCommand(
            prompt: String(repeating: "가", count: 40)
        )
        XCTAssertEqual(long.summary.count, 25)
        XCTAssertTrue(long.summary.hasSuffix("…"))
    }
}
