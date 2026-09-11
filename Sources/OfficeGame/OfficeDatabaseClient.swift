// 이 파일은 캐릭터 이름·역할 지침과 CLI 대화 기록을 로컬 PostgreSQL 백엔드에 전달한다.

import Foundation
import OfficeCore

struct OfficeDatabaseClient: Sendable {
    let baseURL: URL

    func fetchCharacters() async throws -> [StoredCharacterProfile] {
        let url = baseURL.appending(path: "api/characters")
        let (data, response) = try await URLSession.shared.data(from: url)
        try validate(response)
        let payload = try JSONDecoder().decode(
            CharacterListResponse.self,
            from: data
        )
        return payload.characters
    }

    func fetchModelCatalog(
        force: Bool = false
    ) async throws -> AgentModelCatalogSnapshot {
        var components = URLComponents(
            url: baseURL.appending(path: "api/model-catalog"),
            resolvingAgainstBaseURL: false
        )
        if force {
            components?.queryItems = [URLQueryItem(name: "force", value: "1")]
        }
        let url = components?.url
            ?? baseURL.appending(path: "api/model-catalog")
        let (data, response) = try await URLSession.shared.data(from: url)
        try validate(response, data: data)
        return try historyDecoder().decode(
            AgentModelCatalogSnapshot.self,
            from: data
        )
    }

    func updateModelExclusions(
        _ excludedModels: [String],
        for backend: AgentBackend
    ) async throws -> AgentModelCatalogSnapshot {
        var request = URLRequest(
            url: baseURL.appending(path: "api/model-catalog/exclusions")
        )
        request.httpMethod = "PUT"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = try JSONEncoder().encode(
            ModelCatalogExclusionRequest(
                provider: backend,
                excludedModels: excludedModels
            )
        )
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        return try historyDecoder().decode(
            AgentModelCatalogSnapshot.self,
            from: data
        )
    }

    func fetchActiveSessions() async throws -> [StoredActiveSession] {
        let url = baseURL.appending(path: "api/active-sessions")
        let (data, response) = try await URLSession.shared.data(from: url)
        try validate(response)
        let payload = try JSONDecoder().decode(
            ActiveSessionListResponse.self,
            from: data
        )
        return payload.sessions
    }

    func fetchTerminalSessions() async throws -> [StoredTerminalSession] {
        let url = baseURL.appending(path: "api/terminal-sessions")
        let (data, response) = try await URLSession.shared.data(from: url)
        try validate(response, data: data)
        return try historyDecoder()
            .decode(TerminalSessionListResponse.self, from: data)
            .sessions
    }

    func fetchLocalProviderStatuses() async throws -> [LocalProviderStatus] {
        let (data, response) = try await URLSession.shared.data(from: baseURL.appending(path: "api/local-profiles"))
        try validate(response, data: data)
        return try JSONDecoder().decode(LocalProviderList.self, from: data).statuses
    }

    func fetchLocalProviderCatalog() async throws -> LocalProviderList {
        let (data, response) = try await URLSession.shared.data(from: baseURL.appending(path: "api/local-profiles"))
        try validate(response, data: data)
        return try JSONDecoder().decode(LocalProviderList.self, from: data)
    }

    func selectLocalProfile(_ profileID: String?, for character: OfficeCharacter) async throws {
        var request = URLRequest(url: baseURL.appending(path: "api/characters/\(character.rawValue)/local-profile"))
        request.httpMethod = "PUT"
        request.setValue("application/json", forHTTPHeaderField: "content-type")
        request.httpBody = try JSONSerialization.data(withJSONObject: ["profileId": profileID as Any? ?? NSNull()])
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
    }

    func openTerminalSession(
        character: OfficeCharacter
    ) async throws -> TerminalLaunchSpecification {
        var request = URLRequest(
            url: baseURL.appending(path: "api/terminal-sessions")
        )
        request.httpMethod = "POST"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = try JSONEncoder().encode(
            TerminalSessionOpenRequest(characterId: character.rawValue)
        )
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        return try historyDecoder().decode(
            TerminalLaunchSpecification.self,
            from: data
        )
    }

    func closeTerminalSession(
        character: OfficeCharacter
    ) async throws {
        var request = URLRequest(
            url: baseURL
                .appending(path: "api/terminal-sessions")
                .appending(path: character.rawValue)
        )
        request.httpMethod = "DELETE"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = Data("{}".utf8)
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
    }

    /// 터미널에 Esc를 보낸 뒤 호출해 진행 중인 터미널 턴을 중단 처리한다.
    func interruptTerminalSession(
        character: OfficeCharacter
    ) async throws -> TerminalInterruptResult {
        var request = URLRequest(
            url: baseURL
                .appending(path: "api/terminal-sessions")
                .appending(path: character.rawValue)
                .appending(path: "interrupt")
        )
        request.httpMethod = "POST"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = Data("{}".utf8)
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        return try historyDecoder().decode(
            TerminalInterruptResult.self,
            from: data
        )
    }

    func usageSummaryURL(force: Bool = false) -> URL {
        let endpoint = baseURL
            .appending(path: "api")
            .appending(path: "usage-summary")
        guard force else {
            return endpoint
        }
        var components = URLComponents(
            url: endpoint,
            resolvingAgainstBaseURL: false
        )
        components?.queryItems = [
            URLQueryItem(name: "force", value: "1")
        ]
        return components?.url ?? endpoint
    }

    func fetchUsageSummary(
        force: Bool = false
    ) async throws -> AIUsageSnapshot {
        let (data, response) = try await URLSession.shared.data(
            from: usageSummaryURL(force: force)
        )
        try validate(response, data: data)
        return try decodeUsageSummary(data)
    }

    func cliUpdatesURL(force: Bool) -> URL {
        let endpoint = baseURL.appending(path: "api/cli-updates")
        guard force else {
            return endpoint
        }
        var components = URLComponents(
            url: endpoint,
            resolvingAgainstBaseURL: false
        )
        components?.queryItems = [URLQueryItem(name: "force", value: "1")]
        return components?.url ?? endpoint
    }

