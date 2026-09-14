import XCTest
@testable import OfficeGame

final class LocalProviderPresentationTests: XCTestCase {
    func testAddressDecodeAndIPv4Validation() throws {
        let data = Data(#"{"characterId":"right-woman","profileId":"local","address":"222.109.147.73"}"#.utf8)
        XCTAssertEqual(try JSONDecoder().decode(LocalProfileAssignment.self, from: data).address, "222.109.147.73")
        for value in ["222.109.147.73", " 192.168.0.10 "] { XCTAssertTrue(LocalHostAddressInput.isValid(value)) }
        for value in ["", "1.2.3.256", "01.2.3.4", "1.2.3", "1.2.3.4:22", "http://1.2.3.4", "::1", "1.2.3.4;ls"] { XCTAssertFalse(LocalHostAddressInput.isValid(value), value) }
    }
    func testVerifiedReasoningOptionsAndSelectionDecode() throws {
        let json = #"{"profiles":[{"id":"qwen38","enabled":true,"model":"qwen","contextWindow":65536,"reasoningOptions":["default","on","off"]}],"assignments":[{"characterId":"right-woman","profileId":"qwen38","reasoning":"on"}],"statuses":[]}"#
        let catalog = try JSONDecoder().decode(LocalProviderList.self, from: Data(json.utf8))
        XCTAssertEqual(catalog.profiles?.first?.reasoningOptions, ["default", "on", "off"])
        XCTAssertEqual(catalog.assignments?.first?.reasoning, "on")
    }
    func testContextLabelUsesActualProfileWindow() throws {
        for (tokens, expected) in [(32768, "32K"), (65536, "64K"), (262144, "256K")] {
            let json = "{\"id\":\"local-test\",\"enabled\":true,\"model\":\"qwen\",\"contextWindow\":\(tokens)}"
            let profile = try JSONDecoder().decode(LocalModelOption.self, from: Data(json.utf8))
            XCTAssertEqual(profile.contextWindowTitle, expected)
        }
    }

    func testSelectableLocalCatalogAndEmployeeAssignmentDecode() throws {
        let json = #"{"profiles":[{"id":"local-4090","enabled":true,"model":"officestra-local-smoke","title":"qwen3.6-27b","contextWindow":32768}],"assignments":[{"characterId":"left-man","profileId":"local-4090"}],"statuses":[]}"#
        let catalog = try JSONDecoder().decode(LocalProviderList.self, from: Data(json.utf8))
        XCTAssertEqual(catalog.profiles?.first?.displayTitle, "qwen3.6-27b")
        XCTAssertEqual(catalog.assignments?.first?.characterId, "left-man")
        XCTAssertEqual(catalog.assignments?.first?.profileId, "local-4090")
    }

    func testPreviousBackendCatalogStillDecodesBeforeRestart() throws {
        let json = #"{"profiles":[{"id":"local-4090","enabled":false,"model":"qwen-test","contextWindow":32768}],"statuses":[]}"#
        let catalog = try JSONDecoder().decode(LocalProviderList.self, from: Data(json.utf8))
        XCTAssertNil(catalog.assignments)
        XCTAssertEqual(catalog.profiles?.first?.displayTitle, "qwen-test")
    }

    func testLocalFeedIdentitySurvivesCopies() throws {
        let json = #"{"id":"turn","characterId":"boss","characterName":"Test","characterBackend":"claude","backend":"claude","providerKind":"local","origin":"terminal","prompt":"hi","response":"hello","status":"completed","needsInput":false,"startedAt":"2026-09-11T00:00:00Z","updatedAt":"2026-09-11T00:00:01Z","activities":[],"sessionContext":{"usedTokens":3300,"limitTokens":32768}}"#
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        let turn = try decoder.decode(LiveFeedTurn.self, from: Data(json.utf8))
        XCTAssertEqual(turn.providerKind, "local")
        XCTAssertNil(turn.estimatedCostUsd)
        XCTAssertEqual(turn.replacingID(with: "copy").providerKind, "local")
        XCTAssertEqual(turn.sessionContext?.limitTokens, 32768)
    }

    func testProviderStatusesDecodeWithoutCloudQuotaOrPrice() throws {
        for state in ["waiting", "starting", "ready", "error", "idle"] {
            let json = "{\"id\":\"local-test\",\"state\":\"\(state)\",\"error\":null}"
            let status = try JSONDecoder().decode(LocalProviderStatus.self, from: Data(json.utf8))
            XCTAssertEqual(status.state, state)
            XCTAssertFalse(status.displayText.isEmpty)
        }
    }
}
