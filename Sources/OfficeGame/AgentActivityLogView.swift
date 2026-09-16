// 이 파일은 공개 가능한 에이전트 진행 이벤트를 CLI형 타임라인으로 표시한다.

import AppKit
import OfficeCore
import SwiftUI

struct CodexTranscriptPresentation: Equatable {
    let entries: [CodexTranscriptEntry]
    let showsWaiting: Bool
    /// 실행 중에는 숨기고 완료 뒤 타이핑할 타임라인 끝 응답 후보다.
    let deferredResponseMessageID: String?
    /// 완료 뒤 최종 응답 표시와 피드백을 붙일 메시지다. 이미 중간에
    /// 공개된 메시지는 deferred와 분리해 원래 위치를 유지한다.
    let conclusionMessageID: String?

    var latestMessage: CodexTranscriptMessage? {
        for entry in entries.reversed() {
            if case .message(let message) = entry {
                return message
            }
        }
        return nil
    }

    var deferredResponseMessage: CodexTranscriptMessage? {
        guard let deferredResponseMessageID else {
            return nil
        }
        for entry in entries {
            if
                case .message(let message) = entry,
                message.id == deferredResponseMessageID
            {
                return message
            }
        }
        return nil
    }

    func visibleEntries(
        showsAll: Bool,
        compactLimit: Int
    ) -> [CodexTranscriptEntry] {
        let limit = max(1, compactLimit)
        guard !showsAll, entries.count > limit else {
            return entries
        }

        let suffixStart = entries.count - limit
        var visibleIndices = Array(suffixStart..<entries.count)
        if
            let preservedMessageID =
                deferredResponseMessageID ?? conclusionMessageID,
            let candidateIndex = entries.firstIndex(where: {
                $0.id == preservedMessageID
            }),
            !visibleIndices.contains(candidateIndex)
        {
            // 최종 응답 후보가 compact 범위 밖으로 밀려도 반드시 화면에
            // 남겨 완료 콜백과 줄 타이핑이 끊기지 않게 한다.
            visibleIndices[0] = candidateIndex
            visibleIndices.sort()
        }
        return visibleIndices.map { entries[$0] }
    }

    static func make(
        turnID: String,
        activities: [LiveFeedActivity],
        response: String,
        responseUpdatedAt: Date,
        isRunning: Bool,
        isCompleted: Bool = true
    ) -> CodexTranscriptPresentation {
        let visibleActivities = activities.filter {
            !CodexCollaborationSummary.isHiddenHousekeeping($0)
        }
        let messageActivities = visibleActivities.filter {
            $0.kind == "message"
        }
        let promotedMessages = messageActivities.map(\.text)
        let remainingResponse = AgentTranscriptText
            .responseAfterRemovingPromotedMessages(
                response,
                promotedMessages: promotedMessages
            )
        var deferredResponseMessage: CodexTranscriptMessage?

        func transcriptMessage(
            from activity: LiveFeedActivity,
            text: String? = nil,
            occurredAt: Date? = nil
        ) -> CodexTranscriptMessage {
            CodexTranscriptMessage(
                id: "activity:\(activity.id)",
                text: text ?? activity.text,
                occurredAt: occurredAt ?? activity.occurredAt
            )
        }

        if let exactResponse = messageActivities.reversed().first(where: {
            $0.text == response
        }) {
            deferredResponseMessage = transcriptMessage(from: exactResponse)
        }
        if
            deferredResponseMessage == nil,
            AgentTranscriptText
                .isGeneratedImagePreviewSuffix(remainingResponse),
            let previewOwnerID = generatedImagePreviewOwnerID(
                response: response,
                preview: remainingResponse,
                activities: visibleActivities
            ),
            let previewOwner = messageActivities.first(where: {
                "activity:\($0.id)" == previewOwnerID
            })
        {
            deferredResponseMessage = transcriptMessage(
                from: previewOwner,
                text: previewOwner.text + "\n\n" + remainingResponse,
                occurredAt: responseUpdatedAt
            )
        } else if
            deferredResponseMessage == nil,
            !remainingResponse.isEmpty
        {
            deferredResponseMessage = CodexTranscriptMessage(
                id: "response:\(turnID)",
                text: remainingResponse,
                occurredAt: responseUpdatedAt
            )
        }

        if
            deferredResponseMessage == nil,
            !response.isEmpty
        {
            for activity in messageActivities.reversed() {
                let separatorAndMessage = "\n\n" + activity.text
                if
                    response == activity.text
                        || response.hasSuffix(separatorAndMessage)
                {
                    deferredResponseMessage = transcriptMessage(
                        from: activity
                    )
                    break
                }
            }
        }
        if
            deferredResponseMessage == nil,
            let latestMessage = messageActivities.last,
            isRunning || response.isEmpty
        {
            deferredResponseMessage = transcriptMessage(from: latestMessage)
        }

        if isRunning, let latestMessage = messageActivities.last {
            if visibleActivities.last?.id == latestMessage.id {
                // 백엔드는 message 활동을 먼저 내보내고 response 초안을 바로
                // 뒤이어 저장한다. 타임라인 끝 메시지만 최종 후보로 잠시
                // 보류해 완료 전 번쩍임과 중복 타이핑을 막는다.
                deferredResponseMessage = transcriptMessage(from: latestMessage)
            }
        }

        if !isRunning, !isCompleted {
            // 실패·중단 턴의 마지막 공개 진행문을 최종 답변으로
            // 오인하지 않는다. 활동에 없는 부분 응답만 별도 본문으로
            // 보존하고 공개 메시지는 작업 내역에 그대로 남긴다.
            deferredResponseMessage = remainingResponse.isEmpty
                ? nil
                : CodexTranscriptMessage(
                    id: "response:\(turnID)",
                    text: remainingResponse,
                    occurredAt: responseUpdatedAt
                )
        }

        let conclusionMessageID = deferredResponseMessage?.id
        if
            let candidateMessage = deferredResponseMessage,
            let candidateActivity = messageActivities.first(where: {
                "activity:\($0.id)" == candidateMessage.id
            }),
            candidateMessage.text == candidateActivity.text,
            visibleActivities.last?.id != candidateActivity.id
        {
            // 뒤에 명령·도구나 다른 메시지가 이어졌다면 이미 공개된 중간
            // 진행문이다. 완료 전후 모두 원래 위치에 고정하고 최종 표식만
            // 붙여 재이동·재타이핑을 막는다.
            deferredResponseMessage = nil
        }

        let deferredResponseMessageID = deferredResponseMessage?.id
        var entries: [CodexTranscriptEntry] = []

        func isCollaborationActivity(
            _ activity: LiveFeedActivity
        ) -> Bool {
            activity.kind == "collaboration"
                || (
                    activity.kind == "tool"
                        && activity.text.hasPrefix("협업 ·")
                )
        }

        // 공개 메시지는 새 협업 묶음의 경계지만, 같은 검토자의 결과는
        // 메시지 뒤에 도착해도 원래 요청 카드에 귀속한다. 그래야 앞에는
        // `진행 중`, 뒤에는 `완료`가 동시에 남거나 검토자명이 사라지지
        // 않는다. 메시지 뒤의 새로운 검토자 요청만 새 카드가 된다.
        var collaborationBatches: [[LiveFeedActivity]] = []
        var currentCollaborationBatchIndex: Int?
        var collaborationBatchIndexByAgentID: [String: Int] = [:]
        var collaborationBatchIndexByActivityID: [String: Int] = [:]

        for activity in visibleActivities {
            if activity.kind == "message" {
                currentCollaborationBatchIndex = nil
                continue
            }
            guard isCollaborationActivity(activity) else {
                continue
            }

            let agentID = activity.collaboration?.agentThreadID
                ?? activity.id
            let batchIndex: Int
            if let existingIndex = collaborationBatchIndexByAgentID[agentID] {
                batchIndex = existingIndex
            } else if let currentCollaborationBatchIndex {
                batchIndex = currentCollaborationBatchIndex
                collaborationBatchIndexByAgentID[agentID] = batchIndex
            } else {
                batchIndex = collaborationBatches.count
                collaborationBatches.append([])
                currentCollaborationBatchIndex = batchIndex
                collaborationBatchIndexByAgentID[agentID] = batchIndex
            }
            collaborationBatches[batchIndex].append(activity)
            collaborationBatchIndexByActivityID[activity.id] = batchIndex
        }

        var collaborationSummaryByBatchIndex:
            [Int: CodexCollaborationSummary] = [:]
        var collaborationAnchorIDByBatchIndex: [Int: String] = [:]
        for (batchIndex, activities) in collaborationBatches.enumerated() {
            guard
                let anchorID = activities.first?.id,
                let summary = CodexCollaborationSummary.make(
                    from: activities.map(CodexActivityGroupItem.activity)
                )
            else {
                continue
            }
            collaborationAnchorIDByBatchIndex[batchIndex] = anchorID
            collaborationSummaryByBatchIndex[batchIndex] = summary
        }

        func appendActivityPhase(_ activities: [LiveFeedActivity]) {
            guard !activities.isEmpty else {
                return
            }

            var groupKind: CodexActivityGroupKind?
            var groupItems: [CodexActivityGroupItem] = []

            func flushGroup() {
                guard let groupKind, !groupItems.isEmpty else {
                    return
                }
                entries.append(
                    .activityGroup(
                        CodexActivityGroup(
                            kind: groupKind,
                            items: groupItems
                        )
                    )
                )
                groupItems.removeAll(keepingCapacity: true)
            }

            func appendGroupedItem(
                _ item: CodexActivityGroupItem,
                kind: CodexActivityGroupKind
            ) {
                if let groupKind, groupKind != kind {
                    flushGroup()
                }
                groupKind = kind
                groupItems.append(item)
            }

            for activity in activities {
                if isCollaborationActivity(activity) {
                    if
                        let batchIndex =
                            collaborationBatchIndexByActivityID[activity.id],
                        activity.id
                            == collaborationAnchorIDByBatchIndex[batchIndex],
                        let collaborationSummary =
                            collaborationSummaryByBatchIndex[batchIndex]
                    {
                        appendGroupedItem(
                            .collaboration(
                                activity: activity,
                                summary: collaborationSummary
                            ),
                            kind: .collaboration
                        )
                    }
                } else if
                    CodexFileChangeSummary.isFileChange(activity),
                    let summary = CodexFileChangeSummary.make(
                        from: [activity]
                    )
                {
                    appendGroupedItem(
                        .changes(activity: activity, summary: summary),
                        kind: .changes
                    )
                } else {
                    appendGroupedItem(
                        .activity(activity),
                        kind: activity.kind == "thinking" ? .reasoning : .work
                    )
                }
            }

            flushGroup()
        }

        var phaseActivities: [LiveFeedActivity] = []

        for activity in visibleActivities {
            let activityMessageID = "activity:\(activity.id)"
            if activityMessageID == deferredResponseMessageID {
                // 실행 중에는 마지막 공개 메시지를 잠시 보류한다. 다음
                // 메시지가 오면 진행문으로 확정되어 이 자리에 들어오고,
                // 완료되면 맨 아래의 정식 응답으로 한 번만 표시된다.
                continue
            }

            if activity.kind == "message" {
                // 공개 진행문은 접힌 작업 기록에 숨기지 않는다. 직전 작업을
                // 먼저 닫고 원래 시간 위치의 대화 메시지로 남긴다. 메시지
                // 뒤의 협업은 별도 카드가 되어 이전 협업과 섞이지 않는다.
                appendActivityPhase(phaseActivities)
                phaseActivities.removeAll(keepingCapacity: true)
                entries.append(.message(transcriptMessage(from: activity)))
                continue
            }

            phaseActivities.append(activity)
        }

        appendActivityPhase(phaseActivities)
        if let deferredResponseMessage {
            entries.append(.message(deferredResponseMessage))
        }

        return CodexTranscriptPresentation(
            entries: entries,
            // 작업 기록은 이미 일어난 일이고, `생각 중`은 현재 실행 상태다.
            // 둘을 함께 보여 사용자가 완료 전임을 항상 알 수 있게 한다.
            showsWaiting: isRunning,
            deferredResponseMessageID: deferredResponseMessageID,
            conclusionMessageID: conclusionMessageID
        )
    }

