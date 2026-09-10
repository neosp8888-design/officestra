// 이 파일은 시스템 언어에 따른 OfficeGame 표시 언어 선택을 검증한다.

import XCTest
@testable import OfficeGame

final class OfficeLocalizationTests: XCTestCase {
    func testKoreanSystemLanguageUsesKorean() {
        XCTAssertEqual(
            OfficeLocalization.languageIdentifier(for: ["ko-KR", "en-US"]),
            "ko"
        )
    }

    func testLanguageOrderUsesTheFirstSupportedLanguage() {
        XCTAssertEqual(OfficeLocalization.languageIdentifier(for: ["en-US", "ko-KR"]), "en")
        XCTAssertEqual(OfficeLocalization.languageIdentifier(for: ["ja-JP", "ko-KR", "en"]), "ko")
        XCTAssertEqual(OfficeLocalization.languageIdentifier(for: ["EN_us", "ko"]), "en")
        XCTAssertEqual(OfficeLocalization.languageIdentifier(for: []), "en")
        XCTAssertEqual(OfficeLocalization.languageIdentifier(for: ["ja-JP"]), "en")
    }

    func testCountFormattingHandlesSingularPluralAndArgumentOrder() {
        let cases: [(String, [CVarArg], String)] = [
            ("변경 파일 %d개", [1], "1 changed file"),
            ("변경 파일 %d개", [2], "2 changed files"),
            ("%d건", [1], "1 record"),
            ("이전 기록 %d개 보기", [1], "Show 1 earlier record"),
            ("더 이전 기록 %d개 보기", [2], "Show 2 earlier records"),
            ("이전 %@ %d개 보기", ["command", 3], "Show command history (3)"),
            ("%d명 · %d명 완료 · %d명 오류", [1, 0, 1], "1 reviewer · 0 completed · 1 failed"),
            ("추가 요청 %d건", [1], "1 follow-up request"),
            ("좋아요 %d건", [1], "1 like"),
            ("싫어요 %d건", [1], "1 dislike"),
            ("작업 계획 %d단계 중 %d단계 완료", [1, 0], "Work plan: 0 of 1 step completed"),
            ("첨부 파일 %@, 경로 %@", ["보고서.pdf", "/tmp/보고서.pdf"], "Attachment 보고서.pdf, path /tmp/보고서.pdf"),
            ("1턴당 평균 비용(추정) %@ · 전체 %d턴", ["$0.1200", 1], "Average cost per turn (est.) $0.1200 · 1 turn total"),
            ("1턴당 평균 비용(추정) %@ · 전체 %d턴", ["$0.1200", 4], "Average cost per turn (est.) $0.1200 · 4 turns total"),
            ("1턴당 평균 비용(추정) %@ · 분당 %@ · 전체 %d턴", ["$0.1200", "$0.0300", 1], "Average cost (est.) per turn $0.1200 · per minute $0.0300 · 1 turn total"),
            ("1턴당 평균 비용(추정) %@ · 분당 %@ · 전체 %d턴", ["$0.1200", "$0.0300", 4], "Average cost (est.) per turn $0.1200 · per minute $0.0300 · 4 turns total")
        ]
        for (key, arguments, expected) in cases {
            XCTAssertEqual(OfficeLocalization.format(key, arguments: arguments, languages: ["en", "ko"]), expected)
        }
        XCTAssertEqual(OfficeLocalization.format("변경 파일 %d개", arguments: [1], languages: ["ko"]), "변경 파일 1개")
    }

    func testAuditRegressionKeysAreTranslated() {
        let keys = [
            "모던 낮", "모던 밤", "보스", "왼쪽 남자", "왼쪽 여자", "오른쪽 남자",
            "첨부 파일을 확인해줘.", "제목 없는 업무", "기록", "장서 정보", "세션 ID",
            "기록 없음", "응답 복사", "%@, Finder에서 보기", "변경 결과 복사",
            "협업 검토 접기", "협업 검토 자세히 보기", "최근 요청", "최근 결과", "요청",
            "추가 요청", "검토 결과 접기", "검토 결과 자세히", "검토 중", "오류",
            "AI CLI 로그인을 확인하는 중", "characters.json 설정 파일을 찾을 수 없습니다.",
            "백엔드 요청에 실패했습니다.", "PostgreSQL 백엔드에 연결할 수 없습니다.",
            "사용자 승인", "구형 OFFICESTRA 백엔드가 4317을 사용 중입니다."
        ]
        for key in keys {
            let result = OfficeLocalization.string(key, languages: ["en-US", "ko-KR"])
            XCTAssertNotEqual(result, key, key)
            XCTAssertFalse(result.containsHangul, key)
        }
    }

