// 이 파일은 캐릭터 선택과 CLI 세션 및 말풍선 상태를 한곳에서 관리한다.

import Combine
import Foundation
import OfficeCore

struct PendingAgentQuestion: Identifiable, Equatable {
    let id = UUID()
    let character: OfficeCharacter
    let text: String
}

@MainActor
final class CharacterSelectionStore: ObservableObject {
    private struct State: Equatable {
        var selectedCharacterID: OfficeCharacter?
        var isConversationLoading: Bool
    }

    @Published private var state: State
    let selectionWillChange = PassthroughSubject<OfficeCharacter?, Never>()

    var selectedCharacterID: OfficeCharacter? {
        state.selectedCharacterID
    }

    var isConversationLoading: Bool {
        state.isConversationLoading
    }

    init(selectedCharacterID: OfficeCharacter? = .boss) {
        state = State(
            selectedCharacterID: selectedCharacterID,
            // 첫 직원 대화도 실제 NSWindow와 문서 높이가 준비되기 전에는
            // 입력하거나 다른 직원을 선택하지 못하게 한다.
            isConversationLoading: selectedCharacterID != nil
        )
    }

    func select(_ characterID: OfficeCharacter?) {
        guard state.selectedCharacterID != characterID else {
            return
        }
        // SwiftUI가 새 선택을 발행한 뒤에야 대화 호스트를 교체하면
        // 헤더와 선택 버튼만 먼저 바뀌고 이전 대화가 잠깐 노출된다.
        // AppKit 호스트가 전환 차폐를 동기로 설치할 수 있도록 새 상태를
        // 발행하기 직전에 목표 직원을 알린다.
        selectionWillChange.send(characterID)
        state = State(
            selectedCharacterID: characterID,
            isConversationLoading: characterID != nil
        )
    }

    func canSelect(_ characterID: OfficeCharacter) -> Bool {
        !state.isConversationLoading
            || state.selectedCharacterID == characterID
    }

    func completeConversationLoading(for characterID: OfficeCharacter) {
        guard
            state.selectedCharacterID == characterID,
            state.isConversationLoading
        else {
            return
        }
        state.isConversationLoading = false
    }
}

@MainActor
final class CharacterLiveFeedStore: ObservableObject {
    @Published private(set) var turns: [LiveFeedTurn]
    @Published private(set) var isLoadingInitialFeed: Bool
    @Published private(set) var presentationRevision = 0
    private(set) var mountReadinessRevision = 0

    private var latestTurns: [LiveFeedTurn]
    private var latestIsLoadingInitialFeed: Bool
    private var isPresented = false

    init(
        turns: [LiveFeedTurn] = [],
        isLoadingInitialFeed: Bool = true
    ) {
        self.turns = turns
        self.isLoadingInitialFeed = isLoadingInitialFeed
        latestTurns = turns
        latestIsLoadingInitialFeed = isLoadingInitialFeed
    }

    func stage(
        turns: [LiveFeedTurn],
        isLoadingInitialFeed: Bool
    ) {
        latestTurns = turns
        latestIsLoadingInitialFeed = isLoadingInitialFeed
        guard isPresented else {
            return
        }
        publishLatestIfNeeded()
    }

    func setPresented(_ isPresented: Bool) {
        guard self.isPresented != isPresented else {
            return
        }
        self.isPresented = isPresented
        guard isPresented else {
            return
        }
        publishLatestIfNeeded()
    }

    func refreshPresentation() {
        guard isPresented else {
            return
        }
        mountReadinessRevision &+= 1
        presentationRevision &+= 1
    }

    private func publishLatestIfNeeded() {
        let didChangeTurns = turns != latestTurns
        let didChangeLoadingState =
            isLoadingInitialFeed != latestIsLoadingInitialFeed
        guard didChangeTurns || didChangeLoadingState else {
            return
        }
        // SwiftUI의 Published 갱신이 HostedLiveWorkspaceFeed를 다시 계산할 때
        // mount reporter가 의미 있는 피드 변경만 구분해 재확인 예산을
        // 재무장할 수 있도록 먼저 revision을 올린다.
        mountReadinessRevision &+= 1
        if didChangeTurns {
            turns = latestTurns
        }
        if didChangeLoadingState {
            isLoadingInitialFeed = latestIsLoadingInitialFeed
        }
    }
}

struct RealtimeFeedEvent: Decodable, Equatable {
    let type: String
    let turnId: String?
    let characterId: String?
    let automatic: Bool?
    let preTokens: Int?
    let postTokens: Int?
    let limitTokens: Int?
    let errorMessage: String?
    let compactingCharacterIds: [String]?

    init(
        type: String,
        turnId: String? = nil,
        characterId: String? = nil,
        automatic: Bool? = nil,
        preTokens: Int? = nil,
        postTokens: Int? = nil,
        limitTokens: Int? = nil,
        errorMessage: String? = nil,
        compactingCharacterIds: [String]? = nil
    ) {
        self.type = type
        self.turnId = turnId
        self.characterId = characterId
        self.automatic = automatic
        self.preTokens = preTokens
        self.postTokens = postTokens
        self.limitTokens = limitTokens
        self.errorMessage = errorMessage
        self.compactingCharacterIds = compactingCharacterIds
    }
}

struct TerminalRestartRequest: Equatable, Sendable {
    let character: OfficeCharacter
    let revision: Int
}

/// 터미널 모드에서 하단 입력창의 글과 멈춤 요청을 실제 CLI 터미널로 흘려보낸다.
/// 터미널 화면이 붙어 있는 동안만 등록되므로, 등록돼 있으면 곧 터미널 모드다.
/// 그 직원의 터미널 프로세스가 없으면 false를 돌려 호출자가 되돌릴 수 있게 한다.
@MainActor
protocol TerminalInputSink: AnyObject {
    func sendText(_ text: String, to character: OfficeCharacter) -> Bool
    func interrupt(_ character: OfficeCharacter) -> Bool
}

extension RealtimeFeedEvent {
    var officeCharacter: OfficeCharacter? {
        characterId.flatMap(OfficeCharacter.init(rawValue:))
    }
}

enum ContextCompactionNotice: Equatable {
    case completed(
        automatic: Bool,
        preTokens: Int?,
        postTokens: Int?
    )
    case failed(automatic: Bool, message: String?)
}

@MainActor
final class LiveFeedStore: ObservableObject {
    private struct ResponseAnimationState {
        let characterID: String
        let animatesInitialSource: Bool
    }

    @Published private(set) var turns: [LiveFeedTurn] = []
    @Published private(set) var isLoadingInitialFeed = true
    // 실행 후 첫 대화 적재가 끝났는지만 기록한다. 로딩 표시는 이
    // 시점까지 한 번만 쓰고, 이후 직원 전환이나 재조회에서는 이미
    // 받아 둔 대화를 그대로 두어 화면이 비지 않게 한다.
    private(set) var didCompleteFirstFeedLoad = false
    private(set) var persistedTurns: [LiveFeedTurn] = []
    private var optimisticTurns: [String: LiveFeedTurn] = [:]
    private var presentationIDsByTurnID: [String: String] = [:]
    private var turnsByCharacterID: [String: [LiveFeedTurn]] = [:]
    private var characterStores: [String: CharacterLiveFeedStore] = [:]
    private var responseAnimations: [String: ResponseAnimationState] = [:]
    private(set) var selectedCharacterFeedID: String?

    static let minimumTurnsPerCharacter = 10

    static func snapshotTurns(
        from sortedTurns: [LiveFeedTurn],
        recentLimit: Int
    ) -> [LiveFeedTurn] {
        let recentTurns = Array(sortedTurns.prefix(recentLimit))
        var selectedTurnIDs = Set(recentTurns.map(\.id))
        var selectedCounts = Dictionary(
            grouping: recentTurns,
            by: \.characterId
        ).mapValues(\.count)

        for turn in sortedTurns.dropFirst(recentLimit) {
            let selectedCount = selectedCounts[turn.characterId, default: 0]
            if selectedCount < minimumTurnsPerCharacter {
                selectedTurnIDs.insert(turn.id)
                selectedCounts[turn.characterId] = selectedCount + 1
            }
        }

        return sortedTurns.filter { selectedTurnIDs.contains($0.id) }
    }

    func replace(with turns: [LiveFeedTurn]) {
        for optimisticTurn in optimisticTurns.values {
            guard let persistedTurn = matchingPersistedTurn(
                for: optimisticTurn,
                in: turns
            ) else {
                continue
            }
            transferPresentationID(
                from: optimisticTurn.id,
                to: persistedTurn.id
            )
        }
        persistedTurns = turns
        let persistedIDs = Set(turns.map(\.id))
        optimisticTurns = optimisticTurns.filter {
            !persistedIDs.contains($0.key)
                && matchingPersistedTurn(
                    for: $0.value,
                    in: turns
                ) == nil
        }
        publishMergedTurns()
    }

    func insertOptimisticTurn(_ turn: LiveFeedTurn) {
        optimisticTurns[turn.id] = turn
        if presentationIDsByTurnID[turn.id] == nil {
            presentationIDsByTurnID[turn.id] = turn.id
        }
        publishMergedTurns()
    }

    func reconcileOptimisticTurn(
        id: String,
        with persistedTurnID: String
    ) {
        guard let turn = optimisticTurns.removeValue(forKey: id) else {
            return
        }
        transferPresentationID(from: id, to: persistedTurnID)
        if !persistedTurns.contains(where: { $0.id == persistedTurnID }) {
            optimisticTurns[persistedTurnID] = turn.replacingID(
                with: persistedTurnID
            )
        }
        publishMergedTurns()
    }

    func removeOptimisticTurn(id: String) {
        guard optimisticTurns.removeValue(forKey: id) != nil else {
            return
        }
        presentationIDsByTurnID[id] = nil
        publishMergedTurns()
    }

    func updateFeedback(
        for turnID: String,
        to feedback: TurnResponseFeedback?
    ) {
        guard let index = persistedTurns.firstIndex(where: {
            $0.id == turnID
        }) else {
            return
        }
        persistedTurns[index] = persistedTurns[index].replacingFeedback(
            with: feedback
        )
        publishMergedTurns()
    }

    var optimisticCharacterIDs: Set<String> {
        Set(optimisticTurns.values.map(\.characterId))
    }

    func turns(for characterID: String) -> [LiveFeedTurn] {
        turnsByCharacterID[characterID] ?? []
    }

    func presentationID(forTurnID turnID: String) -> String {
        presentationIDsByTurnID[turnID] ?? turnID
    }

    func characterStore(
        for characterID: String
    ) -> CharacterLiveFeedStore {
        if let store = characterStores[characterID] {
            return store
        }
        let store = CharacterLiveFeedStore(
            turns: turnsByCharacterID[characterID] ?? [],
            isLoadingInitialFeed: isLoadingInitialFeed
        )
        characterStores[characterID] = store
        return store
    }

    func selectCharacterFeed(_ characterID: String?) {
        guard selectedCharacterFeedID != characterID else {
            return
        }
        if let selectedCharacterFeedID {
            discardTerminalResponseAnimations(
                forCharacterID: selectedCharacterFeedID
            )
            characterStore(for: selectedCharacterFeedID)
                .setPresented(false)
        }
        selectedCharacterFeedID = characterID
        if let characterID {
            characterStore(for: characterID).setPresented(true)
        }
    }

    func refreshSelectedCharacterFeedAfterMount(
        _ characterID: String
    ) {
        guard selectedCharacterFeedID == characterID else {
            return
        }
        characterStores[characterID]?.refreshPresentation()
    }

    private func matchingPersistedTurn(
        for optimisticTurn: LiveFeedTurn,
        in turns: [LiveFeedTurn]
    ) -> LiveFeedTurn? {
        if let exactMatch = turns.first(where: {
            $0.id == optimisticTurn.id
        }) {
            return exactMatch
        }
        let optimisticPrompt = TaskPromptPresentation(
            prompt: optimisticTurn.prompt
        ).text
        return turns.first { turn in
            turn.characterId == optimisticTurn.characterId
                && TaskPromptPresentation(prompt: turn.prompt).text
                    == optimisticPrompt
                && turn.startedAt
                    >= optimisticTurn.startedAt.addingTimeInterval(-5)
                && turn.startedAt
                    <= optimisticTurn.startedAt.addingTimeInterval(120)
        }
    }