    private static func generatedImagePreviewOwnerID(
        response: String,
        preview: String,
        activities: [LiveFeedActivity]
    ) -> String? {
        guard response.hasSuffix(preview) else {
            return nil
        }
        let responseBeforePreview = String(
            response.dropLast(preview.count)
        )
            .trimmingCharacters(in: .whitespacesAndNewlines)
        for activity in activities.reversed()
        where activity.kind == "message" {
            if
                responseBeforePreview == activity.text
                    || responseBeforePreview.hasSuffix(
                        "\n\n" + activity.text
                    )
            {
                return "activity:\(activity.id)"
            }
        }
        return nil
    }

}

enum CodexTranscriptEntry: Identifiable, Equatable {
    case activityGroup(CodexActivityGroup)
    case message(CodexTranscriptMessage)

    var id: String {
        switch self {
        case .activityGroup(let group):
            group.id
        case .message(let message):
            message.id
        }
    }
}

enum CodexActivityGroupKind: String, Equatable, Hashable {
    case work
    case reasoning
    case command
    case tool
    case changes
    case collaboration
    case other

    init(activity: LiveFeedActivity) {
        switch activity.kind {
        case "thinking":
            self = .reasoning
        case "command":
            self = .command
        case "collaboration":
            self = .collaboration
        case "tool":
            self = activity.text.hasPrefix("협업 ·")
                ? .collaboration
                : .tool
        default:
            self = .other
        }
    }
}

enum CodexActivityGroupItem: Identifiable, Equatable {
    case activity(LiveFeedActivity)
    case changes(
        activity: LiveFeedActivity,
        summary: CodexFileChangeSummary
    )
    case collaboration(
        activity: LiveFeedActivity,
        summary: CodexCollaborationSummary
    )

    var id: String {
        switch self {
        case .activity(let activity),
            .changes(let activity, _),
            .collaboration(let activity, _):
            activity.id
        }
    }

    var activity: LiveFeedActivity {
        switch self {
        case .activity(let activity),
            .changes(let activity, _),
            .collaboration(let activity, _):
            activity
        }
    }

    var changeSummary: CodexFileChangeSummary? {
        guard case .changes(_, let summary) = self else {
            return nil
        }
        return summary
    }

    var collaborationSummary: CodexCollaborationSummary? {
        guard case .collaboration(_, let summary) = self else {
            return nil
        }
        return summary
    }

    var isRunning: Bool {
        switch self {
        case .collaboration(_, let summary):
            summary.isRunning
        case .activity(let activity), .changes(let activity, _):
            activity.status == .running
        }
    }
}

struct CodexActivityGroup: Identifiable, Equatable {
    let kind: CodexActivityGroupKind
    let items: [CodexActivityGroupItem]

    var id: String {
        // 같은 종류의 작업 카드가 공개 메시지 앞뒤에 여러 번 생길 수 있다.
        // 첫 항목을 앵커로 삼아 각 카드의 펼침 상태와 SwiftUI identity를
        // 안정적으로 분리한다.
        "activity-group:\(kind.rawValue):\(items.first?.id ?? "empty")"
    }

    var isRunning: Bool {
        items.contains(where: \.isRunning)
    }

    var latestItem: CodexActivityGroupItem? {
        items.last
    }

    func visibleHistoryItems(
        showsAll: Bool,
        limit: Int
    ) -> [CodexActivityGroupItem] {
        let history = items.dropLast()
        guard !showsAll, history.count > limit else {
            return Array(history)
        }
        return Array(history.suffix(limit))
    }

    func hiddenHistoryItemCount(limit: Int) -> Int {
        max(0, items.count - 1 - limit)
    }
}

struct CodexCollaborationAgentSummary: Identifiable, Equatable {
    let id: String
    let label: String?
    let prompt: String?
    let followUps: [String]
    let result: String?
    let status: LiveFeedActivityStatus
    let updatedAt: Date

    var previewText: String {
        result ?? followUps.last ?? prompt ?? "협업 검토를 진행했습니다."
    }
}

struct CodexCollaborationLatestEvent: Equatable {
    let agentID: String
    let text: String
    let status: LiveFeedActivityStatus
    let occurredAt: Date
}

struct CodexCollaborationSummary: Equatable {
    let activityIDs: [String]
    let agents: [CodexCollaborationAgentSummary]
    let latestEvent: CodexCollaborationLatestEvent?