    func testSystemMessageCatalogAndEveryTemplate() {
        let entries = OfficeSystemMessageLocalization.entries
        XCTAssertGreaterThan(entries.count, 200, "The diagnostic catalog must be bundled in the app.")
        XCTAssertEqual(Set(entries.map(\.source)).count, entries.count)
        for entry in entries {
            var source = entry.source
            var expected = entry.translation
            for index in 0..<10 {
                source = source.replacingOccurrences(of: "{\(index)}", with: "arg_\(index)")
                expected = expected.replacingOccurrences(of: "{\(index)}", with: "arg_\(index)")
            }
            let existing = OfficeLocalization.string(source, languages: ["en"])
            if existing != source { expected = existing }
            XCTAssertFalse(entry.translation.isEmpty, entry.source)
            XCTAssertFalse(entry.translation.containsHangul, entry.source)
            XCTAssertEqual(OfficeSystemMessageLocalization.string(source, languages: ["en"]), expected, entry.source)
            XCTAssertEqual(OfficeSystemMessageLocalization.string(source, languages: ["ko"]), source, entry.source)
        }
    }

    func testSystemMessagesPreserveInsertedPathsUnknownTextAndNestedErrors() {
        XCTAssertEqual(
            OfficeSystemMessageLocalization.string("캐릭터를 찾을 수 없습니다: /tmp/사용자/{1}/100%", languages: ["en"]),
            "Teammate not found: /tmp/사용자/{1}/100%"
        )
        XCTAssertEqual(
            OfficeSystemMessageLocalization.string("오류 · 위키 수정안 2번을 저장하지 못했습니다: 제안을 찾을 수 없습니다.", languages: ["en"]),
            "Error · Could not save wiki proposal 2: Proposal not found."
        )
        XCTAssertEqual(
            OfficeSystemMessageLocalization.string("CLI 원본 출력\n사용자가 업무를 중단했습니다.", languages: ["en"]),
            "CLI 원본 출력\nThe user stopped the task."
        )
        let userText = "사용자가 직접 쓴 본문과 /tmp/한글.txt는 바꾸지 않습니다."
        XCTAssertEqual(OfficeSystemMessageLocalization.string(userText, languages: ["en"]), userText)
    }

