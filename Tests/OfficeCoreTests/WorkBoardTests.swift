import Foundation
import XCTest
@testable import OfficeGame

final class WorkBoardTests: XCTestCase {
    func testTicketEditClearsAssigneeAndDueDateWithoutChangingProject() throws {
        let input = WorkBoardTicketInput(
            projectId: nil,
            title: "검토",
            description: "",
            assigneeId: nil,
            completedById: nil,
            verifiedById: nil,
            state: "deferred",
            ticketType: "research",
            completionEvidence: "출처와 결론",
            userReview: "none",
            userReviewNote: "",
            targetTicketId: nil,
            verificationVerdict: nil,
            operationImpact: nil,
            resolutionReason: "추가 자료 대기",
            pushedCommitSha: nil,
            dueDate: nil,
            completionCriteria: "결과 확인",
            decisionPending: true,
            parentTicketId: nil,
            predecessorIds: [],
            goalIds: [],
            workRecordIds: [],
            reportedActorId: "user"
        )
        let data = try JSONEncoder().encode(input)
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: data) as? [String: Any]
        )
        XCTAssertNil(object["projectId"])
        XCTAssertTrue(object["assigneeId"] is NSNull)
        XCTAssertTrue(object["completedById"] is NSNull)
        XCTAssertTrue(object["verifiedById"] is NSNull)
        XCTAssertTrue(object["dueDate"] is NSNull)
        XCTAssertTrue(object["parentTicketId"] is NSNull)
        XCTAssertEqual(object["predecessorIds"] as? [String], [])
        XCTAssertEqual(object["goalIds"] as? [String], [])
        XCTAssertEqual(object["workRecordIds"] as? [String], [])
        XCTAssertEqual(object["state"] as? String, "deferred")
        XCTAssertEqual(object["ticketType"] as? String, "research")
        XCTAssertEqual(object["completionEvidence"] as? String, "출처와 결론")
        XCTAssertEqual(object["resolutionReason"] as? String, "추가 자료 대기")
        XCTAssertTrue(object["targetTicketId"] is NSNull)
        XCTAssertTrue(object["pushedCommitSha"] is NSNull)
        XCTAssertEqual(object["decisionPending"] as? Bool, true)
        XCTAssertEqual(object["reportedActorId"] as? String, "user")
    }

    func testGoalEditClearsOptionalKPIMeasures() throws {
        let input = WorkBoardGoalInput(
            projectId: nil,
            horizon: "monthly",
            periodStart: "2026-10-01",
            title: "검증",
            description: "",
            metricName: nil,
            metricUnit: nil,
            targetValue: nil,
            actualValue: nil
        )
        let data = try JSONEncoder().encode(input)
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: data) as? [String: Any]
        )
        XCTAssertNil(object["projectId"])
        XCTAssertTrue(object["metricName"] is NSNull)
        XCTAssertTrue(object["targetValue"] is NSNull)
        XCTAssertTrue(object["actualValue"] is NSNull)
    }

    func testProjectWindowRouteKeepsProjectAndPeople() throws {
        let route = WorkBoardWindowRoute(
            projectId: "project-1",
            baseURL: "http://127.0.0.1:4317/",
            assignees: [WorkBoardAssignee(id: "left-man", name: "클대리")]
        )
        XCTAssertEqual(try JSONDecoder().decode(
            WorkBoardWindowRoute.self,
            from: JSONEncoder().encode(route)
        ), route)
    }
}
