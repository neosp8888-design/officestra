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
                            for text in [palette.foreground, palette.secondary] {
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