    func fetchCLIUpdateStatus(
        force: Bool = false
    ) async throws -> CLIUpdateStatus {
        let (data, response) = try await URLSession.shared.data(
            from: cliUpdatesURL(force: force)
        )
        try validate(response, data: data)
        return try historyDecoder().decode(CLIUpdateStatus.self, from: data)
    }

    func cliUpdateApplyRequest(packageID: String?) -> URLRequest {
        var request = URLRequest(
            url: baseURL.appending(path: "api/cli-updates/apply")
        )
        request.httpMethod = "POST"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        let payload = packageID.map { ["id": $0] } ?? [:]
        request.httpBody =
            (try? JSONSerialization.data(withJSONObject: payload))
            ?? Data("{}".utf8)
        return request
    }

    /// CLI별 패키지 설치나 자체 갱신은 오래 걸릴 수 있다. 진행 중 업무가 있으면 백엔드가
    /// 409로 막고, 그 사유를 그대로 보여 준다.
    @discardableResult
    func applyCLIUpdates(
        packageID: String? = nil
    ) async throws -> CLIUpdateStatus {
        let (data, response) = try await URLSession.shared.data(
            for: cliUpdateApplyRequest(packageID: packageID)
        )
        try validate(response, data: data)
        return try historyDecoder()
            .decode(CLIUpdateApplyResponse.self, from: data)
            .status
    }

    func decodeUsageSummary(_ data: Data) throws -> AIUsageSnapshot {
        try historyDecoder().decode(AIUsageSnapshot.self, from: data)
    }

    /// 화이트보드 상세용 집계다. 기간 라벨은 앱의 시간대로 자른다.
    func usageReportURL(
        backend: AgentBackend,
        granularity: UsageReportGranularity,
        timeZone: TimeZone = .autoupdatingCurrent
    ) -> URL {
        let endpoint = baseURL.appending(path: "api/usage-report")
        var components = URLComponents(
            url: endpoint,
            resolvingAgainstBaseURL: false
        )
        components?.queryItems = [
            URLQueryItem(name: "backend", value: backend.rawValue),
            URLQueryItem(name: "granularity", value: granularity.rawValue),
            URLQueryItem(name: "tz", value: timeZone.identifier),
        ]
        return components?.url ?? endpoint
    }

    func fetchUsageReport(
        backend: AgentBackend,
        granularity: UsageReportGranularity
    ) async throws -> UsageReport {
        let (data, response) = try await URLSession.shared.data(
            from: usageReportURL(backend: backend, granularity: granularity)
        )
        try validate(response, data: data)
        return try decodeUsageReport(data)
    }

    func decodeUsageReport(_ data: Data) throws -> UsageReport {
        try historyDecoder().decode(UsageReport.self, from: data)
    }

    func wikiPagesURL(query: String, limit: Int) -> URL {
        let endpoint = baseURL
            .appending(path: "api")
            .appending(path: "wiki")
            .appending(path: "pages")
        var components = URLComponents(
            url: endpoint,
            resolvingAgainstBaseURL: false
        )
        components?.queryItems = [
            URLQueryItem(name: "query", value: query),
            URLQueryItem(name: "limit", value: String(limit)),
        ]
        return components?.url ?? endpoint
    }

    func fetchWikiPages(
        query: String,
        limit: Int
    ) async throws -> [WikiPage] {
        let (data, response) = try await URLSession.shared.data(
            from: wikiPagesURL(query: query, limit: limit)
        )
        try validate(response, data: data)
        return try decodeWikiPages(data)
    }

    /// 승인·거절과 같은 사용자 결정 헤더를 붙인다. 백엔드는 이 헤더가 없는
    /// DELETE를 거절하므로 화면의 삭제 버튼만 문서를 지울 수 있다.
    func wikiPageDeletionRequest(id: String) -> URLRequest {
        var request = URLRequest(
            url: baseURL
                .appending(path: "api")
                .appending(path: "wiki")
                .appending(path: "pages")
                .appending(path: id)
        )
        request.httpMethod = "DELETE"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.setValue(
            "delete:\(id)",
            forHTTPHeaderField: "X-OFFICESTRA-User-Decision"
        )
        return request
    }

    func deleteWikiPage(id: String) async throws {
        let (data, response) = try await URLSession.shared.data(
            for: wikiPageDeletionRequest(id: id)
        )
        try validate(response, data: data)
    }

    func decodeWikiPages(_ data: Data) throws -> [WikiPage] {
        try historyDecoder().decode(WikiPagesResponse.self, from: data).pages
    }

    func wikiProposalsURL(state: String) -> URL {
        let endpoint = baseURL
            .appending(path: "api")
            .appending(path: "wiki")
            .appending(path: "proposals")
        var components = URLComponents(
            url: endpoint,
            resolvingAgainstBaseURL: false
        )
        components?.queryItems = [
            URLQueryItem(name: "state", value: state)
        ]
        return components?.url ?? endpoint
    }

    func fetchWikiProposals(
        state: String = "pending_user"
    ) async throws -> [WikiProposal] {
        let (data, response) = try await URLSession.shared.data(
            from: wikiProposalsURL(state: state)
        )
        try validate(response, data: data)
        return try decodeWikiProposals(data)
    }

    func decodeWikiProposals(_ data: Data) throws -> [WikiProposal] {
        try historyDecoder()
            .decode(WikiProposalsResponse.self, from: data)
            .proposals
    }

    func wikiProposalApprovalRequest(id: String) -> URLRequest {
        var request = URLRequest(
            url: wikiProposalActionURL(id: id, action: "approve")
        )
        request.httpMethod = "POST"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.setValue(
            "approve:\(id)",
            forHTTPHeaderField: "X-OFFICESTRA-User-Decision"
        )
        request.httpBody = Data("{}".utf8)
        return request
    }