    var completedCount: Int {
        agents.filter { $0.status == .completed }.count
    }

    var failedCount: Int {
        agents.filter { $0.status == .failed }.count
    }

    var runningCount: Int {
        agents.filter { $0.status == .running }.count
    }

    var finishedCount: Int {
        completedCount + failedCount
    }

    var isRunning: Bool {
        runningCount > 0
    }

    var progress: Double {
        guard !agents.isEmpty else {
            return 0
        }
        return Double(finishedCount) / Double(agents.count)
    }

    var statusText: String {
        guard !agents.isEmpty else {
            return OfficeLocalization.string("협업 기록")
        }
        if failedCount > 0 {
            return OfficeLocalization.format("%d명 · %d명 완료 · %d명 오류", agents.count, completedCount, failedCount)
        }
        if runningCount > 0 {
            return OfficeLocalization.format("%d명 · %d/%d 완료", agents.count, completedCount, agents.count)
        }
        return OfficeLocalization.format("%d명 검토 완료", agents.count)
    }

    func displayLabel(for agentID: String) -> String {
        guard let index = agents.firstIndex(where: { $0.id == agentID }) else {
            return OfficeLocalization.string("검토자")
        }
        return agents[index].label ?? OfficeLocalization.format("검토자 %d", index + 1)
    }

    static func isHiddenHousekeeping(_ activity: LiveFeedActivity) -> Bool {
        guard activity.kind == "tool" else {
            return false
        }
        let value = activity.text
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .lowercased()
        return [
            "협업 · wait",
            "협업 · wait_agent",
            "협업 · list_agents",
            "협업 · close_agent",
            "협업 · closeagent",
        ].contains(value)
    }

    static func make(
        from items: [CodexActivityGroupItem]
    ) -> CodexCollaborationSummary? {
        struct Builder {
            let id: String
            var label: String?
            var prompt: String?
            var followUps: [String] = []
            var result: String?
            var status: LiveFeedActivityStatus
            var updatedAt: Date
        }

        var order: [String] = []
        var builders: [String: Builder] = [:]
        var latestEvent: CodexCollaborationLatestEvent?
        var activityIDs: [String] = []

        for item in items {
            let activity = item.activity
            guard !isHiddenHousekeeping(activity) else {
                continue
            }
            let detail = activity.collaboration
            let agentID = detail?.agentThreadID ?? activity.id
            if builders[agentID] == nil {
                order.append(agentID)
                builders[agentID] = Builder(
                    id: agentID,
                    label: detail?.agentLabel,
                    prompt: nil,
                    status: resolvedStatus(
                        detail?.agentStatus,
                        fallback: activity.status
                    ),
                    updatedAt: activity.occurredAt
                )
            }
            guard var builder = builders[agentID] else {
                continue
            }

            if let label = nonempty(detail?.agentLabel) {
                builder.label = label
            }
            let action = detail?.action.lowercased() ?? "legacy"
            if let prompt = nonempty(detail?.prompt) {
                if action == "follow_up" {
                    if builder.followUps.last != prompt {
                        builder.followUps.append(prompt)
                    }
                } else if builder.prompt == nil {
                    builder.prompt = prompt
                }
            }
            if let result = nonempty(detail?.message) {
                builder.result = result
            } else if detail == nil {
                builder.result = nonempty(activity.text)
            }
            builder.status = resolvedStatus(
                detail?.agentStatus,
                fallback: activity.status
            )
            builder.updatedAt = activity.occurredAt
            builders[agentID] = builder
            activityIDs.append(activity.id)

            if let preview = nonempty(
                detail?.message ?? detail?.prompt ?? activity.text
            ) {
                latestEvent = CodexCollaborationLatestEvent(
                    agentID: agentID,
                    text: preview,
                    status: builder.status,
                    occurredAt: activity.occurredAt
                )
            }
        }

        let agents = order.compactMap { id -> CodexCollaborationAgentSummary? in
            guard let builder = builders[id] else {
                return nil
            }
            return CodexCollaborationAgentSummary(
                id: builder.id,
                label: builder.label,
                prompt: builder.prompt,
                followUps: builder.followUps,
                result: builder.result,
                status: builder.status,
                updatedAt: builder.updatedAt
            )
        }
        guard !agents.isEmpty else {
            return nil
        }
        return CodexCollaborationSummary(
            activityIDs: activityIDs,
            agents: agents,
            latestEvent: latestEvent
        )
    }

    private static func resolvedStatus(
        _ agentStatus: String?,
        fallback: LiveFeedActivityStatus
    ) -> LiveFeedActivityStatus {
        switch agentStatus?
            .replacingOccurrences(of: "-", with: "_")
            .lowercased()
        {
        case "pending_init", "running":
            return .running
        case "completed", "shutdown":
            return .completed
        case "interrupted", "errored", "not_found":
            return .failed
        default:
            return fallback
        }
    }

    private static func nonempty(_ value: String?) -> String? {
        let text = value?.trimmingCharacters(in: .whitespacesAndNewlines)
        return text?.isEmpty == false ? text : nil
    }
}

struct CodexTranscriptMessage: Identifiable, Equatable {
    let id: String
    let text: String
    let occurredAt: Date
}

enum CodexResponseDisplayMode: Equatable {
    case deferred
    case typing
    case committed
    case `static`
}

enum CodexResponseDisplayPolicy {
    static func mode(
        isDeferredResponseCandidate: Bool,
        isRunning: Bool,
        isCurrentRevisionPresented: Bool,
        animatesResponse: Bool
    ) -> CodexResponseDisplayMode {
        guard isDeferredResponseCandidate else {
            return .static
        }
        if isRunning {
            return .deferred
        }
        if isCurrentRevisionPresented {
            return .committed
        }
        guard animatesResponse else {
            return .static
        }
        return .typing
    }

    static func showsInlineQuestionAnswer(
        needsInput: Bool,
        backend: AgentBackend,
        animatesResponse: Bool
    ) -> Bool {
        guard needsInput else {
            return false
        }
        return backend != .codex || !animatesResponse
    }
}

private struct CodexResponsePresentationRevision: Equatable {
    let messageID: String
    let text: String

    init(message: CodexTranscriptMessage) {
        messageID = message.id
        text = message.text
    }
}

struct CodexFileChangeSummary: Equatable {
    let activityIDs: [String]
    let files: [String]
    let reportedFileCount: Int?
    let additions: Int?
    let deletions: Int?
    let status: LiveFeedActivityStatus

    var id: String {
        "changes:" + activityIDs.joined(separator: ",")
    }

    var title: String {
        if status == .running {
            if let reportedFileCount {
                return OfficeLocalization.format(
                    "파일 %d개를 편집하는 중",
                    reportedFileCount
                )
            }
            return OfficeLocalization.string("파일 변경을 적용하는 중")
        }
        if status == .failed {
            if let reportedFileCount {
                return OfficeLocalization.format(
                    "파일 %d개 변경에 실패했습니다",
                    reportedFileCount
                )
            }
            return OfficeLocalization.string("파일 변경에 실패했습니다")
        }
        if let reportedFileCount {
            return OfficeLocalization.format(
                "파일 %d개를 편집했습니다",
                reportedFileCount
            )
        }
        return files.isEmpty
            ? OfficeLocalization.string("파일 변경을 반영했습니다")
            : OfficeLocalization.string("파일 변경 결과")
    }

    var copyText: String {
        var lines = [title]
        if let additions, let deletions {
            lines.append("+\(additions) -\(deletions)")
        }
        lines.append(contentsOf: files)
        return lines.joined(separator: "\n")
    }

    static func isFileChange(_ activity: LiveFeedActivity) -> Bool {
        guard activity.kind == "tool" else {
            return false
        }
        let firstLine = activity.text.split(
            separator: "\n",
            maxSplits: 1,
            omittingEmptySubsequences: true
        ).first.map(String.init) ?? ""
        return firstLine == "파일 변경 완료"
            || firstLine == "파일 변경을 반영했습니다."
            || isRunningTitle(firstLine)
            || firstLine.hasPrefix("파일 · ")
            || reportedCount(from: firstLine) != nil
    }

