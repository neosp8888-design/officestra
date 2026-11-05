// 이 파일은 Claude Code 활동 기록을 도구 중심 타임라인 항목으로 해석한다.

import Foundation
import OfficeCore

/// Claude 활동 텍스트에서 복원한 도구 호출 한 건이다.
struct ClaudeToolCall: Equatable {
    let name: String
    let detail: String
    let planSteps: [ClaudePlanStep]
    /// 백엔드가 편집 도구 입력에서 센 줄 수다. 옛 기록에는 없다.
    let additions: Int?
    let deletions: Int?

    private static let prefix = "도구 · "
    private static let separator = " · "

    var displayDetail: String {
        name.isEmpty ? OfficeLocalization.systemMessage(detail) : detail
    }

    static func parse(_ activity: LiveFeedActivity) -> ClaudeToolCall {
        let lines = activity.text.components(separatedBy: "\n")
        let header = lines.first ?? ""
        var name = ""
        var detail = ""

        if header.hasPrefix(prefix) {
            let body = String(header.dropFirst(prefix.count))
            if let range = body.range(of: separator) {
                name = String(body[..<range.lowerBound])
                detail = String(body[range.upperBound...])
            } else {
                name = body
            }
        } else if activity.kind == "command" {
            name = "Bash"
            detail = header
        } else {
            detail = header
        }

        let stats = parseEditStats(Array(lines.dropFirst()))
        return ClaudeToolCall(
            name: name,
            detail: detail,
            planSteps: ClaudePlanStep.parse(Array(lines.dropFirst())),
            additions: stats?.additions,
            deletions: stats?.deletions
        )
    }

    /// `+12 -3` 한 줄에서 편집 줄 수를 읽는다.
    private static func parseEditStats(
        _ lines: [String]
    ) -> (additions: Int, deletions: Int)? {
        for line in lines {
            let parts = line
                .trimmingCharacters(in: .whitespaces)
                .split(separator: " ")
            guard
                parts.count == 2,
                parts[0].hasPrefix("+"),
                parts[1].hasPrefix("-"),
                let additions = Int(parts[0].dropFirst()),
                let deletions = Int(parts[1].dropFirst())
            else {
                continue
            }
            return (additions, deletions)
        }
        return nil
    }

    /// MCP 도구는 이름이 길어 마지막 구간만 배지에 쓴다.
    var displayName: String {
        guard !name.isEmpty else {
            return OfficeLocalization.string("도구")
        }
        if name == "도구" || name == "연결 도구" {
            return OfficeLocalization.string(name)
        }
        guard name.hasPrefix("mcp__") else {
            return name
        }
        return name.components(separatedBy: "__").last ?? name
    }

    var family: ClaudeToolFamily {
        ClaudeToolFamily.of(name)
    }
}

/// 도구를 화면 표현이 같은 갈래로 묶는다.
enum ClaudeToolFamily: Equatable {
    case shell
    case read
    case edit
    case search
    case web
    case plan
    case delegate
    case other

    static func of(_ name: String) -> ClaudeToolFamily {
        switch name.lowercased() {
        case "bash", "shell", "terminal", "bashoutput", "killshell":
            .shell
        case "read", "notebookread":
            .read
        case "edit", "write", "multiedit", "notebookedit":
            .edit
        case "grep", "glob":
            .search
        case "websearch", "webfetch":
            .web
        case "todowrite":
            .plan
        case "task", "agent":
            .delegate
        default:
            .other
        }
    }
}

/// TodoWrite 한 줄에 해당하는 계획 단계다.
struct ClaudePlanStep: Equatable, Identifiable {
    enum State: Equatable {
        case done
        case active
        case todo
    }

    let id: Int
    let state: State
    let text: String

