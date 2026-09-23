// 이 파일은 대화 최하단 이동 버튼의 하단 중앙 고정 위치와 치수를 검증한다.

import SwiftUI
import XCTest
@testable import OfficeGame

final class LiveWorkspaceFeedJumpButtonLayoutTests: XCTestCase {
    func testJumpButtonUsesBottomCenterOverlay() {
        XCTAssertEqual(
            LiveWorkspaceFeedJumpButtonLayout.alignment,
            .bottom
        )
    }

    func testJumpButtonKeepsVerticalPosition() {
        XCTAssertEqual(
            LiveWorkspaceFeedJumpButtonLayout.bottomPadding,
            3
        )
    }

    func testJumpButtonUsesSlimCapsuleDimensions() {
        XCTAssertEqual(
            LiveWorkspaceFeedJumpButtonLayout.width,
            32
        )
        XCTAssertEqual(
            LiveWorkspaceFeedJumpButtonLayout.height,
            13
        )
    }
}