    private func transferPresentationID(
        from sourceTurnID: String,
        to destinationTurnID: String
    ) {
        let presentationID =
            presentationIDsByTurnID.removeValue(forKey: sourceTurnID)
            ?? sourceTurnID
        presentationIDsByTurnID[destinationTurnID] = presentationID
    }

    private func publishMergedTurns() {
        var mergedTurns = persistedTurns
        mergedTurns.append(contentsOf: optimisticTurns.values)
        mergedTurns.sort {
            if $0.startedAt == $1.startedAt {
                return $0.id > $1.id
            }
            return $0.startedAt > $1.startedAt
        }
        let mergedTurnIDs = Set(mergedTurns.map(\.id))
        presentationIDsByTurnID = presentationIDsByTurnID.filter {
            mergedTurnIDs.contains($0.key)
        }
        pruneResponseAnimations(for: mergedTurns)
        suppressHiddenInitialResponseAnimations(in: mergedTurns)
        guard turns != mergedTurns else {
            return
        }
        let updatedTurnsByCharacterID = Dictionary(
            grouping: mergedTurns,
            by: \.characterId
        )
        turnsByCharacterID = updatedTurnsByCharacterID
        for (characterID, store) in characterStores {
            store.stage(
                turns: updatedTurnsByCharacterID[characterID] ?? [],
                isLoadingInitialFeed: isLoadingInitialFeed
            )
        }
        turns = mergedTurns
    }

    func finishInitialLoading() {
        guard isLoadingInitialFeed else {
            return
        }
        isLoadingInitialFeed = false
        didCompleteFirstFeedLoad = true
        for (characterID, store) in characterStores {
            store.stage(
                turns: turnsByCharacterID[characterID] ?? [],
                isLoadingInitialFeed: false
            )
        }
    }

    func restoreResponseAnimations(for turns: [LiveFeedTurn]) {
        for turn in turns where turn.status.isRunning {
            guard responseAnimations[turn.id] == nil else {
                continue
            }
            responseAnimations[turn.id] = ResponseAnimationState(
                characterID: turn.characterId,
                animatesInitialSource: restoredTurnAnimatesInitialSource(turn)
            )
        }
    }

    private func restoredTurnAnimatesInitialSource(
        _ turn: LiveFeedTurn
    ) -> Bool {
        guard (turn.backend ?? turn.characterBackend) == .claude else {
            // Codex 공개 메시지는 실행 중에는 숨기고 종료 뒤 처음부터
            // 타이핑하므로, 복원 당시 응답 초안이 있어도 재생을 유지한다.
            return true
        }
        return ClaudeTranscriptPresentation.make(
            turnID: turn.id,
            activities: turn.activities,
            response: turn.response,
            responseUpdatedAt: turn.updatedAt,
            isRunning: true
        ).streamingMessageID == nil
    }

    func beginResponseAnimation(
        for turnID: String,
        characterID: String? = nil,
        notifiesCharacterStore: Bool = true
    ) {
        guard responseAnimations[turnID] == nil else {
            return
        }
        guard
            let resolvedCharacterID = characterID
                ?? turn(withID: turnID)?.characterId
        else {
            return
        }
        responseAnimations[turnID] = ResponseAnimationState(
            characterID: resolvedCharacterID,
            animatesInitialSource: true
        )
        if notifiesCharacterStore {
            characterStores[resolvedCharacterID]?
                .refreshPresentation()
        }
    }

    func shouldAnimateResponse(for turn: LiveFeedTurn) -> Bool {
        responseAnimations[turn.id] != nil
    }

    func shouldAnimateInitialResponse(for turn: LiveFeedTurn) -> Bool {
        responseAnimations[turn.id]?.animatesInitialSource == true
    }

    func finishResponseAnimation(for turnID: String) {
        guard let animation = responseAnimations[turnID] else {
            return
        }
        let turn = turn(withID: turnID)
        guard turn?.status.isRunning != true else {
            return
        }
        responseAnimations[turnID] = nil
        characterStores[animation.characterID]?
            .refreshPresentation()
    }

    private func turn(withID turnID: String) -> LiveFeedTurn? {
        optimisticTurns[turnID]
            ?? persistedTurns.first(where: { $0.id == turnID })
    }

    private func discardTerminalResponseAnimations(
        forCharacterID characterID: String
    ) {
        let turnIDs = responseAnimations.compactMap { element -> String? in
            let (turnID, animation) = element
            guard
                animation.characterID == characterID,
                turn(withID: turnID)?.status.isRunning != true
            else {
                return nil
            }
            return turnID
        }
        for turnID in turnIDs {
            responseAnimations[turnID] = nil
        }
    }

    private func pruneResponseAnimations(for turns: [LiveFeedTurn]) {
        let turnsByID = Dictionary(
            uniqueKeysWithValues: turns.map { ($0.id, $0) }
        )
        let turnIDs = responseAnimations.compactMap { element -> String? in
            let (turnID, animation) = element
            guard let turn = turnsByID[turnID] else {
                return turnID
            }
            guard !turn.status.isRunning else {
                return nil
            }
            if turn.status != .completed
                || turn.response.isEmpty
                || animation.characterID != selectedCharacterFeedID
            {
                return turnID
            }
            return nil
        }
        for turnID in turnIDs {
            responseAnimations[turnID] = nil
        }
    }

    private func suppressHiddenInitialResponseAnimations(
        in turns: [LiveFeedTurn]
    ) {
        let turnsByID = Dictionary(
            uniqueKeysWithValues: turns.map { ($0.id, $0) }
        )
        let turnIDs = responseAnimations.compactMap { element -> String? in
            let (turnID, animation) = element
            guard
                animation.animatesInitialSource,
                animation.characterID != selectedCharacterFeedID,
                let turn = turnsByID[turnID],
                turn.status.isRunning,
                !restoredTurnAnimatesInitialSource(turn)
            else {
                return nil
            }
            return turnID
        }
        for turnID in turnIDs {
            guard let animation = responseAnimations[turnID] else {
                continue
            }
            responseAnimations[turnID] = ResponseAnimationState(
                characterID: animation.characterID,
                animatesInitialSource: false
            )
        }
    }
}

@MainActor
final class ArchiveFeedStore: ObservableObject {
    @Published private(set) var turns: [LiveFeedTurn] = []
    private var revision: [String] = []

    func replaceIfNeeded(with turns: [LiveFeedTurn]) {
        let updatedRevision = turns.map {
            "\($0.id)|\($0.status.rawValue)|"
                + "\($0.workspace?.status.rawValue ?? "")|"
                + "\($0.workspace?.mergedCommit ?? "")|"
                + "\($0.responseSourceWarning ?? "")|"
                + "\($0.wikiProposalWarning ?? "")|"
                + "\($0.feedback?.rawValue ?? "")|"
                + $0.responseSources.map {
                    "\($0.id),\($0.sourceKind.rawValue),\($0.title),"
                        + "\($0.locator),\($0.excerpt ?? "")"
                }
                .joined(separator: ",")
        }
        guard revision != updatedRevision else {
            return
        }
        revision = updatedRevision
        self.turns = turns
    }
}

@MainActor
final class SpeechBubbleStore: ObservableObject {
    @Published private(set) var bubbles: [OfficeCharacter: String] = [:]

    func set(_ text: String, for character: OfficeCharacter) {
        guard bubbles[character] != text else {
            return
        }
        bubbles[character] = text
    }

    func remove(for character: OfficeCharacter) {
        guard bubbles[character] != nil else {
            return
        }
        bubbles[character] = nil
    }

    func replace(with bubbles: [OfficeCharacter: String]) {
        guard self.bubbles != bubbles else {
            return
        }
        self.bubbles = bubbles
    }
}

enum SpeechBubbleIdleChatterPolicy {
    static func candidates(
        characters: [CharacterConfiguration],
        runningCharacters: Set<OfficeCharacter>,
        occupiedCharacters: Set<OfficeCharacter>,
        questionCharacters: Set<OfficeCharacter>,
        failedCharacters: Set<OfficeCharacter>,
        offDutyCharacters: Set<OfficeCharacter>,
        lastCharacter: OfficeCharacter?
    ) -> [CharacterConfiguration] {
        var candidates = characters.filter {
            !runningCharacters.contains($0.id)
                && !occupiedCharacters.contains($0.id)
                && !questionCharacters.contains($0.id)
                && !failedCharacters.contains($0.id)
                && !offDutyCharacters.contains($0.id)
        }
        if candidates.count > 1, let lastCharacter {
            candidates.removeAll { $0.id == lastCharacter }
        }
        return candidates
    }

    static func messages(
        from messages: [String],
        excluding lastMessage: String?
    ) -> [String] {
        guard messages.count > 1, let lastMessage else {
            return messages
        }
        return messages.filter { $0 != lastMessage }
    }
}

struct CharacterIdentitySettingsDraft: Equatable, Sendable {
    let name: String
    let identityPrompt: String

    init(storedCharacter: StoredCharacterProfile) {
        name = storedCharacter.name
        identityPrompt = storedCharacter.identityPrompt
    }
}

struct ActiveSessionRestoreState: Equatable {
    let conversationIDs: [OfficeCharacter: UUID]
    let sessionIDs: [OfficeCharacter: String]

    init(activeSessions: [StoredActiveSession]) {
        var restoredConversationIDs: [OfficeCharacter: UUID] = [:]
        var restoredSessionIDs: [OfficeCharacter: String] = [:]

        for storedSession in activeSessions {
            guard
                let character = OfficeCharacter(
                    rawValue: storedSession.characterId
                ),
                let externalSessionID = storedSession.externalSessionId?
                    .trimmingCharacters(in: .whitespacesAndNewlines),
                !externalSessionID.isEmpty
            else {
                continue
            }
            restoredConversationIDs[character] = storedSession.conversationId
            restoredSessionIDs[character] = externalSessionID
        }

        conversationIDs = restoredConversationIDs
        sessionIDs = restoredSessionIDs
    }
}

enum CharacterSessionInvalidationPolicy {
    static func requiresNewSession(
        previous: CharacterAgentSettings,
        updated: CharacterAgentSettings
    ) -> Bool {
        previous.backend != updated.backend
    }
}

@MainActor
final class AgentDirector: ObservableObject {
    @Published private(set) var characters: [CharacterConfiguration]
    @Published private(set) var names: [OfficeCharacter: String]
    @Published private(set) var pendingQuestions:
        [OfficeCharacter: String] = [:]
    /// 대화 안에서 답변받을 턴을 가리킨다. 답변이 끝나면 비운다.
    @Published private(set) var pendingQuestionTurnIDs:
        [OfficeCharacter: String] = [:]
    @Published private(set) var questionSubmissionErrors:
        [OfficeCharacter: String] = [:]
    @Published private(set) var latestQuestion: PendingAgentQuestion?
    @Published private(set) var runningCharacters: Set<OfficeCharacter> = []
    @Published private(set) var unreviewedCompletedCharacters:
        Set<OfficeCharacter> = []
    @Published private(set) var cancellingCharacters:
        Set<OfficeCharacter> = []
    @Published private(set) var compactingCharacters:
        Set<OfficeCharacter> = []
    @Published private(set) var contextCompactionNotices:
        [OfficeCharacter: ContextCompactionNotice] = [:]
    @Published private(set) var autoCompactPercents:
        [OfficeCharacter: Int] = Dictionary(
            uniqueKeysWithValues: OfficeCharacter.allCases.map { ($0, 90) }
        )
    /// 응답 생성 중에 미리 걸어 둔 다음 업무다. 직원마다 최대 3개.
    @Published private(set) var queuedCommands:
        [OfficeCharacter: QueuedCommandQueue] = [:]
    @Published private(set) var failedCharacters:
        [OfficeCharacter: String] = [:]
    @Published private(set) var offDutyCharacters:
        [OfficeCharacter: String] = [:]
    @Published private(set) var isReadyForSubmissions = false
    @Published private(set) var sessionRestoreError: String?
    @Published private(set) var isUpdatingConfiguration = false
    @Published private(set) var turnPersistenceErrors:
        [OfficeCharacter: String] = [:]
    @Published private(set) var settingsStatus: String?
    @Published private(set) var modelCatalogs:
        [AgentBackend: AgentModelProviderCatalog] = [:]
    @Published private(set) var latestSubmittedCommandID: UUID?
    @Published private(set) var latestSubmittedTurnID: String?
    @Published private(set) var latestStartedCommandID: UUID?
    @Published private(set) var latestCompletedTurnID: String?
    @Published private(set) var latestTerminalTurnID: String?
    @Published private(set) var isRealtimeConnected = false
    @Published private(set) var realtimeConnectionError: String?
    @Published private(set) var terminalSessionRevision = 0
    @Published private(set) var terminalRestartRequest:
        TerminalRestartRequest?
    /// 터미널 화면이 붙어 있는 동안만 채워진다. 입력·예약·멈춤이 이리로 간다.
    weak var terminalInputSink: TerminalInputSink?