    static func parse(_ lines: [String]) -> [ClaudePlanStep] {
        lines.enumerated().compactMap { index, line in
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            guard trimmed.count > 3, trimmed.hasPrefix("[") else {
                return nil
            }
            let marker = trimmed[trimmed.index(trimmed.startIndex, offsetBy: 1)]
            guard trimmed[trimmed.index(trimmed.startIndex, offsetBy: 2)] == "]"
            else {
                return nil
            }
            let state: State = switch marker {
            case "x", "X":
                .done
            case "~", ">":
                .active
            default:
                .todo
            }
            return ClaudePlanStep(
                id: index,
                state: state,
                text: trimmed
                    .dropFirst(3)
                    .trimmingCharacters(in: .whitespaces)
            )
        }
    }
}

/// 도구 그룹 한 칸의 갈래다. 편집과 계획은 별도 항목이라 여기에 오지 않는다.
enum ClaudeToolGroupKind: String, Equatable, Hashable {
    case shell
    case read
    case search
    case web
    case delegate
    case other

    init(family: ClaudeToolFamily) {
        switch family {
        case .shell:
            self = .shell
        case .read:
            self = .read
        case .search:
            self = .search
        case .web:
            self = .web
        case .delegate:
            self = .delegate
        case .edit, .plan, .other:
            self = .other
        }
    }
}

/// 타임라인에 놓이는 한 덩어리다.
enum ClaudeTranscriptEntry: Identifiable, Equatable {
    case thoughts(ClaudeThoughtRun)
    case tools(ClaudeToolRun)
    case edits(ClaudeEditRun)
    case plan(ClaudePlanBoard)
    case message(ClaudeTranscriptMessage)
    case steer(AgentSteerNote)

    var id: String {
        switch self {
        case .thoughts(let run):
            run.id
        case .tools(let run):
            run.id
        case .edits(let run):
            run.id
        case .plan(let board):
            board.id
        case .message(let message):
            message.id
        case .steer(let note):
            note.id
        }
    }
}

struct ClaudeThought: Identifiable, Equatable {
    let activityID: String
    let text: String
    let occurredAt: Date
    let isRunning: Bool

    var id: String { "thought:\(activityID)" }

    /// 스트림 시작만 알리는 자리표시자다. 원문이 끝내 오지 않는 경우도 있다.
    static let placeholderText = "추론 중"

    /// 보여줄 원문이 없으면 카드로 만들지 않는다.
    static func hasContent(_ activity: LiveFeedActivity) -> Bool {
        guard activity.kind == "thinking" else {
            return false
        }
        let text = activity.text.trimmingCharacters(
            in: .whitespacesAndNewlines
        )
        return !text.isEmpty && text != placeholderText
    }
}

/// 한 대화 구간의 추론을 한 칸으로 묶는다. 최신 추론만 원문 그대로 펼쳐 둔다.
struct ClaudeThoughtRun: Identifiable, Equatable {
    let thoughts: [ClaudeThought]

    var id: String { "thoughts:\(thoughts.first?.activityID ?? "empty")" }

    var isRunning: Bool {
        thoughts.contains { $0.isRunning }
    }

    var latestThought: ClaudeThought? {
        thoughts.last
    }

    func visibleHistoryThoughts(
        showsAll: Bool,
        limit: Int
    ) -> [ClaudeThought] {
        let history = thoughts.dropLast()
        guard !showsAll, history.count > limit else {
            return Array(history)
        }
        return Array(history.suffix(limit))
    }

    func hiddenHistoryThoughtCount(limit: Int) -> Int {
        max(0, thoughts.count - 1 - limit)
    }
}

struct ClaudeToolStep: Identifiable, Equatable {
    let activityID: String
    let call: ClaudeToolCall
    let status: LiveFeedActivityStatus
    let occurredAt: Date

    var id: String { activityID }
}

struct ClaudeToolRun: Identifiable, Equatable {
    let kind: ClaudeToolGroupKind
    let steps: [ClaudeToolStep]

    var id: String {
        "tools:\(kind.rawValue):\(steps.first?.activityID ?? "empty")"
    }

    var isRunning: Bool {
        steps.contains { $0.status == .running }
    }

