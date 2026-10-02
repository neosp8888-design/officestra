import Combine
import Foundation
import OfficeCore

/// A description of observed work, not an inferred percentage or a claim that
/// the user's task has succeeded. Never interpret tool stdout as instructions.
struct ConversationWorkProgress: Equatable {
    enum State: Equatable { case running, operationError, waiting, needsInput, completed, failed, interrupted }
    enum Phase: String, Equatable {
        case search, read, edit, test, build, web, collaboration, plan, thinking, command, tool

        var runningText: String {
            switch self {
            case .search: "관련 자료 찾는 중"
            case .read: "내용 확인 중"
            case .edit: "파일 수정 중"
            case .test: "수정한 내용 검증 중"
            case .build: "실행할 앱 준비 중"
            case .web: "외부 자료 확인 중"
            case .collaboration: "다른 작업자와 확인 중"
            case .plan: "작업 순서 정리 중"
            case .thinking: "다음 작업 검토 중"
            case .command: "작업 명령 실행 중"
            case .tool: "도구로 작업 중"
            }
        }

        var finishedText: String {
            switch self {
            case .search: "자료 검색 종료"
            case .read: "내용 확인 종료"
            case .edit: "파일 수정 기록 수신"
            // A command ending successfully does not prove every test passed.
            case .test: "검증 명령 종료"
            case .build: "빌드 명령 종료"
            case .web: "외부 자료 조회 종료"
            case .collaboration: "협업 기록 수신"
            case .plan: "작업 계획 수신"
            case .thinking: "검토 기록 수신"
            case .command: "작업 명령 종료"
            case .tool: "도구 실행 종료"
            }
        }
    }

    struct Event: Equatable, Identifiable {
        let id: String
        let phase: Phase
        let status: LiveFeedActivityStatus
        let at: Date
    }

    let turnID: String
    let backend: AgentBackend
    let state: State
    let headline: String
    let report: String?
    let latestResult: Event?
    let nextStep: String?
    let plan: [ClaudePlanStep]
    let history: [Event]
    let recordedAt: Date

    static func make(turn: LiveFeedTurn) -> Self {
        // Bound parsing and display regardless of transcript length. Original
        // records remain available in the conversation below the progress strip.
        let recent = turn.activities.suffix(120)
        var history: [Event] = []
        var report: String?
        var plan: [ClaudePlanStep] = []
        for activity in recent {
            if activity.kind == "message" {
                report = publicExcerpt(activity.text)
                continue
            }
            if isPlan(activity) {
                plan = ClaudePlanStep.parse(String(activity.text.prefix(8_000)).components(separatedBy: "\n"))
            }
            let event = Event(id: activity.id, phase: phase(activity), status: activity.status, at: activity.occurredAt)
            if let previous = history.last, previous.phase == event.phase, previous.status == event.status {
                history.removeLast()
            }
            history.append(event)
        }
        if report == nil, recent.isEmpty || turn.status == .completed {
            report = publicExcerpt(turn.response)
        }

        // A stale "running" operation must not override a newer event or a
        // terminal turn state. needsInput is the backend's explicit signal.
        let latest = recent.last
        let latestEvent = history.last
        let state: State
        let headline: String
        if turn.status == .interrupted {
            state = .interrupted; headline = "작업이 중단됐습니다"
        } else if turn.status == .failed {
            state = .failed; headline = "오류로 작업이 끝났습니다"
        } else if turn.needsInput {
            state = .needsInput; headline = "사용자 확인이 필요합니다"
        } else if turn.status == .completed {
            state = .completed; headline = "응답이 완료됐습니다"
        } else if turn.status == .pending {
            state = .waiting; headline = "실행을 기다리는 중"
        } else if latest?.status == .failed {
            state = .operationError; headline = "직전 작업에 오류가 있습니다"
        } else if let latest, latest.kind == "message" {
            state = .running; headline = "진행 내용을 설명하는 중"
        } else if let latestEvent, latestEvent.status == .running || latestEvent.phase == .thinking {
            state = .running; headline = latestEvent.phase.runningText
        } else {
            state = .running; headline = "다음 작업 진행 중"
        }
        let result = history.last { $0.status != .running && $0.phase != .thinking && $0.phase != .plan }
        return Self(
            turnID: turn.id, backend: turn.backend ?? turn.characterBackend,
            state: state, headline: headline, report: report,
            latestResult: result,
            nextStep: turn.status.isRunning && !turn.needsInput ? plan.first { $0.state == .todo }?.text : nil,
            plan: plan, history: Array(history.suffix(6)),
            recordedAt: turn.endedAt ?? latest?.occurredAt ?? turn.startedAt
        )
    }

