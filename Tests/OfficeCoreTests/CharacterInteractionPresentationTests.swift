// 이 파일은 사무실 상호작용 레이어가 실제 표시 상태 변화에만 반응하는지 검증한다.

import OfficeCore
import XCTest
@testable import OfficeGame

final class CharacterInteractionPresentationTests: XCTestCase {
    func testPresentationStateChangesForVisibleStatus() throws {
        let configuration = try CharacterConfigurationAsset.load()
        let state = makeState(configuration: configuration)

        XCTAssertNotEqual(
            state,
            makeState(
                configuration: configuration,
                runningCharacters: [.boss]
            )
        )
        XCTAssertNotEqual(
            state,
            makeState(
                configuration: configuration,
                questionCharacters: [.leftMan]
            )
        )
        XCTAssertNotEqual(
            state,
            makeState(
                configuration: configuration,
                failedCharacters: [.rightMan]
            )
        )
        XCTAssertNotEqual(
            state,
            makeState(
                configuration: configuration,
                offDutyCharacters: [.rightWoman]
            )
        )
    }

    func testPresentationStateRemainsEqualForIdenticalVisibleInputs() throws {
        let configuration = try CharacterConfigurationAsset.load()

        XCTAssertEqual(
            makeState(configuration: configuration),
            makeState(configuration: configuration)
        )
    }

    func testWorkingAndCompletionRemainVisibleWithoutTransientBubbleText() throws {
        let configuration = try CharacterConfigurationAsset.load()
        let working = makeState(configuration: configuration, runningCharacters: [.boss])
        let completed = makeState(configuration: configuration, completedCharacters: [.boss])
        let idle = makeState(configuration: configuration)
        XCTAssertEqual(working.bubbleStatus(for: .boss, hasMessage: false), .working)
        XCTAssertEqual(completed.bubbleStatus(for: .boss, hasMessage: false), .completed)
        XCTAssertNotEqual(idle, completed, "Completion must invalidate the equatable office overlay")
        XCTAssertNil(idle.bubbleStatus(for: .boss, hasMessage: true), "Idle chatter must not become a status bubble")
        XCTAssertNil(idle.bubbleStatus(for: .boss, hasMessage: false), "Reviewing completion removes the indicator")
    }

    func testCompactionAndNewWorkTakePriorityOverPreviousCompletion() throws {
        let configuration = try CharacterConfigurationAsset.load()
        let compacting = makeState(configuration: configuration,
            compactingCharacters: [.boss], completedCharacters: [.boss])
        XCTAssertEqual(compacting.bubbleStatus(for: .boss, hasMessage: false), .working)
        let working = makeState(configuration: configuration,
            runningCharacters: [.boss], completedCharacters: [.boss])
        XCTAssertEqual(working.bubbleStatus(for: .boss, hasMessage: true), .working)
        XCTAssertNotEqual(compacting, makeState(configuration: configuration, completedCharacters: [.boss]))
    }

    func testActionableQuestionsAndWarningsPreserveTheirAcknowledgementBehavior() throws {
        let configuration = try CharacterConfigurationAsset.load()
        let question = makeState(configuration: configuration,
            runningCharacters: [.boss], questionCharacters: [.boss])
        XCTAssertEqual(question.bubbleStatus(for: .boss, hasMessage: true), .question)
        let failed = makeState(configuration: configuration, failedCharacters: [.boss])
        XCTAssertEqual(failed.bubbleStatus(for: .boss, hasMessage: true), .failed)
        XCTAssertNil(failed.bubbleStatus(for: .boss, hasMessage: false))
        let offDuty = makeState(configuration: configuration, offDutyCharacters: [.boss])
        XCTAssertEqual(offDuty.bubbleStatus(for: .boss, hasMessage: true), .offDuty)
        XCTAssertNil(offDuty.bubbleStatus(for: .boss, hasMessage: false))
    }

    private func makeState(
        configuration: OfficeAgentConfiguration,
        runningCharacters: Set<OfficeCharacter> = [],
        questionCharacters: Set<OfficeCharacter> = [],
        failedCharacters: Set<OfficeCharacter> = [],
        offDutyCharacters: Set<OfficeCharacter> = [],
        compactingCharacters: Set<OfficeCharacter> = [],
        completedCharacters: Set<OfficeCharacter> = []
    ) -> CharacterInteractionPresentationState {
        CharacterInteractionPresentationState(
            characters: configuration.characters,
            displayNames: Dictionary(uniqueKeysWithValues:
                configuration.characters.map { ($0.id, $0.name) }
            ),
            archiveCabinetHitbox: configuration.archiveCabinetHitbox,
            runningCharacters: runningCharacters,
            questionCharacters: questionCharacters,
            failedCharacters: failedCharacters,
            offDutyCharacters: offDutyCharacters,
            compactingCharacters: compactingCharacters,
            completedCharacters: completedCharacters
        )
    }
}