    let liveFeedStore = LiveFeedStore()
    let turnCostStore = CharacterTurnCostStore()
    let archiveFeedStore = ArchiveFeedStore()
    let speechBubbleStore = SpeechBubbleStore()
    let characterSelectionStore = CharacterSelectionStore()

    var selectedCharacterID: OfficeCharacter? {
        get {
            characterSelectionStore.selectedCharacterID
        }
        set {
            guard characterSelectionStore.selectedCharacterID != newValue
            else {
                return
            }
            liveFeedStore.selectCharacterFeed(newValue?.rawValue)
            characterSelectionStore.select(newValue)
        }
    }

    var workspaceDirectory: String {
        configuration.workdir
    }

    var databaseBaseURL: URL {
        configuration.databaseBaseURL
    }

    func fetchTerminalSessions() async throws -> [StoredTerminalSession] {
        try await database.fetchTerminalSessions()
    }

    func requestTerminalRestart(for character: OfficeCharacter) {
        terminalRestartRequest = TerminalRestartRequest(
            character: character,
            revision: (terminalRestartRequest?.revision ?? 0) + 1
        )
    }

    private let configuration: OfficeAgentConfiguration
    private let database: OfficeDatabaseClient
    private var conversationIDs: [OfficeCharacter: UUID] = [:]
    private var sessionIDs: [OfficeCharacter: String] = [:]
    private var realtimeTask: Task<Void, Never>?
    private var realtimeRefreshTask: Task<Void, Never>?
    private var realtimeSnapshotRefreshPending = false
    private var pendingRealtimeTurnIDs: Set<String> = []
    private var hasLoadedLiveFeedSnapshot = false
    private var hasReceivedRealtimeReady = false
    private var nextLiveFeedRequestSequence = 0
    private var lastAppliedLiveFeedRequestSequence = 0
    private var observedTurnStatuses: [String: LiveTurnStatus] = [:]
    /// 바로 적용으로 중단시킨 직원이다. 이 중단만은 실패·중단 상태에서도
    /// 다음 예약을 이어서 보낸다.
    private var immediateQueueDrainCharacters: Set<OfficeCharacter> = []
    private var acknowledgedWarningMessages: [OfficeCharacter: String] = [:]
    private var bubbleDismissTasks: [OfficeCharacter: Task<Void, Never>] = [:]
    private var idleChatterTask: Task<Void, Never>?
    private var workingBubbleTask: Task<Void, Never>?
    private var activeIdleChatterCharacter: OfficeCharacter?
    private var lastIdleChatterCharacter: OfficeCharacter?
    private var lastIdleChatterMessage: String?
    private var workingBubbleStep = 0
    private var hasAppliedDefaultSelection = false
    private var hasAppliedLatestConversationSelection = false
    private var hasUserChosenCharacter = false
    private static let liveFeedSnapshotLimit = 120
    private static let bubbleLifetime = Duration.seconds(10)
    private static let sessionRestoreRetryDelay = Duration.seconds(2)
    private static let realtimeTurnBatchDelay = Duration.milliseconds(250)
    private static let realtimeRefreshRetryDelay = Duration.seconds(1)
    private static let idleChatterQuietDelaySeconds = 5 ... 12
    private static let workingBubbleRotationDelay =
        Duration.milliseconds(1_400)
    private static let koreanWorkingBubbleMessages = [
        "🫡 오더 접수, 바로 갑니다",
        "🧠 뇌풀가동 ON",
        "⚡ 속도감 미쳤다",
        "🧩 퍼즐 맞추는 중",
        "👀 디테일 감시 중",
        "🛠️ 손 빠르게 움직이는 중",
        "🔎 이슈 냄새 맡는 중",
        "🚀 속도 붙었습니다",
        "🧪 검증까지 야무지게",
        "🎯 핵심만 콕콕",
        "⌨️ 키보드 열일 중",
        "📌 우선순위부터 픽",
        "🔥 집중력 만렙",
        "🧯 불씨 발견, 바로 끔",
        "🧭 길 잃지 않고 진행 중",
        "📦 결과물 포장 중",
        "💻 코드랑 합 맞추는 중",
        "⚙️ 착착 굴러갑니다",
        "🧠 뇌내 회의 중",
        "📈 진척도 쑥쑥",
        "🪄 막힘, 살짝 치워봄",
        "🧱 차근차근 쌓는 중",
        "🔬 디테일 체크 완료각",
        "🗺️ 다음 수 읽는 중",
        "🏃 할 일과 레이스 중",
        "📎 빠진 조건 없는지 확인",
        "🧰 도구함 오픈",
        "🧪 마지막까지 테스트",
        "🎮 이슈 보스전 중",
        "🌊 흐름 탔습니다",
        "🕵️ 원인 추적 모드",
        "📚 자료 폭풍 흡수 중",
        "⚡ 손가락 가속 중",
        "🧷 마감선에 딱 맞추는 중",
        "🧠 생각 정리 완료각",
        "🔄 한 번 더 크로스체크",
        "🫧 복잡한 건 가볍게 정리",
        "💎 퀄리티 반짝이게",
        "🐞 버그 잡으러 출동",
        "🧨 리스크는 미리 컷",
        "📐 각 잡고 마무리 중",
        "🧹 군더더기 정리 중",
        "🏁 끝선 보입니다",
        "🪜 한 단계씩 클리어",
        "💬 필요한 말만 딱 정리",
        "🛰️ 해답 좌표 찍는 중",
        "🥷 조용히 처리 중",
        "🍀 잘 풀리는 중",
        "🧋 당충전 상상 중",
        "✅ 결과물 곧 도착",
    ]
    private static let koreanIdleChatterMessages = [
        "☕ 카페인 1%로 버티는 중",
        "👀 새 업무 뜨면 바로 탑승",
        "🧠 뇌는 이미 출근 완료",
        "🫠 잠깐 멍도 업무의 일부",
        "🍪 간식 레이더 이상 무",
        "😎 할 일 없어도 프로답게",
        "🪑 의자와 동기화 완료",
        "🌱 아이디어 새싹 키우는 중",
        "🎧 집중 플리 고르는 중",
        "🚀 다음 오더 대기 중",
        "📌 오늘 할 일 살짝 정리 중",
        "🔍 누락된 조건 없는지 훑는 중",
        "🧭 다음 이슈 냄새 맡는 중",
        "🧩 개선 포인트 줍줍 중",
        "🛠️ 귀찮은 반복 일단 관찰 중",
        "📈 오늘의 성장 그래프 상상 중",
        "🤝 동료 찬스 기다리는 중",
        "💬 질문 하나 장전 완료",
        "🧪 작게 해보고 크게 웃자",
        "📚 새 기능 스캔 중",
        "📝 미래의 나에게 메모 중",
        "🎯 제일 중요한 거부터 픽",
        "🌤️ 눈에도 로딩 타임 필요",
        "🪴 조용히 레벨업 중",
        "🎵 집중 버튼 찾는 중",
        "🕰️ 급할수록 체크 한 번 더",
        "🛰️ 다른 각도에서 보는 중",
        "🧹 머릿속 탭 정리 중",
        "🔄 역발상 한 스푼 넣는 중",
        "💡 불편함은 개선각",
        "📩 안 읽은 메일과 눈치 게임 중",
        "🔔 알림에 집중력 털린 중",
        "🗓️ 회의가 회의를 낳는 중",
        "⏰ 퇴근 5분 전은 왜 늘 바쁠까",
        "🧾 최종본의 최종본 찾는 중",
        "📎 파일명에 진심인 편",
        "🫥 방금 뭐 했더라 모드",
        "☕ 커피 식기 전에 집중",
        "🖨️ 프린터와 기싸움 중",
        "💬 간단한 부탁, 진짜 맞죠?",
        "📊 숫자랑 눈 마주치는 중",
        "🧠 멀티태스킹은 탭 파티",
        "🚇 출근으로 체력 선지급",
        "🍱 점심 메뉴가 최대 난제",
        "🪫 충전기는 제게도 필요",
        "🧑‍💻 저장 버튼 한 번 더 꾹",
        "🕔 시계만 유난히 슬로모션",
        "📝 할 일 적다 할 일 추가",
        "🔁 같은 설명 리믹스 중",
        "📞 통화 종료와 기억 삭제 동시 실행",
        "🧯 급한 일 위에 긴급 추가",
        "🗂️ 파일 찾다 폴더 정리 중",
        "🪑 회의 끝, 숙제 시작",
        "🫠 네, 가능합니다를 너무 빨리 함",
        "😶 의견 냈더니 담당자 됨",
        "🧩 요구사항이 또 춤추는 중",
        "🌙 야근 오타, 자신감 만렙",
        "🧘 전송 전 심호흡 세 번",
        "🏃 일정이 저보다 빠름",
        "🛌 오늘의 목표, 무사 퇴근"
    ]
    private static let englishWorkingBubbleMessages = [
        "🫡 Order received, on it",
        "🧠 Full brainpower ON",
        "⚡ Speeding right through",
        "🧩 Putting the puzzle together",
        "👀 Watching the details",
        "🛠️ Moving fast",
        "🔎 Sniffing out issues",
        "🚀 Gaining momentum",
        "🧪 Thorough verification",
        "🎯 Hitting key points",
        "⌨️ Keyboard on fire",
        "📌 Prioritizing tasks",
        "🔥 Max concentration",
        "🧯 Spark spotted, putting it out",
        "🧭 Staying on track",
        "📦 Packaging deliverables",
        "💻 Syncing with code",
        "⚙️ Rolling smoothly",
        "🧠 Brainstorming internally",
        "📈 Progress soaring",
        "🪄 Clearing blockers",
        "🧱 Building step by step",
        "🔬 Polishing fine details",
        "🗺️ Reading the next move",
        "🏃 Racing against the clock",
        "📎 Checking edge cases",
        "🧰 Toolbox open",
        "🧪 Testing till the end",
        "🎮 Boss fight with bugs",
        "🌊 Riding the flow",
        "🕵️ Root cause hunting",
        "📚 Diving into docs",
        "⚡ Fingers speeding up",
        "🧷 Beating the deadline",
        "🧠 Thoughts organized",
        "🔄 Cross-checking once more",
        "🫧 Untangling complexity",
        "💎 Shining up the quality",
        "🐞 Hunting down bugs",
        "🧨 Cutting risks early",
        "📐 Tidying up edges",
        "🧹 Cleaning up clutter",
        "🏁 Finish line in sight",
        "🪜 Clearing step by step",
        "💬 Summing up key takeaways",
        "🛰️ Pinpointing the solution",
        "🥷 Handling it quietly",
        "🍀 Everything clicking into place",
        "🧋 Craving some bubble tea",
        "✅ Results landing shortly",
    ]
    private static let englishIdleChatterMessages = [
        "☕ Running on 1% caffeine",
        "👀 Ready to jump on new tasks",
        "🧠 Brain fully booted up",
        "🫠 Spacing out is part of the job",
        "🍪 Snack radar all clear",
        "😎 Keeping it pro even while idle",
        "🪑 Synchronized with my chair",
        "🌱 Sprouting new ideas",
        "🎧 Picking a focus playlist",
        "🚀 Standing by for the next order",
        "📌 Organizing today's agenda",
        "🔍 Scanning for missed details",
        "🧭 Sniffing for the next task",
        "🧩 Gathering improvement ideas",
        "🛠️ Observing repetitive workflows",
        "📈 Imagining today's growth curve",
        "🤝 Waiting for a teammate call",
        "💬 Question locked and loaded",
        "🧪 Start small, celebrate big",
        "📚 Browsing new features",
        "📝 Leaving notes for future me",
        "🎯 Picking the top priority",
        "🌤️ Eyes need loading time too",
        "🪴 Leveling up quietly",
        "🎵 Finding the focus switch",
        "🕰️ Double-checking before rushing",
        "🛰️ Viewing from another angle",
        "🧹 Closing tabs in my brain",
        "🔄 Adding a twist of reverse thinking",
        "💡 Friction is a chance to improve",
        "📩 Staring down unread emails",
        "🔔 Distracted by another ping",
        "🗓️ Meetings giving birth to meetings",
        "⏰ Why is it always busy 5m before clocking out?",
        "🧾 Looking for final_v2_final.txt",
        "📎 Serious about file naming",
        "🫥 'Wait, what was I doing?' mode",
        "☕ Focus before the coffee gets cold",
        "🖨️ Staring contest with the printer",
        "💬 'Quick favor', is it really?",
        "📊 Staring at numbers",
        "🧠 Multitasking is just a tab party",
        "🚇 Commute drained my stamina already",
        "🍱 Lunch menu: the biggest dilemma",
        "🪫 I need a charger too",
        "🧑‍💻 Hitting save one more time",
        "🕔 Clock ticking in slow motion",
        "📝 Writing todos only adds more todos",
        "🔁 Remixing the same explanation",
        "📞 Hanging up and deleting memory simultaneously",
        "🧯 Urgent task on top of an emergency",
        "🗂️ Cleaning folders while searching for a file",
        "🪑 Meeting over, homework begins",
        "🫠 Said 'Sure, I can do that' way too fast",
        "😶 Spoke up and got assigned as owner",
        "🧩 Requirements dancing around again",
        "🌙 Late-night typos with 100% confidence",
        "🧘 Three deep breaths before hitting send",
        "🏃 Deadlines moving faster than me",
        "🛌 Today's goal: clock out in peace"
    ]
    private static var workingBubbleMessages: [String] {
        OfficeLocalization.usesKorean
            ? koreanWorkingBubbleMessages
            : englishWorkingBubbleMessages
    }
    private static var idleChatterMessages: [String] {
        OfficeLocalization.usesKorean
            ? koreanIdleChatterMessages
            : englishIdleChatterMessages
    }