    /// 사용한 도구 이름을 실제 호출 순서대로 요약한다.
    var title: String {
        var order: [String] = []
        var counts: [String: Int] = [:]
        for step in steps {
            let name = step.call.displayName
            if counts[name] == nil {
                order.append(name)
            }
            counts[name, default: 0] += 1
        }
        let visible = order.prefix(3).map { name in
            let count = counts[name] ?? 0
            return count > 1 ? "\(name) \(count)" : name
        }
        let remainder = order.count - visible.count
        return visible.joined(separator: " · ")
            + (remainder > 0 ? OfficeLocalization.format(" 외 %d종", remainder) : "")
    }

    var latestStep: ClaudeToolStep? {
        steps.last
    }

    func visibleHistorySteps(
        showsAll: Bool,
        limit: Int
    ) -> [ClaudeToolStep] {
        let history = steps.dropLast()
        guard !showsAll, history.count > limit else {
            return Array(history)
        }
        return Array(history.suffix(limit))
    }

    func hiddenHistoryStepCount(limit: Int) -> Int {
        max(0, steps.count - 1 - limit)
    }
}

/// 같은 파일의 여러 편집을 한 줄로 합친 결과다.
struct ClaudeEditFile: Identifiable, Equatable {
    let path: String
    let stepID: String
    private(set) var editCount: Int
    private(set) var additions: Int?
    private(set) var deletions: Int?
    private(set) var status: LiveFeedActivityStatus

    var id: String { stepID }

    mutating func merge(_ step: ClaudeToolStep) {
        editCount += 1
        // 한 번이라도 통계가 빠지면 합계를 만들지 않는다.
        if let stepAdditions = step.call.additions,
            let stepDeletions = step.call.deletions,
            let currentAdditions = additions,
            let currentDeletions = deletions
        {
            additions = currentAdditions + stepAdditions
            deletions = currentDeletions + stepDeletions
        } else {
            additions = nil
            deletions = nil
        }
        if step.status == .failed {
            status = .failed
        } else if step.status == .running, status != .failed {
            status = .running
        }
    }
}

struct ClaudeEditRun: Identifiable, Equatable {
    let steps: [ClaudeToolStep]

    var id: String { "edits:\(steps.first?.activityID ?? "empty")" }

    var status: LiveFeedActivityStatus {
        if steps.contains(where: { $0.status == .failed }) {
            return .failed
        }
        if steps.contains(where: { $0.status == .running }) {
            return .running
        }
        return .completed
    }

    /// 같은 파일을 여러 번 고쳐도 한 건으로 센다.
    var fileCount: Int {
        Set(steps.map(\.call.detail).filter { !$0.isEmpty }).count
    }

    /// 화면에 보여줄 파일 목록이다. 같은 파일의 연속 편집을 한 줄로
    /// 합쳐 첫 등장 순서대로 돌려준다.
    var files: [ClaudeEditFile] {
        var order: [String] = []
        var merged: [String: ClaudeEditFile] = [:]

        for step in steps {
            let path = step.call.detail
            guard !path.isEmpty else {
                continue
            }
            guard var existing = merged[path] else {
                order.append(path)
                merged[path] = ClaudeEditFile(
                    path: path,
                    stepID: step.id,
                    editCount: 1,
                    additions: step.call.additions,
                    deletions: step.call.deletions,
                    status: step.status
                )
                continue
            }
            existing.merge(step)
            merged[path] = existing
        }

        return order.compactMap { merged[$0] }
    }

    /// 모든 편집에 통계가 있을 때만 합계를 보여준다. 일부만 더하면
    /// 실제보다 작은 수치를 사실처럼 보여주게 된다.
    var totals: (additions: Int, deletions: Int)? {
        let files = files
        guard !files.isEmpty else {
            return nil
        }
        var additions = 0
        var deletions = 0
        for file in files {
            guard
                let fileAdditions = file.additions,
                let fileDeletions = file.deletions
            else {
                return nil
            }
            additions += fileAdditions
            deletions += fileDeletions
        }
        return (additions, deletions)
    }