    func wikiProposalRejectionRequest(
        id: String,
        reason: String
    ) throws -> URLRequest {
        var request = URLRequest(
            url: wikiProposalActionURL(id: id, action: "reject")
        )
        request.httpMethod = "POST"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.setValue(
            "reject:\(id)",
            forHTTPHeaderField: "X-OFFICESTRA-User-Decision"
        )
        request.httpBody = try JSONEncoder().encode(
            WikiProposalRejectionRequest(reason: reason)
        )
        return request
    }

    func approveWikiProposal(id: String) async throws -> WikiProposal {
        try await submitWikiProposal(
            request: wikiProposalApprovalRequest(id: id)
        )
    }

    func rejectWikiProposal(
        id: String,
        reason: String
    ) async throws -> WikiProposal {
        try await submitWikiProposal(
            request: wikiProposalRejectionRequest(id: id, reason: reason)
        )
    }

    func updateName(
        _ name: String,
        for character: OfficeCharacter
    ) async throws {
        let url = baseURL
            .appending(path: "api/characters")
            .appending(path: character.rawValue)
            .appending(path: "name")
        var request = URLRequest(url: url)
        request.httpMethod = "PUT"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = try JSONEncoder().encode(NameRequest(name: name))
        let (_, response) = try await URLSession.shared.data(for: request)
        try validate(response)
    }

    func updateSettings(
        _ settings: CharacterAgentSettings,
        for character: OfficeCharacter
    ) async throws {
        let url = baseURL
            .appending(path: "api/characters")
            .appending(path: character.rawValue)
            .appending(path: "settings")
        var request = URLRequest(url: url)
        request.httpMethod = "PUT"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = try JSONEncoder().encode(
            AgentSettingsRequest(
                backend: settings.backend,
                model: settings.model,
                effort: settings.effort,
                fastMode: settings.fastMode,
                permission: settings.permission.cliValue(
                    for: settings.backend
                )
            )
        )
        let (_, response) = try await URLSession.shared.data(for: request)
        try validate(response)
    }

    func updateSettings(
        _ updates: [CharacterSettingsBulkUpdate]
    ) async throws -> CharacterSettingsBulkResult {
        guard let request = try bulkSettingsRequest(updates) else {
            return CharacterSettingsBulkResult(
                ok: true,
                characters: [],
                warnings: []
            )
        }
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        let result = try JSONDecoder().decode(
            CharacterSettingsBulkResult.self,
            from: data
        )
        guard result.ok else {
            throw OfficeDatabaseError.requestFailed
        }
        return result
    }

