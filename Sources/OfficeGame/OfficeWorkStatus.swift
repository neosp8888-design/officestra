import Combine
import Foundation

/// Only the current operation is displayed. No summary, plan or history is kept.
enum OfficeWorkPhase: String, Equatable, CaseIterable {
    case working, waiting, search, read, edit, test, build, web, collaboration, plan, thinking, reply, command, tool, compacting

    var label: String {
        switch self {
        case .working: "작업 중"
        case .waiting: "대기 중"
        case .search: "검색 중"
        case .read: "확인 중"
        case .edit: "수정 중"
        case .test: "테스트 중"
        case .build: "빌드 중"
        case .web: "웹 조회 중"
        case .collaboration: "협업 중"
        case .plan: "계획 중"
        case .thinking: "검토 중"
        case .reply: "답변 중"
        case .command: "명령 실행 중"
        case .tool: "도구 실행 중"
        case .compacting: "압축 중"
        }
    }

    static func current(kind: String, text: String, status: String) -> Self {
        if kind == "message" { return .reply }
        if kind == "thinking" { return .thinking }
        // Ending a command is not proof of success or a still-running operation.
        guard status == "running" else { return .working }
        if kind == "command" { return commandPhase(text) }
        if kind == "file_change" { return .edit }
        if kind == "collaboration" { return .collaboration }
        let text = String(text.prefix(600))
        if text.hasPrefix("파일 ·") || text.hasPrefix("파일 변경") { return .edit }
        if text.hasPrefix("Graft ·") { return .search }
        if text.hasPrefix("검색 ·") || text.hasPrefix("웹 검색") { return .web }
        if text.hasPrefix("계획 ·") { return .plan }
        let parts = (text.components(separatedBy: "\n").first ?? "").components(separatedBy: " · ")
        let name = parts.first == "도구" && parts.count > 1 ? parts[1] : ""
        switch ClaudeToolFamily.of(name) {
        case .read: return .read
        case .edit: return .edit
        case .search: return .search
        case .web: return .web
        case .plan: return .plan
        case .delegate: return .collaboration
        case .shell: return commandPhase(parts.dropFirst(2).joined(separator: " · "))
        case .other:
            let name = name.components(separatedBy: "__").last?.components(separatedBy: ".").last?.lowercased() ?? ""
            if ["apply_patch", "write_to_file", "replace_file_content", "multi_replace_file_content"].contains(name) { return .edit }
            if ["view_file", "read_file"].contains(name) { return .read }
            if ["grep_search", "find_by_name"].contains(name) { return .search }
            return .tool
        }
    }

    private static func commandPhase(_ source: String) -> Self {
        var command = String(source.prefix(800)).trimmingCharacters(in: .whitespacesAndNewlines)
        if let range = command.range(of: #"^(?:/[^\s]+/)?(?:zsh|bash|sh)\s+-[a-z]*c\s+["']?"#, options: .regularExpression) {
            command.removeSubrange(range)
        }
        let patterns: [(String, Self)] = [
            (#"^(?:(?:/[^\s]+/)?python[\d.]*\s+\S*office-test\.py\b|(?:/[^\s]+/)?python[\d.]*\s+(?:-B\s+)?-m\s+(?:unittest|pytest)\b|(?:swift\s+test|node\s+--test|pytest|npm\s+(?:test|run\s+test)|pnpm\s+test)\b)"#, .test),
            (#"^(?:swift\s+build\b|(?:\./)?scripts/build-app\.sh\b|(?:npm|pnpm)\s+run\s+build\b)"#, .build),
            (#"^(?:rg|grep|find|ls)\s"#, .search),
            (#"^(?:cat|head|tail|sed|less)\s"#, .read),
            (#"^git\s+(?:status|diff|log|show)\b"#, .read)
        ]
        return patterns.first { command.range(of: $0.0, options: .regularExpression) != nil }?.1 ?? .command
    }
}

struct TerminalWorkActivity: Decodable, Equatable {
    let kind: String
    let text: String
    let status: String
}

/// Observed only by this employee's bubble. Streaming prose does not redraw it.
@MainActor
final class OfficeWorkStatusStore: ObservableObject {
    @Published private(set) var phase: OfficeWorkPhase?
    private var turnID: String?
    private var isRunning = false
    private var terminalPhase: (turnID: String, phase: OfficeWorkPhase)?

    func update(turns: [LiveFeedTurn]) {
        guard let turn = turns.max(by: { $0.startedAt < $1.startedAt }) else {
            turnID = nil; isRunning = false; terminalPhase = nil
            publish(nil)
            return
        }
        turnID = turn.id
        isRunning = turn.status.isRunning
        guard isRunning else {
            if terminalPhase?.turnID == turn.id { terminalPhase = nil }
            publish(nil)
            return
        }
        if turn.status == .pending { publish(.waiting) }
        else if let terminalPhase, terminalPhase.turnID == turn.id { publish(terminalPhase.phase) }
        else if let activity = turn.activities.last {
            publish(.current(kind: activity.kind, text: activity.text, status: activity.status.rawValue))
        } else { publish(.working) }
    }

    func receive(turnID incomingID: String, activity: TerminalWorkActivity) {
        // An event may precede the feed refresh for a newly started turn.
        // Never let a late event replace the state of a different running turn.
        guard turnID == nil || turnID == incomingID || !isRunning else { return }
        if turnID == incomingID && !isRunning { return }
        let next = OfficeWorkPhase.current(kind: activity.kind, text: activity.text, status: activity.status)
        terminalPhase = (incomingID, next)
        if turnID == incomingID && isRunning { publish(next) }
    }

    private func publish(_ next: OfficeWorkPhase?) {
        if phase != next { phase = next }
    }
}