    var title: String {
        switch status {
        case .running:
            OfficeLocalization.string("파일을 편집하는 중")
        case .failed:
            OfficeLocalization.string("파일 편집에 실패했습니다")
        case .completed:
            fileCount > 0
                ? OfficeLocalization.format("파일 %d개를 편집했습니다", fileCount)
                : OfficeLocalization.string("파일을 편집했습니다")
        }
    }

    var copyText: String {
        var lines = [title]
        if let totals {
            lines.append("+\(totals.additions) -\(totals.deletions)")
        }
        let files = files
        if files.isEmpty {
            lines.append(
                contentsOf: steps.map { step in
                    step.call.detail.isEmpty
                        ? step.call.displayName
                        : "\(step.call.displayName) \(step.call.detail)"
                }
            )
        } else {
            lines.append(
                contentsOf: files.map { file in
                    guard
                        let additions = file.additions,
                        let deletions = file.deletions
                    else {
                        return file.path
                    }
                    return "\(file.path) +\(additions) -\(deletions)"
                }
            )
        }
        return lines.joined(separator: "\n")
    }
}

struct ClaudePlanBoard: Identifiable, Equatable {
    let activityID: String
    let steps: [ClaudePlanStep]
    let occurredAt: Date

    var id: String { "plan:\(activityID)" }

    var doneCount: Int {
        steps.filter { $0.state == .done }.count
    }

    var activeStep: ClaudePlanStep? {
        steps.first { $0.state == .active }
    }
}

struct ClaudeTranscriptMessage: Identifiable, Equatable {
    let id: String
    let text: String
    let occurredAt: Date
}

/// Claude 턴 하나를 발생 순서대로 재구성한 결과다.
struct ClaudeTranscriptPresentation: Equatable {
    let entries: [ClaudeTranscriptEntry]
    let showsWaiting: Bool
    /// 글자 단위로 계속 늘어나는 마지막 응답 메시지다.
    let streamingMessageID: String?

    /// 버퍼에 모으는 동안 갈래별 자리를 가리키는 값이다.
    private enum GroupSlot: Hashable {
        case thoughts
        case tools(ClaudeToolGroupKind)
        case edits
    }

    var latestMessage: ClaudeTranscriptMessage? {
        for entry in entries.reversed() {
            if case .message(let message) = entry {
                return message
            }
        }
        return nil
    }