    init(
        startBackgroundTasks: Bool = true,
        workspaceDirectory: String? = nil,
        availableBackends: Set<AgentBackend>? = nil,
        executablePaths: [AgentBackend: String] = [:]
    ) {
        do {
            let loadedConfiguration = try CharacterConfigurationAsset.load()
            if let workspaceDirectory {
                if let availableBackends {
                    configuration = loadedConfiguration.preparingForRuntime(
                        workdir: workspaceDirectory,
                        availableBackends: availableBackends,
                        executablePaths: executablePaths
                    )
                } else {
                    configuration = loadedConfiguration.using(
                        workdir: workspaceDirectory
                    )
                }
            } else {
                configuration = loadedConfiguration
            }
        } catch {
            fatalError(error.localizedDescription)
        }
        characters = configuration.characters
        names = Dictionary(
            uniqueKeysWithValues: configuration.characters.map {
                ($0.id, $0.name)
            }
        )
        database = OfficeDatabaseClient(
            baseURL: configuration.databaseBaseURL
        )
        liveFeedStore.selectCharacterFeed(
            characterSelectionStore.selectedCharacterID?.rawValue
        )

        if startBackgroundTasks {
            Task {
                await restorePersistentState()
                await refreshLiveFeed(announcingTransitions: false)
                startRealtimeUpdates()
            }
            startIdleChatter()
            startWorkingBubbleRotation()
        }
    }

    var selectedCharacter: CharacterConfiguration? {
        guard let selectedCharacterID else {
            return nil
        }
        return characters.first { $0.id == selectedCharacterID }
    }

    var liveTurns: [LiveFeedTurn] {
        liveFeedStore.persistedTurns
    }

    var bubbles: [OfficeCharacter: String] {
        speechBubbleStore.bubbles
    }

    var archiveCabinetHitbox: CharacterHitbox {
        configuration.archiveCabinetHitbox
    }

    var selectedName: String? {
        guard let selectedCharacterID else {
            return nil
        }
        return displayName(for: selectedCharacterID)
    }

    var isSelectedCharacterRunning: Bool {
        guard let selectedCharacterID else {
            return false
        }
        return runningCharacters.contains(selectedCharacterID)
    }

    var isSelectedCharacterCompacting: Bool {
        guard let selectedCharacterID else {
            return false
        }
        return compactingCharacters.contains(selectedCharacterID)
    }

    var isCancellingSelectedCharacter: Bool {
        guard let selectedCharacterID else {
            return false
        }
        return cancellingCharacters.contains(selectedCharacterID)
    }

    func queuedCommands(
        for character: OfficeCharacter
    ) -> [QueuedCommand] {
        queuedCommands[character]?.commands ?? []
    }

    /// 예약에 실려 아직 제출되지 않은 첨부다. 임시 보관 정리에서 제외한다.
    var queuedAttachments: [PendingAttachment] {
        queuedCommands.values.flatMap { queue in
            queue.commands.flatMap(\.attachments)
        }
    }

    var selectedCharacterQueuedCommands: [QueuedCommand] {
        guard let selectedCharacterID else {
            return []
        }
        return queuedCommands(for: selectedCharacterID)
    }

    /// 실행 중인 직원에게만 예약을 받는다. 놀고 있으면 바로 제출하면 된다.
    var selectedCharacterQueueAvailability:
        SelectedCharacterQueueAvailability
    {
        let selectedCharacterID = selectedCharacterID
        return SelectedCharacterQueueAvailability.resolve(
            isReady: isReadyForSubmissions,
            isUpdatingConfiguration: isUpdatingConfiguration,
            hasSelectedCharacter: selectedCharacterID != nil,
            isSelectedCharacterRunning: selectedCharacterID.map {
                runningCharacters.contains($0)
            } ?? false,
            isFull: selectedCharacterID.map {
                queuedCommands[$0]?.isFull == true
            } ?? false
        )
    }

    var canQueueForSelectedCharacter: Bool {
        selectedCharacterQueueAvailability == .available
    }

    func selectDefaultCharacterIfNeeded() {
        guard !hasAppliedDefaultSelection else {
            return
        }
        hasAppliedDefaultSelection = true
        selectedCharacterID = .boss
    }

    /// 앱을 다시 열었을 때 첫 피드 스냅샷에서 가장 최근 대화의 직원을 선택한다.
    private func selectLatestConversationCharacterIfNeeded(
        _ turns: [LiveFeedTurn]
    ) {
        guard
            !hasAppliedLatestConversationSelection,
            !hasUserChosenCharacter,
            let latestCharacter = Self.latestConversationCharacter(
                in: turns
            )
        else {
            return
        }
        hasAppliedLatestConversationSelection = true
        guard latestCharacter != selectedCharacterID else {
            return
        }
        if unreviewedCompletedCharacters.contains(latestCharacter) {
            unreviewedCompletedCharacters.remove(latestCharacter)
        }
        selectedCharacterID = latestCharacter
    }

    static func latestConversationCharacter(
        in turns: [LiveFeedTurn]
    ) -> OfficeCharacter? {
        turns.compactMap { turn -> (OfficeCharacter, Date)? in
            guard let character = OfficeCharacter(
                rawValue: turn.characterId
            ) else {
                return nil
            }
            return (character, turn.updatedAt)
        }
        .max(by: { $0.1 < $1.1 })?.0
    }

    func displayName(for character: OfficeCharacter) -> String {
        names[character] ?? character.rawValue
    }

    func identityPrompt(for character: OfficeCharacter) -> String {
        characters.first { $0.id == character }?.identityPrompt ?? ""
    }

    func pendingQuestion(for character: OfficeCharacter) -> String? {
        pendingQuestions[character]
    }

    func questionSubmissionError(for character: OfficeCharacter) -> String? {
        questionSubmissionErrors[character]
    }

    func pendingQuestionTurnID(
        for character: OfficeCharacter
    ) -> String? {
        pendingQuestionTurnIDs[character]
    }

    func failureMessage(for character: OfficeCharacter) -> String? {
        failedCharacters[character]
    }

    func offDutyReason(for character: OfficeCharacter) -> String? {
        offDutyCharacters[character]
    }

    func dismissViewedBubble(for character: OfficeCharacter) {
        if let message = warningMessage(for: character) {
            acknowledgedWarningMessages[character] = message
        }
        guard
            pendingQuestions[character] == nil,
            !runningCharacters.contains(character),
            !compactingCharacters.contains(character)
        else {
            return
        }
        contextCompactionNotices[character] = nil
        if activeIdleChatterCharacter == character {
            activeIdleChatterCharacter = nil
        }
        bubbleDismissTasks.removeValue(forKey: character)?.cancel()
        speechBubbleStore.remove(for: character)
    }

    func select(_ character: CharacterConfiguration) {
        guard characterSelectionStore.canSelect(character.id) else {
            return
        }
        hasUserChosenCharacter = true
        var remainingCompletedCharacters =
            unreviewedCompletedCharacters
        if let selectedCharacterID {
            remainingCompletedCharacters.remove(selectedCharacterID)
        }
        remainingCompletedCharacters.remove(character.id)
        if remainingCompletedCharacters != unreviewedCompletedCharacters {
            unreviewedCompletedCharacters = remainingCompletedCharacters
        }
        contextCompactionNotices[character.id] = nil
        selectedCharacterID = character.id
        let shouldShowReadyBubble =
            pendingQuestions[character.id] == nil
            && !runningCharacters.contains(character.id)
            && !compactingCharacters.contains(character.id)
            && failedCharacters[character.id] == nil
            &&
            offDutyCharacters[character.id] == nil
        refreshBubblesAfterSelection(
            character.id,
            readyMessage: shouldShowReadyBubble
                ? OfficeLocalization.string("🫡 콜! 준비 완료")
                : nil
        )
    }