    static func make(
        from activities: [LiveFeedActivity]
    ) -> CodexFileChangeSummary? {
        guard !activities.isEmpty else {
            return nil
        }

        var files: [String] = []
        var seenFiles = Set<String>()
        var additions = 0
        var deletions = 0
        var hasCompleteStatistics = true
        var declaredFileCount: Int?
        var hasTruncatedFiles = false

        for activity in activities {
            var activityHasStatistics = false
            let lines = activity.text
                .split(separator: "\n", omittingEmptySubsequences: true)
                .map(String.init)
            guard !lines.isEmpty else {
                hasCompleteStatistics = false
                continue
            }

            if activity.text.hasPrefix("파일 · ") {
                let legacy = legacyFiles(
                    from: String(activity.text.dropFirst("파일 · ".count))
                )
                for file in legacy.files {
                    appendUnique(file, to: &files, seen: &seenFiles)
                }
                if legacy.remainder > 0 {
                    hasTruncatedFiles = true
                    if activities.count == 1 {
                        declaredFileCount = legacy.files.count + legacy.remainder
                    }
                }
                hasCompleteStatistics = false
                continue
            }

            for (index, line) in lines.enumerated() {
                if index == 0 {
                    if let count = reportedCount(from: line) {
                        if activities.count == 1 {
                            declaredFileCount = count
                        }
                        continue
                    }
                    if isRunningTitle(line) {
                        if
                            activities.count == 1,
                            let count = runningCount(from: line)
                        {
                            declaredFileCount = count
                        }
                        continue
                    }
                }
                if let values = statistics(from: line) {
                    additions += values.additions
                    deletions += values.deletions
                    activityHasStatistics = true
                    continue
                }
                if line.hasPrefix("외 ") {
                    hasTruncatedFiles = true
                } else if
                    line != "파일 변경 완료",
                    line != "파일 변경을 반영했습니다."
                {
                    appendUnique(line, to: &files, seen: &seenFiles)
                }
            }
            if !activityHasStatistics {
                hasCompleteStatistics = false
            }
        }

        let exactFileCount: Int?
        if activities.count == 1, let declaredFileCount {
            exactFileCount = declaredFileCount
        } else if !hasTruncatedFiles, !files.isEmpty {
            exactFileCount = files.count
        } else {
            exactFileCount = nil
        }

        let status: LiveFeedActivityStatus
        if activities.contains(where: { $0.status == .failed }) {
            status = .failed
        } else if activities.contains(where: { $0.status == .running }) {
            status = .running
        } else {
            status = .completed
        }

        return CodexFileChangeSummary(
            activityIDs: activities.map(\.id),
            files: files,
            reportedFileCount: exactFileCount,
            additions: hasCompleteStatistics ? additions : nil,
            deletions: hasCompleteStatistics ? deletions : nil,
            status: status
        )
    }

    private static func appendUnique(
        _ value: String,
        to values: inout [String],
        seen: inout Set<String>
    ) {
        let key = fileIdentity(value)
        guard !value.isEmpty, seen.insert(key).inserted else {
            return
        }
        values.append(value)
    }

    static func filePath(from value: String) -> String? {
        for prefix in ["추가 ", "수정 ", "삭제 ", "이동 "]
        where value.hasPrefix(prefix)
        {
            let path = String(value.dropFirst(prefix.count))
            return path.isEmpty ? nil : path
        }
        return value.isEmpty ? nil : value
    }

    private static func fileIdentity(_ value: String) -> String {
        filePath(from: value) ?? value
    }

    private static func legacyFiles(
        from value: String
    ) -> (files: [String], remainder: Int) {
        var body = value
        var remainder = 0
        if let marker = value.range(of: " 외 ", options: .backwards) {
            let suffix = value[marker.upperBound...]
            if suffix.hasSuffix("개"), let count = Int(suffix.dropLast()) {
                body = String(value[..<marker.lowerBound])
                remainder = count
            }
        }
        return (
            body.components(separatedBy: ", ").filter { !$0.isEmpty },
            remainder
        )
    }

    private static func statistics(
        from line: String
    ) -> (additions: Int, deletions: Int)? {
        let parts = line.split(separator: " ")
        guard
            parts.count == 2,
            parts[0].hasPrefix("+"),
            parts[1].hasPrefix("-"),
            let additions = Int(parts[0].dropFirst()),
            let deletions = Int(parts[1].dropFirst())
        else {
            return nil
        }
        return (additions, deletions)
    }

    private static func reportedCount(from line: String) -> Int? {
        let prefix = "파일 "
        let suffix = "개를 편집했습니다"
        guard line.hasPrefix(prefix), line.hasSuffix(suffix) else {
            return nil
        }
        return Int(
            line
                .dropFirst(prefix.count)
                .dropLast(suffix.count)
        )
    }

    private static func isRunningTitle(_ line: String) -> Bool {
        line == "파일 변경을 적용하는 중"
            || runningCount(from: line) != nil
    }

    private static func runningCount(from line: String) -> Int? {
        let prefix = "파일 변경을 적용하는 중 · "
        let suffix = "개"
        guard line.hasPrefix(prefix), line.hasSuffix(suffix) else {
            return nil
        }
        return Int(
            line
                .dropFirst(prefix.count)
                .dropLast(suffix.count)
        )
    }
}

struct CodexTranscriptView: View {
    let turnID: String
    let workspaceDirectory: String
    let activities: [LiveFeedActivity]
    let response: String
    let responseUpdatedAt: Date
    let isRunning: Bool
    let isCompleted: Bool
    let needsInput: Bool
    let animatesResponse: Bool
    let animatesInitialResponse: Bool
    let responseFeedback: TurnResponseFeedback?
    let updateResponseFeedback: (TurnResponseFeedback?) async -> Void
    let onResponsePresented: () -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var showsAllEntries = false
    @State private var presentedResponseRevision:
        CodexResponsePresentationRevision?

    private static let compactEntryLimit = 18

