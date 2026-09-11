import XCTest
@testable import OfficeGame

final class EmployeeMessageSenderTests: XCTestCase {
    func testExplicitSenderWinsAndSupportsRenamedEmployee() throws {
        let sender = try JSONDecoder().decode(EmployeeMessageSender.self,
            from: Data(#"{"characterId":"right-man","name":"새 이름"}"#.utf8))
        XCTAssertEqual(EmployeeMessageSender.resolve(sender, prompt: "로과장입니다. 회신"), sender)
    }

    func testLegacySelfIntroductionAndBatonHeader() {
        XCTAssertEqual(EmployeeMessageSender.resolve(nil, prompt: "로과장입니다. 검토했습니다.")?.characterId, "left-woman")
        XCTAssertEqual(EmployeeMessageSender.resolve(nil, prompt: "[review 2차 회신]\n안과장입니다. 확인했습니다.")?.characterId, "right-man")
    }

    func testUserMentionsAndQuotesDoNotBecomeEmployeeMessages() {
        for prompt in ["로과장에게 물어봐", "안과장 분석은 어때?", "\"코대리입니다.\"", "예시: 백부장입니다.", "일반 질문", ""] {
            XCTAssertNil(EmployeeMessageSender.resolve(nil, prompt: prompt), prompt)
        }
        XCTAssertNil(EmployeeMessageSender.resolve(.init(characterId: "unknown", name: "직원"), prompt: "로과장입니다."))
    }

    func testOldHistoryPayloadRemainsDecodableAndNewSenderIsDecoded() throws {
        let old = #"{"id":"turn","sessionId":"session","prompt":"hi","response":"hello","startedAt":0}"#
        let decoder = JSONDecoder()
        XCTAssertNil(try decoder.decode(HistoryTurn.self, from: Data(old.utf8)).promptSender)
        let new = old.dropLast() + #", "promptSender":{"characterId":"boss","name":"백부장"}}"#
        XCTAssertEqual(try decoder.decode(HistoryTurn.self, from: Data(new.utf8)).promptSender?.characterId, "boss")
    }
}
