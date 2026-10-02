import AppKit
import OfficeCore
import SwiftUI
import XCTest
@testable import OfficeGame

final class OfficeGamePaletteTests: XCTestCase {
    func testTextContrastAcrossThemesProvidersAndInteractionStates() throws {
        for dark in [false, true] {
            let palette = OfficeGamePalette(isDark: dark)
            for backend: AgentBackend in [.codex, .claude, .antigravity] {
                let accent = try rgb(DashboardPalette.providerAccent(for: backend))
                for emphasis in OfficeGameEmphasis.allCases {
                    for highlighted in [false, true] {
                        let base = try rgb(palette.base)
                        let tinted = blend(accent, over: base,
                            opacity: palette.tintOpacity(for: emphasis, highlighted: highlighted))
                        let facet = emphasis == .hero
                            ? blend([1, 1, 1], over: tinted, opacity: dark ? (highlighted ? 0.06 : 0.04) : 0.48)
                            : tinted
                        // Check both ends of the gradient and its brightest facet.
                        for background in [base, tinted, facet] {
                            let foregrounds = [palette.foreground, palette.secondary]
                                + (backend == .codex ? [palette.codexForeground] : [])
                            for text in foregrounds {
                                let ratio = contrast(try rgb(text), background)
                                XCTAssertGreaterThanOrEqual(ratio, 4.5,
                                    "dark=\(dark), provider=\(backend), emphasis=\(emphasis), highlighted=\(highlighted)")
                            }
                        }
                    }
                }
            }
        }
    }

    func testPromptOriginColorsReadInBothThemesAndStayApartFromModelColors() throws {
        for dark in [false, true] {
            let palette = OfficeGamePalette(isDark: dark)
            let base = try rgb(palette.base)
            for origin in LiveTurnPromptOrigin.allCases {
                let accent = try rgb(origin.accent)
                let bubble = blend(accent, over: base, opacity: palette.tintOpacity(for: .prompt, highlighted: false))
                XCTAssertGreaterThanOrEqual(contrast(try rgb(palette.foreground), bubble), 4.5, "dark=\(dark), origin=\(origin)")
                if origin == .remote {
                    // The "원격 지시" label is bold 11 pt text in the accent color.
                    XCTAssertGreaterThanOrEqual(contrast(accent, bubble), 3, "dark=\(dark)")
                }
            }
        }
        let origins = try LiveTurnPromptOrigin.allCases.map { try hue(rgb($0.accent)) }
        let models = try [AgentBackend.codex, .claude, .antigravity]
            .map { try hue(rgb(DashboardPalette.providerAccent(for: $0))) }
        for (index, origin) in origins.enumerated() {
            for model in models {
                XCTAssertGreaterThan(hueDistance(origin, model), 30, "origin=\(LiveTurnPromptOrigin.allCases[index])")
            }
            for other in origins[(index + 1)...] {
                XCTAssertGreaterThan(hueDistance(origin, other), 30)
            }
        }
    }

    private func hue(_ components: [Double]) -> Double {
        let color = NSColor(srgbRed: components[0], green: components[1], blue: components[2], alpha: 1)
        return Double(color.hueComponent) * 360
    }

    private func hueDistance(_ a: Double, _ b: Double) -> Double {
        let distance = abs(a - b).truncatingRemainder(dividingBy: 360)
        return min(distance, 360 - distance)
    }

    private func rgb(_ color: Color) throws -> [Double] {
        let color = try XCTUnwrap(NSColor(color).usingColorSpace(.sRGB))
        return [Double(color.redComponent), Double(color.greenComponent), Double(color.blueComponent)]
    }

    private func blend(_ color: [Double], over background: [Double], opacity: Double) -> [Double] {
        zip(color, background).map { $0 * opacity + $1 * (1 - opacity) }
    }

    private func contrast(_ a: [Double], _ b: [Double]) -> Double {
        func luminance(_ components: [Double]) -> Double {
            let linear = components.map { $0 <= 0.04045 ? $0 / 12.92 : pow(($0 + 0.055) / 1.055, 2.4) }
            return zip(linear, [0.2126, 0.7152, 0.0722]).map(*).reduce(0, +)
        }
        let x = luminance(a), y = luminance(b)
        return (max(x, y) + 0.05) / (min(x, y) + 0.05)
    }
}
