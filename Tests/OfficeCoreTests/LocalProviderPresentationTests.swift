import XCTest
@testable import OfficeGame

final class LocalProviderPresentationTests: XCTestCase {
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
