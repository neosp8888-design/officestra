// 이 파일은 2D·3D 캐릭터 클릭 영역과 이름이 포함된 말풍선을 장면 위에 표시한다.

import OfficeCore
import SwiftUI

struct CharacterInteractionPresentationState: Equatable {
    let characters: [CharacterConfiguration]
    let displayNames: [OfficeCharacter: String]
    let archiveCabinetHitbox: CharacterHitbox
    let runningCharacters: Set<OfficeCharacter>
    let questionCharacters: Set<OfficeCharacter>
    let failedCharacters: Set<OfficeCharacter>
    let offDutyCharacters: Set<OfficeCharacter>

    init(
        characters: [CharacterConfiguration],
        displayNames: [OfficeCharacter: String],
        archiveCabinetHitbox: CharacterHitbox,
        runningCharacters: Set<OfficeCharacter>,
        questionCharacters: Set<OfficeCharacter>,
        failedCharacters: Set<OfficeCharacter>,
        offDutyCharacters: Set<OfficeCharacter>
    ) {
        self.characters = characters
        self.displayNames = displayNames
        self.archiveCabinetHitbox = archiveCabinetHitbox
        self.runningCharacters = runningCharacters
        self.questionCharacters = questionCharacters
        self.failedCharacters = failedCharacters
        self.offDutyCharacters = offDutyCharacters
    }

    @MainActor
    init(director: AgentDirector) {
        self.init(
            characters: director.characters,
            displayNames: Dictionary(uniqueKeysWithValues:
                director.characters.map { character in
                    (
                        character.id,
                        director.displayName(for: character.id)
                    )
                }
            ),
            archiveCabinetHitbox: director.archiveCabinetHitbox,
            runningCharacters: director.runningCharacters,
            questionCharacters: Set(director.pendingQuestions.keys),
            failedCharacters: Set(director.failedCharacters.keys),
            offDutyCharacters: Set(director.offDutyCharacters.keys)
        )
    }
}

struct CharacterInteractionLayer: View, Equatable {
    private let speechBubbleStore: SpeechBubbleStore
    private let presentation: CharacterInteractionPresentationState
    private let selectCharacter: (CharacterConfiguration) -> Void
    let artStyle: OfficeArtStyle
    let onMonitorTapped: (OfficeCharacter) -> Void
    let onArchiveCabinetTapped: () -> Void
    let onWhiteboardTapped: () -> Void
    let onBubbleTapped: (OfficeCharacter, String) -> Void

    init(
        director: AgentDirector,
        artStyle: OfficeArtStyle,
        onMonitorTapped: @escaping (OfficeCharacter) -> Void,
        onArchiveCabinetTapped: @escaping () -> Void,
        onWhiteboardTapped: @escaping () -> Void,
        onBubbleTapped: @escaping (OfficeCharacter, String) -> Void
    ) {
        speechBubbleStore = director.speechBubbleStore
        presentation = CharacterInteractionPresentationState(
            director: director
        )
        selectCharacter = { character in
            director.select(character)
        }
        self.artStyle = artStyle
        self.onMonitorTapped = onMonitorTapped
        self.onArchiveCabinetTapped = onArchiveCabinetTapped
        self.onWhiteboardTapped = onWhiteboardTapped
        self.onBubbleTapped = onBubbleTapped
    }

    static func == (
        lhs: CharacterInteractionLayer,
        rhs: CharacterInteractionLayer
    ) -> Bool {
        lhs.speechBubbleStore === rhs.speechBubbleStore
            && lhs.presentation == rhs.presentation
            && lhs.artStyle == rhs.artStyle
    }

