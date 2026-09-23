import AppKit
import XCTest
@testable import OfficeGame

@MainActor
final class LiveWorkspaceFeedJumpVisibilityTests: XCTestCase {
    func testAppearsAfterUserScrollsOneViewportAndHidesAtBottom() {
        let visibility = LiveWorkspaceFeedJumpVisibility()
        func snapshot(
            distanceFromBottom: CGFloat,
            viewportHeight: CGFloat = 400,
            contentHeight: CGFloat = 1200
        ) -> LiveWorkspaceFeedScrollSnapshot {
            LiveWorkspaceFeedScrollSnapshot(
                distanceFromTop: max(0, contentHeight - viewportHeight - distanceFromBottom),
                distanceFromBottom: distanceFromBottom,
                viewportHeight: viewportHeight,
                contentHeight: contentHeight
            )
        }

        // Growing content alone must not make the control appear.
        visibility.update(snapshot(distanceFromBottom: 500), bottomTolerance: 20)
        XCTAssertFalse(visibility.isVisible)

        visibility.userScrollStarted()
        visibility.update(snapshot(distanceFromBottom: 399), bottomTolerance: 20)
        XCTAssertFalse(visibility.isVisible)
        visibility.update(snapshot(distanceFromBottom: 400), bottomTolerance: 20)
        XCTAssertTrue(visibility.isVisible)
        visibility.userScrollEnded(snapshot(distanceFromBottom: 100), bottomTolerance: 20)
        XCTAssertTrue(visibility.isVisible)

        visibility.update(snapshot(distanceFromBottom: 20), bottomTolerance: 20)
        XCTAssertFalse(visibility.isVisible)
        visibility.update(snapshot(distanceFromBottom: 500), bottomTolerance: 20)
        XCTAssertFalse(visibility.isVisible)

        visibility.userScrollStarted()
        visibility.update(snapshot(
            distanceFromBottom: 0,
            viewportHeight: 400,
            contentHeight: 300
        ), bottomTolerance: 20)
        XCTAssertFalse(visibility.isVisible)
    }
}