    func bulkSettingsRequest(
        _ updates: [CharacterSettingsBulkUpdate]
    ) throws -> URLRequest? {
        guard !updates.isEmpty else {
            return nil
        }
        let url = baseURL
            .appending(path: "api")
            .appending(path: "characters")
            .appending(path: "settings")
            .appending(path: "bulk")
        var request = URLRequest(url: url)
        request.httpMethod = "PUT"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "Content-Type"
        )
        request.httpBody = try JSONEncoder().encode(
            CharacterSettingsBulkRequest(updates: updates)
        )
        return request
    }

    func synchronizeRuntimeCLIPaths(
        _ executablePaths: [AgentBackend: String]
    ) async throws -> RuntimeCLIPathsResult {
        guard let request = try runtimeCLIPathsRequest(executablePaths) else {
            return RuntimeCLIPathsResult(
                ok: true,
                updatedCharacterIds: []
            )
        }
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        let result = try JSONDecoder().decode(
            RuntimeCLIPathsResult.self,
            from: data
        )
        guard result.ok else {
            throw OfficeDatabaseError.requestFailed
        }
        return result
    }

    func runtimeCLIPathsRequest(
        _ executablePaths: [AgentBackend: String]
    ) throws -> URLRequest? {
        let executables = Dictionary(
            uniqueKeysWithValues: executablePaths.compactMap { backend, path in
                let normalized = path.trimmingCharacters(
                    in: .whitespacesAndNewlines
                )
                return normalized.isEmpty
                    ? nil
                    : (backend.rawValue, normalized)
            }
        )
        guard !executables.isEmpty else {
            return nil
        }
        let url = baseURL
            .appending(path: "api")
            .appending(path: "runtime")
            .appending(path: "cli-paths")
        var request = URLRequest(url: url)
        request.httpMethod = "PUT"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "Content-Type"
        )
        request.httpBody = try JSONEncoder().encode(
            RuntimeCLIPathsRequest(executables: executables)
        )
        return request
    }

    func updateIdentityPrompt(
        _ identityPrompt: String,
        for character: OfficeCharacter
    ) async throws {
        let url = baseURL
            .appending(path: "api/characters")
            .appending(path: character.rawValue)
            .appending(path: "identity-prompt")
        var request = URLRequest(url: url)
        request.httpMethod = "PUT"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = try JSONEncoder().encode(
            IdentityPromptRequest(identityPrompt: identityPrompt)
        )
        let (_, response) = try await URLSession.shared.data(for: request)
        try validate(response)
    }

    func updateAutoCompactPercent(
        _ percent: Int,
        for character: OfficeCharacter
    ) async throws -> CharacterContextSettings {
        let url = baseURL
            .appending(path: "api/characters")
            .appending(path: character.rawValue)
            .appending(path: "context-settings")
        var request = URLRequest(url: url)
        request.httpMethod = "PUT"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = try JSONEncoder().encode(
            CharacterContextSettingsRequest(autoCompactPercent: percent)
        )
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        return try JSONDecoder().decode(
            CharacterContextSettings.self,
            from: data
        )
    }

    func compactContext(
        for character: OfficeCharacter
    ) async throws -> ContextCompactionResult {
        let url = baseURL
            .appending(path: "api/characters")
            .appending(path: character.rawValue)
            .appending(path: "context")
            .appending(path: "compact")
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = Data("{}".utf8)
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        return try JSONDecoder().decode(
            ContextCompactionResult.self,
            from: data
        )
    }

    func fetchCharacterHistory(
        for character: OfficeCharacter
    ) async throws -> CharacterHistory {
        let url = baseURL
            .appending(path: "api/characters")
            .appending(path: character.rawValue)
            .appending(path: "history")
        let (data, response) = try await URLSession.shared.data(from: url)
        try validate(response)
        return try historyDecoder().decode(
            CharacterHistory.self,
            from: data
        )
    }

    func fetchGlobalHistory(
        character: OfficeCharacter?,
        from: Date?,
        to: Date?
    ) async throws -> [GlobalHistoryTurn] {
        let endpoint = baseURL.appending(path: "api/history")
        guard var components = URLComponents(
            url: endpoint,
            resolvingAgainstBaseURL: false
        ) else {
            throw OfficeDatabaseError.requestFailed
        }
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [
            .withInternetDateTime,
            .withFractionalSeconds,
        ]
        var queryItems: [URLQueryItem] = []
        if let character {
            queryItems.append(
                URLQueryItem(
                    name: "characterId",
                    value: character.rawValue
                )
            )
        }
        if let from {
            queryItems.append(
                URLQueryItem(name: "from", value: formatter.string(from: from))
            )
        }
        if let to {
            queryItems.append(
                URLQueryItem(name: "to", value: formatter.string(from: to))
            )
        }
        components.queryItems = queryItems
        guard let url = components.url else {
            throw OfficeDatabaseError.requestFailed
        }

        let (data, response) = try await URLSession.shared.data(from: url)
        try validate(response)
        return try historyDecoder().decode(
            GlobalHistoryResponse.self,
            from: data
        )
        .turns
    }

    func fetchLiveFeed(limit: Int) async throws -> LiveFeedResponse {
        let endpoint = baseURL
            .appending(path: "api")
            .appending(path: "live-feed")
        guard var components = URLComponents(
            url: endpoint,
            resolvingAgainstBaseURL: false
        ) else {
            throw OfficeDatabaseError.requestFailed
        }
        components.queryItems = [
            URLQueryItem(name: "limit", value: String(limit))
        ]
        guard let url = components.url else {
            throw OfficeDatabaseError.requestFailed
        }
        let (data, response) = try await URLSession.shared.data(from: url)
        try validate(response)
        return try historyDecoder().decode(
            LiveFeedResponse.self,
            from: data
        )
    }

    func fetchLiveFeedTurn(id: String) async throws -> LiveFeedTurnResponse {
        let url = baseURL
            .appending(path: "api")
            .appending(path: "live-feed")
            .appending(path: id)
        let (data, response) = try await URLSession.shared.data(from: url)
        try validate(response)
        return try historyDecoder().decode(
            LiveFeedTurnResponse.self,
            from: data
        )
    }

    func updateTurnFeedback(
        turnID: String,
        feedback: TurnResponseFeedback?
    ) async throws -> TurnResponseFeedback? {
        let url = baseURL
            .appending(path: "api")
            .appending(path: "turns")
            .appending(path: turnID)
            .appending(path: "feedback")
        var request = URLRequest(url: url)
        request.httpMethod = "PUT"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = try JSONEncoder().encode(
            TurnFeedbackRequest(feedback: feedback)
        )
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        return try JSONDecoder().decode(
            TurnFeedbackResponse.self,
            from: data
        ).feedback
    }

    func fetchArchiveFeed(
        query: String?,
        limit: Int,
        offset: Int
    ) async throws -> ArchiveFeedPage {
        let endpoint = baseURL
            .appending(path: "api")
            .appending(path: "archive-feed")
        guard var components = URLComponents(
            url: endpoint,
            resolvingAgainstBaseURL: false
        ) else {
            throw OfficeDatabaseError.requestFailed
        }
        var queryItems = [
            URLQueryItem(name: "limit", value: String(limit)),
            URLQueryItem(name: "offset", value: String(offset)),
        ]
        if let query, !query.isEmpty {
            queryItems.append(URLQueryItem(name: "query", value: query))
        }
        components.queryItems = queryItems
        guard let url = components.url else {
            throw OfficeDatabaseError.requestFailed
        }
        let (data, response) = try await URLSession.shared.data(from: url)
        try validate(response)
        return try historyDecoder().decode(ArchiveFeedPage.self, from: data)
    }

    func startAgentJob(
        character: OfficeCharacter,
        prompt: String,
        conversationID: UUID,
        attachmentPaths: [String]
    ) async throws -> StartedAgentJob {
        let url = baseURL
            .appending(path: "api")
            .appending(path: "agent-jobs")
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        request.httpBody = try JSONEncoder().encode(
            StartAgentJobRequest(
                characterId: character.rawValue,
                prompt: prompt,
                conversationId: conversationID,
                attachmentPaths: attachmentPaths
            )
        )
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        return try JSONDecoder().decode(StartedAgentJob.self, from: data)
    }

    func cancelAgentJob(
        character: OfficeCharacter
    ) async throws -> CancelledAgentJob {
        let url = baseURL
            .appending(path: "api")
            .appending(path: "agent-jobs")
            .appending(path: character.rawValue)
        var request = URLRequest(url: url)
        request.httpMethod = "DELETE"
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        return try JSONDecoder().decode(CancelledAgentJob.self, from: data)
    }

    var realtimeWebSocketURL: URL {
        var components = URLComponents(
            url: baseURL,
            resolvingAgainstBaseURL: false
        )
        components?.scheme = baseURL.scheme == "https" ? "wss" : "ws"
        components?.path = "/ws"
        components?.query = nil
        return components?.url ?? baseURL
    }

    func recordTurn(_ turn: DatabaseTurn) async throws {
        let url = baseURL.appending(path: "api/turns")
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue(
            "application/json",
            forHTTPHeaderField: "content-type"
        )
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        request.httpBody = try encoder.encode(turn)
        let (_, response) = try await URLSession.shared.data(for: request)
        try validate(response)
    }

    private func validate(_ response: URLResponse) throws {
        guard
            let response = response as? HTTPURLResponse,
            (200..<300).contains(response.statusCode)
        else {
            throw OfficeDatabaseError.requestFailed
        }
    }

    private func validate(_ response: URLResponse, data: Data) throws {
        guard
            let response = response as? HTTPURLResponse,
            (200..<300).contains(response.statusCode)
        else {
            let payload = try? JSONDecoder().decode(
                BackendErrorResponse.self,
                from: data
            )
            throw OfficeDatabaseError.backend(
                payload?.error ?? "백엔드 요청에 실패했습니다."
            )
        }
    }

    private func wikiProposalActionURL(
        id: String,
        action: String
    ) -> URL {
        baseURL
            .appending(path: "api")
            .appending(path: "wiki")
            .appending(path: "proposals")
            .appending(path: id)
            .appending(path: action)
    }

    private func submitWikiProposal(
        request: URLRequest
    ) async throws -> WikiProposal {
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
        return try historyDecoder()
            .decode(WikiProposalResponse.self, from: data)
            .proposal
    }

    private func historyDecoder() -> JSONDecoder {
        let decoder = JSONDecoder()
        let fractional = ISO8601DateFormatter()
        fractional.formatOptions = [
            .withInternetDateTime,
            .withFractionalSeconds,
        ]
        let standard = ISO8601DateFormatter()
        standard.formatOptions = [.withInternetDateTime]
        decoder.dateDecodingStrategy = .custom { decoder in
            let container = try decoder.singleValueContainer()
            let value = try container.decode(String.self)

            if let date = fractional.date(from: value) {
                return date
            }

            if let date = standard.date(from: value) {
                return date
            }
            throw DecodingError.dataCorruptedError(
                in: container,
                debugDescription: "날짜 형식이 올바르지 않습니다."
            )
        }
        return decoder
    }
}

