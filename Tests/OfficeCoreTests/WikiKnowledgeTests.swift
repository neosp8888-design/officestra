// 이 파일은 승인형 사내 위키의 API 계약과 패널 표시 정책을 검증한다.

import Foundation
import XCTest
@testable import OfficeGame

final class WikiKnowledgeTests: XCTestCase {
    private let client = OfficeDatabaseClient(
        baseURL: URL(string: "http://127.0.0.1:4317")!
    )

    func testPagesRequestIncludesQueryAndLimit() throws {
        let url = client.wikiPagesURL(query: "캐시 비용", limit: 60)
        let components = try XCTUnwrap(
            URLComponents(url: url, resolvingAgainstBaseURL: false)
        )

        XCTAssertEqual(components.path, "/api/wiki/pages")
        XCTAssertEqual(
            Dictionary(
                uniqueKeysWithValues: components.queryItems?.map {
                    ($0.name, $0.value ?? "")
                } ?? []
            ),
            ["query": "캐시 비용", "limit": "60"]
        )
    }

    func testPendingProposalRequestUsesRequiredState() throws {
        let url = client.wikiProposalsURL(state: "pending_user")
        let components = try XCTUnwrap(
            URLComponents(url: url, resolvingAgainstBaseURL: false)
        )

        XCTAssertEqual(components.path, "/api/wiki/proposals")
        XCTAssertEqual(
            components.queryItems,
            [URLQueryItem(name: "state", value: "pending_user")]
        )
    }

    func testApprovalRequestPostsEmptyJSONBody() throws {
        let request = client.wikiProposalApprovalRequest(id: "proposal-1")

        XCTAssertEqual(request.httpMethod, "POST")
        XCTAssertEqual(
            request.url?.path,
            "/api/wiki/proposals/proposal-1/approve"
        )
        XCTAssertEqual(
            request.value(forHTTPHeaderField: "content-type"),
            "application/json"
        )
        XCTAssertEqual(
            request.value(
                forHTTPHeaderField: "X-OFFICESTRA-User-Decision"
            ),
            "approve:proposal-1"
        )
        XCTAssertEqual(request.httpBody, Data("{}".utf8))
    }

    func testDeletionRequestSendsUserDecisionHeader() {
        let request = client.wikiPageDeletionRequest(id: "page-3")

        XCTAssertEqual(request.httpMethod, "DELETE")
        XCTAssertEqual(request.url?.path, "/api/wiki/pages/page-3")
        XCTAssertEqual(
            request.value(forHTTPHeaderField: "content-type"),
            "application/json"
        )
        XCTAssertEqual(
            request.value(
                forHTTPHeaderField: "X-OFFICESTRA-User-Decision"
            ),
            "delete:page-3"
        )
        XCTAssertNil(request.httpBody)
    }

    func testRejectionRequestKeepsOptionalReason() throws {
        let request = try client.wikiProposalRejectionRequest(
            id: "proposal-2",
            reason: "근거가 부족합니다."
        )
        let body = try XCTUnwrap(request.httpBody)
        let payload = try JSONDecoder().decode(
            [String: String].self,
            from: body
        )

        XCTAssertEqual(request.httpMethod, "POST")
        XCTAssertEqual(
            request.url?.path,
            "/api/wiki/proposals/proposal-2/reject"
        )
        XCTAssertEqual(
            request.value(
                forHTTPHeaderField: "X-OFFICESTRA-User-Decision"
            ),
            "reject:proposal-2"
        )
        XCTAssertEqual(payload, ["reason": "근거가 부족합니다."])
    }

    func testWikiResponsesDecodeFractionalAndStandardDates() throws {
        let pages = try client.decodeWikiPages(
            Data(
                """
                {
                  "pages": [{
                    "id": "page-1",
                    "pageKey": "cost/cache",
                    "title": "캐시 비용",
                    "body": "# 정책\\n본문",
                    "updatedAt": "2026-08-13T10:11:12.345Z",
                    "sources": [{
                      "workRecordId": "record-1",
                      "title": "비용 검증",
                      "excerpt": "캐시 비용을 확인했습니다."
                    }]
                  }]
                }
                """.utf8
            )
        )
        let proposals = try client.decodeWikiProposals(
            Data(
                """
                {
                  "proposals": [{
                    "id": "proposal-1",
                    "state": "pending_user",
                    "pageKey": "cost/cache",
                    "title": "캐시 비용 갱신",
                    "body": "검토할 본문",
                    "approvalTier": "user",
                    "sourceRecordIds": ["record-1"],
                    "createdAt": "2026-08-13T10:12:00Z"
                  }]
                }
                """.utf8
            )
        )

        XCTAssertEqual(pages.first?.sources.first?.workRecordId, "record-1")
        XCTAssertEqual(pages.first?.body, "# 정책\n본문")
        XCTAssertEqual(proposals.first?.state, "pending_user")
        XCTAssertEqual(proposals.first?.sourceRecordIds, ["record-1"])
    }

    func testPageSelectionKeepsCurrentOrFallsBackToFirst() throws {
        let pages = try client.decodeWikiPages(
            Data(
                """
                {
                  "pages": [
                    {
                      "id": "page-1",
                      "pageKey": "one",
                      "title": "첫 문서",
                      "body": "본문",
                      "updatedAt": "2026-08-13T10:00:00Z",
                      "sources": []
                    },
                    {
                      "id": "page-2",
                      "pageKey": "two",
                      "title": "둘째 문서",
                      "body": "본문",
                      "updatedAt": "2026-08-13T10:00:00Z",
                      "sources": []
                    }
                  ]
                }
                """.utf8
            )
        )

        XCTAssertEqual(
            WikiKnowledgeSelection.resolvedPageID(
                current: "page-2",
                pages: pages
            ),
            "page-2"
        )
        XCTAssertEqual(
            WikiKnowledgeSelection.resolvedPageID(
                current: "missing",
                pages: pages
            ),
            "page-1"
        )
        XCTAssertNil(
            WikiKnowledgeSelection.resolvedPageID(
                current: "page-1",
                pages: []
            )
        )
    }

    func testRejectionReasonIsOptionalAndWhitespaceIsRemoved() {
        XCTAssertEqual(
            WikiKnowledgeSelection.normalizedRejectionReason("  보류  \n"),
            "보류"
        )
        XCTAssertEqual(
            WikiKnowledgeSelection.normalizedRejectionReason("   "),
            ""
        )
    }

    func testWikiUsesStackedLayoutOnlyAtNarrowWidth() {
        XCTAssertTrue(WikiKnowledgeLayout.usesStackedLayout(for: 360))
        XCTAssertFalse(WikiKnowledgeLayout.usesStackedLayout(for: 470))
    }

    func testDetailPanelIncludesCompactWikiDestination() {
        XCTAssertEqual(
            OfficeDetailSelection.allCases,
            [.archive, .usage, .wiki, .workBoard]
        )
        XCTAssertEqual(OfficeDetailSelection.wiki.title, "사내 위키")
        XCTAssertEqual(
            OfficeDetailSelection.archive.subtitle,
            "직원 업무 기록"
        )
        XCTAssertEqual(
            OfficeDetailSelection.usage.subtitle,
            "현재 사용 가능량"
        )
        XCTAssertEqual(
            OfficeDetailSelection.wiki.subtitle,
            "승인된 지식과 확인"
        )
        XCTAssertEqual(
            OfficeDetailSelection.wiki.icon,
            "text.book.closed.fill"
        )
        XCTAssertEqual(OfficeDetailSelection.workBoard.title, "업무 보드")
    }
}