    static func make(
        turnID: String,
        activities: [LiveFeedActivity],
        response: String,
        responseUpdatedAt: Date,
        isRunning: Bool
    ) -> ClaudeTranscriptPresentation {
        var entries: [ClaudeTranscriptEntry] = []
        var currentSlot: GroupSlot?
        var thoughtBuffer: [ClaudeThought] = []
        var toolBuffer: [ClaudeToolStep] = []
        var editBuffer: [ClaudeToolStep] = []

        // 코덱스 카드와 같은 규칙이다. 연속된 같은 갈래만 한 칸에 모으고,
        // 갈래가 바뀌면 그 자리에서 칸을 끊어 발생 순서를 그대로 지킨다.
        // 중간에 다른 도구가 낀 같은 갈래를 앞 칸으로 끌어올리지 않는다.
        func flushGroups() {
            switch currentSlot {
            case .thoughts:
                if !thoughtBuffer.isEmpty {
                    entries.append(
                        .thoughts(ClaudeThoughtRun(thoughts: thoughtBuffer))
                    )
                }
            case .tools(let kind):
                if !toolBuffer.isEmpty {
                    entries.append(
                        .tools(ClaudeToolRun(kind: kind, steps: toolBuffer))
                    )
                }
            case .edits:
                if !editBuffer.isEmpty {
                    entries.append(.edits(ClaudeEditRun(steps: editBuffer)))
                }
            case nil:
                break
            }
            currentSlot = nil
            thoughtBuffer.removeAll(keepingCapacity: true)
            toolBuffer.removeAll(keepingCapacity: true)
            editBuffer.removeAll(keepingCapacity: true)
        }

        func reserve(_ slot: GroupSlot) {
            guard currentSlot != slot else {
                return
            }
            flushGroups()
            currentSlot = slot
        }

        for activity in activities {
            switch activity.kind {
            case "thinking":
                // 원문 없는 자리표시자는 아래쪽 `생각 중` 표시가 대신한다.
                guard ClaudeThought.hasContent(activity) else {
                    continue
                }
                reserve(.thoughts)
                thoughtBuffer.append(
                    ClaudeThought(
                        activityID: activity.id,
                        text: activity.text,
                        occurredAt: activity.occurredAt,
                        isRunning: activity.status == .running
                    )
                )
            case "message":
                flushGroups()
                entries.append(
                    .message(
                        ClaudeTranscriptMessage(
                            id: "activity:\(activity.id)",
                            text: activity.text,
                            occurredAt: activity.occurredAt
                        )
                    )
                )
            case AgentSteerNote.activityKind:
                // 실행 중 끼워 넣은 입력은 그 시점에서 작업 묶음을 끊는다.
                flushGroups()
                if let note = AgentSteerNote(activity) {
                    entries.append(.steer(note))
                }
            default:
                let call = ClaudeToolCall.parse(activity)
                let step = ClaudeToolStep(
                    activityID: activity.id,
                    call: call,
                    status: activity.status,
                    occurredAt: activity.occurredAt
                )
                switch call.family {
                case .plan:
                    flushGroups()
                    // 계획은 갱신되는 하나의 보드이므로 최신 위치에만 남긴다.
                    entries.removeAll { entry in
                        if case .plan = entry {
                            return true
                        }
                        return false
                    }
                    entries.append(
                        .plan(
                            ClaudePlanBoard(
                                activityID: activity.id,
                                steps: call.planSteps,
                                occurredAt: activity.occurredAt
                            )
                        )
                    )
                case .edit:
                    reserve(.edits)
                    editBuffer.append(step)
                default:
                    let kind = ClaudeToolGroupKind(family: call.family)
                    reserve(.tools(kind))
                    toolBuffer.append(step)
                }
            }
        }
        flushGroups()

        let promotedMessages = activities
            .filter { $0.kind == "message" }
            .map(\.text)
        let remainingResponse = AgentTranscriptText
            .responseAfterRemovingPromotedMessages(
                response,
                promotedMessages: promotedMessages
            )

        var streamingMessageID: String?
        if
            AgentTranscriptText
                .isGeneratedImagePreviewSuffix(remainingResponse),
            let messageIndex = entries.lastIndex(where: { entry in
                if case .message = entry {
                    return true
                }
                return false
            }),
            case .message(let message) = entries[messageIndex]
        {
            entries[messageIndex] = .message(
                ClaudeTranscriptMessage(
                    id: message.id,
                    text: message.text + "\n\n" + remainingResponse,
                    occurredAt: responseUpdatedAt
                )
            )
        } else if !remainingResponse.isEmpty {
            let id = "response:\(turnID)"
            entries.append(
                .message(
                    ClaudeTranscriptMessage(
                        id: id,
                        text: remainingResponse,
                        occurredAt: responseUpdatedAt
                    )
                )
            )
            streamingMessageID = isRunning ? id : nil
        }

        return ClaudeTranscriptPresentation(
            entries: entries,
            // 자리표시자 추론만 돌고 있으면 화면에 남는 카드가 없으므로
            // 아래쪽 `생각 중`으로 진행 중임을 알린다.
            showsWaiting: isRunning
                && streamingMessageID == nil
                && !activities.contains { activity in
                    // 전달을 기다리는 끼어들기는 모델 진행 카드가 아니다.
                    activity.status == .running
                        && activity.kind != AgentSteerNote.activityKind
                        && !(
                            activity.kind == "thinking"
                                && !ClaudeThought.hasContent(activity)
                        )
                },
            streamingMessageID: streamingMessageID
        )
    }
}
