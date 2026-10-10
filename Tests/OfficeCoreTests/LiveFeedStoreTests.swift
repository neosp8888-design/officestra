// 이 파일은 서버 응답 전 임시 업무 카드의 유지와 실제 턴 치환을 검증한다.

import Combine
import OfficeCore
import XCTest
@testable import OfficeGame

@MainActor
final class LiveFeedStoreTests: XCTestCase {
    func testCharacterSelectionPublishesOnlyWhenValueChanges() {
        let store = CharacterSelectionStore()
        var publicationCount = 0
        let cancellable = store.objectWillChange.sink {
            publicationCount += 1
        }

        store.select(.boss)
        store.select(.leftMan)
        store.select(.leftMan)

        XCTAssertEqual(store.selectedCharacterID, .leftMan)
        XCTAssertEqual(publicationCount, 1)
        withExtendedLifetime(cancellable) {}
    }

    func testCharacterSelectionWillChangePrecedesPublishedSnapshot() {
        let store = CharacterSelectionStore()
        store.completeConversationLoading(for: .boss)
        var targetCharacterID: OfficeCharacter?
        var observedCharacterID: OfficeCharacter?
        var observedLoading = true
        let cancellable = store.selectionWillChange.sink { characterID in
            targetCharacterID = characterID
            observedCharacterID = store.selectedCharacterID
            observedLoading = store.isConversationLoading
        }

        store.select(.leftMan)

        XCTAssertEqual(targetCharacterID, .leftMan)
        XCTAssertEqual(
            observedCharacterID,
            .boss,
            "전환 차폐가 이전 snapshot을 보는 동안 먼저 설치되어야 합니다."
        )
        XCTAssertFalse(observedLoading)
        XCTAssertEqual(store.selectedCharacterID, .leftMan)
        XCTAssertTrue(store.isConversationLoading)
        withExtendedLifetime(cancellable) {}
    }

    func testCharacterTurnIndexTracksMergedAndReconciledTurns() {
        let store = LiveFeedStore()
        let submittedAt = Date(timeIntervalSinceReferenceDate: 900)
        let bossTurn = makeTurn(
            id: "boss-turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "총괄",
            startedAt: submittedAt
        )
        let optimistic = makeTurn(
            id: "local-turn",
            characterID: OfficeCharacter.rightWoman.rawValue,
            prompt: "수정",
            startedAt: submittedAt.addingTimeInterval(1)
        )

        store.replace(with: [bossTurn])
        store.insertOptimisticTurn(optimistic)

        XCTAssertEqual(
            store.turns(for: OfficeCharacter.boss.rawValue).map(\.id),
            ["boss-turn"]
        )
        XCTAssertEqual(
            store.turns(for: OfficeCharacter.rightWoman.rawValue).map(\.id),
            ["local-turn"]
        )

        store.reconcileOptimisticTurn(
            id: "local-turn",
            with: "server-turn"
        )

        XCTAssertEqual(
            store.turns(for: OfficeCharacter.rightWoman.rawValue).map(\.id),
            ["server-turn"]
        )
    }

    func testHiddenCharacterFeedStagesUpdatesUntilPresented() {
        let store = LiveFeedStore()
        let startedAt = Date(timeIntervalSinceReferenceDate: 950)
        let initialTurn = makeTurn(
            id: "boss-turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "초기 업무",
            startedAt: startedAt
        )
        store.replace(with: [initialTurn])

        let characterStore = store.characterStore(
            for: OfficeCharacter.boss.rawValue
        )
        var publicationCount = 0
        let cancellable = characterStore.objectWillChange.sink {
            publicationCount += 1
        }
        let updatedTurn = makeTurn(
            id: "boss-turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "초기 업무",
            response: "진행 중",
            startedAt: startedAt,
            updatedAt: startedAt.addingTimeInterval(1)
        )

        store.replace(with: [updatedTurn])

        XCTAssertEqual(characterStore.turns, [initialTurn])
        XCTAssertEqual(publicationCount, 0)

        store.selectCharacterFeed(OfficeCharacter.boss.rawValue)

        XCTAssertEqual(characterStore.turns, [updatedTurn])
        XCTAssertEqual(publicationCount, 1)
        withExtendedLifetime(cancellable) {}
    }

    func testPresentedCharacterFeedIgnoresUnrelatedUpdates() {
        let store = LiveFeedStore()
        let startedAt = Date(timeIntervalSinceReferenceDate: 975)
        let bossTurn = makeTurn(
            id: "boss-turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "총괄",
            startedAt: startedAt
        )
        let coworkerTurn = makeTurn(
            id: "coworker-turn",
            characterID: OfficeCharacter.rightWoman.rawValue,
            prompt: "보조",
            startedAt: startedAt.addingTimeInterval(1)
        )
        store.replace(with: [coworkerTurn, bossTurn])

        let characterStore = store.characterStore(
            for: OfficeCharacter.boss.rawValue
        )
        store.selectCharacterFeed(OfficeCharacter.boss.rawValue)
        var publicationCount = 0
        let cancellable = characterStore.objectWillChange.sink {
            publicationCount += 1
        }
        let updatedCoworkerTurn = makeTurn(
            id: "coworker-turn",
            characterID: OfficeCharacter.rightWoman.rawValue,
            prompt: "보조",
            response: "진행 중",
            startedAt: startedAt.addingTimeInterval(1),
            updatedAt: startedAt.addingTimeInterval(2)
        )

        store.replace(with: [updatedCoworkerTurn, bossTurn])

        XCTAssertEqual(characterStore.turns, [bossTurn])
        XCTAssertEqual(publicationCount, 0)
        withExtendedLifetime(cancellable) {}
    }

    func testSelectedCharacterFeedPublishesSnapshotLoadedAfterSelection() {
        let store = LiveFeedStore()
        let characterID = OfficeCharacter.leftWoman.rawValue
        let characterStore = store.characterStore(for: characterID)
        var publicationCount = 0
        let cancellable = characterStore.objectWillChange.sink {
            publicationCount += 1
        }

        store.selectCharacterFeed(characterID)
        store.replace(with: [
            makeTurn(
                id: "left-woman-turn",
                characterID: characterID,
                prompt: "초기 스냅샷",
                startedAt: Date(timeIntervalSinceReferenceDate: 990)
            ),
        ])
        store.finishInitialLoading()

        XCTAssertEqual(characterStore.turns.map(\.id), ["left-woman-turn"])
        XCTAssertFalse(characterStore.isLoadingInitialFeed)
        XCTAssertEqual(publicationCount, 2)
        withExtendedLifetime(cancellable) {}
    }

    func testSelectingLoadedCharacterExposesTurnsBeforeSelectionPublishes() {
        let store = LiveFeedStore()
        let characterID = OfficeCharacter.leftWoman.rawValue
        store.replace(with: [
            makeTurn(
                id: "existing-turn",
                characterID: characterID,
                prompt: "이미 불러온 대화",
                startedAt: Date(timeIntervalSinceReferenceDate: 995)
            ),
        ])
        store.finishInitialLoading()

        store.selectCharacterFeed(characterID)
        let characterStore = store.characterStore(for: characterID)

        XCTAssertEqual(characterStore.turns.map(\.id), ["existing-turn"])
        XCTAssertFalse(characterStore.isLoadingInitialFeed)
        XCTAssertEqual(store.selectedCharacterFeedID, characterID)
    }

