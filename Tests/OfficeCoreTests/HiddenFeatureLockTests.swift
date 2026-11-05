// 숨긴 기능 암호 확인과 화분 클릭 영역이 다른 클릭 영역을 가리지 않는지 확인한다.
import OfficeCore
import XCTest
@testable import OfficeGame

final class HiddenFeatureLockTests: XCTestCase {
    func testDigestIsStandardSHA256Hex() {
        XCTAssertEqual(
            HiddenFeatureLock.digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        )
    }

    func testWrongPasswordsDoNotUnlock() {
        for password in ["", " ", "password", "6516", "zang"] {
            XCTAssertFalse(HiddenFeatureLock.matches(password), "'\(password)' must not unlock")
        }
    }

    func testPlantHitboxesStayOnCanvasAndAwayFromOtherTargets() {
        let canvas = CGRect(origin: .zero, size: OfficeCanvasGeometry.designSize)
        for style in OfficeArtStyle.allCases {
            let plants = OfficeInteractionGeometry.plantHitboxes(artStyle: style)
            XCTAssertEqual(plants.count, 2)
            let whiteboard = OfficeWhiteboardGeometry.interactionRect(for: style)
            for plant in plants {
                XCTAssertTrue(canvas.contains(plant), "\(style) plant \(plant) leaves the canvas")
                XCTAssertFalse(plant.intersects(whiteboard), "\(style) plant \(plant) covers the whiteboard")
            }
        }
    }
}