    func submit(
        _ prompt: String,
        attachmentPaths: [String] = [],
        to requestedCharacter: OfficeCharacter? = nil,
        onRequestFinished: (() -> Void)? = nil,
        onSubmissionFailed: (() -> Void)? = nil
    ) {
        let character: CharacterConfiguration?
        if let requestedCharacter {
            character = characters.first { $0.id == requestedCharacter }
        } else {
            character = selectedCharacter
        }

        guard
            let character,
            isReadyForSubmissions,
            !isUpdatingConfiguration,
            !prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
            !runningCharacters.contains(character.id),
            !compactingCharacters.contains(character.id)
        else {
            onSubmissionFailed?()
            onRequestFinished?()
            return
        }

        let questionBeingAnswered = pendingQuestions[character.id]
        let questionTurnIDBeingAnswered =
            pendingQuestionTurnIDs[character.id]
        let conversationID = conversationIDs[character.id] ?? UUID()
        conversationIDs[character.id] = conversationID
        hasUserChosenCharacter = true
        selectedCharacterID = character.id
        pendingQuestions[character.id] = nil
        pendingQuestionTurnIDs[character.id] = nil
        questionSubmissionErrors[character.id] = nil
        turnPersistenceErrors[character.id] = nil
        unreviewedCompletedCharacters.remove(character.id)
        contextCompactionNotices[character.id] = nil
        acknowledgedWarningMessages[character.id] = nil
        failedCharacters[character.id] = nil
        offDutyCharacters[character.id] = nil
        if latestQuestion?.character == character.id {
            latestQuestion = nil
        }
        runningCharacters.insert(character.id)
        showWorkingBubble(for: character.id)
        let commandID = UUID()
        let optimisticTurnID = "local-\(commandID.uuidString.lowercased())"
        let submittedAt = Date()
        liveFeedStore.insertOptimisticTurn(
            LiveFeedTurn(
                id: optimisticTurnID,
                characterId: character.id.rawValue,
                characterName: displayName(for: character.id),
                characterBackend: character.backend,
                backend: character.backend,
                model: character.model,
                effort: character.effort,
                fastMode: character.fastMode,
                externalSessionId: nil,
                conversationWorkdir: nil,
                prompt: TaskPromptPresentation.canonicalPrompt(
                    text: prompt,
                    attachmentPaths: attachmentPaths,
                    emptyPrompt: OfficeLocalization.string("첨부 파일을 확인해줘.")
                ),
                response: "",
                feedback: nil,
                status: .running,
                needsInput: false,
                errorMessage: nil,
                responseSourceWarning: nil,
                wikiProposalWarning: nil,
                startedAt: submittedAt,
                endedAt: nil,
                updatedAt: submittedAt,
                estimatedCostUsd: nil,
                sessionContext: nil,
                activities: [],
                sources: nil,
                workspace: nil
            )
        )
        latestSubmittedTurnID = optimisticTurnID
        latestSubmittedCommandID = commandID

        Task {
            defer {
                onRequestFinished?()
            }
            do {
                let started = try await database.startAgentJob(
                    character: character.id,
                    prompt: prompt,
                    conversationID: conversationID,
                    attachmentPaths: attachmentPaths
                )
                conversationIDs[character.id] = started.conversationId
                await synchronizeActiveSession(for: character.id)
                liveFeedStore.beginResponseAnimation(
                    for: started.turnId,
                    characterID: character.id.rawValue,
                    notifiesCharacterStore: false
                )
                liveFeedStore.reconcileOptimisticTurn(
                    id: optimisticTurnID,
                    with: started.turnId
                )
                latestSubmittedTurnID = started.turnId
                scheduleRealtimeFeedRefresh(turnID: started.turnId)
                latestStartedCommandID = commandID
            } catch {
                liveFeedStore.removeOptimisticTurn(id: optimisticTurnID)
                runningCharacters.remove(character.id)
                onSubmissionFailed?()
                let message = error.localizedDescription
                if AgentUsageLimitClassifier.isLimitReached(message) {
                    pendingQuestions[character.id] = nil
                    pendingQuestionTurnIDs[character.id] = nil
                    questionSubmissionErrors[character.id] = nil
                    failedCharacters[character.id] = nil
                    offDutyCharacters[character.id] = message
                    if latestQuestion?.character == character.id {
                        latestQuestion = nil
                    }
                    showBubble(
                        OfficeLocalization.string("오늘 할당량 끝, 퇴근 모드 🌙"),
                        for: character.id,
                        autoDismiss: false
                    )
                } else if let questionBeingAnswered {
                    pendingQuestions[character.id] = questionBeingAnswered
                    pendingQuestionTurnIDs[character.id] =
                        questionTurnIDBeingAnswered
                    questionSubmissionErrors[character.id] =
                        message
                    latestQuestion = PendingAgentQuestion(
                        character: character.id,
                        text: questionBeingAnswered
                    )
                    showBubble(
                        questionBeingAnswered,
                        for: character.id,
                        autoDismiss: false
                    )
                } else {
                    failedCharacters[character.id] = message
                    showBubble(
                        OfficeLocalization.format("앗, 작업 멈춤\n%@", message),
                        for: character.id,
                        autoDismiss: false
                    )
                }
            }
        }
    }

    func cancelSelectedJob() {
        guard let character = selectedCharacterID else {
            return
        }
        cancelJob(for: character)
    }

    /// 터미널 모드에서 하단 입력창의 글을 그 직원의 CLI에 타이핑한 것처럼 보낸다.
    /// 응답 생성 중이면 호출자가 예약으로 돌리므로 여기서는 받지 않는다.
    @discardableResult
    func submitToTerminal(
        _ prompt: String,
        for character: OfficeCharacter
    ) -> Bool {
        let trimmed = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        guard
            !trimmed.isEmpty,
            isReadyForSubmissions,
            !isUpdatingConfiguration,
            !runningCharacters.contains(character),
            !compactingCharacters.contains(character),
            let sink = terminalInputSink,
            sink.sendText(trimmed, to: character)
        else {
            return false
        }
        hasUserChosenCharacter = true
        pendingQuestions[character] = nil
        pendingQuestionTurnIDs[character] = nil
        questionSubmissionErrors[character] = nil
        unreviewedCompletedCharacters.remove(character)
        if latestQuestion?.character == character {
            latestQuestion = nil
        }
        return true
    }

    private func cancelJob(for character: OfficeCharacter) {
        guard
            runningCharacters.contains(character),
            !cancellingCharacters.contains(character)
        else {
            return
        }

        cancellingCharacters.insert(character)
        showBubble(
            OfficeLocalization.string("작업 정리 중... 🧹"),
            for: character,
            autoDismiss: false
        )

        // 터미널 모드에서는 CLI에 Esc를 보내 실제로 멈춘 뒤, 백엔드에 턴을
        // 중단 처리하게 한다. Claude는 중단 때 Stop 훅이 오지 않아 백엔드가
        // 스스로 알 수 없다. 그 직원의 터미널이 없으면 GUI 업무이므로
        // 기존 경로로 내려간다.
        if let sink = terminalInputSink, sink.interrupt(character) {
            Task {
                defer {
                    cancellingCharacters.remove(character)
                }
                do {
                    let result = try await database.interruptTerminalSession(
                        character: character
                    )
                    scheduleRealtimeFeedRefresh(turnID: result.turnId)
                } catch {
                    turnPersistenceErrors[character] =
                        OfficeLocalization.format("중단 요청이 삐끗했어요 · %@", error.localizedDescription)
                    immediateQueueDrainCharacters.remove(character)
                    scheduleRealtimeFeedRefresh(turnID: nil)
                }
            }
            return
        }

        Task {
            defer {
                cancellingCharacters.remove(character)
            }
            do {
                let cancelled = try await database.cancelAgentJob(
                    character: character
                )
                scheduleRealtimeFeedRefresh(turnID: cancelled.turnId)
            } catch {
                turnPersistenceErrors[character] =
                    OfficeLocalization.format("중단 요청이 삐끗했어요 · %@", error.localizedDescription)
                immediateQueueDrainCharacters.remove(character)
                scheduleRealtimeFeedRefresh(turnID: nil)
            }
        }
    }

    /// 응답 생성 중인 직원에게 다음 업무를 미리 걸어 둔다.
    @discardableResult
    func enqueueCommand(
        _ prompt: String,
        attachments: [PendingAttachment] = [],
        for character: OfficeCharacter
    ) -> Bool {
        let trimmed = prompt.trimmingCharacters(
            in: .whitespacesAndNewlines
        )
        guard
            !trimmed.isEmpty,
            isReadyForSubmissions,
            !isUpdatingConfiguration,
            runningCharacters.contains(character)
        else {
            return false
        }

        var queue = queuedCommands[character] ?? QueuedCommandQueue()
        guard
            queue.enqueue(
                QueuedCommand(prompt: trimmed, attachments: attachments)
            )
        else {
            return false
        }
        queuedCommands[character] = queue
        return true
    }

    func cancelQueuedCommand(
        id: UUID,
        for character: OfficeCharacter
    ) {
        guard var queue = queuedCommands[character] else {
            return
        }
        guard let removed = queue.remove(id: id) else {
            return
        }
        queuedCommands[character] = queue
        releaseStagedAttachments(removed.attachments)
    }

    /// 진행 중인 작업을 중단하고 고른 예약으로 곧바로 다시 물어본다.
    func applyQueuedCommandNow(
        id: UUID,
        for character: OfficeCharacter
    ) {
        guard
            var queue = queuedCommands[character],
            queue.moveToFront(id: id)
        else {
            return
        }
        queuedCommands[character] = queue

        guard runningCharacters.contains(character) else {
            // 이미 응답이 끝났으면 중단할 것이 없으므로 바로 보낸다.
            drainQueuedCommand(for: character)
            return
        }
        immediateQueueDrainCharacters.insert(character)
        cancelJob(for: character)
    }

    private func drainQueuedCommand(for character: OfficeCharacter) {
        immediateQueueDrainCharacters.remove(character)
        guard
            var queue = queuedCommands[character],
            !queue.isEmpty,
            !runningCharacters.contains(character)
        else {
            return
        }
        guard let next = queue.removeFirst() else {
            return
        }
        queuedCommands[character] = queue
        if let sink = terminalInputSink {
            // 터미널 모드에서는 예약을 CLI에 타이핑한 것처럼 보낸다. 첨부는
            // 경로를 이어 적어 CLI가 직접 읽게 한다. 그 직원의 터미널이 없어
            // 전달이 안 되면 예약을 되돌려 적어 둔 업무가 사라지지 않게 한다.
            let lines = [next.prompt]
                + next.attachments.map(\.stagedURL.path)
            if !sink.sendText(lines.joined(separator: "\n"), to: character) {
                var restored = queuedCommands[character] ?? QueuedCommandQueue()
                restored.restoreToFront(next)
                queuedCommands[character] = restored
            }
            return
        }
        submit(
            next.prompt,
            attachmentPaths: next.attachments.map(\.stagedURL.path),
            to: character,
            onRequestFinished: { [weak self] in
                self?.releaseStagedAttachments(next.attachments)
            },
            // 제출이 실패하면 예약을 되돌린다. 되돌리지 않으면 사용자가
            // 적어 둔 업무가 말없이 사라진다.
            onSubmissionFailed: { [weak self] in
                guard let self else {
                    return
                }
                var queue = self.queuedCommands[character]
                    ?? QueuedCommandQueue()
                queue.restoreToFront(next)
                self.queuedCommands[character] = queue
            }
        )
    }

    private func releaseStagedAttachments(
        _ attachments: [PendingAttachment]
    ) {
        guard
            !attachments.isEmpty,
            let inbox = try? AttachmentInbox.live()
        else {
            return
        }
        for attachment in attachments {
            inbox.remove(attachment)
        }
    }

    func updateResponseFeedback(
        turnID: String,
        feedback: TurnResponseFeedback?
    ) async {
        guard
            let turn = liveTurns.first(where: { $0.id == turnID }),
            turn.status == .completed
        else {
            return
        }

        let previousFeedback = turn.feedback
        liveFeedStore.updateFeedback(for: turnID, to: feedback)
        do {
            let storedFeedback = try await database.updateTurnFeedback(
                turnID: turnID,
                feedback: feedback
            )
            liveFeedStore.updateFeedback(for: turnID, to: storedFeedback)
        } catch {
            liveFeedStore.updateFeedback(
                for: turnID,
                to: previousFeedback
            )
            let message = error.localizedDescription
            if realtimeConnectionError != message {
                realtimeConnectionError = message
            }
        }
    }

    func agentSettings(
        for character: OfficeCharacter
    ) -> CharacterAgentSettings {
        characters.first { $0.id == character }?.agentSettings
            ?? CharacterAgentSettings(
                backend: .codex,
                model: AgentBackend.codex.defaultModel,
                effort: "high",
                fastMode: true,
                permission: .workspaceWrite
            )
    }

    func modelOptions(for backend: AgentBackend) -> [AgentModelOption] {
        if let catalog = modelCatalogs[backend],
           !catalog.visibleModels.isEmpty
        {
            return catalog.visibleModels
        }
        return backend.modelOptions.map { model in
            AgentModelOption(
                id: model,
                title: backend.modelTitle(model),
                efforts: backend.effortOptions(for: model),
                defaultEffort: backend.effortOptions(for: model).contains("high")
                    ? "high"
                    : (backend.effortOptions(for: model).first ?? "high"),
                supportsFastMode: backend.supportsFastMode(model: model)
            )
        }
    }

    func allDiscoveredModelOptions(
        for backend: AgentBackend
    ) -> [AgentModelOption] {
        modelCatalogs[backend]?.models ?? modelOptions(for: backend)
    }