    func testRapidCharacterSelectionPublishesOnlyLatestFeedUpdates() {
        let store = LiveFeedStore()
        let bossID = OfficeCharacter.boss.rawValue
        let leftWomanID = OfficeCharacter.leftWoman.rawValue
        let rightManID = OfficeCharacter.rightMan.rawValue
        let bossStore = store.characterStore(for: bossID)
        let leftWomanStore = store.characterStore(for: leftWomanID)
        let rightManStore = store.characterStore(for: rightManID)

        store.selectCharacterFeed(bossID)
        store.selectCharacterFeed(leftWomanID)
        store.selectCharacterFeed(rightManID)
        store.replace(with: [
            makeTurn(
                id: "boss-turn",
                characterID: bossID,
                prompt: "총괄",
                startedAt: Date(timeIntervalSinceReferenceDate: 997)
            ),
            makeTurn(
                id: "left-woman-turn",
                characterID: leftWomanID,
                prompt: "분석",
                startedAt: Date(timeIntervalSinceReferenceDate: 998)
            ),
            makeTurn(
                id: "right-man-turn",
                characterID: rightManID,
                prompt: "검증",
                startedAt: Date(timeIntervalSinceReferenceDate: 999)
            ),
        ])

        XCTAssertTrue(bossStore.turns.isEmpty)
        XCTAssertTrue(leftWomanStore.turns.isEmpty)
        XCTAssertEqual(rightManStore.turns.map(\.id), ["right-man-turn"])
        XCTAssertEqual(store.selectedCharacterFeedID, rightManID)
    }

    func testRestoredRunningCodexTurnDefersExistingTextUntilCompletion() {
        let store = LiveFeedStore()
        let running = makeTurn(
            id: "restored-running-turn",
            characterID: OfficeCharacter.leftWoman.rawValue,
            prompt: "재시작 전 업무",
            response: "이미 작성된 응답",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_001)
        )
        store.restoreResponseAnimations(for: [running])
        store.replace(with: [running])

        XCTAssertTrue(store.shouldAnimateResponse(for: running))
        XCTAssertTrue(store.shouldAnimateInitialResponse(for: running))

        let updatedRunning = makeTurn(
            id: running.id,
            characterID: running.characterId,
            prompt: running.prompt,
            response: "이미 작성된 응답 + 새 문장",
            status: .running,
            startedAt: running.startedAt,
            updatedAt: running.updatedAt.addingTimeInterval(1)
        )
        store.replace(with: [updatedRunning])

