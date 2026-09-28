import SwiftUI

/// Shared game-card vocabulary. Large reading surfaces stay quiet; interactive
/// controls and character cards use progressively stronger model-colored light.
enum OfficeGameEmphasis: CaseIterable {
    case panel, control, hero, transcript, conversation
}

struct OfficeGamePalette {
    let isDark: Bool

    var base: Color {
        isDark ? Color(red: 0.075, green: 0.09, blue: 0.12)
            : Color(red: 0.97, green: 0.98, blue: 0.995)
    }
    var foreground: Color {
        isDark ? .white : Color(red: 0.10, green: 0.15, blue: 0.21)
    }
    var secondary: Color {
        isDark ? Color(red: 0.92, green: 0.94, blue: 0.97)
            : Color(red: 0.29, green: 0.34, blue: 0.40)
    }

    var codexForeground: Color {
        isDark ? Color(red: 0.65, green: 0.95, blue: 0.84)
            : Color(red: 0.045, green: 0.38, blue: 0.30)
    }

    func tintOpacity(for emphasis: OfficeGameEmphasis, highlighted: Bool) -> Double {
        switch emphasis {
        case .hero: isDark ? (highlighted ? 0.66 : 0.48) : (highlighted ? 0.22 : 0.12)
        case .control: isDark ? (highlighted ? 0.34 : 0.14) : (highlighted ? 0.17 : 0.06)
        case .panel: isDark ? 0.065 : 0.025
        case .transcript: isDark ? 0.18 : 0.10
        case .conversation: 0
        }
    }
}

struct OfficeGameSurface: View {
    var accent: Color = DashboardPalette.accent
    var emphasis: OfficeGameEmphasis = .panel
    var highlighted = false
    var cornerRadius: CGFloat = 14
    @Environment(\.colorScheme) private var colorScheme

    @ViewBuilder
    var body: some View {
        if emphasis == .conversation {
            RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                .fill(colorScheme == .dark ? OfficeGamePalette(isDark: true).base : .white)
                .overlay {
                    RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                        .strokeBorder(Color.primary.opacity(colorScheme == .dark ? 0.10 : 0.06), lineWidth: 1)
                }
                .allowsHitTesting(false)
                .accessibilityHidden(true)
        } else {
            decoratedSurface
        }
    }

    private var decoratedSurface: some View {
        let palette = OfficeGamePalette(isDark: colorScheme == .dark)
        return ZStack {
            palette.base
            LinearGradient(
                colors: [accent.opacity(palette.tintOpacity(for: emphasis, highlighted: highlighted)),
                    accent.opacity(emphasis == .hero ? (palette.isDark ? 0.12 : 0.025)
                        : (emphasis == .transcript ? (palette.isDark ? 0.09 : 0.05) : 0.015)),
                    accent.opacity(emphasis == .transcript ? (palette.isDark ? 0.06 : 0.035) : 0)],
                startPoint: .topLeading, endPoint: .bottomTrailing
            )
            if emphasis == .hero {
                OfficeGameFacet().fill(.white.opacity(palette.isDark ? (highlighted ? 0.06 : 0.04) : 0.48))
            }
            VStack {
                LinearGradient(colors: [.clear, .white.opacity(palette.isDark ? 0.36 : 0.9), .clear],
                    startPoint: .leading, endPoint: .trailing)
                    .frame(height: 1)
                Spacer(minLength: 0)
                if emphasis == .hero || highlighted {
                    accent.opacity(highlighted ? 0.75 : 0.35).frame(height: 2)
                }
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                .strokeBorder(
                    LinearGradient(colors: [
                        .white.opacity(palette.isDark ? (highlighted ? 0.65 : 0.25) : 0.96),
                        accent.opacity(emphasis == .panel ? 0.10 : (highlighted ? 0.48 : 0.25)),
                        .primary.opacity(palette.isDark ? 0.08 : 0.10)],
                        startPoint: .topLeading, endPoint: .bottomTrailing),
                    lineWidth: 1
                )
        }
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }
}

/// No polling, per-frame state updates or repeatForever animation. The caller
/// owns label sizing so adopting this style does not alter its hit target.
struct OfficeGameButtonStyle: ButtonStyle {
    var accent: Color = DashboardPalette.accent
    var emphasis: OfficeGameEmphasis = .control
    var isSelected = false
    var cornerRadius: CGFloat = 14
    var horizontalPadding: CGFloat = 0
    /// Dense transcript tools must not multiply glow/shadow layers while scrolling.
    var compact = false
    var foregroundColor: Color? = nil
    @State private var isHovered = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.isEnabled) private var isEnabled
    @Environment(\.colorScheme) private var colorScheme