    var body: some View {
        GeometryReader { geometry in
            let fittedFrame = OfficeCanvasGeometry.fittedFrame(
                in: geometry.size
            )
            let scale =
                fittedFrame.width / OfficeCanvasGeometry.designSize.width

            ZStack {
                ForEach(presentation.characters) { character in
                    let hitbox =
                        OfficeInteractionGeometry.characterHitbox(
                            for: character.id,
                            artStyle: artStyle,
                            fallback: character.hitbox.rect
                        )

                    Button {
                        selectCharacter(character)
                    } label: {
                        Color.clear
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .onDrag {
                        EmployeeMention.dragProvider(for: character.id)
                    } preview: {
                        Label(presentation.displayNames[character.id] ?? character.name, systemImage: "at")
                            .padding(10)
                            .background(.regularMaterial, in: Capsule())
                    }
                    .frame(
                        width: hitbox.width * scale,
                        height: hitbox.height * scale
                    )
                    .position(
                        x: fittedFrame.minX
                            + hitbox.midX * scale,
                        y: fittedFrame.minY
                            + hitbox.midY * scale
                    )
                    .accessibilityLabel(
                        OfficeLocalization.format(
                            "%@ 선택",
                            presentation.displayNames[character.id] ?? character.name
                        )
                    )
                }

                let archiveCabinetHitbox =
                    OfficeInteractionGeometry.archiveCabinetHitbox(
                        artStyle: artStyle,
                        fallback: presentation.archiveCabinetHitbox.rect
                    )

                Button(action: onArchiveCabinetTapped) {
                    Color.clear
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .frame(
                    width: archiveCabinetHitbox.width * scale,
                    height: archiveCabinetHitbox.height * scale
                )
                .position(
                    x: fittedFrame.minX
                        + archiveCabinetHitbox.midX * scale,
                    y: fittedFrame.minY
                        + archiveCabinetHitbox.midY * scale
                )
                .accessibilityLabel(OfficeLocalization.string("전체 대화 보관함 열기"))
                .help(OfficeLocalization.string("전체 대화 보관함"))

                let whiteboardHitbox =
                    OfficeWhiteboardGeometry.interactionRect(
                        for: artStyle
                    )

                Button(action: onWhiteboardTapped) {
                    Color.clear
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .frame(
                    width: whiteboardHitbox.width * scale,
                    height: whiteboardHitbox.height * scale
                )
                .position(
                    x: fittedFrame.minX
                        + whiteboardHitbox.midX * scale,
                    y: fittedFrame.minY
                        + whiteboardHitbox.midY * scale
                )
                .accessibilityLabel(OfficeLocalization.string("화이트보드 상세 열기"))
                .help(OfficeLocalization.string("CLI 한도 상세"))

                ForEach(presentation.characters) { character in
                    let monitorHitbox =
                        OfficeInteractionGeometry.monitorHitbox(
                            for: character.id,
                            artStyle: artStyle,
                            fallback: character.monitorHitbox.rect
                        )

                    Button {
                        onMonitorTapped(character.id)
                    } label: {
                        Color.clear
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .frame(
                        width: monitorHitbox.width * scale,
                        height: monitorHitbox.height * scale
                    )
                    .position(
                        x: fittedFrame.minX
                            + monitorHitbox.midX * scale,
                        y: fittedFrame.minY
                            + monitorHitbox.midY * scale
                    )
                    .accessibilityLabel(
                        OfficeLocalization.format(
                            "%@ 대화 열기",
                            presentation.displayNames[character.id] ?? character.name
                        )
                    )
                    .help(
                        OfficeLocalization.format(
                            "%@ 대화 내역",
                            presentation.displayNames[character.id] ?? character.name
                        )
                    )
                }

                CharacterSpeechBubbleLayer(
                    speechBubbleStore: speechBubbleStore,
                    presentation: presentation,
                    artStyle: artStyle,
                    fittedFrame: fittedFrame,
                    scale: scale,
                    onBubbleTapped: onBubbleTapped
                )
            }
        }
    }
}

struct CharacterSpeechBubbleLayer: View {
    @ObservedObject var speechBubbleStore: SpeechBubbleStore
    let presentation: CharacterInteractionPresentationState
    let artStyle: OfficeArtStyle
    let fittedFrame: CGRect
    let scale: CGFloat
    let onBubbleTapped: (OfficeCharacter, String) -> Void

    var body: some View {
        ForEach(presentation.characters) { character in
            if let message = speechBubbleStore.bubbles[character.id] {
                let isThinking = presentation.runningCharacters.contains(
                    character.id
                )
                let isQuestion = presentation.questionCharacters.contains(
                    character.id
                )
                let isFailure = presentation.failedCharacters.contains(
                    character.id
                )
                let isOffDuty = presentation.offDutyCharacters.contains(
                    character.id
                )
                let bubbleAnchor = OfficeInteractionGeometry.bubbleAnchor(
                    for: character.id,
                    artStyle: artStyle,
                    fallback: character.bubble.point
                )

                Button {
                    onBubbleTapped(character.id, message)
                } label: {
                    CharacterSpeechBubble(
                        name: presentation.displayNames[character.id]
                            ?? character.name,
                        message: message,
                        isThinking: isThinking,
                        isQuestion: isQuestion,
                        isFailure: isFailure,
                        isOffDuty: isOffDuty,
                        tailEdge: character.id == .boss
                            ? .leading
                            : .bottom
                    )
                    .id(message)
                    .transition(
                        .asymmetric(
                            insertion: .scale(scale: 0.88)
                                .combined(with: .opacity),
                            removal: .opacity
                        )
                    )
                }
                .buttonStyle(.plain)
                .position(
                    OfficeBubbleLayout.position(
                        for: character.id,
                        bubbleAnchor: bubbleAnchor,
                        fittedFrame: fittedFrame,
                        scale: scale,
                        artStyle: artStyle,
                        fallbackHitbox: character.hitbox.rect
                    )
                )
                .allowsHitTesting(!isThinking)
                .transition(.scale(scale: 0.88).combined(with: .opacity))
                .accessibilityLabel(
                    bubbleAccessibilityLabel(
                        for: character,
                        isQuestion: isQuestion,
                        isOffDuty: isOffDuty,
                        isFailure: isFailure
                    )
                )
                .help(
                    bubbleHelpText(
                        isQuestion: isQuestion,
                        isOffDuty: isOffDuty,
                        isFailure: isFailure
                    )
                )
            }
        }
        .animation(
            .spring(response: 0.30, dampingFraction: 0.72),
            value: speechBubbleStore.bubbles
        )
    }

    private func bubbleAccessibilityLabel(
        for character: CharacterConfiguration,
        isQuestion: Bool,
        isOffDuty: Bool,
        isFailure: Bool
    ) -> String {
        let name = displayName(for: character)
        if isQuestion {
            return OfficeLocalization.format("%@ 질문에 답변하기", name)
        } else if isOffDuty {
            return OfficeLocalization.format("%@ 퇴근 사유 보기", name)
        } else if isFailure {
            return OfficeLocalization.format("%@ 중단 원인 보기", name)
        } else {
            return OfficeLocalization.format("%@ 응답 전문 보기", name)
        }
    }

    private func bubbleHelpText(
        isQuestion: Bool,
        isOffDuty: Bool,
        isFailure: Bool
    ) -> String {
        if isQuestion {
            return OfficeLocalization.string("질문에 답변하기")
        } else if isOffDuty {
            return OfficeLocalization.string("퇴근 사유 보기")
        } else if isFailure {
            return OfficeLocalization.string("중단 원인 보기")
        } else {
            return OfficeLocalization.string("응답 전문 보기")
        }
    }

    private func displayName(for character: CharacterConfiguration) -> String {
        presentation.displayNames[character.id] ?? character.name
    }
}

enum OfficeBubbleLayout {
    static let bossMaximumWidth: CGFloat = 80
    static let bossMinimumFaceGap: CGFloat = 4
    static let bossFaceGapAtDesignScale: CGFloat = 18

    static func position(
        for character: OfficeCharacter,
        bubbleAnchor: CGPoint,
        fittedFrame: CGRect,
        scale: CGFloat,
        artStyle: OfficeArtStyle,
        fallbackHitbox: CGRect
    ) -> CGPoint {
        let idealPosition = CGPoint(
            x: fittedFrame.minX + bubbleAnchor.x * scale,
            y: fittedFrame.minY + bubbleAnchor.y * scale
        )
        guard character == .boss else {
            return idealPosition
        }

        let protectedRightEdge: CGFloat
        if let faceBounds = OfficeInteractionGeometry.faceBounds(
            for: .boss,
            artStyle: artStyle
        ) {
            protectedRightEdge = fittedFrame.minX
                + faceBounds.maxX * scale
        } else {
            let bossHitbox = OfficeInteractionGeometry.characterHitbox(
                for: .boss,
                artStyle: artStyle,
                fallback: fallbackHitbox
            )
            protectedRightEdge = fittedFrame.minX
                + bossHitbox.maxX * scale
        }
        let faceGap = max(
            bossMinimumFaceGap,
            bossFaceGapAtDesignScale * scale
        )
        return CGPoint(
            x: max(
                idealPosition.x,
                protectedRightEdge + faceGap + bossMaximumWidth / 2
            ),
            y: idealPosition.y
        )
    }
}

private struct CharacterSpeechBubble: View {
    let name: String
    let message: String
    let isThinking: Bool
    let isQuestion: Bool
    let isFailure: Bool
    let isOffDuty: Bool
    let tailEdge: SpeechBubbleTailEdge

    var body: some View {
        Group {
            if tailEdge == .leading {
                HStack(spacing: -1) {
                    SpeechBubbleTail(edge: .leading)
                        .fill(bubbleColor)
                        .frame(width: 6, height: 10)
                    bubbleCard
                }
            } else {
                VStack(spacing: -1) {
                    bubbleCard
                    SpeechBubbleTail(edge: .bottom)
                        .fill(bubbleColor)
                        .frame(width: 10, height: 6)
                }
            }
        }
        .opacity(isThinking ? 0.9 : 1)
    }

    private var bubbleCard: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 3) {
                Text(name)
                    .font(.system(size: 8.5, weight: .bold))
                    .foregroundStyle(Color.black.opacity(0.62))
                    .lineLimit(1)

                statusLabel
            }

            Text(message)
                .font(.system(size: 9.5, weight: .semibold))
                .foregroundStyle(Color.black.opacity(0.86))
                .lineLimit(isThinking ? 1 : 5)
                .minimumScaleFactor(isThinking ? 0.78 : 1)
                .multilineTextAlignment(.leading)
        }
        .padding(.horizontal, 7)
        .padding(.vertical, 5)
        .frame(maxWidth: 75, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(bubbleColor)
                .shadow(color: .black.opacity(0.16), radius: 4, y: 2)
        )
        .overlay {
            if isQuestion || isFailure || isOffDuty {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .stroke(statusColor, lineWidth: 1)
            }
        }
    }

    @ViewBuilder
    private var statusLabel: some View {
        if isQuestion {
            Label(
                OfficeLocalization.string("답장 픽"),
                systemImage: "questionmark.bubble.fill"
            )
            .font(.system(size: 7, weight: .bold))
            .foregroundStyle(
                Color(red: 0.56, green: 0.35, blue: 0.08)
            )
        } else if isOffDuty {
            Label(
                OfficeLocalization.string("오늘 마감"),
                systemImage: "moon.zzz.fill"
            )
            .font(.system(size: 7, weight: .bold))
            .foregroundStyle(
                Color(red: 0.23, green: 0.32, blue: 0.57)
            )
        } else if isFailure {
            Label(
                OfficeLocalization.string("앗차"),
                systemImage: "exclamationmark.triangle.fill"
            )
            .font(.system(size: 7, weight: .bold))
            .foregroundStyle(
                Color(red: 0.67, green: 0.14, blue: 0.12)
            )
        }
    }

    private var statusColor: Color {
        isOffDuty
            ? Color(red: 0.33, green: 0.43, blue: 0.72)
            : isFailure
            ? Color(red: 0.78, green: 0.20, blue: 0.17)
            : Color(red: 0.78, green: 0.52, blue: 0.16)
    }

    private var bubbleColor: Color {
        isQuestion
            ? Color(red: 1.0, green: 0.97, blue: 0.86).opacity(0.98)
            : isOffDuty
            ? Color(red: 0.91, green: 0.94, blue: 1.0).opacity(0.98)
            : isFailure
            ? Color(red: 1.0, green: 0.91, blue: 0.90).opacity(0.98)
            : .white.opacity(0.96)
    }
}

private enum SpeechBubbleTailEdge {
    case bottom
    case leading
}

private struct SpeechBubbleTail: Shape {
    let edge: SpeechBubbleTailEdge

    func path(in rect: CGRect) -> Path {
        var path = Path()
        switch edge {
        case .bottom:
            path.move(to: CGPoint(x: rect.minX, y: rect.minY))
            path.addLine(to: CGPoint(x: rect.maxX, y: rect.minY))
            path.addLine(to: CGPoint(x: rect.midX, y: rect.maxY))
        case .leading:
            path.move(to: CGPoint(x: rect.maxX, y: rect.minY))
            path.addLine(to: CGPoint(x: rect.maxX, y: rect.maxY))
            path.addLine(to: CGPoint(x: rect.minX, y: rect.midY))
        }
        path.closeSubpath()
        return path
    }
}