    var body: some View {
        let presentation = TranscriptPresentationCache.shared.presentation(
            provider: .codex,
            turnID: turnID,
            activities: activities,
            response: response,
            responseUpdatedAt: responseUpdatedAt,
            isRunning: isRunning,
            isCompleted: isCompleted
        ) {
            CodexTranscriptPresentation.make(
                turnID: turnID,
                activities: activities,
                response: response,
                responseUpdatedAt: responseUpdatedAt,
                isRunning: isRunning,
                isCompleted: isCompleted
            )
        }
        let hiddenCount = max(
            0,
            presentation.entries.count - Self.compactEntryLimit
        )
        let visibleEntries = presentation.visibleEntries(
            showsAll: showsAllEntries,
            compactLimit: Self.compactEntryLimit
        )
        let latestMessage = presentation.latestMessage
        let deferredResponseMessage = presentation.deferredResponseMessage
        let deferredResponseRevision = deferredResponseMessage.map(
            CodexResponsePresentationRevision.init(message:)
        )
        let conclusionMessageID = isCompleted
            ? presentation.conclusionMessageID ?? latestMessage?.id
            : nil

        VStack(alignment: .leading, spacing: 14) {
            if hiddenCount > 0, !showsAllEntries {
                Button {
                    withAnimation(.easeInOut(duration: 0.16)) {
                        showsAllEntries = true
                    }
                } label: {
                    Label(
                        OfficeLocalization.format("이전 기록 %d개 보기", hiddenCount),
                        systemImage: "clock.arrow.circlepath"
                    )
                    .font(.system(size: 10.5, weight: .semibold))
                    .foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
            }

            ForEach(visibleEntries) { entry in
                transcriptEntry(
                    entry,
                    isConclusion: entry.id == conclusionMessageID,
                    isDeferredResponseCandidate:
                        entry.id == presentation.deferredResponseMessageID
                )
            }

            if presentation.showsWaiting {
                CodexWaitingView()
                    .transition(
                        reduceMotion
                            ? .opacity
                            : .opacity.combined(with: .scale(scale: 0.98))
                    )
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .animation(
            reduceMotion ? nil : .easeOut(duration: 0.18),
            value: presentation.showsWaiting
        )
        .onChange(of: isRunning) { _, running in
            if
                animatesResponse,
                !running,
                (
                    deferredResponseRevision == nil
                        || presentedResponseRevision
                            == deferredResponseRevision
                )
            {
                onResponsePresented()
            }
        }
        .onChange(of: presentedResponseRevision) { _, revision in
            if
                animatesResponse,
                !isRunning,
                revision == deferredResponseRevision
            {
                onResponsePresented()
            }
        }
        .task(id: isCompleted) {
            if
                animatesResponse,
                isCompleted,
                deferredResponseRevision == nil
            {
                onResponsePresented()
            }
        }
        .onDisappear {
            if animatesResponse, !isRunning {
                onResponsePresented()
            }
        }
    }

    @ViewBuilder
    private func transcriptEntry(
        _ entry: CodexTranscriptEntry,
        isConclusion: Bool,
        isDeferredResponseCandidate: Bool
    ) -> some View {
        switch entry {
        case .activityGroup(let group):
            if
                group.kind == .collaboration,
                let summary = group.items
                    .compactMap(\.collaborationSummary)
                    .last
                    ?? CodexCollaborationSummary.make(from: group.items)
            {
                CodexCollaborationGroupView(
                    summary: summary,
                    workspaceDirectory: workspaceDirectory
                )
            } else if
                group.kind == .changes,
                let summary = CodexFileChangeSummary.make(
                    from: group.items.map(\.activity)
                )
            {
                // 파일 변경은 일반 작업 카드 안에 다시 카드를 중첩하지
                // 않고, 합산된 전용 카드 하나로 바로 표시한다.
                CodexFileChangeSummaryView(
                    summary: summary,
                    workspaceDirectory: workspaceDirectory
                )
            } else {
                CodexActivityGroupView(
                    group: group,
                    workspaceDirectory: workspaceDirectory
                )
                .equatable()
            }
        case .message(let message):
            let revision = CodexResponsePresentationRevision(
                message: message
            )
            let displayMode = CodexResponseDisplayPolicy.mode(
                isDeferredResponseCandidate:
                    isDeferredResponseCandidate,
                isRunning: isRunning,
                isCurrentRevisionPresented:
                    presentedResponseRevision == revision,
                animatesResponse: animatesResponse
            )
            if displayMode != .deferred {
                CodexMessageView(
                    turnID: turnID,
                    workspaceDirectory: workspaceDirectory,
                    message: message,
                    isConclusion: isConclusion,
                    needsInput: isConclusion && needsInput,
                    responseDisplayMode: displayMode,
                    typingIdentity: "\(turnID):\(message.id)",
                    animatesResponse: animatesResponse,
                    animatesInitialResponse: animatesInitialResponse,
                    responseFeedback: responseFeedback,
                    updateResponseFeedback: updateResponseFeedback,
                    onResponsePresented: {
                        guard isDeferredResponseCandidate else {
                            return
                        }
                        presentedResponseRevision = revision
                    }
                )
            }
        }
    }
}

private struct CodexWaitingView: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(spacing: 9) {
            CoreAnimationDotsView(
                dotSize: 4,
                spacing: 2.5,
                travel: 2.5,
                color: NSColor(
                    calibratedRed: 0.13,
                    green: 0.55,
                    blue: 0.52,
                    alpha: 1
                ),
                isAnimated: !reduceMotion
            )
            .frame(width: 20, height: 16)
            .accessibilityHidden(true)

            Text(OfficeLocalization.string("생각 중"))
                .font(.system(size: 12.5, weight: .semibold))
                .foregroundStyle(.secondary)

            Spacer(minLength: 0)
        }
        .padding(.horizontal, 10)
        .frame(height: 36)
        .background(
            DashboardPalette.accent.opacity(0.055),
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .accessibilityElement(children: .combine)
        .accessibilityLabel(OfficeLocalization.string("생각 중"))
    }
}

/// 실행 상태 변화만으로는 열지 않고 완료 전환에서만 닫는다.
func transcriptGroupExpansionState(
    current: Bool,
    isRunning: Bool
) -> Bool {
    isRunning ? current : false
}

private struct CodexCollaborationGroupView: View {
    let summary: CodexCollaborationSummary
    let workspaceDirectory: String

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var isExpanded = false
    @State private var expandedResultIDs: Set<String> = []

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Button {
                withAnimation(
                    reduceMotion ? nil : .easeInOut(duration: 0.18)
                ) {
                    isExpanded.toggle()
                }
            } label: {
                header
            }
            .buttonStyle(.plain)
            .accessibilityLabel(
                isExpanded
                    ? OfficeLocalization.string("협업 검토 접기")
                    : OfficeLocalization.string("협업 검토 자세히 보기")
            )

            progressBar

            if isExpanded {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(
                        Array(summary.agents.enumerated()),
                        id: \.element.id
                    ) { index, agent in
                        CodexCollaborationAgentView(
                            agent: agent,
                            index: index,
                            workspaceDirectory: workspaceDirectory,
                            isResultExpanded: resultBinding(for: agent.id)
                        )

                        if index < summary.agents.count - 1 {
                            Divider()
                                .opacity(0.55)
                                .padding(.leading, 33)
                        }
                    }
                }
                .transition(.opacity)
            } else if let latestEvent = summary.latestEvent {
                latestPreview(latestEvent)
                    .transition(.opacity)
            }
        }
        .padding(11)
        .background(
            LinearGradient(
                colors: [
                    Color.purple.opacity(0.075),
                    Color.primary.opacity(0.025),
                ],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            ),
            in: RoundedRectangle(cornerRadius: 12, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .stroke(Color.purple.opacity(0.15), lineWidth: 1)
        }
        .onChange(of: summary.isRunning) { _, running in
            guard !running else {
                return
            }
            withAnimation(
                reduceMotion ? nil : .easeInOut(duration: 0.16)
            ) {
                isExpanded = false
                expandedResultIDs.removeAll()
            }
        }
    }

    private var header: some View {
        HStack(spacing: 9) {
            Image(systemName: "person.2.fill")
                .font(.system(size: 12, weight: .bold))
                .foregroundStyle(Color.purple)
                .frame(width: 28, height: 28)
                .background(
                    Color.purple.opacity(0.11),
                    in: RoundedRectangle(cornerRadius: 8, style: .continuous)
                )

            VStack(alignment: .leading, spacing: 2) {
                Text(OfficeLocalization.string("협업 검토"))
                    .font(.system(size: 12.5, weight: .bold))
                    .foregroundStyle(.primary)

                Text(summary.statusText)
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(.secondary)
            }

            Spacer(minLength: 6)

            if summary.isRunning {
                ProgressView()
                    .controlSize(.mini)
                    .tint(.purple)
            }

            Image(systemName: "chevron.right")
                .font(.system(size: 9, weight: .bold))
                .foregroundStyle(.tertiary)
                .rotationEffect(.degrees(isExpanded ? 90 : 0))
        }
        .contentShape(Rectangle())
    }

    private var progressBar: some View {
        GeometryReader { geometry in
            ZStack(alignment: .leading) {
                Capsule()
                    .fill(Color.purple.opacity(0.10))
                Capsule()
                    .fill(
                        summary.failedCount > 0
                            ? Color.orange.opacity(0.82)
                            : Color.purple.opacity(0.78)
                    )
                    .frame(
                        width: max(
                            summary.progress > 0 ? 3 : 0,
                            geometry.size.width * summary.progress
                        )
                    )
            }
        }
        .frame(height: 3)
        .animation(
            reduceMotion ? nil : .easeOut(duration: 0.2),
            value: summary.progress
        )
        .accessibilityElement()
        .accessibilityLabel(summary.statusText)
    }

    private func latestPreview(
        _ latestEvent: CodexCollaborationLatestEvent
    ) -> some View {
        HStack(alignment: .top, spacing: 8) {
            collaborationStatusIcon(latestEvent.status)
                .frame(width: 15, height: 15)

            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(summary.displayLabel(for: latestEvent.agentID))
                        .font(.system(size: 10.5, weight: .bold))
                        .foregroundStyle(.secondary)

                    Text(
                        latestEvent.status == .running
                            ? OfficeLocalization.string("최근 요청")
                            : OfficeLocalization.string("최근 결과")
                    )
                        .font(.system(size: 8.5, weight: .bold))
                        .foregroundStyle(Color.purple)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 2)
                        .background(
                            Color.purple.opacity(0.09),
                            in: Capsule()
                        )
                }

