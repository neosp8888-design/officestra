import OfficeCore
import SwiftUI

/// The same selection drives the shared composer, active split pane and header.
struct ComposerCharacterPicker: View {
    @ObservedObject private var director: AgentDirector
    @ObservedObject private var selection: CharacterSelectionStore
    @State private var isPresented = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.colorScheme) private var colorScheme

    private var palette: OfficeGamePalette { OfficeGamePalette(isDark: colorScheme == .dark) }

    init(director: AgentDirector) {
        self.director = director
        _selection = ObservedObject(wrappedValue: director.characterSelectionStore)
    }

    private var current: OfficeCharacter { selection.selectedCharacterID ?? .boss }
    private var accent: Color {
        DashboardPalette.providerAccent(for: director.selectedCharacter?.backend ?? .codex)
    }

    var body: some View {
        Button { isPresented.toggle() } label: {
            HStack(spacing: 9) {
                portrait(for: current, accent: accent, size: 34)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 3) {
                        Text(director.displayName(for: current))
                            .font(.system(size: 12, weight: .bold))
                            .lineLimit(1)
                        status(for: current)
                    }
                    Text((director.selectedCharacter?.backend.title ?? "Codex").uppercased())
                        .font(.system(size: 8, weight: .bold, design: .rounded))
                        .tracking(1.2)
                        .foregroundStyle(palette.secondary)
                }
                Spacer(minLength: 1)
                Image(systemName: "chevron.up.chevron.down")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(palette.foreground.opacity(0.88))
                    .frame(width: 20, height: 26)
                    .background(palette.foreground.opacity(0.06), in: RoundedRectangle(cornerRadius: 6))
            }
            .padding(.horizontal, 10)
            .frame(width: 180, height: CommandComposerLayout.minimumHeight + 12)
            .contentShape(RoundedRectangle(cornerRadius: 14))
        }
        .buttonStyle(OfficeGameButtonStyle(accent: accent, emphasis: .hero, isSelected: isPresented))
        .accessibilityLabel(OfficeLocalization.string("직원 선택"))
        .accessibilityValue(director.displayName(for: current))
        .accessibilityIdentifier("composerCharacterPicker")
        .help(OfficeLocalization.string("직원 선택"))
        .popover(isPresented: $isPresented, arrowEdge: .top) {
            VStack(alignment: .leading, spacing: 14) {
                HStack(spacing: 8) {
                    Image(systemName: "person.crop.square.stack.fill")
                        .foregroundStyle(accent)
                    Text(OfficeLocalization.string("직원 선택"))
                        .font(.system(size: 13, weight: .bold))
                    Spacer()
                    Text(director.displayName(for: current))
                        .font(.system(size: 10, weight: .medium))
                        .foregroundStyle(palette.secondary)
                }

                LazyVGrid(columns: [GridItem(.flexible(), spacing: 10), GridItem(.flexible(), spacing: 10)], spacing: 10) {
                  ForEach(director.characters.filter { $0.id != current }) { character in
                    let color = DashboardPalette.providerAccent(for: character.backend)
                    Button {
                        guard selection.canSelect(character.id) else { return }
                        isPresented = false
                        director.select(character)
                    } label: {
                        VStack(spacing: 8) {
                            portrait(for: character.id, accent: color, size: 46)
                            HStack(spacing: 3) {
                                Text(director.displayName(for: character.id))
                                    .font(.system(size: 12, weight: .bold))
                                    .lineLimit(1)
                                status(for: character.id)
                            }
                            .frame(height: 20)
                            Text(character.backend.title.uppercased())
                                .font(.system(size: 8, weight: .bold, design: .rounded))
                                .tracking(1)
                                .foregroundStyle(palette.secondary)
                        }
                        .frame(maxWidth: .infinity)
                        .frame(height: 124)
                        .contentShape(RoundedRectangle(cornerRadius: 14))
                    }
                    .buttonStyle(OfficeGameButtonStyle(accent: color, emphasis: .hero))
                    .disabled(!selection.canSelect(character.id))
                    .onDrag { EmployeeMention.dragProvider(for: character.id) }
                    .accessibilityLabel(OfficeLocalization.format(
                        "%@ 선택", director.displayName(for: character.id)
                    ))
                    .accessibilityIdentifier("commandCharacter-\(character.id.rawValue)")
                  }
                }
            }
            .foregroundStyle(palette.foreground)
            .padding(16)
            .frame(width: 320)
            .officeGameSurface(cornerRadius: 0)
            .environment(\.colorScheme, colorScheme)
        }
        .onChange(of: selection.selectedCharacterID) { _, _ in
            isPresented = false
        }
    }

    private func portrait(for character: OfficeCharacter, accent: Color, size: CGFloat) -> some View {
        CharacterAvatar(
            name: director.displayName(for: character),
            characterID: character.rawValue,
            size: size
        )
        .padding(3)
        .background(accent.opacity(0.24), in: Circle())
        .overlay {
            Circle().strokeBorder(
                LinearGradient(colors: [.white.opacity(0.85), accent.opacity(0.5), .white.opacity(0.12)],
                    startPoint: .topLeading, endPoint: .bottomTrailing),
                lineWidth: 1
            )
        }
        .accessibilityHidden(true)
    }

    @ViewBuilder
    private func status(for character: OfficeCharacter) -> some View {
        let running = director.runningCharacters.contains(character)
        let compacting = director.compactingCharacters.contains(character)
        let completed = director.unreviewedCompletedCharacters.contains(character)
        let notice = director.contextCompactionNotice(for: character)
        if running || compacting || completed || notice != nil {
            CharacterTaskStatusIndicator(
                isRunning: running,
                isCompacting: compacting,
                isCompleted: completed,
                compactionNotice: notice,
                reduceMotion: reduceMotion
            )
        }
    }
}