    func testBackendDiagnosticLiteralsStayCoveredByEnglishResources() throws {
        let root = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let modules = [
            "character-settings", "model-catalog", "runtime-cli-paths", "cli-updates",
            "codex-context-compactor", "claude-persistent-worker", "terminal-sessions",
            "work-record-provenance", "wiki-knowledge", "turn-feedback", "server",
            "agent-runtime", "structured-turn-result", "usage-summary", "usage-report"
        ]
        // Extract direct error constructors and JSON error literals, not arbitrary
        // quoted conversation content. Concatenated strings are covered by catalog tests.
        let pattern = try NSRegularExpression(
            pattern: #"(?:(?:new\s+\w*Error|throw\s+invalid|super)\(\s*|error:\s*)(?:"((?:\\.|[^"\\])*)"|`([^`]*?)`)"#,
            options: [.dotMatchesLineSeparators]
        )
        let interpolation = try NSRegularExpression(pattern: #"\$\{[^}]*\}"#)
        var checked = 0
        for module in modules {
            let source = try String(contentsOf: root.appendingPathComponent("backend/src/\(module).mjs"), encoding: .utf8)
            let ns = source as NSString
            for match in pattern.matches(in: source, range: NSRange(location: 0, length: ns.length)) {
                let tail = ns.substring(from: NSMaxRange(match.range)).trimmingCharacters(in: .whitespacesAndNewlines)
                if tail.hasPrefix("+") { continue }
                let message: String
                if match.range(at: 1).location != NSNotFound {
                    let literal = "\"" + ns.substring(with: match.range(at: 1)) + "\""
                    message = try JSONDecoder().decode(String.self, from: Data(literal.utf8))
                } else {
                    let template = ns.substring(with: match.range(at: 2))
                    message = interpolation.stringByReplacingMatches(in: template, range: NSRange(template.startIndex..., in: template), withTemplate: "argument")
                }
                guard message.containsHangul else { continue }
                checked += 1
                let value = OfficeSystemMessageLocalization.string(message, languages: ["en"])
                XCTAssertFalse(value.containsHangul, "\(module): \(message)")
            }
        }
        XCTAssertGreaterThan(checked, 200)
    }

    @MainActor
    func testUIActivityAndToolPresentationLeaveConversationContentIntact() throws {
        let original = UserDefaults.standard.object(forKey: "AppleLanguages")
        UserDefaults.standard.set(["en-US", "ko-KR"], forKey: "AppleLanguages")
        defer {
            if let original { UserDefaults.standard.set(original, forKey: "AppleLanguages") }
            else { UserDefaults.standard.removeObject(forKey: "AppleLanguages") }
        }
        for kind in ["message", "thinking", "command"] {
            let activity = try activity(kind: kind, text: "사용자가 업무를 중단했습니다.")
            XCTAssertEqual(activity.displayText, activity.text)
        }
        let tool = try activity(kind: "tool", text: "오류 · 사용자가 업무를 중단했습니다.")
        XCTAssertEqual(tool.displayText, "Error · The user stopped the task.")
        XCTAssertEqual(tool.text, "오류 · 사용자가 업무를 중단했습니다.")
        XCTAssertEqual(ClaudeToolCall.parse(tool).displayName, "Tool")
        XCTAssertEqual(ClaudeToolCall.parse(tool).displayDetail, tool.displayText)
        let namedTool = try activity(kind: "tool", text: "도구 · Read · /tmp/사용자.txt")
        XCTAssertEqual(ClaudeToolCall.parse(namedTool).displayDetail, "/tmp/사용자.txt")
        XCTAssertEqual(ClaudeToolCall.parse(try activity(kind: "tool", text: "도구 · 도구")).displayName, "Tool")
        XCTAssertEqual(ClaudeToolCall.parse(try activity(kind: "tool", text: "도구 · 연결 도구")).displayName, "Connected tool")
        let toolWithOutput = try activity(kind: "tool", text: "도구 완료\n사용자가 업무를 중단했습니다.")
        XCTAssertEqual(toolWithOutput.displayText, "Tool completed\n사용자가 업무를 중단했습니다.")
        XCTAssertEqual(OfficeLocalization.historyNoun("명령"), "command")
        let date = Date(timeIntervalSince1970: 1_788_609_600)
        XCTAssertFalse(OfficeLocalization.date(date, dateStyle: .complete, time: .standard).containsHangul)
    }

    private func activity(kind: String, text: String) throws -> LiveFeedActivity {
        let data = try JSONSerialization.data(withJSONObject: [
            "id": UUID().uuidString, "kind": kind, "text": text,
            "occurredAt": 0, "status": "completed"
        ])
        return try JSONDecoder().decode(LiveFeedActivity.self, from: data)
    }

    func testNonKoreanSystemLanguagesUseEnglish() {
        XCTAssertEqual(
            OfficeLocalization.languageIdentifier(for: ["en-US"]),
            "en"
        )
        XCTAssertEqual(
            OfficeLocalization.languageIdentifier(for: ["ja-JP", "en-US"]),
            "en"
        )
    }

    func testLocalizedStringsReuseLoadedLanguageTables() {
        XCTAssertEqual(
            OfficeLocalization.string(
                "대화 보관함",
                languages: ["ko-KR"]
            ),
            "대화 보관함"
        )
        XCTAssertEqual(
            OfficeLocalization.string(
                "대화 보관함",
                languages: ["en-US"]
            ),
            "Conversation Archive"
        )
        XCTAssertEqual(
            OfficeLocalization.string(
                "직원 업무 기록",
                languages: ["en-US"]
            ),
            "Team work records"
        )
        XCTAssertEqual(
            OfficeLocalization.string(
                "현재 사용 가능량",
                languages: ["en-US"]
            ),
            "Current availability"
        )
        XCTAssertEqual(
            OfficeLocalization.string(
                "번역에 없는 동적 명령",
                languages: ["en-US"]
            ),
            "번역에 없는 동적 명령"
        )
    }

    func testDockerDataChoicesAreLocalizedInEnglish() {
        XCTAssertEqual(
            OfficeLocalization.string(
                "현재 OFFICESTRA 데이터 사용",
                languages: ["en-US"]
            ),
            "Use Current OFFICESTRA Data"
        )
        XCTAssertEqual(
            OfficeLocalization.string(
                "기존 OFFICESTRA 데이터 사용",
                languages: ["en-US"]
            ),
            "Use Existing OFFICESTRA Data"
        )
        XCTAssertEqual(
            OfficeLocalization.string(
                "현재 형식과 구형 형식의 OFFICESTRA 데이터가 모두 발견됐습니다. 사용할 데이터를 선택하세요. 어느 쪽도 삭제되지 않습니다.",
                languages: ["en-US"]
            ),
            "Both current-format and legacy-format OFFICESTRA data were found. Choose which data to use. Neither volume will be deleted."
        )
    }

    func testEnglishDefaultIdentityPromptReturnsToCanonicalKoreanValue() {
        XCTAssertEqual(
            OfficeLocalization.canonicalIdentityPrompt(
                "Understand work instructions precisely and report the execution plan and results concisely."
            ),
            "업무 지시를 정확히 이해하고 실행 계획과 결과를 간결하게 보고한다."
        )
    }

    func testNewUIStringsAreLocalizedInEnglish() {
        let testCases: [(korean: String, english: String)] = [
            ("사내 위키", "Company Wiki"),
            ("승인된 지식과 확인", "Approved knowledge and verification"),
            ("위키 검색", "Search wiki"),
            ("거절 사유 (선택)", "Rejection reason (optional)"),
            ("예약이 가득 찼습니다 · 최대 %d개", "Queue is full · max %d"),
            ("터미널 상태를 확인하지 못했습니다", "Could not check terminal status"),
            ("5시간", "5H"),
            ("7일", "7D"),
            ("주간", "Weekly"),
            ("오늘 비용", "Today"),
            ("30일 비용", "30D"),
            ("답변 필요", "Needs answer"),
            ("작성 중인 응답", "Response in progress"),
            ("최종 응답", "Final response"),
            ("코드 복사", "Copy code"),
            ("파일 편집에 실패했습니다", "Failed to edit files"),
            ("미리보기에서 열기", "Open in Preview"),
            ("파일 목록 복사", "Copy file list"),
            ("변경 파일 %d개", "%d changed files"),
            ("지금 작업을 중단하고 이 예약으로 다시 질문", "Stop current task and send this queued prompt"),
            ("🫡 콜! 준비 완료", "🫡 Ready when you are!"),
            ("오늘 할당량 끝, 퇴근 모드 🌙", "Quota reached for today, off duty 🌙"),
            ("앗, 작업 멈춤\n%@", "Oops, task stopped\n%@"),
            ("작업 정리 중... 🧹", "Cleaning up tasks... 🧹"),
            ("작업이 멈췄어요.", "Task has stopped."),
            ("이전 기록 보기", "Show previous record"),
            ("다음 기록 보기", "Show next record"),
            ("목록", "List"),
            ("기록 목록으로 돌아가기", "Back to record list"),
            ("펼쳐 보기", "Expand"),
            ("기록을 넓은 창으로 펼치기", "Open the record in a larger view"),
            ("펼쳐 보기 닫기", "Close the expanded view"),
            ("이전 문서 보기", "Show previous document"),
            ("다음 문서 보기", "Show next document"),
            ("본문", "Body"),
            ("문서를 넓은 창으로 펼치기", "Open the document in a larger view"),
            ("문서 삭제", "Delete document"),
            ("일별", "Daily"),
            ("월별", "Monthly"),
            ("사용 현황 상세", "Usage details"),
            ("직원 × 모델 평가", "Ratings by employee × model"),
            ("평가가 많은 조합이 위에 옵니다.", "Combinations with the most ratings come first."),
            ("일별 사용", "Daily usage"),
            ("월별 사용", "Monthly usage"),
            ("모델별", "By model"),
            ("추론별", "By reasoning"),
            ("직원별", "By employee"),
            ("직원", "Employee"),
            ("기간", "Period"),
            ("턴", "Turns"),
            ("비용", "Cost"),
            ("좋아요율", "Like rate"),
            ("싫어요율", "Dislike rate"),
            ("토큰 %@", "%@ tokens"),
            ("좋아요 %d건", "%d likes"),
            ("싫어요 %d건", "%d dislikes"),
            ("제공자", "Provider"),
            ("집계 단위", "Granularity"),
            ("최근 30일 기록이 없습니다", "No records in the last 30 days"),
            ("최근 12개월 기록이 없습니다", "No records in the last 12 months"),
            ("이 제공자로 진행한 업무가 쌓이면 여기에 집계됩니다.", "Work done with this provider will be summarized here."),
            ("사용 현황을 불러오지 못했습니다", "Could not load usage details"),
            ("사용 현황을 불러오는 중", "Loading usage details"),
            ("%@ 사용 현황 상세 열기", "Open %@ usage details"),
            ("기간별 비용", "Cost by period"),
            ("오늘 포함 최근 30일 · %@ ~ %@", "Last 30 days including today · %@ – %@"),
            ("이번 달 포함 최근 12개월 · %@ ~ %@", "Last 12 months including this month · %@ – %@"),
            ("%@, 턴 %d, 좋아요 %d, 싫어요 %d, 좋아요율 %@", "%@, %d turns, %d likes, %d dislikes, like rate %@"),
            ("‘%@’ 문서를 삭제할까요?", "Delete the document ‘%@’?"),
            ("삭제한 문서는 목록과 검색에서 사라집니다. 같은 키의 제안이 다시 승인되면 다시 나타납니다.", "A deleted document disappears from the list and search. It reappears if a proposal with the same key is approved again."),
            ("‘%@’ 문서를 삭제했습니다.", "Deleted the document ‘%@’."),
            ("Antigravity 준비", "Set Up Antigravity"),
            ("Antigravity, Claude Code 또는 Codex가 확인할 파일을 선택하세요.", "Select files for Antigravity, Claude Code, or Codex to review."),
            ("응답 중인 터미널이 있습니다. 대화 모드로 돌아가면 해당 CLI 프로세스가 종료됩니다.", "A terminal is currently responding. Switching to conversation mode will terminate that CLI process."),
            ("일반 파일만 첨부할 수 있습니다: %@", "Only regular files can be attached: %@"),
            ("%@ · 새 버전", "%@ · New Version"),
            ("설치본 %@ → %@", "Installed %@ → %@"),
            ("Node 실행 파일을 찾을 수 없습니다.", "Node executable not found."),
            ("launchctl 실행에 실패했습니다.", "Failed to run launchctl."),
            ("생각 중", "Thinking"),
            ("협업 검토", "Review collaboration"),
            ("업무가 진행 중입니다.", "Work is in progress."),
            ("1턴당 평균 비용(추정) —", "Average cost per turn (est.) —"),
            ("전체 기간 · 비용이 기록된 종료 턴 기준", "All time · Finished turns with recorded costs")
        ]

        for testCase in testCases {
            XCTAssertEqual(
                OfficeLocalization.string(testCase.korean, languages: ["en-US"]),
                testCase.english,
                "Failed to localize '\(testCase.korean)'"
            )
        }
    }

    func testWhiteboardLabelsAreCompactInEnglishAndOriginalInKorean() {
        XCTAssertEqual(OfficeLocalization.string("5시간", languages: ["en-US"]), "5H")
        XCTAssertEqual(OfficeLocalization.string("7일", languages: ["en-US"]), "7D")
        XCTAssertEqual(OfficeLocalization.string("오늘 비용", languages: ["en-US"]), "Today")
        XCTAssertEqual(OfficeLocalization.string("30일 비용", languages: ["en-US"]), "30D")

        XCTAssertEqual(OfficeLocalization.string("5시간", languages: ["ko-KR"]), "5시간")
        XCTAssertEqual(OfficeLocalization.string("7일", languages: ["ko-KR"]), "7일")
        XCTAssertEqual(OfficeLocalization.string("오늘 비용", languages: ["ko-KR"]), "오늘 비용")
        XCTAssertEqual(OfficeLocalization.string("30일 비용", languages: ["ko-KR"]), "30일 비용")
    }
}

private extension String {
    var containsHangul: Bool {
        unicodeScalars.contains { (0xAC00...0xD7A3).contains($0.value) }
    }
}
