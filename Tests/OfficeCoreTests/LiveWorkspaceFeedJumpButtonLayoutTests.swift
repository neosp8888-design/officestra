// 이 파일은 대화 최하단 이동 버튼의 고정 위치를 검증한다.

import SwiftUI
import XCTest
@testable import OfficeGame

final class LiveWorkspaceFeedJumpButtonLayoutTests: XCTestCase {
    func testJumpButtonUsesBottomLeadingOverlay() {
        XCTAssertEqual(
            LiveWorkspaceFeedJumpButtonLayout.alignment,
            .bottomLeading
        )
    }

    func testJumpButtonKeepsExistingVerticalPosition() {
        XCTAssertEqual(
            LiveWorkspaceFeedJumpButtonLayout.bottomPadding,
            12
        )
    }

    func testJumpButtonCenterAlignsWithCharacterAvatarCenter() {
        let buttonCenter =
            LiveWorkspaceFeedJumpButtonLayout.leadingPadding
            + LiveWorkspaceFeedJumpButtonLayout.diameter / 2
        let avatarCenter =
            LiveWorkspaceFeedJumpButtonLayout.contentHorizontalPadding
            + LiveWorkspaceFeedJumpButtonLayout.avatarDiameter / 2

        XCTAssertEqual(buttonCenter, avatarCenter)
        XCTAssertEqual(
            LiveWorkspaceFeedJumpButtonLayout.leadingPadding,
            27
        )
    }
}