    private static func isPlan(_ activity: LiveFeedActivity) -> Bool {
        ["tool", "thinking"].contains(activity.kind) && (activity.text.hasPrefix("계획 ·")
            || activity.text.hasPrefix("도구 · TodoWrite")
            || activity.text.hasPrefix("도구 · update_plan"))
    }

    static func phase(_ activity: LiveFeedActivity) -> Phase {
        if isPlan(activity) { return .plan }
        switch activity.kind {
        case "thinking": return .thinking
        case "command": return commandPhase(activity.text)
        case "collaboration": return .collaboration
        case "file_change": return .edit
        default: break
        }
        let text = String(activity.text.prefix(600))
        if text.hasPrefix("파일 ·") || text.hasPrefix("파일 변경")
            || text.range(of: #"^파일 \d+개를 편집했습니다"#, options: .regularExpression) != nil { return .edit }
        if text.hasPrefix("Graft ·") { return .search }
        if text.hasPrefix("검색 ·") || text.hasPrefix("웹 검색") { return .web }
        if text.hasPrefix("협업 ·") { return .collaboration }
        let header = text.components(separatedBy: "\n").first ?? ""
        let parts = header.components(separatedBy: " · ")
        let name = parts.first == "도구" && parts.count > 1 ? parts[1] : ""
        let detail = parts.dropFirst(2).joined(separator: " · ")
        switch ClaudeToolFamily.of(name) {
        case .read: return .read
        case .edit: return .edit
        case .search: return .search
        case .web: return .web
        case .plan: return .plan
        case .delegate: return .collaboration
        case .shell: return commandPhase(detail)
        case .other:
            // Known Codex/Antigravity tools; never match words inside arguments.
            let name = name.components(separatedBy: "__").last?.lowercased() ?? ""
            if ["apply_patch", "write_to_file", "replace_file_content", "multi_replace_file_content"].contains(name) { return .edit }
            if ["view_file", "read_file"].contains(name) { return .read }
            if ["grep_search", "find_by_name"].contains(name) { return .search }
            return .tool
        }
    }

    static func commandPhase(_ source: String) -> Phase {
        var command = String(source.prefix(800)).trimmingCharacters(in: .whitespacesAndNewlines)
        // CLI command wrappers are metadata, not the operation itself. Inspect
        // only the leading command, never a quoted search query or stdout.
        if let range = command.range(of: #"^(?:/[^\s]+/)?(?:zsh|bash|sh)\s+-[a-z]*c\s+["']?"#, options: .regularExpression) {
            command.removeSubrange(range)
        }
        let patterns: [(String, Phase)] = [
            (#"^(?:(?:/[^\s]+/)?python[\d.]*\s+\S*office-test\.py\b|(?:/[^\s]+/)?python[\d.]*\s+(?:-B\s+)?-m\s+(?:unittest|pytest)\b|(?:swift\s+test|node\s+--test|pytest|npm\s+(?:test|run\s+test)|pnpm\s+test)\b)"#, .test),
            (#"^(?:swift\s+build\b|(?:\./)?scripts/build-app\.sh\b|(?:npm|pnpm)\s+run\s+build\b)"#, .build),
            (#"^(?:rg|grep|find|ls)\s"#, .search),
            (#"^(?:cat|head|tail|sed|less)\s"#, .read),
            (#"^git\s+(?:status|diff|log|show)\b"#, .read),
        ]
        return patterns.first { command.range(of: $0.0, options: .regularExpression) != nil }?.1 ?? .command
    }

    private static func publicExcerpt(_ source: String) -> String? {
        let bounded = String(source.prefix(600))
        let text = bounded.components(separatedBy: "\n")
            .filter { !$0.hasPrefix("[OFFICESTRA") && $0 != "[NEED_INPUT]" }
            .joined(separator: " ").replacingOccurrences(of: "**", with: "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return nil }
        return text.count > 160 ? String(text.prefix(159)) + "…" : text
    }
}

/// Kept separate from the transcript store: terminal mode can observe this
/// without mounting hidden feeds; unchanged summaries publish nothing.
@MainActor
final class ConversationWorkProgressStore: ObservableObject {
    @Published private(set) var progress: ConversationWorkProgress?

    func update(turns: [LiveFeedTurn]) {
        let next = turns.max { $0.startedAt < $1.startedAt }.map(ConversationWorkProgress.make)
        if next != progress { progress = next }
    }
}