    func modelOption(
        for backend: AgentBackend,
        model: String?
    ) -> AgentModelOption? {
        guard let model else { return nil }
        return allDiscoveredModelOptions(for: backend).first {
            $0.id == model
        } ?? modelOptions(for: backend).first { $0.id == model }
    }

    func defaultModelOption(for backend: AgentBackend) -> AgentModelOption {
        modelOptions(for: backend).first
            ?? AgentModelOption(
                id: backend.defaultModel,
                title: backend.modelTitle(backend.defaultModel),
                efforts: backend.effortOptions(for: backend.defaultModel),
                defaultEffort: "high",
                supportsFastMode: backend.supportsFastMode(
                    model: backend.defaultModel
                )
            )
    }

    func modelTitle(for backend: AgentBackend, model: String) -> String {
        modelOption(for: backend, model: model)?.title
            ?? backend.modelTitle(model)
    }

    func effortOptions(
        for backend: AgentBackend,
        model: String?
    ) -> [String] {
        modelOption(for: backend, model: model)?.efforts
            ?? backend.effortOptions(for: model)
    }

    func supportsFastMode(
        for backend: AgentBackend,
        model: String?
    ) -> Bool {
        modelOption(for: backend, model: model)?.supportsFastMode
            ?? backend.supportsFastMode(model: model)
    }

    func excludedModels(for backend: AgentBackend) -> Set<String> {
        Set(modelCatalogs[backend]?.excludedModels ?? [])
    }

    func refreshModelCatalog(force: Bool = false) async throws {
        applyModelCatalogSnapshot(
            try await database.fetchModelCatalog(force: force)
        )
    }

    func updateModelExclusions(
        _ excludedModels: Set<String>,
        for backend: AgentBackend
    ) async throws {
        applyModelCatalogSnapshot(
            try await database.updateModelExclusions(
                excludedModels.sorted(),
                for: backend
            )
        )
    }

    func applyModelCatalogSnapshot(_ snapshot: AgentModelCatalogSnapshot) {
        modelCatalogs = Dictionary(
            uniqueKeysWithValues: snapshot.providers.map {
                ($0.backend, $0)
            }
        )
    }

    func fetchCharacterIdentitySettings(
        for character: OfficeCharacter
    ) async throws -> CharacterIdentitySettingsDraft {
        let storedCharacters = try await database.fetchCharacters()
        guard let storedCharacter = storedCharacters.first(
            where: { $0.id == character.rawValue }
        ) else {
            throw CharacterIdentitySettingsError.characterNotFound
        }
        return CharacterIdentitySettingsDraft(
            storedCharacter: storedCharacter
        )
    }

    func saveCharacterIdentitySettings(
        name: String,
        identityPrompt: String,
        for character: OfficeCharacter
    ) async throws {
        let trimmedName = name.trimmingCharacters(
            in: .whitespacesAndNewlines
        )
        let trimmedIdentityPrompt = identityPrompt.trimmingCharacters(
            in: .whitespacesAndNewlines
        )
        guard (1...30).contains(trimmedName.count) else {
            throw CharacterIdentitySettingsError.invalidNameLength
        }
        guard (1...1_200).contains(trimmedIdentityPrompt.count) else {
            throw CharacterIdentitySettingsError.invalidIdentityPromptLength
        }
        guard isReadyForSubmissions else {
            throw CharacterIdentitySettingsError.notReady(
                sessionRestoreError
                    ?? OfficeLocalization.string("세션 복구가 끝난 뒤 설정할 수 있습니다.")
            )
        }
        guard
            !runningCharacters.contains(character),
            !compactingCharacters.contains(character)
        else {
            throw CharacterIdentitySettingsError.busy
        }
        guard !isUpdatingConfiguration else {
            throw CharacterIdentitySettingsError.configurationBusy
        }

        isUpdatingConfiguration = true
        settingsStatus = nil
        defer {
            isUpdatingConfiguration = false
        }

        let currentName = names[character] ?? character.rawValue
        let currentIdentityPrompt = self.identityPrompt(for: character)
        do {
            if trimmedIdentityPrompt != currentIdentityPrompt {
                try await database.updateIdentityPrompt(
                    trimmedIdentityPrompt,
                    for: character
                )
            }
            if trimmedName != currentName {
                try await database.updateName(trimmedName, for: character)
            }

            if trimmedIdentityPrompt != currentIdentityPrompt {
                apply(
                    identityPrompt: trimmedIdentityPrompt,
                    to: character
                )
            }
            if trimmedName != currentName {
                names[character] = trimmedName
            }
            settingsStatus = OfficeLocalization.string("설정을 저장했습니다.")
        } catch {
            await restorePersistentState()
            settingsStatus = error.localizedDescription
            throw error
        }
    }

    func updateAgentSettings(
        _ settings: CharacterAgentSettings,
        for character: OfficeCharacter
    ) async {
        settingsStatus = nil
        guard isReadyForSubmissions else {
            settingsStatus =
                sessionRestoreError ?? OfficeLocalization.string("세션 복구가 끝난 뒤 설정할 수 있습니다.")
            return
        }
        guard
            !runningCharacters.contains(character),
            !compactingCharacters.contains(character)
        else {
            settingsStatus =
                OfficeLocalization.string("진행 중인 업무나 컨텍스트 압축이 끝난 뒤 설정할 수 있습니다.")
            return
        }
        guard !isUpdatingConfiguration else {
            settingsStatus = OfficeLocalization.string("다른 설정을 저장하는 중입니다.")
            return
        }
        isUpdatingConfiguration = true
        defer {
            isUpdatingConfiguration = false
        }
        do {
            try await database.updateSettings(settings, for: character)
            apply(settings, to: character)
            settingsStatus = OfficeLocalization.string("설정을 저장했습니다.")
        } catch {
            let message = error.localizedDescription
            await restorePersistentState()
            settingsStatus = message
        }
    }

    func autoCompactPercent(for character: OfficeCharacter) -> Int {
        min(95, max(20, autoCompactPercents[character] ?? 90))
    }