struct DatabaseTurn: Encodable, Sendable {
    let turnId: UUID
    let conversationId: UUID
    let characterId: String
    let externalSessionId: String?
    let backend: AgentBackend
    let model: String?
    let effort: String
    let fastMode: Bool
    let prompt: String
    let response: String
    let title: String
    let workdir: String
    let startedAt: Date
    let finishedAt: Date
}

private struct CharacterListResponse: Decodable {
    let characters: [StoredCharacterProfile]
}

private struct ModelCatalogExclusionRequest: Encodable {
    let provider: AgentBackend
    let excludedModels: [String]
}

private struct ActiveSessionListResponse: Decodable {
    let sessions: [StoredActiveSession]
}

private struct TerminalSessionListResponse: Decodable {
    let sessions: [StoredTerminalSession]
}

struct LocalProviderList: Decodable {
    let statuses: [LocalProviderStatus]
    let profiles: [LocalModelOption]?
    let assignments: [LocalProfileAssignment]?
}

struct LocalModelOption: Decodable, Identifiable, Equatable, Sendable {
    let id: String
    let enabled: Bool
    let model: String
    let title: String?
    let contextWindow: Int
    var displayTitle: String { title ?? model }
}

struct LocalProfileAssignment: Decodable, Sendable {
    let characterId: String
    let profileId: String
}

struct LocalProviderStatus: Decodable, Identifiable, Equatable, Sendable {
    let id: String
    let state: String
    let error: String?

    var displayText: String {
        switch state {
        case "waiting": return OfficeLocalization.string("로컬 AI · ComfyUI 또는 다른 작업 종료 대기")
        case "starting": return OfficeLocalization.string("로컬 AI · 연결 및 모델 준비 중")
        case "ready": return OfficeLocalization.string("로컬 AI · 준비됨")
        case "error": return OfficeLocalization.string("로컬 AI · 연결 실패, 다음 요청에서 재연결")
        default: return OfficeLocalization.string("로컬 AI · 유휴, 다음 요청에서 모델 준비")
        }
    }
}

private struct TerminalSessionOpenRequest: Encodable {
    let characterId: String
}

private struct WikiPagesResponse: Decodable {
    let pages: [WikiPage]
}

private struct WikiProposalsResponse: Decodable {
    let proposals: [WikiProposal]
}

private struct WikiProposalResponse: Decodable {
    let proposal: WikiProposal
}

private struct WikiProposalRejectionRequest: Encodable {
    let reason: String
}

struct WikiPage: Decodable, Identifiable, Equatable, Sendable {
    let id: String
    let pageKey: String
    let title: String
    let body: String
    let updatedAt: Date
    let sources: [WikiPageSource]
}

struct WikiPageSource: Decodable, Identifiable, Equatable, Sendable {
    let workRecordId: String
    let title: String
    let excerpt: String

    var id: String {
        workRecordId
    }
}

struct WikiProposal: Decodable, Identifiable, Equatable, Sendable {
    let id: String
    let state: String
    let pageKey: String
    let title: String
    let body: String
    let approvalTier: String
    let sourceRecordIds: [String]
    let createdAt: Date
}

struct StoredCharacterProfile: Decodable, Sendable {
    let id: String
    let name: String
    let backend: AgentBackend
    let model: String?
    let effort: String
    let fastMode: Bool
    var autoCompactPercent: Int? = nil
    let permission: String
    let identityPrompt: String
}

struct AgentModelCatalogSnapshot: Decodable, Equatable, Sendable {
    let providers: [AgentModelProviderCatalog]
}

struct AgentModelProviderCatalog:
    Decodable,
    Equatable,
    Identifiable,
    Sendable
{
    let backend: AgentBackend
    let models: [AgentModelOption]
    let excludedModels: [String]
    let fetchedAt: Date?
    let lastAttemptedAt: Date?
    let lastError: String?

    var id: AgentBackend { backend }

    var visibleModels: [AgentModelOption] {
        let excluded = Set(excludedModels)
        return models.filter { !excluded.contains($0.id) }
    }
}

