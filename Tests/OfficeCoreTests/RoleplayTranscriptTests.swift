// 롤플레이 답변의 화자·대사·속마음 구분과 화자색 고정을 확인한다.
import XCTest
@testable import OfficeGame

final class RoleplayTranscriptTests: XCTestCase {
    typealias Segment = RoleplayTranscript.Segment
    typealias Speaker = RoleplayTranscript.Speaker

    func testCueLineWithEmojiBecomesSpeakerDialogue() {
        let segments = RoleplayTranscript.segments("👩 헬레나: \"어서 와.\"")
        XCTAssertEqual(segments, [.dialogue(speaker: Speaker(emoji: "👩", name: "헬레나"), text: "\"어서 와.\"")])
    }

    func testCueLineWithoutEmojiAndCurlyQuotes() {
        let segments = RoleplayTranscript.segments("민수 : “괜찮아?”")
        XCTAssertEqual(segments, [.dialogue(speaker: Speaker(emoji: nil, name: "민수"), text: "“괜찮아?”")])
    }

    func testPlainQuoteLineIsDialogueWithoutSpeaker() {
        XCTAssertEqual(RoleplayTranscript.segments("  \"누구세요?\"  "), [.dialogue(speaker: nil, text: "\"누구세요?\"")])
    }

    func testSingleQuotesAreThoughts() {
        XCTAssertEqual(
            RoleplayTranscript.segments("헬레나: '귀찮네.'\n‘정말일까.’"),
            [
                .thought(speaker: Speaker(emoji: nil, name: "헬레나"), text: "'귀찮네.'"),
                .thought(speaker: nil, text: "‘정말일까.’"),
            ]
        )
    }

    func testNarrationLinesMergeAroundDialogue() {
        let text = "문이 열렸다.\n바람이 분다.\n\n👨 카이: \"늦었어.\"\n그는 웃었다. \"농담이야\"라고 덧붙였다."
        XCTAssertEqual(RoleplayTranscript.segments(text), [
            .narration("문이 열렸다.\n바람이 분다."),
            .dialogue(speaker: Speaker(emoji: "👨", name: "카이"), text: "\"늦었어.\""),
            .narration("그는 웃었다. \"농담이야\"라고 덧붙였다."),
        ])
    }

    func testSentenceWithColonIsNotTreatedAsSpeaker() {
        let text = "그녀가 말했다. 이유는 하나: \"싫어\""
        XCTAssertEqual(RoleplayTranscript.segments(text), [.narration(text)])
    }

    func testFallbackSpeakerFillsOnlyRepliesWithoutAnyTag() {
        let claire = Speaker(emoji: "👩", name: "Claire")
        XCTAssertEqual(
            RoleplayTranscript.segments("그녀가 웃었다.\n\"안녕?\"\n'귀엽네.'", fallbackSpeaker: claire),
            [
                .narration("그녀가 웃었다."),
                .dialogue(speaker: claire, text: "\"안녕?\""),
                .thought(speaker: claire, text: "'귀엽네.'"),
            ]
        )
        let tagged = "👨 카이: \"왔어?\"\n\"누구야?\""
        XCTAssertEqual(RoleplayTranscript.segments(tagged, fallbackSpeaker: claire), RoleplayTranscript.segments(tagged))
    }

    func testFallbackSpeakersUseMostFrequentEarlierTagThenDefault() {
        let texts = [
            "\"이름표 없음\"",
            "👩 Claire: \"하나\"\n👨 카이: \"둘\"",
            "👨 카이: \"셋\"\n👩 Claire: \"넷\"\nClaire: '다섯'",
            "\"이름표 없음\"",
        ]
        let speakers = RoleplayTranscript.fallbackSpeakers(for: texts, defaultName: "코대리")
        XCTAssertEqual(speakers, [
            Speaker(emoji: nil, name: "코대리"),
            Speaker(emoji: nil, name: "코대리"),
            Speaker(emoji: "👩", name: "Claire"),
            Speaker(emoji: "👩", name: "Claire"),
        ])
    }

    func testColorIndexIsStableAndInRange() {
        let first = RoleplayTranscript.colorIndex(for: "헬레나", count: 6)
        XCTAssertEqual(first, RoleplayTranscript.colorIndex(for: "헬레나", count: 6))
        XCTAssertTrue((0..<6).contains(first))
        XCTAssertTrue((0..<6).contains(RoleplayTranscript.colorIndex(for: String(repeating: "가", count: 40), count: 6)))
    }
}