    func hasActiveSession(for character: OfficeCharacter) -> Bool {
        !(sessionIDs[character] ?? "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .isEmpty
    }

    func refreshActiveSessionAvailability(
        for character: OfficeCharacter
    ) async throws -> Bool {
        let activeSessions = try await database.fetchActiveSessions()
        let restored = ActiveSessionRestoreState(activeSessions: activeSessions)
        conversationIDs[character] = restored.conversationIDs[character]
        sessionIDs[character] = restored.sessionIDs[character]
        return hasActiveSession(for: character)
    }

    func sessionContextLimit(for character: OfficeCharacter) -> Int? {
        liveTurns.first {
            $0.characterId == character.rawValue &&
                $0.sessionContext != nil
        }?.sessionContext?.limitTokens
    }

    func contextCompactionNotice(
        for character: OfficeCharacter
    ) -> ContextCompactionNotice? {
        contextCompactionNotices[character]
    }

    func updateAutoCompactPercent(
        _ percent: Int,
        for character: OfficeCharacter
    ) async throws {
        guard isReadyForSubmissions else {
            throw AgentContextCompactionError.notReady(
                sessionRestoreError ?? OfficeLocalization.string("세션 복구가 끝난 뒤 설정할 수 있습니다.")
            )
        }
        guard
            !runningCharacters.contains(character),
            !compactingCharacters.contains(character)
        else {
            throw AgentContextCompactionError.busy
        }
        guard !isUpdatingConfiguration else {
            throw AgentContextCompactionError.configurationBusy
        }
        let normalized = min(95, max(20, percent))
        isUpdatingConfiguration = true
        defer { isUpdatingConfiguration = false }
        let stored = try await database.updateAutoCompactPercent(
            normalized,
            for: character
        )
        var updated = autoCompactPercents
        updated[character] = stored.autoCompactPercent
        autoCompactPercents = updated
    }

    func compactContext(
        for character: OfficeCharacter
    ) async throws -> ContextCompactionResult {
        guard isReadyForSubmissions else {
            throw AgentContextCompactionError.notReady(
                sessionRestoreError ?? OfficeLocalization.string("세션 복구가 끝난 뒤 압축할 수 있습니다.")
            )
        }
        guard
            !runningCharacters.contains(character),
            !compactingCharacters.contains(character)
        else {
            throw AgentContextCompactionError.busy
        }
        guard !isUpdatingConfiguration else {
            throw AgentContextCompactionError.configurationBusy
        }
        guard try await refreshActiveSessionAvailability(for: character) else {
            throw AgentContextCompactionError.noSession
        }
        compactingCharacters.insert(character)
        defer { compactingCharacters.remove(character) }
        let result = try await database.compactContext(for: character)
        _ = await refreshLiveFeed(announcingTransitions: false)
        return result
    }

    func characterHistory(
        for character: OfficeCharacter
    ) async throws -> CharacterHistory {
        try await database.fetchCharacterHistory(for: character)
    }

    func globalHistory(
        character: OfficeCharacter?,
        from: Date?,
        to: Date?
    ) async throws -> [GlobalHistoryTurn] {
        try await database.fetchGlobalHistory(
            character: character,
            from: from,
            to: to
        )
    }

    func archiveFeed(
        query: String?,
        limit: Int,
        offset: Int
    ) async throws -> ArchiveFeedPage {
        try await database.fetchArchiveFeed(
            query: query,
            limit: limit,
            offset: offset
        )
    }

    private func showBubble(
        _ text: String,
        for character: OfficeCharacter,
        autoDismiss: Bool = true
    ) {
        if activeIdleChatterCharacter == character {
            activeIdleChatterCharacter = nil
        }
        bubbleDismissTasks.removeValue(forKey: character)?.cancel()
        speechBubbleStore.set(text, for: character)

        guard autoDismiss else {
            return
        }

        bubbleDismissTasks[character] = Task { [weak self] in
            try? await Task.sleep(for: Self.bubbleLifetime)
            guard !Task.isCancelled else {
                return
            }
            self?.speechBubbleStore.remove(for: character)
            if self?.activeIdleChatterCharacter == character {
                self?.activeIdleChatterCharacter = nil
            }
            self?.bubbleDismissTasks[character] = nil
        }
    }

    private func refreshBubblesAfterSelection(
        _ character: OfficeCharacter,
        readyMessage: String?
    ) {
        for task in bubbleDismissTasks.values {
            task.cancel()
        }
        bubbleDismissTasks = [:]
        activeIdleChatterCharacter = nil
        var updatedBubbles = bubbles.filter {
            pendingQuestions[$0.key] != nil
                || runningCharacters.contains($0.key)
                || compactingCharacters.contains($0.key)
                || contextCompactionNotices[$0.key] != nil
                || !isWarningAcknowledged(for: $0.key)
                    && warningMessage(for: $0.key) != nil
        }
        if let readyMessage {
            updatedBubbles[character] = readyMessage
        }
        speechBubbleStore.replace(with: updatedBubbles)

        guard readyMessage != nil else {
            return
        }
        bubbleDismissTasks[character] = Task { [weak self] in
            try? await Task.sleep(for: Self.bubbleLifetime)
            guard !Task.isCancelled else {
                return
            }
            self?.speechBubbleStore.remove(for: character)
            self?.bubbleDismissTasks[character] = nil
        }
    }

    private func startIdleChatter() {
        idleChatterTask = Task { [weak self] in
            do {
                while !Task.isCancelled {
                    let quietDelay = Int.random(
                        in: Self.idleChatterQuietDelaySeconds
                    )
                    try await Task.sleep(for: .seconds(quietDelay))

                    if self?.showIdleChatter() == true {
                        try await Task.sleep(for: Self.bubbleLifetime)
                    }
                }
            } catch {
                return
            }
        }
    }

    private func showIdleChatter() -> Bool {
        guard activeIdleChatterCharacter == nil else {
            return false
        }

        let idleCharacters = SpeechBubbleIdleChatterPolicy.candidates(
            characters: characters,
            runningCharacters: runningCharacters.union(
                compactingCharacters
            ),
            occupiedCharacters: Set(bubbles.keys),
            questionCharacters: Set(pendingQuestions.keys),
            failedCharacters: Set(failedCharacters.keys),
            offDutyCharacters: Set(offDutyCharacters.keys),
            lastCharacter: lastIdleChatterCharacter
        )
        let messages = SpeechBubbleIdleChatterPolicy.messages(
            from: Self.idleChatterMessages,
            excluding: lastIdleChatterMessage
        )

        guard
            let character = idleCharacters.randomElement(),
            let message = messages.randomElement()
        else {
            return false
        }

        lastIdleChatterCharacter = character.id
        lastIdleChatterMessage = message
        showBubble(message, for: character.id)
        activeIdleChatterCharacter = character.id
        return true
    }

    private func startWorkingBubbleRotation() {
        workingBubbleTask?.cancel()
        workingBubbleTask = Task { [weak self] in
            do {
                while !Task.isCancelled {
                    try await Task.sleep(
                        for: Self.workingBubbleRotationDelay
                    )
                    self?.rotateWorkingBubbles()
                }
            } catch {
                return
            }
        }
    }

    private func rotateWorkingBubbles() {
        guard !runningCharacters.isEmpty else {
            return
        }
        workingBubbleStep =
            (workingBubbleStep + 1) % Self.workingBubbleMessages.count
        for character in runningCharacters {
            showWorkingBubble(for: character)
        }
    }

    private func showWorkingBubble(for character: OfficeCharacter) {
        guard
            runningCharacters.contains(character),
            !cancellingCharacters.contains(character)
        else {
            return
        }
        let characterOffset =
            (OfficeCharacter.allCases.firstIndex(of: character) ?? 0) * 3
        let messageIndex =
            (workingBubbleStep + characterOffset)
            % Self.workingBubbleMessages.count
        showBubble(
            Self.workingBubbleMessages[messageIndex],
            for: character,
            autoDismiss: false
        )
    }

    private func restorePersistentState() async {
        isReadyForSubmissions = false
        sessionRestoreError = nil
        conversationIDs = [:]
        sessionIDs = [:]

        while !Task.isCancelled {
            do {
                let storedCharacters = try await database.fetchCharacters()
                var restoredAutoCompactPercents = Dictionary(
                    uniqueKeysWithValues:
                        OfficeCharacter.allCases.map { ($0, 90) }
                )
                for stored in storedCharacters {
                    guard let character = OfficeCharacter(rawValue: stored.id)
                    else {
                        continue
                    }
                    names[character] = stored.name
                    apply(
                        CharacterAgentSettings(
                            backend: stored.backend,
                            model: stored.model,
                            effort: stored.effort,
                            fastMode: stored.fastMode,
                            permission: AgentPermission(
                                cliValue: stored.permission
                            )
                        ),
                        to: character
                    )
                    apply(
                        identityPrompt: stored.identityPrompt,
                        to: character
                    )
                    restoredAutoCompactPercents[character] = min(
                        95,
                        max(20, stored.autoCompactPercent ?? 90)
                    )
                }
                autoCompactPercents = restoredAutoCompactPercents

                if let snapshot = try? await database.fetchModelCatalog() {
                    applyModelCatalogSnapshot(snapshot)
                }

                let activeSessions = try await database.fetchActiveSessions()
                applyActiveSessionRestoreState(
                    ActiveSessionRestoreState(activeSessions: activeSessions)
                )
                sessionRestoreError = nil
                isReadyForSubmissions = true
                return
            } catch {
                conversationIDs = [:]
                sessionIDs = [:]
                sessionRestoreError = error.localizedDescription
                do {
                    try await Task.sleep(
                        for: Self.sessionRestoreRetryDelay
                    )
                } catch {
                    return
                }
            }
        }
    }

    private func synchronizeActiveSession(for character: OfficeCharacter) async {
        guard let activeSessions = try? await database.fetchActiveSessions()
        else {
            return
        }
        let restored = ActiveSessionRestoreState(activeSessions: activeSessions)
        guard
            let conversationID = restored.conversationIDs[character],
            let sessionID = restored.sessionIDs[character]
        else {
            return
        }
        conversationIDs[character] = conversationID
        sessionIDs[character] = sessionID
    }

    private func applyActiveSessionRestoreState(
        _ restored: ActiveSessionRestoreState
    ) {
        conversationIDs = restored.conversationIDs
        sessionIDs = restored.sessionIDs
    }

    private func startRealtimeUpdates() {
        realtimeTask?.cancel()
        realtimeRefreshTask?.cancel()
        realtimeRefreshTask = nil
        realtimeSnapshotRefreshPending = false
        pendingRealtimeTurnIDs.removeAll()
        realtimeTask = Task { [weak self] in
            guard let self else {
                return
            }
            while !Task.isCancelled {
                let socket = URLSession.shared.webSocketTask(
                    with: self.database.realtimeWebSocketURL
                )
                socket.resume()
                do {
                    while !Task.isCancelled {
                        let message = try await socket.receive()
                        if !self.isRealtimeConnected {
                            self.isRealtimeConnected = true
                        }
                        if self.realtimeConnectionError != nil {
                            self.realtimeConnectionError = nil
                        }
                        let event = self.realtimeEvent(from: message)
                        let handledRealtimeState = event.map {
                            self.applyRealtimeEvent($0)
                        } ?? false
                        if event?.type == "ready" {
                            let shouldRefreshSnapshot =
                                self.hasReceivedRealtimeReady
                                    || !self.hasLoadedLiveFeedSnapshot
                            self.hasReceivedRealtimeReady = true
                            if shouldRefreshSnapshot {
                                self.scheduleRealtimeFeedRefresh(turnID: nil)
                            }
                        } else if !handledRealtimeState {
                            self.scheduleRealtimeFeedRefresh(
                                turnID: event?.turnId
                            )
                        }
                    }
                } catch {
                    if self.isRealtimeConnected {
                        self.isRealtimeConnected = false
                    }
                    if !Task.isCancelled {
                        let message = error.localizedDescription
                        if self.realtimeConnectionError != message {
                            self.realtimeConnectionError = message
                        }
                    }
                }
                socket.cancel(with: .goingAway, reason: nil)
                if Task.isCancelled {
                    return
                }
                try? await Task.sleep(for: .seconds(2))
            }
        }
    }

    private func realtimeEvent(
        from message: URLSessionWebSocketTask.Message
    ) -> RealtimeFeedEvent? {
        let data: Data
        switch message {
        case .data(let messageData):
            data = messageData
        case .string(let text):
            data = Data(text.utf8)
        @unknown default:
            return nil
        }
        return try? JSONDecoder().decode(
            RealtimeFeedEvent.self,
            from: data
        )
    }

    @discardableResult
    func applyRealtimeEvent(
        _ event: RealtimeFeedEvent,
        schedulesFeedRefresh: Bool = true
    ) -> Bool {
        switch event.type {
        case "ready":
            Task { [weak self] in
                try? await self?.refreshModelCatalog()
            }
            let synchronized = Set(
                (event.compactingCharacterIds ?? []).compactMap(
                    OfficeCharacter.init(rawValue:)
                )
            )
            let newlyCompacting = synchronized.subtracting(
                compactingCharacters
            )
            compactingCharacters = synchronized
            for character in newlyCompacting {
                contextCompactionNotices[character] = nil
                showContextCompactionStarted(
                    for: character,
                    automatic: false
                )
            }
            return true

        case "context.compaction.started":
            guard let character = event.officeCharacter else {
                return false
            }
            compactingCharacters.insert(character)
            contextCompactionNotices[character] = nil
            showContextCompactionStarted(
                for: character,
                automatic: event.automatic == true
            )
            return true

        case "context.compacted":
            guard let character = event.officeCharacter else {
                return false
            }
            compactingCharacters.remove(character)
            contextCompactionNotices[character] = .completed(
                automatic: event.automatic == true,
                preTokens: event.preTokens,
                postTokens: event.postTokens
            )
            let title = event.automatic == true
                ? OfficeLocalization.string("자동 컨텍스트 압축 완료")
                : OfficeLocalization.string("컨텍스트 압축 완료")
            showBubble("✅ \(title)", for: character)
            if schedulesFeedRefresh {
                scheduleRealtimeFeedRefresh(turnID: nil)
            }
            return true

        case "context.compaction.failed":
            guard let character = event.officeCharacter else {
                return false
            }
            compactingCharacters.remove(character)
            contextCompactionNotices[character] = .failed(
                automatic: event.automatic == true,
                message: event.errorMessage.map(OfficeLocalization.systemMessage)
            )
            let title = event.automatic == true
                ? OfficeLocalization.string("자동 컨텍스트 압축 실패")
                : OfficeLocalization.string("컨텍스트 압축 실패")
            var lines = ["⚠️ \(title)"]
            if let detail = event.errorMessage?.trimmingCharacters(
                in: .whitespacesAndNewlines
            ), !detail.isEmpty {
                lines.append(OfficeLocalization.systemMessage(detail))
            }
            showBubble(
                lines.joined(separator: "\n"),
                for: character,
                autoDismiss: false
            )
            return true

        case "terminal.changed":
            terminalSessionRevision &+= 1
            return true

        case "model-catalog.changed":
            Task { [weak self] in
                try? await self?.refreshModelCatalog()
            }
            return true

        default:
            return false
        }
    }

    private func showContextCompactionStarted(
        for character: OfficeCharacter,
        automatic: Bool
    ) {
        let title = automatic
            ? OfficeLocalization.string("자동 컨텍스트 압축 중")
            : OfficeLocalization.string("컨텍스트 압축 중")
        showBubble("🗜️ \(title)", for: character, autoDismiss: false)
    }

    private func scheduleRealtimeFeedRefresh(turnID: String?) {
        if let turnID = turnID?.trimmingCharacters(
            in: .whitespacesAndNewlines
        ), !turnID.isEmpty {
            pendingRealtimeTurnIDs.insert(turnID)
        } else {
            realtimeSnapshotRefreshPending = true
        }
        guard realtimeRefreshTask == nil else {
            return
        }

        realtimeRefreshTask = Task { [weak self] in
            guard let self else {
                return
            }
            defer {
                self.realtimeRefreshTask = nil
            }

            while
                self.realtimeSnapshotRefreshPending
                    || !self.pendingRealtimeTurnIDs.isEmpty
            {
                guard !Task.isCancelled else {
                    return
                }

                if self.realtimeSnapshotRefreshPending {
                    self.realtimeSnapshotRefreshPending = false
                    let refreshed = await self.refreshLiveFeed(
                        announcingTransitions: true
                    )
                    if !refreshed, !Task.isCancelled {
                        self.realtimeSnapshotRefreshPending = true
                        do {
                            try await Task.sleep(
                                for: Self.realtimeRefreshRetryDelay
                            )
                        } catch {
                            return
                        }
                    }
                    continue
                }

                do {
                    try await Task.sleep(
                        for: Self.realtimeTurnBatchDelay
                    )
                } catch {
                    return
                }
                let turnIDs = self.pendingRealtimeTurnIDs
                self.pendingRealtimeTurnIDs.removeAll()
                for turnID in turnIDs {
                    let refreshed = await self.refreshLiveFeedTurn(
                        turnID,
                        announcingTransitions: true
                    )
                    if !refreshed {
                        self.realtimeSnapshotRefreshPending = true
                        break
                    }
                }
                if self.realtimeSnapshotRefreshPending, !Task.isCancelled {
                    do {
                        try await Task.sleep(
                            for: Self.realtimeRefreshRetryDelay
                        )
                    } catch {
                        return
                    }
                }
            }
        }
    }

    @discardableResult
    private func refreshLiveFeed(
        announcingTransitions: Bool
    ) async -> Bool {
        nextLiveFeedRequestSequence += 1
        let requestSequence = nextLiveFeedRequestSequence

        do {
            let snapshot = try await database.fetchLiveFeed(
                limit: Self.liveFeedSnapshotLimit
            )
            guard requestSequence > lastAppliedLiveFeedRequestSequence else {
                return true
            }
            lastAppliedLiveFeedRequestSequence = requestSequence
            turnCostStore.apply(snapshot.costSummary)
            applyLiveFeed(
                snapshot.turns,
                announcingTransitions: announcingTransitions
            )
            hasLoadedLiveFeedSnapshot = true
            liveFeedStore.finishInitialLoading()
            if realtimeConnectionError != nil {
                realtimeConnectionError = nil
            }
            return true
        } catch {
            let message = error.localizedDescription
            if realtimeConnectionError != message {
                realtimeConnectionError = message
            }
            return false
        }
    }

    @discardableResult
    private func refreshLiveFeedTurn(
        _ turnID: String,
        announcingTransitions: Bool
    ) async -> Bool {
        do {
            let snapshot = try await database.fetchLiveFeedTurn(id: turnID)
            let turn = snapshot.turn
            turnCostStore.apply(snapshot.costSummary)
            if let character = OfficeCharacter(rawValue: turn.characterId) {
                // session.changed는 활성 세션을 DB에 확정한 뒤 전송된다.
                // 새 CLI 세션의 ID가 작업 시작 응답보다 늦게 도착해도
                // 여기서 다시 동기화한다.
                await synchronizeActiveSession(for: character)
            }
            var turns = liveTurns.filter { $0.id != turn.id }
            turns.append(turn)
            turns.sort {
                if $0.startedAt == $1.startedAt {
                    return $0.id > $1.id
                }
                return $0.startedAt > $1.startedAt
            }
            applyLiveFeed(
                LiveFeedStore.snapshotTurns(
                    from: turns,
                    recentLimit: Self.liveFeedSnapshotLimit
                ),
                announcingTransitions: announcingTransitions
            )
            if realtimeConnectionError != nil {
                realtimeConnectionError = nil
            }
            return true
        } catch {
            let message = error.localizedDescription
            if realtimeConnectionError != message {
                realtimeConnectionError = message
            }
            return false
        }
    }

    private func applyLiveFeed(
        _ turns: [LiveFeedTurn],
        announcingTransitions: Bool
    ) {
        guard hasFeedRevisionChanged(turns) else {
            return
        }

        let previousStatuses = observedTurnStatuses
        let previousRunningCharacters = runningCharacters
        if !hasLoadedLiveFeedSnapshot {
            liveFeedStore.restoreResponseAnimations(for: turns)
        } else if announcingTransitions {
            for turn in turns where
                turn.status.isRunning && previousStatuses[turn.id] == nil
            {
                liveFeedStore.beginResponseAnimation(
                    for: turn.id,
                    characterID: turn.characterId,
                    notifiesCharacterStore: false
                )
            }
        }
        liveFeedStore.replace(with: turns)
        archiveFeedStore.replaceIfNeeded(with: turns)
        selectLatestConversationCharacterIfNeeded(turns)
        observedTurnStatuses = Dictionary(
            uniqueKeysWithValues: turns.map { ($0.id, $0.status) }
        )

        let runningTurns = turns.filter { $0.status.isRunning }
        let updatedRunningCharacters = Set(
            runningTurns.compactMap {
                OfficeCharacter(rawValue: $0.characterId)
            }
        ).union(
            liveFeedStore.optimisticCharacterIDs.compactMap(
                OfficeCharacter.init(rawValue:)
            )
        )
        if runningCharacters != updatedRunningCharacters {
            runningCharacters = updatedRunningCharacters
        }
        for character in runningCharacters
        where
            !previousRunningCharacters.contains(character)
                || bubbles[character] == nil
        {
            showWorkingBubble(for: character)
        }

        var latestTurns:
            [OfficeCharacter: LiveFeedTurn] = [:]
        for turn in turns {
            guard
                let character = OfficeCharacter(
                    rawValue: turn.characterId
                ),
                latestTurns[character] == nil
            else {
                continue
            }
            latestTurns[character] = turn
        }

        for (character, turn) in latestTurns {
            if turn.status == .completed, turn.needsInput {
                if pendingQuestions[character] != turn.response {
                    pendingQuestions[character] = turn.response
                    latestQuestion = PendingAgentQuestion(
                        character: character,
                        text: turn.response
                    )
                    showBubble(
                        turn.response,
                        for: character,
                        autoDismiss: false
                    )
                }
                pendingQuestionTurnIDs[character] = turn.id
            } else if
                !turn.status.isRunning,
                pendingQuestions[character] != nil
            {
                pendingQuestions[character] = nil
                pendingQuestionTurnIDs[character] = nil
            }

            if
                !announcingTransitions,
                turn.status == .failed || turn.status == .interrupted
            {
                applyTerminalTurn(turn, for: character)
            } else if turn.status == .completed, !turn.needsInput {
                acknowledgedWarningMessages[character] = nil
                if failedCharacters[character] != nil {
                    failedCharacters[character] = nil
                }
                if offDutyCharacters[character] != nil {
                    offDutyCharacters[character] = nil
                }
            }

            guard
                announcingTransitions,
                previousStatuses[turn.id]?.isRunning == true,
                !turn.status.isRunning
            else {
                continue
            }
            latestTerminalTurnID = turn.id
            if turn.status == .completed {
                unreviewedCompletedCharacters.insert(character)
                latestCompletedTurnID = turn.id
            }
            applyTerminalTurn(turn, for: character)

            let isImmediateRequest = immediateQueueDrainCharacters
                .contains(character)
            if QueuedCommandDrainPolicy.shouldDrain(
                status: turn.status,
                isImmediateRequest: isImmediateRequest
            ) {
                drainQueuedCommand(for: character)
            } else if isImmediateRequest {
                immediateQueueDrainCharacters.remove(character)
            }
        }
    }

    private func hasFeedRevisionChanged(
        _ turns: [LiveFeedTurn]
    ) -> Bool {
        guard liveTurns.count == turns.count else {
            return true
        }

        return zip(liveTurns, turns).contains { $0 != $1 }
    }

    private func applyTerminalTurn(
        _ turn: LiveFeedTurn,
        for character: OfficeCharacter
    ) {
        switch turn.status {
        case .completed:
            acknowledgedWarningMessages[character] = nil
            if failedCharacters[character] != nil {
                failedCharacters[character] = nil
            }
            if offDutyCharacters[character] != nil {
                offDutyCharacters[character] = nil
            }
            if turn.needsInput {
                if pendingQuestions[character] != turn.response {
                    pendingQuestions[character] = turn.response
                    latestQuestion = PendingAgentQuestion(
                        character: character,
                        text: turn.response
                    )
                    showBubble(
                        turn.response,
                        for: character,
                        autoDismiss: false
                    )
                }
                pendingQuestionTurnIDs[character] = turn.id
            } else {
                if pendingQuestions[character] != nil {
                    pendingQuestions[character] = nil
                }
                pendingQuestionTurnIDs[character] = nil
                showBubble(turn.response, for: character)
            }
        case .failed, .interrupted:
            let originalMessage = turn.errorMessage ?? OfficeLocalization.string("작업이 멈췄어요.")
            let message = OfficeLocalization.systemMessage(originalMessage)
            if AgentUsageLimitClassifier.isLimitReached(originalMessage) {
                if failedCharacters[character] != nil {
                    failedCharacters[character] = nil
                }
                if offDutyCharacters[character] != message {
                    offDutyCharacters[character] = message
                }
                if !isWarningAcknowledged(for: character) {
                    showBubble(
                        OfficeLocalization.string("오늘 할당량 끝, 퇴근 모드 🌙"),
                        for: character,
                        autoDismiss: false
                    )
                }
            } else {
                if offDutyCharacters[character] != nil {
                    offDutyCharacters[character] = nil
                }
                if failedCharacters[character] != message {
                    failedCharacters[character] = message
                }
                if !isWarningAcknowledged(for: character) {
                    showBubble(
                        OfficeLocalization.format("앗, 작업 멈춤\n%@", message),
                        for: character,
                        autoDismiss: false
                    )
                }
            }
        case .pending, .running:
            break
        }
    }

    private func persistTurn(
        _ turn: DatabaseTurn,
        for character: OfficeCharacter
    ) async {
        var attempt = 0
        while !Task.isCancelled {
            do {
                try await database.recordTurn(turn)
                turnPersistenceErrors[character] = nil
                return
            } catch {
                attempt += 1
                turnPersistenceErrors[character] =
                    error.localizedDescription
                try? await Task.sleep(
                    for: .seconds(min(attempt, 10))
                )
            }
        }
    }

    private func apply(
        _ settings: CharacterAgentSettings,
        to character: OfficeCharacter
    ) {
        guard let index = characters.firstIndex(
            where: { $0.id == character }
        ) else {
            return
        }
        let previous = characters[index]
        if CharacterSessionInvalidationPolicy.requiresNewSession(
            previous: previous.agentSettings,
            updated: settings
        ) {
            invalidateSession(for: character)
        }
        characters[index] = previous.applying(settings)
    }

    private func apply(
        identityPrompt: String,
        to character: OfficeCharacter
    ) {
        guard let index = characters.firstIndex(
            where: { $0.id == character }
        ) else {
            return
        }
        let previous = characters[index]
        guard previous.identityPrompt != identityPrompt else {
            return
        }
        characters[index] = previous.applying(identityPrompt: identityPrompt)
    }

    private func invalidateSession(for character: OfficeCharacter) {
        conversationIDs[character] = nil
        sessionIDs[character] = nil
        pendingQuestions[character] = nil
        pendingQuestionTurnIDs[character] = nil
        questionSubmissionErrors[character] = nil
        acknowledgedWarningMessages[character] = nil
        failedCharacters[character] = nil
        offDutyCharacters[character] = nil
        bubbleDismissTasks.removeValue(forKey: character)?.cancel()
        speechBubbleStore.remove(for: character)
        if latestQuestion?.character == character {
            latestQuestion = nil
        }
    }

    private func warningMessage(for character: OfficeCharacter) -> String? {
        failedCharacters[character] ?? offDutyCharacters[character]
    }

    private func isWarningAcknowledged(
        for character: OfficeCharacter
    ) -> Bool {
        guard let message = warningMessage(for: character) else {
            return false
        }
        return acknowledgedWarningMessages[character] == message
    }

    private func characterWithCurrentName(
        _ character: CharacterConfiguration
    ) -> CharacterConfiguration {
        CharacterConfiguration(
            id: character.id,
            name: displayName(for: character.id),
            seat: character.seat,
            backend: character.backend,
            identityPrompt: character.identityPrompt,
            model: character.model,
            effort: character.effort,
            fastMode: character.fastMode,
            permission: character.permission,
            executablePath: character.executablePath,
            hitbox: character.hitbox,
            monitorHitbox: character.monitorHitbox,
            bubble: character.bubble
        )
    }
}

private enum AgentContextCompactionError: LocalizedError {
    case notReady(String)
    case noSession
    case busy
    case configurationBusy