struct CharacterContextSettings: Decodable, Equatable, Sendable {
    let id: String
    let autoCompactPercent: Int
}

struct ContextCompactionResult: Decodable, Equatable, Sendable {
    let ok: Bool
    let automatic: Bool
    let backend: AgentBackend
    let sessionId: String
    let preTokens: Int?
    let postTokens: Int?
    let limitTokens: Int?
}

struct StoredActiveSession: Decodable, Sendable {
    let characterId: String
    let externalSessionId: String?
    let conversationId: UUID
}

struct TerminalInterruptResult: Decodable, Equatable, Sendable {
    let interrupted: Bool
    let turnId: String?
}

struct StoredTerminalSession: Decodable, Identifiable, Equatable, Sendable {
    let terminalSessionId: String
    let characterId: String
    let backend: AgentBackend
    let externalSessionId: String?
    let conversationId: String
    let runningTurnId: String?
    let startedAt: Date

    var id: String { terminalSessionId }
}

struct TerminalLaunchSpecification: Decodable, Equatable, Sendable {
    let terminalSessionId: String
    let executable: String
    let args: [String]
    let cwd: String
    let env: [String: String]
    let externalSessionId: String?
    let conversationId: String
    let backend: AgentBackend
}

struct CharacterSettingsBulkUpdate: Encodable, Equatable, Sendable {
    let characterId: String
    let backend: AgentBackend
    let model: String?
    let effort: String
    let fastMode: Bool
    let permission: String

    init(
        character: OfficeCharacter,
        settings: CharacterAgentSettings
    ) {
        characterId = character.rawValue
        backend = settings.backend
        model = settings.model
        effort = settings.effort
        fastMode = settings.fastMode
        permission = settings.permission.cliValue(for: settings.backend)
    }
}

struct CharacterSettingsBulkResult: Decodable, Sendable {
    let ok: Bool
    let characters: [StoredCharacterProfile]
    let warnings: [String]
}

struct CharacterHistory: Decodable, Sendable {
    let character: HistoryCharacter
    let sessions: [HistorySession]
}

struct HistoryCharacter: Decodable, Sendable {
    let id: String
    let name: String
    let seat: String
    let backend: AgentBackend
}

struct HistorySession: Decodable, Identifiable, Sendable {
    let id: String
    let externalId: String?
    let startedAt: Date
    let endedAt: Date?
    let turns: [HistoryTurn]
}

struct HistoryTurn: Decodable, Identifiable, Sendable {
    let id: String
    let sessionId: String
    let prompt: String
    let response: String
    let sources: [LiveFeedSource]?
    let executionBackend: AgentBackend?
    let executionModel: String?
    let executionEffort: String?
    let executionFastMode: Bool?
    let origin: String?
    let conversationWorkdir: String?
    let responseSourceWarning: String?
    let wikiProposalWarning: String?
    let startedAt: Date
    let endedAt: Date?

    var responseSources: [LiveFeedSource] {
        sources ?? []
    }
}

struct GlobalHistoryTurn: Decodable, Identifiable, Sendable {
    let id: String
    let characterId: String
    let characterName: String
    let backend: AgentBackend
    let executionBackend: AgentBackend?
    let executionModel: String?
    let executionEffort: String?
    let executionFastMode: Bool?
    let origin: String?
    let externalSessionId: String?
    let conversationWorkdir: String?
    let prompt: String
    let response: String
    let sources: [LiveFeedSource]?
    let responseSourceWarning: String?
    let wikiProposalWarning: String?
    let startedAt: Date
    let endedAt: Date?

    var responseSources: [LiveFeedSource] {
        sources ?? []
    }
}

enum LiveTurnStatus: String, Decodable, Sendable {
    case pending
    case running
    case completed
    case failed
    case interrupted

    var isRunning: Bool {
        self == .pending || self == .running
    }
}

enum TurnResponseFeedback: String, Codable, Equatable, Sendable {
    case liked
    case disliked

    static func toggled(
        current: TurnResponseFeedback?,
        selection: TurnResponseFeedback
    ) -> TurnResponseFeedback? {
        current == selection ? nil : selection
    }
}

struct LiveFeedActivity: Decodable, Identifiable, Equatable, Sendable {
    let id: String
    let kind: String
    let text: String
    let status: LiveFeedActivityStatus
    let collaboration: LiveFeedCollaboration?
    let occurredAt: Date

    // Keep transcript/command contents intact; translate only generated wrappers.
    var displayText: String {
        switch kind {
        case "tool":
            // Only the first line is our wrapper. Tool stdout can contain any
            // user data (or large logs) and must not be translated or scanned.
            if let newline = text.firstIndex(of: "\n") {
                OfficeLocalization.systemMessage(String(text[..<newline])) + text[newline...]
            } else {
                OfficeLocalization.systemMessage(text)
            }
        case "thinking" where text == "추론 중":
            OfficeLocalization.systemMessage(text)
        case "command" where text.hasSuffix(" [민감 인자 숨김]"):
            OfficeLocalization.systemMessage(text)
        default:
            text
        }
    }

    private enum CodingKeys: String, CodingKey {
        case id
        case kind
        case text
        case status
        case collaboration
        case occurredAt
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        kind = try container.decode(String.self, forKey: .kind)
        text = try container.decode(String.self, forKey: .text)
        status = try container.decodeIfPresent(
            LiveFeedActivityStatus.self,
            forKey: .status
        ) ?? .completed
        collaboration = try container.decodeIfPresent(
            LiveFeedCollaboration.self,
            forKey: .collaboration
        )
        occurredAt = try container.decode(Date.self, forKey: .occurredAt)
    }
}

struct LiveFeedCollaboration: Decodable, Equatable, Sendable {
    let action: String
    let agentThreadID: String?
    let agentLabel: String?
    let prompt: String?
    let message: String?
    let agentStatus: String?