    func makeBody(configuration: Configuration) -> some View {
        let highlighted = isEnabled && (isHovered || isSelected)
        label(configuration)
            .padding(.horizontal, horizontalPadding)
            .background {
                OfficeGameSurface(accent: accent, emphasis: emphasis,
                    highlighted: highlighted, cornerRadius: cornerRadius)
            }
            .shadow(color: compact ? .clear : accent.opacity(highlighted ? 0.20 : 0.06),
                radius: compact ? 0 : (highlighted ? 6 : 2), y: compact ? 0 : 2)
            .scaleEffect(reduceMotion ? 1 : (configuration.isPressed ? 0.97
                : (highlighted && emphasis == .hero ? 1.015 : 1)))
            .animation(reduceMotion ? nil : .easeOut(duration: 0.16), value: highlighted)
            .animation(reduceMotion ? nil : .easeOut(duration: 0.10), value: configuration.isPressed)
            .opacity(isEnabled ? 1 : 0.5)
            .officeHover { isHovered = $0 }
    }

    @ViewBuilder
    private func label(_ configuration: Configuration) -> some View {
        if compact {
            // Preserve model/status colors supplied by the caller.
            configuration.label
        } else {
            configuration.label
                .foregroundStyle(foregroundColor ?? OfficeGamePalette(isDark: colorScheme == .dark).foreground)
        }
    }
}

private struct OfficeToolEmphasisKey: EnvironmentKey {
    static let defaultValue = false
}

private struct OfficeHoverEffectsEnabledKey: EnvironmentKey {
    static let defaultValue = true
}

extension EnvironmentValues {
    var officeHoverEffectsEnabled: Bool {
        get { self[OfficeHoverEffectsEnabledKey.self] }
        set { self[OfficeHoverEffectsEnabledKey.self] = newValue }
    }

    var officeToolEmphasized: Bool {
        get { self[OfficeToolEmphasisKey.self] }
        set { self[OfficeToolEmphasisKey.self] = newValue }
    }
}

/// Text and icon actions stay unboxed. Hover changes their stroke weight only.
struct OfficeTextToolStyle: ButtonStyle {
    @State private var isHovered = false
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .environment(\.officeToolEmphasized, isEnabled && (isHovered || configuration.isPressed))
            .contentShape(Rectangle())
            .opacity(isEnabled ? 1 : 0.5)
            .officeHover { isHovered = $0 }
    }
}

private struct OfficeToolFont: ViewModifier {
    let size: CGFloat
    let weight: Font.Weight
    @Environment(\.officeToolEmphasized) private var isEmphasized

    func body(content: Content) -> some View {
        content.font(.system(size: size, weight: isEmphasized ? .heavy : weight))
    }
}

private struct OfficeHoverEffect: ViewModifier {
    let action: (Bool) -> Void
    @Environment(\.officeHoverEffectsEnabled) private var isEnabled

    @ViewBuilder
    func body(content: Content) -> some View {
        if isEnabled {
            content.onHover(perform: action)
        } else {
            content
        }
    }
}

extension View {
    /// Conversation surfaces do not install decorative hover tracking.
    func officeHover(perform action: @escaping (Bool) -> Void) -> some View {
        modifier(OfficeHoverEffect(action: action))
    }

    func officeTextTool() -> some View {
        buttonStyle(OfficeTextToolStyle())
    }

    func officeToolFont(size: CGFloat, weight: Font.Weight = .semibold) -> some View {
        modifier(OfficeToolFont(size: size, weight: weight))
    }

    /// Small actions use the same material without a halo or added layout padding.
    func officeGameTool(accent: Color = DashboardPalette.accent, selected: Bool = false) -> some View {
        buttonStyle(OfficeGameButtonStyle(accent: accent, isSelected: selected,
            cornerRadius: 7, compact: true))
    }

    func officeGameSurface(accent: Color = DashboardPalette.accent,
                           emphasis: OfficeGameEmphasis = .panel,
                           cornerRadius: CGFloat = 14) -> some View {
        background { OfficeGameSurface(accent: accent, emphasis: emphasis, cornerRadius: cornerRadius) }
    }
}

private struct OfficeGameFacet: Shape {
    func path(in rect: CGRect) -> Path {
        Path { path in
            path.move(to: CGPoint(x: rect.width * 0.62, y: 0))
            path.addLine(to: CGPoint(x: rect.maxX, y: 0))
            path.addLine(to: CGPoint(x: rect.maxX, y: rect.height))
            path.addLine(to: CGPoint(x: rect.width * 0.34, y: rect.height))
            path.closeSubpath()
        }
    }
}