    var errorDescription: String? {
        switch self {
        case let .notReady(message):
            message
        case .noSession:
            OfficeLocalization.string("압축할 활성 CLI 세션이 없습니다.")
        case .busy:
            OfficeLocalization.string("현재 업무나 컨텍스트 압축이 끝난 뒤 다시 시도하세요.")
        case .configurationBusy:
            OfficeLocalization.string("다른 설정 저장이 끝난 뒤 다시 시도하세요.")
        }
    }
}

private enum CharacterIdentitySettingsError: LocalizedError {
    case characterNotFound
    case invalidNameLength
    case invalidIdentityPromptLength
    case notReady(String)
    case busy
    case configurationBusy

    var errorDescription: String? {
        switch self {
        case .characterNotFound:
            OfficeLocalization.string("직원 설정을 찾을 수 없습니다.")
        case .invalidNameLength:
            OfficeLocalization.string("이름은 1자 이상 30자 이하여야 합니다.")
        case .invalidIdentityPromptLength:
            OfficeLocalization.string("업무 지침은 1자 이상 1,200자 이하여야 합니다.")
        case let .notReady(message):
            message
        case .busy:
            OfficeLocalization.string("이 직원의 업무나 컨텍스트 압축이 끝난 뒤 설정할 수 있습니다.")
        case .configurationBusy:
            OfficeLocalization.string("다른 설정을 저장하는 중입니다.")
        }
    }
}
