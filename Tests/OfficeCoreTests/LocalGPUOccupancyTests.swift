// 로컬 모델 시작이 그래픽 메모리 점유로 막혔을 때의 응답 해석과 팝업 선택지를 검증한다.

import XCTest
@testable import OfficeGame

final class LocalGPUOccupancyTests: XCTestCase {
    func testGPUOccupiedResponseBecomesReleasePrompt() throws {
        let data = Data(
            """
            {"error":"4090 그래픽 메모리를 다른 작업이 쓰고 있어 로컬 모델을 시작할 수 없습니다.",
             "code":"local-gpu-occupied",
             "gpu":{"vramPct":80,"ramPct":41,"comfyUI":"idle"}}
            """.utf8
        )

        guard
            case let .localGPUOccupied(message, occupancy) =
                OfficeDatabaseClient.backendError(from: data)
        else {
            return XCTFail("그래픽 메모리 점유 응답을 일반 오류로 바꾸면 안 됩니다.")
        }
        XCTAssertTrue(message.hasPrefix("4090 그래픽 메모리"))
        XCTAssertEqual(
            occupancy,
            LocalGPUOccupancy(vramPct: 80, ramPct: 41, comfyUI: .idle)
        )
    }

    func testOtherBackendErrorsStayPlainMessages() {
        let data = Data(#"{"error":"로컬 모델을 먼저 선택하세요."}"#.utf8)
        guard
            case let .backend(message) =
                OfficeDatabaseClient.backendError(from: data)
        else {
            return XCTFail("일반 오류는 기존 메시지 경로를 유지해야 합니다.")
        }
        XCTAssertEqual(message, "로컬 모델을 먼저 선택하세요.")
    }

    func testReleaseOptionsFollowComfyUIState() {
        func options(
            _ state: LocalGPUOccupancy.ComfyUIState
        ) -> [LocalComfyRelease] {
            LocalGPUOccupancy(vramPct: 80, ramPct: 40, comfyUI: state)
                .releaseOptions
        }

        XCTAssertEqual(options(.idle), [.free, .terminate])
        XCTAssertEqual(options(.running), [.interrupt, .terminate])
        XCTAssertEqual(options(.unavailable), [.terminate])
        XCTAssertEqual(
            options(.offline),
            [],
            "ComfyUI가 아닌 프로그램은 앱이 종료하거나 비우지 않습니다."
        )
    }

    func testPromptTextIsTranslatedForEnglish() {
        let keys = [
            "4090 그래픽 메모리 사용 중",
            "ComfyUI 메모리 비우고 시작",
            "생성 중인 작업 중단하고 비우기",
            "ComfyUI 종료하고 시작",
            "ComfyUI가 4090 그래픽 메모리 %d%%를 쓰고 있어 로컬 모델을 시작할 수 없습니다. 메모리를 비울 방법을 고르세요.",
            "ComfyUI가 이미지를 생성하며 4090 그래픽 메모리 %d%%를 쓰고 있습니다. 작업을 중단하거나 ComfyUI를 종료해야 로컬 모델을 시작할 수 있습니다.",
            "ComfyUI가 응답하지 않고 4090 그래픽 메모리 %d%%가 사용 중입니다. ComfyUI를 종료해야 로컬 모델을 시작할 수 있습니다.",
            "ComfyUI가 아닌 다른 프로그램이 4090 그래픽 메모리 %d%%를 쓰고 있습니다. 그 프로그램을 정리한 뒤 다시 시작하세요.",
        ]
        for key in keys {
            XCTAssertNotEqual(
                OfficeLocalization.string(key, languages: ["en"]),
                key,
                key
            )
        }
        XCTAssertTrue(
            OfficeLocalization.format(
                "ComfyUI가 4090 그래픽 메모리 %d%%를 쓰고 있어 로컬 모델을 시작할 수 없습니다. 메모리를 비울 방법을 고르세요.",
                arguments: [80],
                languages: ["en"]
            ).contains("80%")
        )
    }
}
