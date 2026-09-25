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
            state: "blocked",
            dueDate: nil,
            completionCriteria: "결과 확인",
            decisionPending: true,
            parentTicketId: nil,
            predecessorIds: [],
            goalIds: [],
            workRecordIds: []
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
        XCTAssertEqual(object["state"] as? String, "blocked")
        XCTAssertEqual(object["decisionPending"] as? Bool, true)
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
