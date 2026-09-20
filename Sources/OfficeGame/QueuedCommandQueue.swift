// 이 파일은 응답 생성 중인 직원에게 미리 걸어두는 다음 업무 예약을 다룬다.

import Foundation
import OfficeCore

struct QueuedCommand: Identifiable, Equatable, Sendable {
    let id: UUID
    let prompt: String
    let attachments: [PendingAttachment]
    let replyRecipient: OfficeCharacter?

    init(
        id: UUID = UUID(),
        prompt: String,
        attachments: [PendingAttachment] = [],
        replyRecipient: OfficeCharacter? = nil
    ) {
        self.id = id
        self.prompt = prompt
        self.attachments = attachments
        self.replyRecipient = replyRecipient
    }

    /// 예약 칩에 보여줄 한 줄 요약이다.
    var summary: String {
        let condensed = prompt
            .split(whereSeparator: \.isNewline)
            .joined(separator: " ")
            .trimmingCharacters(in: .whitespaces)
        guard condensed.count > 24 else {
            return condensed
        }
        return condensed.prefix(24) + "…"
    }
}

struct QueuedCommandQueue: Equatable {
    static let maximumCount = 3

    private(set) var commands: [QueuedCommand] = []

    var isFull: Bool {
        commands.count >= Self.maximumCount
    }

    var isEmpty: Bool {
        commands.isEmpty
    }

    /// 가득 찼으면 새 예약을 받지 않는다. 오래된 예약을 조용히 밀어내면
    /// 사용자가 적어둔 업무가 말없이 사라진다.
    mutating func enqueue(_ command: QueuedCommand) -> Bool {
        guard !isFull else {
            return false
        }
        commands.append(command)
        return true
    }

    @discardableResult
    mutating func remove(id: UUID) -> QueuedCommand? {
        guard let index = commands.firstIndex(where: { $0.id == id }) else {
            return nil
        }
        return commands.remove(at: index)
    }

    @discardableResult
    mutating func removeFirst() -> QueuedCommand? {
        commands.isEmpty ? nil : commands.removeFirst()
    }

    /// 제출이 실패한 예약을 원래 자리로 되돌린다. 큐가 가득 차 있어도
    /// 이미 우리 것이던 자리를 되찾는 것이므로 최대 개수로 막지 않는다.
    mutating func restoreToFront(_ command: QueuedCommand) {
        guard !commands.contains(where: { $0.id == command.id }) else {
            return
        }
        commands.insert(command, at: 0)
    }

    /// 바로 적용은 고른 예약을 맨 앞으로 옮긴 뒤 배수 경로를 그대로
    /// 태운다. 순서만 바꾸므로 나머지 예약은 그대로 남는다.
    @discardableResult
    mutating func moveToFront(id: UUID) -> Bool {
        guard let index = commands.firstIndex(where: { $0.id == id }) else {
            return false
        }
        let command = commands.remove(at: index)
        commands.insert(command, at: 0)
        return true
    }
}

enum QueuedCommandDrainPolicy {
    /// 완료된 턴 다음에만 자동으로 이어 간다. 사용자가 중단을 눌렀거나
    /// 작업이 실패했는데 다음 예약이 자동으로 돌면 중단한 의미가 없다.
    /// 그 경우 예약은 남겨 두고 사용자가 직접 고르게 한다.
    /// 바로 적용은 사용자가 명시적으로 요청한 중단이므로 예외다.
    static func shouldDrain(
        status: LiveTurnStatus,
        isImmediateRequest: Bool
    ) -> Bool {
        if isImmediateRequest {
            return true
        }
        return status == .completed
    }
}