                Text(collaborationPreview(latestEvent.text))
                    .font(.system(size: 11.5, weight: .medium))
                    .foregroundStyle(.secondary)
                    .lineLimit(3)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }

            Text(OfficeLocalization.date(latestEvent.occurredAt,
                dateStyle: .omitted,
                time: .standard
            ))
                .font(.system(size: 8.5, design: .monospaced))
                .foregroundStyle(.tertiary)
        }
        .padding(.horizontal, 2)
        .padding(.top, 1)
    }

    private func resultBinding(for id: String) -> Binding<Bool> {
        Binding(
            get: { expandedResultIDs.contains(id) },
            set: { expanded in
                if expanded {
                    expandedResultIDs.insert(id)
                } else {
                    expandedResultIDs.remove(id)
                }
            }
        )
    }
}

private struct CodexCollaborationAgentView: View {
    let agent: CodexCollaborationAgentSummary
    let index: Int
    let workspaceDirectory: String
    @Binding var isResultExpanded: Bool

    var body: some View {
        HStack(alignment: .top, spacing: 9) {
            Text("\(index + 1)")
                .font(.system(size: 9.5, weight: .bold, design: .rounded))
                .foregroundStyle(Color.purple)
                .frame(width: 24, height: 24)
                .background(
                    Color.purple.opacity(0.10),
                    in: Circle()
                )

            VStack(alignment: .leading, spacing: 7) {
                HStack(spacing: 7) {
                    Text(agent.label ?? OfficeLocalization.format("검토자 %d", index + 1))
                        .font(.system(size: 11.5, weight: .bold))

                    CollaborationStatusBadge(status: agent.status)

                    Spacer(minLength: 4)

                    Text(OfficeLocalization.date(agent.updatedAt,
                        dateStyle: .omitted,
                        time: .standard
                    ))
                        .font(.system(size: 8.5, design: .monospaced))
                        .foregroundStyle(.tertiary)
                }

                if let prompt = agent.prompt {
                    collaborationTextSection(
                        title: "요청",
                        text: prompt,
                        lineLimit: 5
                    )
                }

                if let followUp = agent.followUps.last {
                    collaborationTextSection(
                        title: agent.followUps.count > 1
                            ? OfficeLocalization.format("추가 요청 %d건", agent.followUps.count)
                            : OfficeLocalization.string("추가 요청"),
                        text: followUp,
                        lineLimit: 4
                    )
                }

                if let result = agent.result {
                    DisclosureGroup(isExpanded: $isResultExpanded) {
                        if isResultExpanded {
                            ConversationMarkdownView(
                                source: result,
                                fontSize: 11.5,
                                fileBaseDirectory: workspaceDirectory
                            )
                            .padding(.top, 7)
                        }
                    } label: {
                        VStack(alignment: .leading, spacing: 3) {
                            Text(
                                isResultExpanded
                                    ? OfficeLocalization.string("검토 결과 접기")
                                    : OfficeLocalization.string("검토 결과 자세히")
                            )
                                .font(.system(size: 9.5, weight: .bold))
                                .foregroundStyle(Color.purple)

                            if !isResultExpanded {
                                Text(collaborationPreview(result))
                                    .font(.system(size: 11, weight: .medium))
                                    .foregroundStyle(.secondary)
                                    .lineLimit(4)
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .contentShape(Rectangle())
                    }
                    .tint(.purple)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 9)
    }

    private func collaborationTextSection(
        title: String,
        text: String,
        lineLimit: Int
    ) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(OfficeLocalization.string(title))
                .font(.system(size: 9, weight: .bold))
                .foregroundStyle(.tertiary)
                .textCase(.uppercase)

            Text(text)
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(.secondary)
                .lineLimit(lineLimit)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

private struct CollaborationStatusBadge: View {
    let status: LiveFeedActivityStatus

    var body: some View {
        Text(OfficeLocalization.string(label))
            .font(.system(size: 8.5, weight: .bold))
            .foregroundStyle(color)
            .padding(.horizontal, 6)
            .padding(.vertical, 2.5)
            .background(color.opacity(0.10), in: Capsule())
    }

    private var label: String {
        switch status {
        case .running:
            "검토 중"
        case .completed:
            "완료"
        case .failed:
            "오류"
        }
    }

    private var color: Color {
        collaborationStatusColor(status)
    }
}

@ViewBuilder
private func collaborationStatusIcon(
    _ status: LiveFeedActivityStatus
) -> some View {
    if status == .running {
        ProgressView()
            .controlSize(.mini)
            .tint(.purple)
    } else {
        Image(
            systemName: status == .failed
                ? "exclamationmark.circle.fill"
                : "checkmark.circle.fill"
        )
        .font(.system(size: 12, weight: .semibold))
        .foregroundStyle(collaborationStatusColor(status))
    }
}

private func collaborationStatusColor(
    _ status: LiveFeedActivityStatus
) -> Color {
    switch status {
    case .running:
        .purple
    case .completed:
        .green
    case .failed:
        .red
    }
}

private func collaborationPreview(_ text: String) -> String {
    text
        .split(whereSeparator: \.isWhitespace)
        .joined(separator: " ")
}

private struct CodexActivityGroupView: View, Equatable {
    let group: CodexActivityGroup
    let workspaceDirectory: String

    @State private var isExpanded = false
    @State private var showsAllHistory = false

    private static let compactHistoryLimit = 20

    static func == (
        lhs: CodexActivityGroupView,
        rhs: CodexActivityGroupView
    ) -> Bool {
        lhs.group == rhs.group
            && lhs.workspaceDirectory == rhs.workspaceDirectory
    }

    var body: some View {
        let historyCount = max(0, group.items.count - 1)
        let hiddenCount = group.hiddenHistoryItemCount(
            limit: Self.compactHistoryLimit
        )
        VStack(alignment: .leading, spacing: 8) {
            groupHeader

            if historyCount > 0 {
                DisclosureGroup(isExpanded: $isExpanded) {
                    if isExpanded {
                        LazyVStack(alignment: .leading, spacing: 5) {
                            if hiddenCount > 0, !showsAllHistory {
                                Button {
                                    showsAllHistory = true
                                } label: {
                                    Label(
                                        OfficeLocalization.format("더 이전 기록 %d개 보기", hiddenCount),
                                        systemImage: "clock.arrow.circlepath"
                                    )
                                    .font(
                                        .system(
                                            size: 9.5,
                                            weight: .semibold
                                        )
                                    )
                                    .foregroundStyle(.secondary)
                                }
                                .buttonStyle(.plain)
                                .padding(.vertical, 5)
                            }

                            ForEach(visibleHistory) { item in
                                itemView(item, isLatest: false)
                            }
                        }
                        .padding(.top, 5)
                    }
                } label: {
                    Text(
                        isExpanded
                            ? OfficeLocalization.format("이전 %@ 숨기기", historyNoun)
                            : OfficeLocalization.format("이전 %@ %d개 보기", historyNoun, historyCount)
                    )
                        .font(.system(size: 9.5, weight: .semibold))
                        .foregroundStyle(.secondary)
                        .contentShape(Rectangle())
                }
                .tint(.secondary)
            }

            if let latestItem = group.latestItem {
                // 접었을 때는 최신 한 건만, 펼쳤을 때는 과거부터
                // 최신까지 위에서 아래로 자연스럽게 읽히게 한다.
                itemView(latestItem, isLatest: true)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 9)
        .background(
            Color.primary.opacity(0.035),
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(Color.primary.opacity(0.06))
        }
    }

    private var groupHeader: some View {
        HStack(spacing: 8) {
            Image(systemName: groupIcon)
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(group.isRunning ? groupColor : .secondary)
                .frame(width: 18)

            Text(groupTitle)
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(.secondary)

            Spacer(minLength: 6)

            Text(OfficeLocalization.format("%d개", group.items.count))
                .font(.system(size: 9, weight: .semibold))
                .foregroundStyle(.tertiary)
        }
    }

    @ViewBuilder
    private func itemView(
        _ item: CodexActivityGroupItem,
        isLatest: Bool
    ) -> some View {
        switch item {
        case .activity(let activity):
            activityRow(activity, isLatest: isLatest)
        case .changes(_, let summary):
            CodexFileChangeSummaryView(
                summary: summary,
                workspaceDirectory: workspaceDirectory
            )
        case .collaboration(_, let summary):
            CodexCollaborationGroupView(
                summary: summary,
                workspaceDirectory: workspaceDirectory
            )
        }
    }

    private func activityRow(
        _ activity: LiveFeedActivity,
        isLatest: Bool
    ) -> some View {
        HStack(alignment: .top, spacing: 8) {
            statusView(activity.status)
                .frame(width: 15, height: 15)

            Text(activity.displayText)
                .font(activityFont(for: activity))
                .foregroundStyle(.secondary)
                .lineLimit(
                    isLatest && activity.kind == "thinking" ? nil : 4
                )
                .frame(maxWidth: .infinity, alignment: .leading)

            Text(OfficeLocalization.date(activity.occurredAt,
                dateStyle: .omitted,
                time: .standard
            ))
                .font(.system(size: 8.5, design: .monospaced))
                .foregroundStyle(.tertiary)
        }
        .padding(.vertical, isLatest ? 4 : 3)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(
            "\(groupTitle), \(activity.displayText), "
                + OfficeLocalization.date(activity.occurredAt,
                    dateStyle: .omitted,
                    time: .standard
                )
        )
    }

    private var visibleHistory: [CodexActivityGroupItem] {
        group.visibleHistoryItems(
            showsAll: showsAllHistory,
            limit: Self.compactHistoryLimit
        )
    }

    @ViewBuilder
    private func statusView(
        _ status: LiveFeedActivityStatus
    ) -> some View {
        if status == .running {
            ProgressView()
                .controlSize(.mini)
                .tint(groupColor)
        } else {
            Image(
                systemName: status == .failed
                    ? "xmark.circle.fill"
                    : "checkmark.circle.fill"
            )
            .foregroundStyle(status == .failed ? Color.red : groupColor)
        }
    }

    private func activityFont(for activity: LiveFeedActivity) -> Font {
        switch activity.kind {
        case "thinking", "message":
            .system(size: 12.5, weight: .medium)
        default:
            .system(size: 10.5, design: .monospaced)
        }
    }

    private var groupTitle: String {
        switch group.kind {
        case .work:
            OfficeLocalization.string("작업 내역")
        case .reasoning:
            OfficeLocalization.string("추론")
        case .command:
            OfficeLocalization.string("명령 실행")
        case .tool:
            OfficeLocalization.string("도구 사용")
        case .changes:
            OfficeLocalization.string("파일 변경")
        case .collaboration:
            OfficeLocalization.string("협업 검토")
        case .other:
            OfficeLocalization.string("기타 작업")
        }
    }

    private var historyNoun: String {
        switch group.kind {
        case .work:
            OfficeLocalization.historyNoun("작업")
        case .reasoning:
            OfficeLocalization.historyNoun("추론")
        case .command:
            OfficeLocalization.historyNoun("명령")
        case .tool:
            OfficeLocalization.historyNoun("도구 사용")
        case .changes:
            OfficeLocalization.historyNoun("파일 변경")
        case .collaboration:
            OfficeLocalization.historyNoun("협업")
        case .other:
            OfficeLocalization.historyNoun("작업")
        }
    }

    private var groupIcon: String {
        switch group.kind {
        case .work:
            "list.bullet.rectangle"
        case .reasoning:
            "brain.head.profile"
        case .command:
            "terminal"
        case .tool:
            "wrench.and.screwdriver"
        case .changes:
            "doc.badge.gearshape"
        case .collaboration:
            "person.2.fill"
        case .other:
            "ellipsis.circle"
        }
    }

    private var groupColor: Color {
        switch group.kind {
        case .work:
            DashboardPalette.accent
        case .reasoning:
            DashboardPalette.accent
        case .command:
            .indigo
        case .tool:
            .orange
        case .changes:
            .green
        case .collaboration:
            .purple
        case .other:
            .secondary
        }
    }
}

private struct CodexMessageView: View {
    let turnID: String
    let workspaceDirectory: String
    let message: CodexTranscriptMessage
    let isConclusion: Bool
    let needsInput: Bool
    let responseDisplayMode: CodexResponseDisplayMode
    let typingIdentity: String
    let animatesResponse: Bool
    let animatesInitialResponse: Bool
    let responseFeedback: TurnResponseFeedback?
    let updateResponseFeedback: (TurnResponseFeedback?) async -> Void
    let onResponsePresented: () -> Void

    @State private var copied = false
    @State private var copyResetTask: Task<Void, Never>?

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            if isConclusion || needsInput {
                Label(
                    OfficeLocalization.string(headerTitle),
                    systemImage: headerIcon
                )
                .font(.system(size: 10.5, weight: .bold))
                .foregroundStyle(
                    needsInput ? Color.orange : DashboardPalette.accent
                )
            }

            if responseDisplayMode == .typing {
                CompletedResponseLineTypingView(
                    typingIdentity: typingIdentity,
                    source: message.text,
                    fontSize: 14,
                    fileBaseDirectory: workspaceDirectory,
                    compactsChangedFileLists: isConclusion,
                    animates: animatesResponse,
                    animatesInitialSource: animatesInitialResponse,
                    presentsTyping: true,
                    onFinishedTyping: onResponsePresented
                )
            } else {
                // 타자가 끝난 응답은 한 개의 Markdown 트리로 수렴시킨다.
                // 줄별 selectable 뷰를 남기면 직원 전환 직후 스크롤에서
                // SelectionOverlay가 대량 재배치될 수 있다.
                renderedMessage
            }

            ResponseMessageFooter(
                occurredAt: message.occurredAt,
                copied: copied,
                accentColor: DashboardPalette.accent,
                accessibilityID: "copyMessage-\(message.id)",
                showsFeedback: isConclusion && !needsInput,
                feedback: responseFeedback,
                feedbackAccessibilityIDPrefix: turnID,
                copy: copyMessage,
                feedbackChanged: updateResponseFeedback
            )
        }
        .padding(.vertical, 2)
        .onChange(of: message.text) { _, _ in
            copyResetTask?.cancel()
            copied = false
        }
        .onDisappear {
            copyResetTask?.cancel()
        }
    }

    private var headerTitle: String {
        if needsInput {
            return "답변 필요"
        }
        return "최종 응답"
    }

    private var headerIcon: String {
        if needsInput {
            return "questionmark.bubble.fill"
        }
        return "checkmark.bubble.fill"
    }

    private var renderedMessage: some View {
        ConversationMarkdownView(
            source: message.text,
            fontSize: 14,
            fileBaseDirectory: workspaceDirectory,
            compactsChangedFileLists: isConclusion
        )
    }

    private func copyMessage() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(message.text, forType: .string)
        copied = true

        copyResetTask?.cancel()
        copyResetTask = Task {
            try? await Task.sleep(for: .seconds(1.4))
            if !Task.isCancelled {
                copied = false
            }
        }
    }
}

private struct CodexFileChangeSummaryView: View {
    let summary: CodexFileChangeSummary
    let workspaceDirectory: String

    @State private var copied = false
    @State private var copyResetTask: Task<Void, Never>?

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 9) {
                Image(systemName: "doc.badge.gearshape")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(statusColor)
                    .frame(width: 28, height: 28)
                    .background(
                        statusColor.opacity(0.10),
                        in: RoundedRectangle(cornerRadius: 8)
                    )

                VStack(alignment: .leading, spacing: 2) {
                    Text(summary.title)
                        .font(.system(size: 12.5, weight: .bold))

                    if let additions = summary.additions,
                        let deletions = summary.deletions
                    {
                        HStack(spacing: 5) {
                            Text("+\(additions)")
                                .foregroundStyle(.green)
                            Text("-\(deletions)")
                                .foregroundStyle(.red)
                        }
                        .font(.system(size: 10.5, weight: .bold, design: .monospaced))
                    }
                }

                Spacer(minLength: 6)

                Button {
                    copySummary()
                } label: {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(
                            copied ? DashboardPalette.accent : Color.secondary
                        )
                }
                .buttonStyle(.plain)
                .help(
                    copied
                        ? OfficeLocalization.string("변경 결과 복사됨")
                        : OfficeLocalization.string("변경 결과 복사")
                )
                .accessibilityLabel(
                    copied
                        ? OfficeLocalization.string("변경 결과 복사됨")
                        : OfficeLocalization.string("변경 결과 복사")
                )
                .accessibilityIdentifier("copyChanges-\(summary.id)")
            }

            if !summary.files.isEmpty {
                VStack(alignment: .leading, spacing: 5) {
                    ForEach(Array(summary.files.enumerated()), id: \.offset) {
                        index, file in
                        WorkspaceFileRevealButton(
                            title: CodexFileChangeSummary.filePath(
                                from: file
                            ) ?? file,
                            path: CodexFileChangeSummary.filePath(from: file),
                            workspaceDirectory: workspaceDirectory,
                            foregroundColor: .secondary,
                            accessibilityIdentifier:
                                "revealChange-\(summary.id)-\(index)"
                        )
                    }
                }
                .padding(.top, 2)
            }
        }
        .padding(11)
        .background(
            Color.primary.opacity(0.035),
            in: RoundedRectangle(cornerRadius: 11, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .stroke(Color.primary.opacity(0.07))
        }
        .onChange(of: summary.copyText) { _, _ in
            copyResetTask?.cancel()
            copied = false
        }
        .onDisappear {
            copyResetTask?.cancel()
        }
    }

    private var statusColor: Color {
        switch summary.status {
        case .running:
            DashboardPalette.accent
        case .completed:
            .green
        case .failed:
            .red
        }
    }

    private func copySummary() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(summary.copyText, forType: .string)
        copied = true

        copyResetTask?.cancel()
        copyResetTask = Task {
            try? await Task.sleep(for: .seconds(1.4))
            if !Task.isCancelled {
                copied = false
            }
        }
    }
}