    private enum CodingKeys: String, CodingKey {
        case action
        case agentThreadID = "agentThreadId"
        case agentLabel
        case prompt
        case message
        case agentStatus
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        action = try container.decodeIfPresent(
            String.self,
            forKey: .action
        ) ?? "other"
        agentThreadID = try container.decodeIfPresent(
            String.self,
            forKey: .agentThreadID
        )
        agentLabel = try container.decodeIfPresent(
            String.self,
            forKey: .agentLabel
        )
        prompt = try container.decodeIfPresent(
            String.self,
            forKey: .prompt
        )
        message = try container.decodeIfPresent(
            String.self,
            forKey: .message
        )
        agentStatus = try container.decodeIfPresent(
            String.self,
            forKey: .agentStatus
        )
    }
}

enum LiveFeedActivityStatus: String, Decodable, Sendable {
    case running
    case completed
    case failed
}

enum WorkspaceReviewStatus: String, Decodable, Equatable, Sendable {
    case active
    case awaitingApproval = "awaiting_approval"
    case merging
    case merged
    case rejected
    case closed
    case conflict
    case failed
}

struct WorkspaceChangedFile: Decodable, Equatable, Identifiable, Sendable {
    let status: String
    let path: String
    let previousPath: String?

    init(
        status: String,
        path: String,
        previousPath: String? = nil
    ) {
        self.status = status
        self.path = path
        self.previousPath = previousPath
    }

    var id: String {
        "\(status)|\(previousPath ?? "")|\(path)"
    }
}

struct TurnWorkspaceReview: Decodable, Equatable, Sendable {
    let status: WorkspaceReviewStatus
    let repositoryRoot: String
    let worktreePath: String
    let executionWorkdir: String?
    let branchName: String
    let baseBranch: String
    let baseCommit: String
    let reviewTree: String?
    let headCommit: String?
    let changedFiles: [WorkspaceChangedFile]
    let mergedCommit: String?
    let errorMessage: String?
    let diff: String?
    let diffTruncated: Bool?

    func fileBaseDirectory(fallback: String) -> String {
        if let executionWorkdir, !executionWorkdir.isEmpty {
            return executionWorkdir
        }
        if status == .merged, !repositoryRoot.isEmpty {
            return repositoryRoot
        }
        if !worktreePath.isEmpty {
            return worktreePath
        }
        if !repositoryRoot.isEmpty {
            return repositoryRoot
        }
        return fallback
    }

    func reviewFileBaseDirectory(fallback: String) -> String {
        if status == .merged, !repositoryRoot.isEmpty {
            return repositoryRoot
        }
        if !worktreePath.isEmpty {
            return worktreePath
        }
        if !repositoryRoot.isEmpty {
            return repositoryRoot
        }
        return fallback
    }

}

struct SessionContextUsage: Decodable, Equatable, Sendable {
    let usedTokens: Int
    let limitTokens: Int

    var remainingTokens: Int {
        max(0, limitTokens - usedTokens)
    }

    var remainingRatio: Double {
        guard limitTokens > 0 else {
            return 0
        }
        return Double(remainingTokens) / Double(limitTokens)
    }
}

enum LiveFeedSourceKind: String, Decodable, Sendable {
    case rag
    case database
    case file
    case web
    case tool
    case skill

    var title: String {
        switch self {
        case .rag:
            "RAG"
        case .database:
            "DB"
        case .file:
            "파일"
        case .web:
            "웹"
        case .tool:
            "도구"
        case .skill:
            "스킬"
        }
    }
}

struct LiveFeedSource: Decodable, Identifiable, Equatable, Sendable {
    let id: String
    let sourceKind: LiveFeedSourceKind
    let title: String
    let locator: String
    let excerpt: String?
    let ragDocumentId: String?
    let workRecordId: String?

    var filePath: String? {
        guard sourceKind == .file else {
            return nil
        }
        let components = locator.split(separator: ":", omittingEmptySubsequences: false)
        guard components.count > 1, let suffix = components.last else {
            return locator
        }
        let lineRange = suffix.split(separator: "-", omittingEmptySubsequences: false)
        guard
            !lineRange.isEmpty,
            lineRange.allSatisfy({ !$0.isEmpty && $0.allSatisfy(\.isNumber) })
        else {
            return locator
        }
        return components.dropLast().joined(separator: ":")
    }

    var webURL: URL? {
        guard
            sourceKind == .web,
            let components = URLComponents(string: locator),
            let scheme = components.scheme?.lowercased(),
            scheme == "http" || scheme == "https",
            components.user == nil,
            components.password == nil,
            let host = components.host,
            !host.isEmpty
        else {
            return nil
        }
        return components.url
    }
}

struct LiveFeedTurn: Decodable, Identifiable, Equatable, Sendable {
    let providerKind: String?
    let id: String
    let characterId: String
    let characterName: String
    let characterBackend: AgentBackend
    let backend: AgentBackend?
    let model: String?
    let effort: String?
    let fastMode: Bool?
    let origin: String?
    let externalSessionId: String?
    let conversationWorkdir: String?
    let prompt: String
    let response: String
    let feedback: TurnResponseFeedback?
    let status: LiveTurnStatus
    let needsInput: Bool
    let errorMessage: String?
    let responseSourceWarning: String?
    let wikiProposalWarning: String?
    let startedAt: Date
    let endedAt: Date?
    let updatedAt: Date
    let estimatedCostUsd: Double?
    let sessionContext: SessionContextUsage?
    let activities: [LiveFeedActivity]
    let sources: [LiveFeedSource]?
    let workspace: TurnWorkspaceReview?