        XCTAssertTrue(store.shouldAnimateResponse(for: updatedRunning))
        XCTAssertTrue(
            store.shouldAnimateInitialResponse(for: updatedRunning)
        )
    }

    func testStartedResponseAnimationSurvivesStreamingUntilTerminal() {
        let store = LiveFeedStore()
        let running = makeTurn(
            id: "new-running-turn",
            characterID: OfficeCharacter.leftWoman.rawValue,
            prompt: "새 업무",
            response: "작성 중",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_002)
        )
        store.selectCharacterFeed(running.characterId)
        store.replace(with: [running])
        store.beginResponseAnimation(for: running.id)

        XCTAssertTrue(store.shouldAnimateResponse(for: running))
        XCTAssertTrue(store.shouldAnimateInitialResponse(for: running))
        store.finishResponseAnimation(for: running.id)
        XCTAssertTrue(store.shouldAnimateResponse(for: running))

        let completed = makeTurn(
            id: running.id,
            characterID: running.characterId,
            prompt: running.prompt,
            response: "최종 응답",
            status: .completed,
            startedAt: running.startedAt,
            updatedAt: running.updatedAt.addingTimeInterval(1)
        )
        store.replace(with: [completed])
        store.finishResponseAnimation(for: completed.id)

        XCTAssertFalse(store.shouldAnimateResponse(for: completed))
    }

    func testRestoredEmptyRunningTurnAnimatesFirstArrivingResponse() {
        let store = LiveFeedStore()
        let running = makeTurn(
            id: "restored-empty-running-turn",
            characterID: OfficeCharacter.leftWoman.rawValue,
            prompt: "응답 대기 중",
            response: "",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_002.5)
        )

        store.restoreResponseAnimations(for: [running])
        store.replace(with: [running])

        XCTAssertTrue(store.shouldAnimateResponse(for: running))
        XCTAssertTrue(store.shouldAnimateInitialResponse(for: running))
    }

    func testRestoredClaudeTurnAnimatesFirstResponseAfterPromotedMessage()
        throws
    {
        let store = LiveFeedStore()
        let promotedMessage = try makeActivity(
            id: "promoted-message",
            kind: "message",
            text: "완료 전 보고",
            status: "completed"
        )
        let running = makeTurn(
            id: "restored-promoted-claude-turn",
            characterID: OfficeCharacter.leftWoman.rawValue,
            prompt: "계속 진행",
            response: "완료 전 보고",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_002.75),
            backend: .claude,
            activities: [promotedMessage]
        )

        store.restoreResponseAnimations(for: [running])
        store.replace(with: [running])

        XCTAssertTrue(store.shouldAnimateResponse(for: running))
        XCTAssertTrue(store.shouldAnimateInitialResponse(for: running))
    }

    func testRestoredClaudeTurnShowsExistingStreamingRemainderImmediately()
        throws
    {
        let store = LiveFeedStore()
        let promotedMessage = try makeActivity(
            id: "promoted-message-with-remainder",
            kind: "message",
            text: "완료 전 보고",
            status: "completed"
        )
        let running = makeTurn(
            id: "restored-streaming-claude-turn",
            characterID: OfficeCharacter.leftWoman.rawValue,
            prompt: "계속 진행",
            response: "완료 전 보고\n\n이미 작성 중",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_002.875),
            backend: .claude,
            activities: [promotedMessage]
        )

        store.restoreResponseAnimations(for: [running])
        store.replace(with: [running])

        XCTAssertTrue(store.shouldAnimateResponse(for: running))
        XCTAssertFalse(store.shouldAnimateInitialResponse(for: running))
    }

    func testHiddenRestoredClaudeTurnDoesNotReplayAccumulatedResponse()
        throws
    {
        let store = LiveFeedStore()
        let promotedMessage = try makeActivity(
            id: "hidden-promoted-message",
            kind: "message",
            text: "완료 전 보고",
            status: "completed"
        )
        let running = makeTurn(
            id: "hidden-restored-claude-turn",
            characterID: OfficeCharacter.leftWoman.rawValue,
            prompt: "계속 진행",
            response: "완료 전 보고",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_002.9),
            backend: .claude,
            activities: [promotedMessage]
        )
        store.selectCharacterFeed(OfficeCharacter.boss.rawValue)
        store.restoreResponseAnimations(for: [running])
        store.replace(with: [running])
        XCTAssertTrue(store.shouldAnimateInitialResponse(for: running))

        let updatedRunning = makeTurn(
            id: running.id,
            characterID: running.characterId,
            prompt: running.prompt,
            response: "완료 전 보고\n\n숨은 동안 쌓인 응답",
            status: .running,
            startedAt: running.startedAt,
            updatedAt: running.updatedAt.addingTimeInterval(1),
            backend: .claude,
            activities: [promotedMessage]
        )
        store.replace(with: [updatedRunning])

        XCTAssertTrue(store.shouldAnimateResponse(for: updatedRunning))
        XCTAssertFalse(
            store.shouldAnimateInitialResponse(for: updatedRunning)
        )
    }

    func testSelectedRestoredClaudeTurnAnimatesFirstArrivingResponse()
        throws
    {
        let store = LiveFeedStore()
        let promotedMessage = try makeActivity(
            id: "selected-promoted-message",
            kind: "message",
            text: "완료 전 보고",
            status: "completed"
        )
        let running = makeTurn(
            id: "selected-restored-claude-turn",
            characterID: OfficeCharacter.leftWoman.rawValue,
            prompt: "계속 진행",
            response: "완료 전 보고",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_002.95),
            backend: .claude,
            activities: [promotedMessage]
        )
        store.selectCharacterFeed(running.characterId)
        store.restoreResponseAnimations(for: [running])
        store.replace(with: [running])

        let updatedRunning = makeTurn(
            id: running.id,
            characterID: running.characterId,
            prompt: running.prompt,
            response: "완료 전 보고\n\n지금 도착한 응답",
            status: .running,
            startedAt: running.startedAt,
            updatedAt: running.updatedAt.addingTimeInterval(1),
            backend: .claude,
            activities: [promotedMessage]
        )
        store.replace(with: [updatedRunning])

        XCTAssertTrue(store.shouldAnimateInitialResponse(for: updatedRunning))
    }

    func testHiddenRestoredCodexTurnKeepsDeferredCompletionAnimation() {
        let store = LiveFeedStore()
        let running = makeTurn(
            id: "hidden-restored-codex-turn",
            characterID: OfficeCharacter.leftWoman.rawValue,
            prompt: "계속 진행",
            response: "",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_002.975)
        )
        store.selectCharacterFeed(OfficeCharacter.boss.rawValue)
        store.restoreResponseAnimations(for: [running])
        store.replace(with: [running])

        let updatedRunning = makeTurn(
            id: running.id,
            characterID: running.characterId,
            prompt: running.prompt,
            response: "숨은 동안 쌓인 Codex 응답",
            status: .running,
            startedAt: running.startedAt,
            updatedAt: running.updatedAt.addingTimeInterval(1)
        )
        store.replace(with: [updatedRunning])

        XCTAssertTrue(store.shouldAnimateResponse(for: updatedRunning))
        XCTAssertTrue(
            store.shouldAnimateInitialResponse(for: updatedRunning)
        )
    }

    func testHiddenTerminalAndRemovedTurnsDiscardResponseAnimations() {
        let store = LiveFeedStore()
        let running = makeTurn(
            id: "hidden-running-turn",
            characterID: OfficeCharacter.leftWoman.rawValue,
            prompt: "숨은 업무",
            response: "작성 중",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_003)
        )
        store.restoreResponseAnimations(for: [running])
        store.replace(with: [running])

        let completed = makeTurn(
            id: running.id,
            characterID: running.characterId,
            prompt: running.prompt,
            response: "완료",
            status: .completed,
            startedAt: running.startedAt,
            updatedAt: running.updatedAt.addingTimeInterval(1)
        )
        store.replace(with: [completed])

        XCTAssertFalse(store.shouldAnimateResponse(for: completed))

        let removed = makeTurn(
            id: "removed-running-turn",
            characterID: OfficeCharacter.rightMan.rawValue,
            prompt: "삭제될 업무",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_004)
        )
        store.restoreResponseAnimations(for: [removed])
        store.replace(with: [removed])
        store.replace(with: [])

        XCTAssertFalse(store.shouldAnimateResponse(for: removed))
    }

    func testEmptyTerminalResponseDiscardsAnimationWhileSelected() {
        let store = LiveFeedStore()
        let running = makeTurn(
            id: "empty-terminal-turn",
            characterID: OfficeCharacter.leftWoman.rawValue,
            prompt: "실패할 업무",
            response: "작성 중",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_005)
        )
        store.selectCharacterFeed(running.characterId)
        store.restoreResponseAnimations(for: [running])
        store.replace(with: [running])

        let failed = makeTurn(
            id: running.id,
            characterID: running.characterId,
            prompt: running.prompt,
            response: "",
            status: .failed,
            startedAt: running.startedAt,
            updatedAt: running.updatedAt.addingTimeInterval(1)
        )
        store.replace(with: [failed])

        XCTAssertFalse(store.shouldAnimateResponse(for: failed))
    }

    func testNoncompletedTerminalResponseDiscardsAnimationWhileSelected() {
        let store = LiveFeedStore()
        let running = makeTurn(
            id: "interrupted-response-turn",
            characterID: OfficeCharacter.rightMan.rawValue,
            prompt: "중단될 업무",
            response: "부분 응답",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_005.5)
        )
        store.selectCharacterFeed(running.characterId)
        store.restoreResponseAnimations(for: [running])
        store.replace(with: [running])

        let interrupted = makeTurn(
            id: running.id,
            characterID: running.characterId,
            prompt: running.prompt,
            response: "부분 응답",
            status: .interrupted,
            startedAt: running.startedAt,
            updatedAt: running.updatedAt.addingTimeInterval(1)
        )
        store.replace(with: [interrupted])

        XCTAssertFalse(store.shouldAnimateResponse(for: interrupted))
    }

    func testSelectingLoadedFeedPublishesMountRefresh() {
        let store = LiveFeedStore()
        let characterID = OfficeCharacter.leftWoman.rawValue
        store.replace(with: [
            makeTurn(
                id: "loaded-mount-turn",
                characterID: characterID,
                prompt: "재시작 뒤 대화",
                startedAt: Date(timeIntervalSinceReferenceDate: 1_006)
            ),
        ])
        store.finishInitialLoading()
        let characterStore = store.characterStore(for: characterID)
        var publicationCount = 0
        let cancellable = characterStore.objectWillChange.sink {
            publicationCount += 1
        }

        store.selectCharacterFeed(characterID)

        XCTAssertEqual(characterStore.presentationRevision, 0)
        XCTAssertEqual(publicationCount, 0)

        store.refreshSelectedCharacterFeedAfterMount(characterID)

        XCTAssertEqual(characterStore.presentationRevision, 1)
        XCTAssertEqual(publicationCount, 1)
        withExtendedLifetime(cancellable) {}
    }

    func testResponseAnimationStatePublishesOnlyToPresentedCharacter() {
        let store = LiveFeedStore()
        let characterID = OfficeCharacter.leftWoman.rawValue
        let running = makeTurn(
            id: "animation-publication-turn",
            characterID: characterID,
            prompt: "애니메이션",
            status: .running,
            startedAt: Date(timeIntervalSinceReferenceDate: 1_007)
        )
        store.replace(with: [running])
        store.finishInitialLoading()
        let characterStore = store.characterStore(for: characterID)
        store.selectCharacterFeed(characterID)
        let initialRevision = characterStore.presentationRevision

        store.beginResponseAnimation(for: running.id)

        XCTAssertEqual(
            characterStore.presentationRevision,
            initialRevision + 1
        )
    }

    func testStalePostMountRefreshCannotPublishAfterRapidSelection() {
        let store = LiveFeedStore()
        let firstID = OfficeCharacter.leftWoman.rawValue
        let latestID = OfficeCharacter.rightMan.rawValue
        store.replace(with: [
            makeTurn(
                id: "first-mounted-turn",
                characterID: firstID,
                prompt: "첫 직원",
                startedAt: Date(timeIntervalSinceReferenceDate: 1_008)
            ),
            makeTurn(
                id: "latest-mounted-turn",
                characterID: latestID,
                prompt: "마지막 직원",
                startedAt: Date(timeIntervalSinceReferenceDate: 1_009)
            ),
        ])
        store.finishInitialLoading()
        let firstStore = store.characterStore(for: firstID)
        let latestStore = store.characterStore(for: latestID)

        store.selectCharacterFeed(firstID)
        store.selectCharacterFeed(latestID)
        store.refreshSelectedCharacterFeedAfterMount(firstID)
        store.refreshSelectedCharacterFeedAfterMount(latestID)

        XCTAssertEqual(firstStore.presentationRevision, 0)
        XCTAssertEqual(latestStore.presentationRevision, 1)
        XCTAssertEqual(store.selectedCharacterFeedID, latestID)
    }

    func testMetadataStoreDefersPublicationBeyondViewUpdate() async {
        let initial = LiveWorkspaceFeedMetadata(
            latestTerminalTurnID: nil,
            latestSubmittedCommandID: nil,
            latestStartedCommandID: nil
        )
        let updated = LiveWorkspaceFeedMetadata(
            latestTerminalTurnID: "terminal-turn",
            latestSubmittedCommandID: UUID(
                uuidString: "00000000-0000-0000-0000-000000000001"
            ),
            latestStartedCommandID: UUID(
                uuidString: "11111111-1111-1111-1111-111111111111"
            )
        )
        let store = LiveWorkspaceFeedMetadataStore(metadata: initial)
        let publication = expectation(
            description: "다음 MainActor 차례에서 메타데이터 발행"
        )
        let cancellable = store.objectWillChange.sink {
            publication.fulfill()
        }

        store.setMetadata(updated)

        XCTAssertEqual(store.metadata, initial)

        await fulfillment(of: [publication], timeout: 1)

        XCTAssertEqual(store.metadata, updated)
        withExtendedLifetime(cancellable) {}
    }

    func testMetadataStoreCoalescesSubmitTransitionsToLatestSnapshot() async {
        let submittedCommandID = UUID(
            uuidString: "22222222-2222-2222-2222-222222222221"
        )
        let startedCommandID = UUID(
            uuidString: "22222222-2222-2222-2222-222222222222"
        )
        let initial = LiveWorkspaceFeedMetadata(
            latestTerminalTurnID: nil,
            latestSubmittedCommandID: nil,
            latestStartedCommandID: nil
        )
        let local = LiveWorkspaceFeedMetadata(
            latestTerminalTurnID: nil,
            latestSubmittedCommandID: submittedCommandID,
            latestStartedCommandID: nil
        )
        let server = LiveWorkspaceFeedMetadata(
            latestTerminalTurnID: nil,
            latestSubmittedCommandID: submittedCommandID,
            latestStartedCommandID: nil
        )
        let started = LiveWorkspaceFeedMetadata(
            latestTerminalTurnID: nil,
            latestSubmittedCommandID: submittedCommandID,
            latestStartedCommandID: startedCommandID
        )
        XCTAssertEqual(
            local,
            server,
            "임시 턴 ID가 서버 ID로 바뀌어도 제출 스크롤 기준은 "
                + "같은 command ID로 유지되어야 합니다."
        )
        let store = LiveWorkspaceFeedMetadataStore(metadata: initial)
        var publicationCount = 0
        let publication = expectation(
            description: "마지막 제출 메타데이터만 한 번 발행"
        )
        publication.assertForOverFulfill = true
        let cancellable = store.objectWillChange.sink {
            publicationCount += 1
            publication.fulfill()
        }

        store.setMetadata(local)
        store.setMetadata(server)
        store.setMetadata(started)

        await fulfillment(of: [publication], timeout: 1)

        XCTAssertEqual(store.metadata, started)
        XCTAssertEqual(publicationCount, 1)
        withExtendedLifetime(cancellable) {}
    }

    func testPresentationStoreDefersPublicationBeyondViewUpdate() async {
        let store = LiveWorkspaceFeedPresentationStore(isPresented: false)
        var publicationCount = 0
        let publication = expectation(
            description: "다음 MainActor 차례에서 표시 상태 발행"
        )
        let cancellable = store.objectWillChange.sink {
            publicationCount += 1
            publication.fulfill()
        }

        store.setPresented(true)

        XCTAssertFalse(store.isPresented)
        XCTAssertEqual(publicationCount, 0)

        await fulfillment(of: [publication], timeout: 1)

        XCTAssertTrue(store.isPresented)
        XCTAssertEqual(publicationCount, 1)
        withExtendedLifetime(cancellable) {}
    }

    func testPresentationStoreCoalescesRapidVisibilityChanges() async {
        let store = LiveWorkspaceFeedPresentationStore(isPresented: false)
        var publicationCount = 0
        let publication = expectation(
            description: "마지막 표시 상태만 한 번 발행"
        )
        publication.assertForOverFulfill = true
        let cancellable = store.objectWillChange.sink {
            publicationCount += 1
            publication.fulfill()
        }

        store.setPresented(true)
        store.setPresented(false)
        store.setPresented(true)

        await fulfillment(of: [publication], timeout: 1)

        XCTAssertTrue(store.isPresented)
        XCTAssertEqual(publicationCount, 1)
        withExtendedLifetime(cancellable) {}
    }

    func testOptimisticTurnSurvivesUnrelatedFeedAndReconcilesByID() {
        let store = LiveFeedStore()
        let submittedAt = Date(timeIntervalSinceReferenceDate: 1_000)
        let optimistic = makeTurn(
            id: "local-command",
            characterID: OfficeCharacter.rightMan.rawValue,
            prompt: "원인을 확인해줘.",
            startedAt: submittedAt
        )
        let unrelated = makeTurn(
            id: "other-turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "다른 업무",
            startedAt: submittedAt.addingTimeInterval(-30)
        )

        store.insertOptimisticTurn(optimistic)
        store.replace(with: [unrelated])

        XCTAssertEqual(
            store.turns.map(\.id),
            ["local-command", "other-turn"]
        )

        store.reconcileOptimisticTurn(
            id: "local-command",
            with: "server-turn"
        )

        XCTAssertEqual(
            store.turns.map(\.id),
            ["server-turn", "other-turn"]
        )

        let persisted = makeTurn(
            id: "server-turn",
            characterID: OfficeCharacter.rightMan.rawValue,
            prompt: "원인을 확인해줘.",
            response: "확인했습니다.",
            startedAt: submittedAt.addingTimeInterval(0.1)
        )
        store.replace(with: [persisted, unrelated])

        XCTAssertEqual(store.turns.count, 2)
        XCTAssertEqual(store.turns.first?.id, "server-turn")
        XCTAssertEqual(store.turns.first?.response, "확인했습니다.")
    }

    func testOptimisticServerTransitionKeepsCardPresentationIdentity() {
        let store = LiveFeedStore()
        let optimistic = makeTurn(
            id: "local-stable-card",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "동일 직원에게 이어서 보내는 업무",
            startedAt: Date(timeIntervalSinceReferenceDate: 1_500)
        )

        store.insertOptimisticTurn(optimistic)
        let localPresentationID = store.presentationID(
            forTurnID: optimistic.id
        )
        store.reconcileOptimisticTurn(
            id: optimistic.id,
            with: "server-stable-card"
        )

        XCTAssertEqual(
            store.presentationID(forTurnID: "server-stable-card"),
            localPresentationID,
            "optimistic turn을 서버 turn으로 바꿀 때 SwiftUI 카드가 "
                + "삭제·재삽입되면 대화 문서와 스크롤 정체성이 흔들립니다."
        )

        store.replace(with: [
            optimistic.replacingID(with: "server-stable-card"),
        ])

        XCTAssertEqual(
            store.presentationID(forTurnID: "server-stable-card"),
            localPresentationID,
            "서버 snapshot이 도착한 뒤에도 같은 화면 카드 정체성을 "
                + "유지해야 합니다."
        )
    }

    func testMatchingServerTurnRemovesOptimisticDuplicateBeforeReconcile() {
        let store = LiveFeedStore()
        let submittedAt = Date(timeIntervalSinceReferenceDate: 2_000)
        store.insertOptimisticTurn(
            makeTurn(
                id: "local-command",
                characterID: OfficeCharacter.rightMan.rawValue,
                prompt: "같은 업무",
                startedAt: submittedAt
            )
        )
        let localPresentationID = store.presentationID(
            forTurnID: "local-command"
        )

        let persisted = makeTurn(
            id: "server-turn",
            characterID: OfficeCharacter.rightMan.rawValue,
            prompt: "같은 업무",
            startedAt: submittedAt.addingTimeInterval(0.2)
        )
        store.replace(with: [persisted])

        XCTAssertEqual(store.turns.map(\.id), ["server-turn"])
        XCTAssertEqual(
            store.presentationID(forTurnID: "server-turn"),
            localPresentationID,
            "명시적 reconcile보다 서버 snapshot이 먼저 와도 카드 "
                + "정체성을 이어받아야 합니다."
        )
        XCTAssertEqual(
            store.optimisticCharacterIDs,
            Set<String>()
        )
    }

    func testFailedStartRemovesOptimisticTurn() {
        let store = LiveFeedStore()
        store.insertOptimisticTurn(
            makeTurn(
                id: "local-command",
                characterID: OfficeCharacter.rightMan.rawValue,
                prompt: "실패할 업무",
                startedAt: Date()
            )
        )

        store.removeOptimisticTurn(id: "local-command")

        XCTAssertTrue(store.turns.isEmpty)
        XCTAssertTrue(store.optimisticCharacterIDs.isEmpty)
    }

    func testLatestConversationCharacterUsesNewestValidTurn() {
        let baseDate = Date(timeIntervalSinceReferenceDate: 3_000)
        let turns = [
            makeTurn(
                id: "older-turn",
                characterID: OfficeCharacter.boss.rawValue,
                prompt: "이전 업무",
                startedAt: baseDate,
                updatedAt: baseDate.addingTimeInterval(10)
            ),
            makeTurn(
                id: "newer-turn",
                characterID: OfficeCharacter.leftWoman.rawValue,
                prompt: "최근 업무",
                startedAt: baseDate.addingTimeInterval(20),
                updatedAt: baseDate.addingTimeInterval(30)
            ),
            makeTurn(
                id: "unknown-turn",
                characterID: "unknown-character",
                prompt: "알 수 없는 직원",
                startedAt: baseDate.addingTimeInterval(40),
                updatedAt: baseDate.addingTimeInterval(50)
            ),
        ]

        XCTAssertEqual(
            AgentDirector.latestConversationCharacter(in: turns),
            .leftWoman
        )
        XCTAssertNil(AgentDirector.latestConversationCharacter(in: []))
    }

    func testFeedFollowStopsOnlyForUserScrollAndResumesAtBottom() {
        var state = LiveWorkspaceFeedFollowState()

        XCTAssertTrue(state.isFollowingLatest)

        state.userWillScroll()
        XCTAssertFalse(state.isFollowingLatest)

        state.userDidScroll(
            distanceFromBottom: 240,
            tolerance: 20
        )
        XCTAssertFalse(state.isFollowingLatest)

        state.userDidScroll(
            distanceFromBottom: 12,
            tolerance: 20
        )
        XCTAssertTrue(state.isFollowingLatest)

        state.userWillScroll()
        state.resume()
        XCTAssertTrue(state.isFollowingLatest)
    }

    func testProgrammaticBottomScrollStopsAfterTwoStablePasses() {
        var policy = LiveWorkspaceFeedScrollPolicy()

        XCTAssertFalse(
            policy.shouldStop(
                distanceFromBottom: 12,
                tolerance: 20
            )
        )
        XCTAssertTrue(
            policy.shouldStop(
                distanceFromBottom: 8,
                tolerance: 20
            )
        )
    }

    func testSubmittedScrollResetsStabilityAndCapsRetries() {
        var policy = LiveWorkspaceFeedScrollPolicy()

        XCTAssertFalse(
            policy.shouldStop(
                distanceFromBottom: 10,
                tolerance: 20
            )
        )
        XCTAssertFalse(
            policy.shouldStop(
                distanceFromBottom: 80,
                tolerance: 20
            )
        )
        XCTAssertFalse(
            policy.shouldStop(
                distanceFromBottom: 15,
                tolerance: 20
            )
        )
        XCTAssertEqual(
            LiveWorkspaceFeedScrollPolicy.submittedMaximumAttempts,
            1,
            "제출 직후 하단 보정은 레이아웃 확정 뒤 1회를 넘으면 "
                + "문서 높이 변동과 겹쳐 스크롤바 왕복을 만듭니다."
        )
    }

    func testDisplayAnchorKeepsInitialMountWindowBeforePinning() {
        let anchor = LiveWorkspaceFeedDisplayAnchor()

        XCTAssertEqual(
            anchor.effectiveLimit(
                turnIDsNewestFirst: ["t9", "t8", "t7", "t6"]
            ),
            LiveWorkspaceFeedPagingPolicy.initialVisibleTurnCount,
            "앵커 고정 전 첫 마운트는 스냅샷 보유량만큼만 표시합니다."
        )
    }

    func testHeavyFeedStartsWithSmallAdaptiveWindow() {
        let visibleCount = LiveWorkspaceFeedPagingPolicy
            .initialVisibleTurnCount(
                layoutWeightsNewestFirst: [
                    28_000,
                    14_000,
                    35_000,
                    18_000,
                    2_000,
                ]
            )

        XCTAssertEqual(
            visibleCount,
            LiveWorkspaceFeedPagingPolicy.minimumInitialVisibleTurnCount,
            "무거운 세 번째 턴은 첫 mount에서 미루어야 합니다."
        )
        let anchor = LiveWorkspaceFeedDisplayAnchor(
            initialLimit: visibleCount
        )
        XCTAssertEqual(
            anchor.effectiveLimit(
                turnIDsNewestFirst: ["t5", "t4", "t3", "t2", "t1"]
            ),
            2
        )
    }

    func testNormalFeedStillShowsTenTurnsInitially() {
        let visibleCount = LiveWorkspaceFeedPagingPolicy
            .initialVisibleTurnCount(
                layoutWeightsNewestFirst: Array(repeating: 2_000, count: 12)
            )

        XCTAssertEqual(
            visibleCount,
            LiveWorkspaceFeedPagingPolicy.initialVisibleTurnCount
        )
    }

    func testAdaptiveWindowAlwaysKeepsLatestTwoTurns() {
        XCTAssertEqual(
            LiveWorkspaceFeedPagingPolicy.initialVisibleTurnCount(
                layoutWeightsNewestFirst: [100_000, 100_000, 1]
            ),
            2
        )
    }

    func testDisplayAnchorGrowsWindowWhenNewTurnArrivesInFront() {
        var anchor = LiveWorkspaceFeedDisplayAnchor()
        anchor = anchor.pinning(turnIDsNewestFirst: ["t2", "t1"])
        XCTAssertEqual(anchor.oldestVisibleTurnID, "t1")
        XCTAssertEqual(
            anchor.effectiveLimit(turnIDsNewestFirst: ["t2", "t1"]),
            2
        )

        // 새 optimistic 턴이 index 0에 삽입돼도 기존 카드는 창에서
        // 밀려나지 않고 창이 2→3으로 늘어난다.
        XCTAssertEqual(
            anchor.effectiveLimit(
                turnIDsNewestFirst: ["local-new", "t2", "t1"]
            ),
            3,
            "새 턴 삽입이 기존 표시 카드를 제거하는 이동 창으로 "
                + "동작하면 문서 높이가 급락해 흰 화면이 됩니다."
        )

        // 연속 제출에도 계속 누적된다.
        XCTAssertEqual(
            anchor.effectiveLimit(
                turnIDsNewestFirst: [
                    "local-next", "server-new", "t2", "t1",
                ]
            ),
            4
        )
    }

    func testDisplayAnchorSurvivesOptimisticServerIDSwap() {
        var anchor = LiveWorkspaceFeedDisplayAnchor()
        // 마운트 시점: 완료 카드 2개가 보이는 상태에서 고정.
        anchor = anchor.pinning(turnIDsNewestFirst: ["t2", "t1"])
        XCTAssertEqual(anchor.oldestVisibleTurnID, "t1")

        // 제출로 optimistic 턴이 앞에 삽입된 뒤 재고정.
        anchor = anchor.pinning(
            turnIDsNewestFirst: ["local-a", "t2", "t1"]
        )
        XCTAssertEqual(anchor.oldestVisibleTurnID, "t1")

        // 앵커가 아닌 턴의 local → server ID 교체는 창을 흔들지 않는다.
        XCTAssertEqual(
            anchor.effectiveLimit(
                turnIDsNewestFirst: ["server-a", "t2", "t1"]
            ),
            3
        )
    }

    func testDisplayAnchorFallsBackToLastKnownLimitWhenAnchorVanishes() {
        var anchor = LiveWorkspaceFeedDisplayAnchor()
        let ids = (0..<20).map { "t\(19 - $0)" }
        anchor = anchor.pinning(limit: 12, turnIDsNewestFirst: ids)
        XCTAssertEqual(anchor.oldestVisibleTurnID, "t8")
        XCTAssertEqual(anchor.effectiveLimit(turnIDsNewestFirst: ids), 12)

        // 앵커 턴이 목록에서 사라져도(스냅샷 정리) 직전 표시 규모를
        // 유지해 창 축소로 카드가 제거되는 일을 막는다.
        let pruned = ids.filter { $0 != "t8" }
        XCTAssertEqual(
            anchor.effectiveLimit(turnIDsNewestFirst: pruned),
            12
        )

        // 이후 재고정하면 새 목록 기준으로 앵커가 복구된다.
        anchor = anchor.pinning(turnIDsNewestFirst: pruned)
        XCTAssertEqual(anchor.oldestVisibleTurnID, pruned[11])
    }

    func testDisplayAnchorTrimsWindowGrownByNewTurnsBackToLimit() {
        var anchor = LiveWorkspaceFeedDisplayAnchor()
        let ids = (0..<20).map { "t\(19 - $0)" }
        anchor = anchor.pinning(limit: 10, turnIDsNewestFirst: ids)

        // 일하는 동안 새 턴 5개가 들어오면 앵커 고정으로 창이 15가 된다.
        let grown = (0..<5).map { "n\(4 - $0)" } + ids
        XCTAssertEqual(anchor.effectiveLimit(turnIDsNewestFirst: grown), 15)

        anchor = anchor.trimming(toLimit: 10, turnIDsNewestFirst: grown)
        XCTAssertEqual(anchor.effectiveLimit(turnIDsNewestFirst: grown), 10)
        XCTAssertEqual(anchor.oldestVisibleTurnID, grown[9])
        // 창 밖이어도 실행 중인 턴은 계속 표시된다.
        XCTAssertTrue(
            LiveWorkspaceFeedPagingPolicy.includesTurn(
                at: 12,
                visibleTurnLimit: 10,
                isRunning: true,
                isLatestTerminalTurn: false
            )
        )
    }

    func testDisplayAnchorTrimmingKeepsSmallerWindow() {
        var anchor = LiveWorkspaceFeedDisplayAnchor()
        let ids = (0..<20).map { "t\(19 - $0)" }
        anchor = anchor.pinning(limit: 6, turnIDsNewestFirst: ids)

        anchor = anchor.trimming(toLimit: 10, turnIDsNewestFirst: ids)
        XCTAssertEqual(anchor.effectiveLimit(turnIDsNewestFirst: ids), 6)
        XCTAssertEqual(anchor.oldestVisibleTurnID, "t14")
    }

    func testTrimToLatestOnlyWhenFollowingAndNewestTurnChanges() {
        XCTAssertTrue(
            LiveWorkspaceFeedPagingPolicy.shouldTrimToLatest(
                previousNewestID: "t1",
                newestID: "t2",
                didPerformInitialScroll: true,
                isFollowingLatest: true
            )
        )
        // 과거를 읽는 중에는 카드를 지우지 않는다.
        XCTAssertFalse(
            LiveWorkspaceFeedPagingPolicy.shouldTrimToLatest(
                previousNewestID: "t1",
                newestID: "t2",
                didPerformInitialScroll: true,
                isFollowingLatest: false
            )
        )
        // 과거 불러오기는 가장 새 턴이 그대로라 바로 접지 않는다.
        // 짧은 대화에서 맨 위도 하단으로 판정될 때의 반복을 막는다.
        XCTAssertFalse(
            LiveWorkspaceFeedPagingPolicy.shouldTrimToLatest(
                previousNewestID: "t2",
                newestID: "t2",
                didPerformInitialScroll: true,
                isFollowingLatest: true
            )
        )
        XCTAssertFalse(
            LiveWorkspaceFeedPagingPolicy.shouldTrimToLatest(
                previousNewestID: "t1",
                newestID: "t2",
                didPerformInitialScroll: false,
                isFollowingLatest: true
            )
        )
        XCTAssertFalse(
            LiveWorkspaceFeedPagingPolicy.shouldTrimToLatest(
                previousNewestID: "t1",
                newestID: nil,
                didPerformInitialScroll: true,
                isFollowingLatest: true
            )
        )
    }

    func testDisplayAnchorPagingExpandsTowardOlderTurns() {
        var anchor = LiveWorkspaceFeedDisplayAnchor()
        let ids = (0..<30).map { "t\(29 - $0)" }
        anchor = anchor.pinning(turnIDsNewestFirst: ids)
        XCTAssertEqual(anchor.effectiveLimit(turnIDsNewestFirst: ids), 10)

        let nextLimit = LiveWorkspaceFeedPagingPolicy.nextVisibleTurnLimit(
            current: anchor.effectiveLimit(turnIDsNewestFirst: ids),
            total: ids.count
        )
        XCTAssertEqual(nextLimit, 13)
        anchor = anchor.pinning(
            limit: nextLimit,
            turnIDsNewestFirst: ids
        )
        XCTAssertEqual(
            anchor.effectiveLimit(turnIDsNewestFirst: ids),
            13
        )
        XCTAssertEqual(anchor.oldestVisibleTurnID, ids[12])
    }

    func testFreshEmployeeMountShowsSnapshotWindowWithoutArchivedNotice() {
        // 스냅샷이 직원당 최근 10턴을 보유하므로 첫 화면도 10건을
        // 그대로 보여준다. 이보다 좁히면 평소 대화가 두 건만 보이고
        // 나머지가 숨김 안내로 밀린다.
        XCTAssertEqual(
            LiveWorkspaceFeedPagingPolicy.initialVisibleTurnCount,
            LiveFeedStore.minimumTurnsPerCharacter
        )

        let includedIndices = (0..<12).filter { index in
            LiveWorkspaceFeedPagingPolicy.includesTurn(
                at: index,
                visibleTurnLimit:
                    LiveWorkspaceFeedPagingPolicy.initialVisibleTurnCount,
                isRunning: false,
                isLatestTerminalTurn: false
            )
        }

        XCTAssertEqual(includedIndices, Array(0..<10))
        XCTAssertTrue(
            LiveWorkspaceFeedPagingPolicy.includesTurn(
                at: 15,
                visibleTurnLimit:
                    LiveWorkspaceFeedPagingPolicy.initialVisibleTurnCount,
                isRunning: true,
                isLatestTerminalTurn: false
            ),
            "과거 위치에 남은 실행 중 턴은 첫 화면에서도 숨기면 안 됩니다."
        )
        XCTAssertTrue(
            LiveWorkspaceFeedPagingPolicy.includesTurn(
                at: 14,
                visibleTurnLimit:
                    LiveWorkspaceFeedPagingPolicy.initialVisibleTurnCount,
                isRunning: false,
                isLatestTerminalTurn: true
            ),
            "방금 완료된 턴은 첫 화면에서도 숨기면 안 됩니다."
        )
    }

    func testTopLoadAddsThreeTurnsAtATime() {
        XCTAssertEqual(LiveWorkspaceFeedPagingPolicy.pageSize, 3)
        XCTAssertEqual(
            LiveWorkspaceFeedPagingPolicy.nextVisibleTurnLimit(
                current:
                    LiveWorkspaceFeedPagingPolicy.initialVisibleTurnCount,
                total: 10
            ),
            10
        )
        XCTAssertEqual(
            LiveWorkspaceFeedPagingPolicy.nextVisibleTurnLimit(
                current: 10,
                total: 30
            ),
            13
        )
        XCTAssertEqual(
            LiveWorkspaceFeedPagingPolicy.nextVisibleTurnLimit(
                current: 30,
                total: 50
            ),
            30
        )
    }

    func testExecutionModeTitleTreatsHistoricalValuesAsStandard() {
        XCTAssertEqual(agentExecutionModeTitle(true), "Fast")
        XCTAssertEqual(agentExecutionModeTitle(false), "Standard")
        XCTAssertEqual(agentExecutionModeTitle(nil), "Standard")
    }

    func testReplacingOptimisticIDPreservesWorkspaceReview() {
        let workspace = makeWorkspace(status: .awaitingApproval)
        let source = makeSource(kind: .database)
        let turn = makeTurn(
            id: "local-command",
            characterID: OfficeCharacter.rightMan.rawValue,
            prompt: "변경 업무",
            startedAt: Date(),
            sources: [source],
            wikiProposalWarning: "지식 제안 저장 실패",
            workspace: workspace
        )

        let replaced = turn.replacingID(with: "server-turn")

        XCTAssertEqual(replaced.id, "server-turn")
        XCTAssertEqual(replaced.workspace, workspace)
        XCTAssertEqual(replaced.responseSources, [source])
        XCTAssertEqual(replaced.wikiProposalWarning, "지식 제안 저장 실패")
        XCTAssertEqual(
            turn.replacingFeedback(with: .liked).wikiProposalWarning,
            "지식 제안 저장 실패"
        )
    }

    func testWikiProposalWarningDecodesForHistoryAndLiveFeedTurns() throws {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        let warning = "위키 수정안을 저장하지 못했습니다."

        let history = try decoder.decode(
            HistoryTurn.self,
            from: Data(
                #"{"id":"history","sessionId":"session","prompt":"질문","response":"답변","wikiProposalWarning":"위키 수정안을 저장하지 못했습니다.","startedAt":"2026-08-13T00:00:00Z"}"#.utf8
            )
        )
        let global = try decoder.decode(
            GlobalHistoryTurn.self,
            from: Data(
                #"{"id":"global","characterId":"boss","characterName":"백부장","backend":"codex","prompt":"질문","response":"답변","wikiProposalWarning":"위키 수정안을 저장하지 못했습니다.","startedAt":"2026-08-13T00:00:00Z"}"#.utf8
            )
        )
        let live = try decoder.decode(
            LiveFeedTurn.self,
            from: Data(
                #"{"id":"live","characterId":"boss","characterName":"백부장","characterBackend":"codex","prompt":"질문","response":"답변","status":"completed","needsInput":false,"wikiProposalWarning":"위키 수정안을 저장하지 못했습니다.","startedAt":"2026-08-13T00:00:00Z","updatedAt":"2026-08-13T00:00:00Z","activities":[]}"#.utf8
            )
        )

        XCTAssertEqual(history.wikiProposalWarning, warning)
        XCTAssertEqual(global.wikiProposalWarning, warning)
        XCTAssertEqual(live.wikiProposalWarning, warning)
    }

    func testLiveFeedSourcesDecodeAllKindsAndLocators() throws {
        let payload = Data(
            #"""
            [
              {
                "id": "source-rag",
                "sourceKind": "rag",
                "title": "이전 결정",
                "locator": "rag_documents/record",
                "excerpt": null,
                "ragDocumentId": null,
                "workRecordId": null
              },
              {
                "id": "source-database",
                "sourceKind": "database",
                "title": "업무 기록",
                "locator": "work_records/record",
                "excerpt": "세션을 유지한다.",
                "ragDocumentId": null,
                "workRecordId": null
              },
              {
                "id": "source-file",
                "sourceKind": "file",
                "title": "README",
                "locator": "/repo/README.md:14-18",
                "excerpt": null,
                "ragDocumentId": null,
                "workRecordId": null
              },
              {
                "id": "source-web",
                "sourceKind": "web",
                "title": "공식 문서",
                "locator": "https://example.com/guide?q=office",
                "excerpt": null,
                "ragDocumentId": null,
                "workRecordId": null
              },
              {
                "id": "source-tool",
                "sourceKind": "tool",
                "title": "실행 도구",
                "locator": "web.search",
                "excerpt": null,
                "ragDocumentId": null,
                "workRecordId": null
              },
              {
                "id": "source-skill",
                "sourceKind": "skill",
                "title": "사용 스킬",
                "locator": "openai-docs",
                "excerpt": null,
                "ragDocumentId": null,
                "workRecordId": null
              }
            ]
            """#.utf8
        )

        let sources = try JSONDecoder().decode(
            [LiveFeedSource].self,
            from: payload
        )

        XCTAssertEqual(
            sources.map(\.sourceKind),
            [.rag, .database, .file, .web, .tool, .skill]
        )
        XCTAssertEqual(
            sources.map(\.sourceKind.title),
            ["RAG", "DB", "파일", "웹", "도구", "스킬"]
        )
        XCTAssertEqual(sources[2].filePath, "/repo/README.md")
        XCTAssertEqual(
            sources[3].webURL?.absoluteString,
            "https://example.com/guide?q=office"
        )
        XCTAssertNil(sources[4].webURL)
        XCTAssertNil(sources[5].webURL)
    }

    func testWebSourceAllowsOnlySafeHTTPURLs() {
        let https = makeSource(
            kind: .web,
            locator: "https://example.com/reference"
        )
        let http = makeSource(
            kind: .web,
            locator: "http://example.com/reference"
        )
        let credentials = makeSource(
            kind: .web,
            locator: "https://user:password@example.com/reference"
        )
        let script = makeSource(
            kind: .web,
            locator: "javascript:alert(1)"
        )
        let missingHost = makeSource(
            kind: .web,
            locator: "https:///reference"
        )

        XCTAssertEqual(https.webURL?.scheme, "https")
        XCTAssertEqual(http.webURL?.scheme, "http")
        XCTAssertNil(credentials.webURL)
        XCTAssertNil(script.webURL)
        XCTAssertNil(missingHost.webURL)
        XCTAssertNil(makeSource(kind: .file).webURL)
    }

    func testArchiveRefreshesWhenOnlyResponseSourcesChange() {
        let store = ArchiveFeedStore()
        let startedAt = Date(timeIntervalSinceReferenceDate: 7_000)
        let original = makeTurn(
            id: "turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "근거 확인",
            startedAt: startedAt
        )
        let sourced = makeTurn(
            id: "turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "근거 확인",
            startedAt: startedAt,
            sources: [makeSource(kind: .file)]
        )

        store.replaceIfNeeded(with: [original])
        store.replaceIfNeeded(with: [sourced])

        XCTAssertEqual(store.turns.first?.responseSources.count, 1)
        XCTAssertEqual(store.turns.first?.responseSources.first?.sourceKind, .file)
    }

    func testArchiveRefreshesWhenOnlyResponseSourceWarningClears() {
        let store = ArchiveFeedStore()
        let startedAt = Date(timeIntervalSinceReferenceDate: 7_100)
        let warning = makeTurn(
            id: "turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "근거 확인",
            startedAt: startedAt,
            responseSourceWarning: "근거 형식을 읽지 못했습니다."
        )
        let cleared = makeTurn(
            id: "turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "근거 확인",
            startedAt: startedAt
        )

        store.replaceIfNeeded(with: [warning])
        store.replaceIfNeeded(with: [cleared])

        XCTAssertNil(store.turns.first?.responseSourceWarning)
    }

    func testArchiveRefreshesWhenOnlyWikiProposalWarningChanges() {
        let store = ArchiveFeedStore()
        let startedAt = Date(timeIntervalSinceReferenceDate: 7_200)
        let original = makeTurn(
            id: "turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "지식 제안",
            startedAt: startedAt
        )
        let warning = makeTurn(
            id: "turn",
            characterID: OfficeCharacter.boss.rawValue,
            prompt: "지식 제안",
            startedAt: startedAt,
            wikiProposalWarning: "위키 수정안을 저장하지 못했습니다."
        )

        store.replaceIfNeeded(with: [original])
        store.replaceIfNeeded(with: [warning])

        XCTAssertEqual(
            store.turns.first?.wikiProposalWarning,
            "위키 수정안을 저장하지 못했습니다."
        )
    }

    func testSnapshotKeepsMinimumTurnsForEachCharacter() {
        let baseDate = Date(timeIntervalSinceReferenceDate: 11_000)
        var turns = (0 ..< 120).map { index in
            makeTurn(
                id: "busy-\(index)",
                characterID: OfficeCharacter.rightMan.rawValue,
                prompt: "최근 업무",
                startedAt: baseDate.addingTimeInterval(-Double(index))
            )
        }
        turns.append(contentsOf: (0 ..< 12).map { index in
            makeTurn(
                id: "quiet-\(index)",
                characterID: OfficeCharacter.leftWoman.rawValue,
                prompt: "오래된 업무",
                startedAt:
                    baseDate.addingTimeInterval(-Double(120 + index))
            )
        })

        let snapshot = LiveFeedStore.snapshotTurns(
            from: turns,
            recentLimit: 120
        )

        XCTAssertEqual(snapshot.count, 130)
        XCTAssertEqual(
            snapshot.filter {
                $0.characterId == OfficeCharacter.leftWoman.rawValue
            }.map(\.id),
            (0 ..< 10).map { "quiet-\($0)" }
        )
    }

    func testWorkspaceFileBaseFollowsReviewLifecycle() {
        let awaiting = makeWorkspace(status: .awaitingApproval)
        let merged = makeWorkspace(status: .merged)

        XCTAssertEqual(
            awaiting.fileBaseDirectory(fallback: "/fallback"),
            "/tmp/worktree/project"
        )
        XCTAssertEqual(
            awaiting.reviewFileBaseDirectory(fallback: "/fallback"),
            "/tmp/worktree"
        )
        XCTAssertEqual(
            merged.fileBaseDirectory(fallback: "/fallback"),
            "/repo/project"
        )
        XCTAssertEqual(
            merged.reviewFileBaseDirectory(fallback: "/fallback"),
            "/repo"
        )
    }

    func testLiveFeedWorkspaceDecodesWithoutOnDemandDiff() throws {
        let payload = Data(
            #"""
            {
              "status": "awaiting_approval",
              "repositoryRoot": "/repo",
              "worktreePath": "/tmp/worktree",
              "executionWorkdir": "/tmp/worktree/project",
              "branchName": "officestra/right-man/task",
              "baseBranch": "main",
              "baseCommit": "base",
              "reviewTree": "review-tree",
              "headCommit": "head",
              "changedFiles": [
                {"status": "M", "path": "README.md"}
              ],
              "mergedCommit": null,
              "errorMessage": null
            }
            """#.utf8
        )

        let workspace = try JSONDecoder().decode(
            TurnWorkspaceReview.self,
            from: payload
        )

        XCTAssertEqual(workspace.status, .awaitingApproval)
        XCTAssertEqual(workspace.reviewTree, "review-tree")
        XCTAssertEqual(workspace.executionWorkdir, "/tmp/worktree/project")
        XCTAssertEqual(workspace.changedFiles.first?.path, "README.md")
        XCTAssertNil(workspace.diff)
        XCTAssertNil(workspace.diffTruncated)
    }

    func testWorkspaceReviewMetadataRemainsBackwardCompatible() throws {
        let payload = Data(
            #"""
            {
              "status": "awaiting_approval",
              "repositoryRoot": "/repo",
              "worktreePath": "/tmp/worktree",
              "branchName": "officestra/right-man/task",
              "baseBranch": "main",
              "baseCommit": "base",
              "headCommit": "head",
              "changedFiles": [],
              "mergedCommit": null,
              "errorMessage": null
            }
            """#.utf8
        )

        let workspace = try JSONDecoder().decode(
            TurnWorkspaceReview.self,
            from: payload
        )

        XCTAssertNil(workspace.executionWorkdir)
        XCTAssertNil(workspace.reviewTree)
    }

    private func makeTurn(
        id: String,
        characterID: String,
        prompt: String,
        response: String = "",
        status: LiveTurnStatus = .running,
        startedAt: Date,
        updatedAt: Date? = nil,
        sources: [LiveFeedSource]? = nil,
        responseSourceWarning: String? = nil,
        wikiProposalWarning: String? = nil,
        workspace: TurnWorkspaceReview? = nil,
        backend: AgentBackend = .codex,
        activities: [LiveFeedActivity] = []
    ) -> LiveFeedTurn {
        LiveFeedTurn(
            id: id,
            characterId: characterID,
            characterName: characterID,
            characterBackend: backend,
            backend: backend,
            model: backend == .claude ? "claude-sonnet-5" : "gpt-5.6-sol",
            effort: "high",
            fastMode: true,
            externalSessionId: nil,
            conversationWorkdir: "/repo",
            prompt: prompt,
            response: response,
            feedback: nil,
            status: status,
            needsInput: false,
            errorMessage: nil,
            responseSourceWarning: responseSourceWarning,
            wikiProposalWarning: wikiProposalWarning,
            startedAt: startedAt,
            endedAt: nil,
            updatedAt: updatedAt ?? startedAt,
            estimatedCostUsd: nil,
            sessionContext: nil,
            activities: activities,
            sources: sources,
            workspace: workspace
        )
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

    private func makeWorkspace(
        status: WorkspaceReviewStatus,
        reviewTree: String? = "review-tree",
        diff: String? = nil,
        diffTruncated: Bool? = nil
    ) -> TurnWorkspaceReview {
        TurnWorkspaceReview(
            status: status,
            repositoryRoot: "/repo",
            worktreePath: "/tmp/worktree",
            executionWorkdir:
                status == .merged
                    ? "/repo/project"
                    : "/tmp/worktree/project",
            branchName: "officestra/right-man/task",
            baseBranch: "main",
            baseCommit: "base",
            reviewTree: reviewTree,
            headCommit: "head",
            changedFiles: [
                WorkspaceChangedFile(status: "M", path: "README.md")
            ],
            mergedCommit: status == .merged ? "merged" : nil,
            errorMessage: nil,
            diff: diff,
            diffTruncated: diffTruncated
        )
    }

    private func makeSource(
        kind: LiveFeedSourceKind,
        locator: String? = nil
    ) -> LiveFeedSource {
        LiveFeedSource(
            id: "source-\(kind.rawValue)",
            sourceKind: kind,
            title: "근거",
            locator: locator
                ?? (kind == .file ? "/repo/README.md:14" : "record/14"),
            excerpt: nil,
            ragDocumentId: nil,
            workRecordId: nil
        )
    }
}