struct WorkspaceFileRevealTarget: Equatable {
    let url: URL
    let selectsItem: Bool

    static func resolve(
        path: String,
        workspaceDirectory: String,
        fileManager: FileManager = .default
    ) -> WorkspaceFileRevealTarget? {
        guard let requestedURL = fileURL(
            path: path,
            workspaceDirectory: workspaceDirectory
        ) else {
            return nil
        }
        if fileManager.fileExists(atPath: requestedURL.path) {
            return WorkspaceFileRevealTarget(
                url: requestedURL,
                selectsItem: true
            )
        }
        let compactedPath = compactedRelativePath(from: path)
        if
            let compactedPath,
            let matchedURL = existingLegacyWorktreeFileURL(
                relativePath: compactedPath,
                workspaceDirectory: workspaceDirectory,
                fileManager: fileManager
            )
        {
            return WorkspaceFileRevealTarget(
                url: matchedURL,
                selectsItem: true
            )
        }

        let compactedWorkspaceURL: URL?
        if compactedPath != nil {
            let workspaceURL = URL(
                fileURLWithPath: workspaceDirectory,
                isDirectory: true
            ).standardizedFileURL
            var isDirectory = ObjCBool(false)
            guard
                fileManager.fileExists(
                    atPath: workspaceURL.path,
                    isDirectory: &isDirectory
                ),
                isDirectory.boolValue
            else {
                return nil
            }
            compactedWorkspaceURL = workspaceURL
        } else {
            compactedWorkspaceURL = nil
        }

        var ancestor = requestedURL.deletingLastPathComponent()
        while true {
            var isDirectory = ObjCBool(false)
            if
                fileManager.fileExists(
                    atPath: ancestor.path,
                    isDirectory: &isDirectory
                ),
                isDirectory.boolValue
            {
                return WorkspaceFileRevealTarget(
                    url: ancestor,
                    selectsItem: false
                )
            }
            if ancestor == compactedWorkspaceURL {
                return nil
            }
            let parent = ancestor.deletingLastPathComponent()
            guard parent.path != ancestor.path else {
                return nil
            }
            ancestor = parent
        }
    }