    init(
        id: String,
        characterId: String,
        characterName: String,
        characterBackend: AgentBackend,
        backend: AgentBackend?,
        model: String?,
        effort: String?,
        fastMode: Bool?,
        origin: String? = nil,
        providerKind: String? = nil,
        externalSessionId: String?,
        conversationWorkdir: String?,
        prompt: String,
        response: String,
        feedback: TurnResponseFeedback?,
        status: LiveTurnStatus,
        needsInput: Bool,
        errorMessage: String?,
        responseSourceWarning: String?,
        wikiProposalWarning: String?,
        startedAt: Date,
        endedAt: Date?,
        updatedAt: Date,
        estimatedCostUsd: Double?,
        sessionContext: SessionContextUsage?,
        activities: [LiveFeedActivity],
        sources: [LiveFeedSource]?,
        workspace: TurnWorkspaceReview?
    ) {
        self.id = id
        self.characterId = characterId
        self.characterName = characterName
        self.characterBackend = characterBackend
        self.backend = backend
        self.model = model
        self.effort = effort
        self.fastMode = fastMode
        self.origin = origin
        self.providerKind = providerKind
        self.externalSessionId = externalSessionId
        self.conversationWorkdir = conversationWorkdir
        self.prompt = prompt
        self.response = response
        self.feedback = feedback
        self.status = status
        self.needsInput = needsInput
        self.errorMessage = errorMessage
        self.responseSourceWarning = responseSourceWarning
        self.wikiProposalWarning = wikiProposalWarning
        self.startedAt = startedAt
        self.endedAt = endedAt
        self.updatedAt = updatedAt
        self.estimatedCostUsd = estimatedCostUsd
        self.sessionContext = sessionContext
        self.activities = activities
        self.sources = sources
        self.workspace = workspace
    }

    var responseSources: [LiveFeedSource] {
        sources ?? []
    }

    func replacingID(with id: String) -> LiveFeedTurn {
        LiveFeedTurn(
            id: id,
            characterId: characterId,
            characterName: characterName,
            characterBackend: characterBackend,
            backend: backend,
            model: model,
            effort: effort,
            fastMode: fastMode,
            origin: origin,
            providerKind: providerKind,
            externalSessionId: externalSessionId,
            conversationWorkdir: conversationWorkdir,
            prompt: prompt,
            response: response,
            feedback: feedback,
            status: status,
            needsInput: needsInput,
            errorMessage: errorMessage,
            responseSourceWarning: responseSourceWarning,
            wikiProposalWarning: wikiProposalWarning,
            startedAt: startedAt,
            endedAt: endedAt,
            updatedAt: updatedAt,
            estimatedCostUsd: estimatedCostUsd,
            sessionContext: sessionContext,
            activities: activities,
            sources: sources,
            workspace: workspace
        )
    }

    func replacingFeedback(
        with feedback: TurnResponseFeedback?
    ) -> LiveFeedTurn {
        LiveFeedTurn(
            id: id,
            characterId: characterId,
            characterName: characterName,
            characterBackend: characterBackend,
            backend: backend,
            model: model,
            effort: effort,
            fastMode: fastMode,
            origin: origin,
            providerKind: providerKind,
            externalSessionId: externalSessionId,
            conversationWorkdir: conversationWorkdir,
            prompt: prompt,
            response: response,
            feedback: feedback,
            status: status,
            needsInput: needsInput,
            errorMessage: errorMessage,
            responseSourceWarning: responseSourceWarning,
            wikiProposalWarning: wikiProposalWarning,
            startedAt: startedAt,
            endedAt: endedAt,
            updatedAt: updatedAt,
            estimatedCostUsd: estimatedCostUsd,
            sessionContext: sessionContext,
            activities: activities,
            sources: sources,
            workspace: workspace
        )
    }
}

struct ArchiveFeedPage: Decodable, Sendable {
    let turns: [LiveFeedTurn]
    let total: Int
}

func agentExecutionModeTitle(_ fastMode: Bool?) -> String {
    fastMode == true ? "Fast" : "Standard"
}

private struct GlobalHistoryResponse: Decodable {
    let turns: [GlobalHistoryTurn]
}

struct LiveFeedResponse: Decodable {
    let turns: [LiveFeedTurn]
    let costSummary: CharacterTurnCostSnapshot?
}

struct LiveFeedTurnResponse: Decodable {
    let turn: LiveFeedTurn
    let costSummary: CharacterTurnCostSnapshot?
}

private struct TurnFeedbackRequest: Encodable {
    let feedback: TurnResponseFeedback?

    private enum CodingKeys: String, CodingKey {
        case feedback
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        if let feedback {
            try container.encode(feedback, forKey: .feedback)
        } else {
            try container.encodeNil(forKey: .feedback)
        }
    }
}

private struct TurnFeedbackResponse: Decodable {
    let feedback: TurnResponseFeedback?
}

private struct StartAgentJobRequest: Encodable {
    let characterId: String
    let prompt: String
    let conversationId: UUID
    let attachmentPaths: [String]
}

struct StartedAgentJob: Decodable, Sendable {
    let turnId: String
    let conversationId: UUID
    let status: String
}

struct CancelledAgentJob: Decodable, Sendable {
    let turnId: String
    let status: String
}

private struct NameRequest: Encodable {
    let name: String
}

private struct AgentSettingsRequest: Encodable {
    let backend: AgentBackend
    let model: String?
    let effort: String
    let fastMode: Bool
    let permission: String
}

private struct CharacterSettingsBulkRequest: Encodable {
    let updates: [CharacterSettingsBulkUpdate]
}

private struct RuntimeCLIPathsRequest: Encodable {
    let executables: [String: String]
}

struct RuntimeCLIPathsResult: Decodable, Equatable, Sendable {
    let ok: Bool
    let updatedCharacterIds: [String]
}

private struct IdentityPromptRequest: Encodable {
    let identityPrompt: String
}

private struct CharacterContextSettingsRequest: Encodable {
    let autoCompactPercent: Int
}

enum OfficeDatabaseError: LocalizedError {
    case requestFailed
    case backend(String)

    var errorDescription: String? {
        switch self {
        case .requestFailed:
            OfficeLocalization.string("PostgreSQL 백엔드에 연결할 수 없습니다.")
        case .backend(let message):
            OfficeLocalization.systemMessage(message)
        }
    }
}

private struct BackendErrorResponse: Decodable {
    let error: String
}
