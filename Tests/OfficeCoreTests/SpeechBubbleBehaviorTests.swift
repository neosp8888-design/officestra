// 이 파일은 말풍선 열람과 유휴 직원 자동 대화 정책을 검증한다.

import AppKit
import OfficeCore
import SwiftUI
import XCTest
@testable import OfficeGame

@MainActor
final class SpeechBubbleBehaviorTests: XCTestCase {
    func testViewedOrdinaryBubbleDisappearsImmediately() {
        let director = AgentDirector(startBackgroundTasks: false)
        director.speechBubbleStore.set("완료했습니다.", for: .rightWoman)

        director.dismissViewedBubble(for: .rightWoman)

        XCTAssertNil(director.bubbles[.rightWoman])
    }

    func testMountedEquatableLayerKeepsIdleChatterHidden() throws {
        let director = AgentDirector(startBackgroundTasks: false)
        director.speechBubbleStore.set("완료했습니다.", for: .rightWoman)
        let size = NSSize(width: 384, height: 256)
        let hostingView = NSHostingView(
            rootView: CharacterInteractionLayer(
                director: director,
                artStyle: .twoD,
                onMonitorTapped: { _ in },
                onArchiveCabinetTapped: {},
                onWhiteboardTapped: {},
                onBubbleTapped: { _, _ in }
            )
            .equatable()
            .frame(width: size.width, height: size.height)
            .background(Color.black)
        )
        hostingView.frame = NSRect(origin: .zero, size: size)
        settle(hostingView, for: 0.1)

        let originalFrame = hostingView.frame
        let before = try renderedBitmap(of: hostingView)
        let beforeBrightPixels = sampledBrightPixelCount(in: before)
        // The status-only design intentionally hides ordinary idle chatter.
        XCTAssertEqual(beforeBrightPixels, 0)

        director.dismissViewedBubble(for: .rightWoman)
        settle(hostingView, for: 0.6)

        let after = try renderedBitmap(of: hostingView)
        let afterBrightPixels = sampledBrightPixelCount(in: after)
        XCTAssertEqual(hostingView.frame, originalFrame)
        XCTAssertEqual(afterBrightPixels, 0)
    }

    func testMountedWarningBubbleDisappearsWithoutResize() throws {
        let director = AgentDirector(startBackgroundTasks: false)
        director.speechBubbleStore.set("작업을 확인해 주세요.", for: .rightWoman)
        let size = NSSize(width: 384, height: 256)
        let frame = NSRect(origin: .zero, size: size)
        let presentation = CharacterInteractionPresentationState(
            characters: director.characters,
            displayNames: [:],
            archiveCabinetHitbox: director.archiveCabinetHitbox,
            runningCharacters: [], questionCharacters: [],
            failedCharacters: [.rightWoman], offDutyCharacters: []
        )
        let hostingView = NSHostingView(
            rootView: CharacterSpeechBubbleLayer(
                speechBubbleStore: director.speechBubbleStore,
                presentation: presentation,
                artStyle: .twoD,
                fittedFrame: frame,
                scale: size.width / OfficeCanvasGeometry.designSize.width,
                selectCharacter: { _ in },
                onBubbleTapped: { _, _ in }
            )
            .frame(width: size.width, height: size.height)
            .background(Color.black)
        )
        hostingView.frame = frame
        settle(hostingView, for: 0.1)
        let beforeBrightPixels = sampledBrightPixelCount(in: try renderedBitmap(of: hostingView))
        XCTAssertGreaterThan(beforeBrightPixels, 25)

        // Keep the same view and presentation: only the observed message store
        // changes, as it does when a warning is acknowledged in the app.
        director.dismissViewedBubble(for: .rightWoman)
        settle(hostingView, for: 0.6)
        let afterBrightPixels = sampledBrightPixelCount(in: try renderedBitmap(of: hostingView))
        XCTAssertEqual(hostingView.frame, frame)
        XCTAssertLessThan(afterBrightPixels, beforeBrightPixels / 4)
    }

    func testIdleEmployeeCanSpeakWhileAnotherEmployeeIsWorking() throws {
        let configuration = try CharacterConfigurationAsset.load()

        let candidates = SpeechBubbleIdleChatterPolicy.candidates(
            characters: configuration.characters,
            runningCharacters: [.rightWoman],
            occupiedCharacters: [.rightWoman],
            questionCharacters: [],
            failedCharacters: [],
            offDutyCharacters: [],
            lastCharacter: nil
        )

        XCTAssertEqual(
            Set(candidates.map(\.id)),
            Set(OfficeCharacter.allCases).subtracting([.rightWoman])
        )
    }

    func testIdleChatterSkipsBusyAndProtectedEmployees() throws {
        let configuration = try CharacterConfigurationAsset.load()

        let candidates = SpeechBubbleIdleChatterPolicy.candidates(
            characters: configuration.characters,
            runningCharacters: [.boss],
            occupiedCharacters: [.leftMan],
            questionCharacters: [.leftWoman],
            failedCharacters: [.rightMan],
            offDutyCharacters: [],
            lastCharacter: .rightWoman
        )

        XCTAssertEqual(candidates.map(\.id), [.rightWoman])
    }

    func testIdleChatterAvoidsPreviousEmployeeWhenOthersAreAvailable() throws {
        let configuration = try CharacterConfigurationAsset.load()

        let candidates = SpeechBubbleIdleChatterPolicy.candidates(
            characters: configuration.characters,
            runningCharacters: [.boss],
            occupiedCharacters: [],
            questionCharacters: [],
            failedCharacters: [],
            offDutyCharacters: [],
            lastCharacter: .leftMan
        )

        XCTAssertFalse(candidates.map(\.id).contains(.leftMan))
        XCTAssertFalse(candidates.isEmpty)
    }

    func testIdleChatterDoesNotImmediatelyRepeatItsMessage() {
        XCTAssertEqual(
            SpeechBubbleIdleChatterPolicy.messages(
                from: ["첫 문구", "둘째 문구", "셋째 문구"],
                excluding: "둘째 문구"
            ),
            ["첫 문구", "셋째 문구"]
        )
    }

    private func settle(_ view: NSView, for interval: TimeInterval) {
        view.layoutSubtreeIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(interval))
        view.layoutSubtreeIfNeeded()
        view.displayIfNeeded()
    }

    private func renderedBitmap(of view: NSView) throws -> NSBitmapImageRep {
        guard let bitmap = view.bitmapImageRepForCachingDisplay(
            in: view.bounds
        ) else {
            throw XCTSkip("말풍선 레이어 비트맵을 만들 수 없습니다.")
        }
        view.cacheDisplay(in: view.bounds, to: bitmap)
        return bitmap
    }

    private func sampledBrightPixelCount(in bitmap: NSBitmapImageRep) -> Int {
        stride(from: 0, to: bitmap.pixelsHigh, by: 4).reduce(into: 0) {
            count,
            y in
            for x in stride(from: 0, to: bitmap.pixelsWide, by: 4) {
                guard let color = bitmap.colorAt(x: x, y: y)?
                    .usingColorSpace(.deviceRGB)
                else {
                    continue
                }
                if color.redComponent > 0.65,
                   color.greenComponent > 0.65,
                   color.blueComponent > 0.65,
                   color.alphaComponent > 0.2
                {
                    count += 1
                }
            }
        }
    }
}