    static func fileURL(
        path: String,
        workspaceDirectory: String
    ) -> URL? {
        let trimmed = path.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            return nil
        }
        if let compactedPath = compactedRelativePath(from: trimmed) {
            guard !workspaceDirectory.isEmpty else {
                return nil
            }
            return URL(
                fileURLWithPath: workspaceDirectory,
                isDirectory: true
            )
                .appendingPathComponent(compactedPath)
                .standardizedFileURL
        }
        guard
            !trimmed.hasPrefix("…"),
            !trimmed.hasPrefix("...")
        else {
            return nil
        }
        if let url = URL(string: trimmed), url.isFileURL {
            return url.standardizedFileURL
        }

        let expanded = NSString(string: trimmed).expandingTildeInPath
        if expanded.hasPrefix("/") {
            return URL(fileURLWithPath: expanded).standardizedFileURL
        }
        guard !workspaceDirectory.isEmpty else {
            return nil
        }
        return URL(
            fileURLWithPath: workspaceDirectory,
            isDirectory: true
        )
            .appendingPathComponent(expanded)
            .standardizedFileURL
    }

    private static func compactedRelativePath(from path: String) -> String? {
        let trimmed = path.trimmingCharacters(in: .whitespacesAndNewlines)
        let prefix: String
        if trimmed.hasPrefix("…/") {
            prefix = "…/"
        } else if trimmed.hasPrefix(".../") {
            prefix = ".../"
        } else {
            return nil
        }

        let relativePath = String(trimmed.dropFirst(prefix.count))
        let components = relativePath.split(
            separator: "/",
            omittingEmptySubsequences: false
        )
        guard
            !components.isEmpty,
            components.allSatisfy({
                !$0.isEmpty && $0 != "." && $0 != ".."
            })
        else {
            return nil
        }
        return components.joined(separator: "/")
    }

    private static func existingLegacyWorktreeFileURL(
        relativePath: String,
        workspaceDirectory: String,
        fileManager: FileManager
    ) -> URL? {
        guard !workspaceDirectory.isEmpty else {
            return nil
        }
        let workspaceURL = URL(
            fileURLWithPath: workspaceDirectory,
            isDirectory: true
        ).standardizedFileURL
        let components = relativePath.split(separator: "/").map(String.init)
        let repositoryRelativeComponents: ArraySlice<String>
        if
            components.count >= 3,
            isRepositoryWorkspaceIdentifier(components[0]),
            hasUUIDSuffix(components[1])
        {
            repositoryRelativeComponents = components.dropFirst(2)
        } else if components.count >= 2, hasUUIDSuffix(components[0]) {
            repositoryRelativeComponents = components.dropFirst()
        } else {
            return nil
        }

        let candidate = workspaceURL
            .appendingPathComponent(
                repositoryRelativeComponents.joined(separator: "/")
            )
            .standardizedFileURL
        return fileManager.fileExists(atPath: candidate.path)
            ? candidate
            : nil
    }

    private static func isRepositoryWorkspaceIdentifier(_ value: String) -> Bool {
        value.range(
            of: #"^[0-9a-fA-F]{8,64}$"#,
            options: .regularExpression
        ) != nil
    }

    private static func hasUUIDSuffix(_ value: String) -> Bool {
        guard value.count > 37 else {
            return false
        }
        let separatorIndex = value.index(value.endIndex, offsetBy: -37)
        return value[separatorIndex] == "-" &&
            UUID(uuidString: String(value.suffix(36))) != nil
    }
}

struct WorkspaceFileRevealButton: View {
    let title: String
    let path: String?
    let workspaceDirectory: String
    let foregroundColor: Color
    let accessibilityIdentifier: String

    var body: some View {
        if let path,
            WorkspaceFileRevealTarget.fileURL(
                path: path,
                workspaceDirectory: workspaceDirectory
            ) != nil
        {
            Button {
                reveal(path)
            } label: {
                HStack(spacing: 5) {
                    Text(title)
                        .font(.system(size: 10.5, design: .monospaced))
                        .foregroundStyle(foregroundColor)
                        .lineLimit(1)
                        .truncationMode(.middle)

                    Image(systemName: "arrow.up.forward.square")
                        .font(.system(size: 8.5, weight: .semibold))
                        .foregroundStyle(Color.accentColor)
                        .accessibilityHidden(true)
                }
                .contentShape(Rectangle())
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            .help(OfficeLocalization.string("Finder에서 보기"))
            .accessibilityLabel(OfficeLocalization.format("%@, Finder에서 보기", title))
            .accessibilityIdentifier(accessibilityIdentifier)
        } else {
            Text(title)
                .font(.system(size: 10.5, design: .monospaced))
                .foregroundStyle(foregroundColor)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func reveal(_ path: String) {
        guard let target = WorkspaceFileRevealTarget.resolve(
            path: path,
            workspaceDirectory: workspaceDirectory
        ) else {
            return
        }
        if target.selectsItem {
            NSWorkspace.shared.activateFileViewerSelecting([target.url])
        } else {
            _ = NSWorkspace.shared.open(target.url)
        }
    }
}
