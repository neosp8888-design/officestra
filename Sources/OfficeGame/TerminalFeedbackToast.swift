// 터미널 모드에서 방금 끝난 턴을 입력창 위에서 좋아요·싫어요로 평가하는 토스트다.
import OfficeCore
import SwiftUI

/// 어떤 턴을 토스트로 띄울지 정하는 순수 규칙이다.
enum TerminalFeedbackToastPresentation {
    /// 완료 뒤 이 시간 안에 알아챈 턴만 띄운다. 앱을 다시 열었을 때
    /// 예전 턴이 새로 끝난 것처럼 나타나지 않게 한다.
    static let freshnessWindow: TimeInterval = 90
    /// 평가하지 않으면 이 시간 뒤에 사라진다.
    static let visibleDuration: TimeInterval = 30
    /// 평가한 뒤 잠깐 결과를 보여 주고 사라진다.
    static let ratedDuration: TimeInterval = 1.4

    static func candidate(
        in turns: [LiveFeedTurn],
        character: OfficeCharacter,
        shownTurnIDs: Set<String>,
        now: Date = Date()
    ) -> LiveFeedTurn? {
        turns
            .filter { turn in
                turn.origin == "terminal"
                    && turn.characterId == character.rawValue
                    && turn.status == .completed
                    && !turn.needsInput
                    && !shownTurnIDs.contains(turn.id)
                    && now.timeIntervalSince(turn.endedAt ?? turn.updatedAt)
                        <= freshnessWindow
            }
            .max { left, right in
                (left.endedAt ?? left.updatedAt)
                    < (right.endedAt ?? right.updatedAt)
            }
    }
}

/// 직원 선택 줄 없이도 대상을 알 수 있도록 이름을 붙여 입력창 위에 띄운다.
struct TerminalFeedbackToastRow: View {
    @ObservedObject private var director: AgentDirector
    @ObservedObject private var feedStore: LiveFeedStore
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    @State private var activeTurnIDs: [OfficeCharacter: String] = [:]
    @State private var shownTurnIDs: Set<String> = []
    @State private var dismissTasks: [OfficeCharacter: Task<Void, Never>] = [:]

    init(director: AgentDirector) {
        self.director = director
        _feedStore = ObservedObject(wrappedValue: director.liveFeedStore)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(director.characters.filter { activeTurnIDs[$0.id] != nil }) { character in
                ZStack {
                    if
                        let turnID = activeTurnIDs[character.id],
                        let turn = feedStore.turns.first(where: { $0.id == turnID })
                    {
                        TerminalFeedbackToast(
                            turn: turn,
                            feedbackChanged: { feedback in
                                await director.updateResponseFeedback(
                                    turnID: turn.id,
                                    feedback: feedback
                                )
                                scheduleDismiss(
                                    character.id,
                                    after: TerminalFeedbackToastPresentation.ratedDuration
                                )
                            },
                            dismiss: { hide(character.id) }
                        )
                        .fixedSize()
                        .transition(
                            reduceMotion
                                ? .opacity
                                : .move(edge: .bottom).combined(with: .opacity)
                        )
                    }
                }
            }
        }
        .animation(.easeOut(duration: 0.22), value: activeTurnIDs)
        .onChange(of: feedStore.turns) { _, turns in
            present(from: turns)
        }
        .onAppear {
            // 처음 나타날 때 이미 끝나 있던 턴은 띄우지 않는다.
            for turn in feedStore.turns where turn.origin == "terminal" {
                shownTurnIDs.insert(turn.id)
            }
        }
        .onDisappear {
            for task in dismissTasks.values {
                task.cancel()
            }
        }
    }

    private func present(from turns: [LiveFeedTurn]) {
        for character in director.characters {
            guard
                activeTurnIDs[character.id] == nil,
                let candidate = TerminalFeedbackToastPresentation.candidate(
                    in: turns,
                    character: character.id,
                    shownTurnIDs: shownTurnIDs
                )
            else {
                continue
            }
            shownTurnIDs.insert(candidate.id)
            activeTurnIDs[character.id] = candidate.id
            scheduleDismiss(
                character.id,
                after: TerminalFeedbackToastPresentation.visibleDuration
            )
        }
    }

    private func scheduleDismiss(
        _ character: OfficeCharacter,
        after seconds: TimeInterval
    ) {
        dismissTasks[character]?.cancel()
        dismissTasks[character] = Task { @MainActor in
            try? await Task.sleep(for: .seconds(seconds))
            guard !Task.isCancelled else {
                return
            }
            hide(character)
        }
    }

    private func hide(_ character: OfficeCharacter) {
        dismissTasks[character]?.cancel()
        dismissTasks[character] = nil
        activeTurnIDs[character] = nil
    }
}

struct TerminalFeedbackToast: View {
    let turn: LiveFeedTurn
    let feedbackChanged: (TurnResponseFeedback?) async -> Void
    let dismiss: () -> Void

    @State private var isUpdatingFeedback = false
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        HStack(spacing: 8) {
            Text(turn.characterName)
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(.secondary)
            feedbackButton(
                .disliked,
                filled: "hand.thumbsdown.fill",
                outline: "hand.thumbsdown",
                color: .gray,
                label: turn.feedback == .disliked ? "싫어요 취소" : "싫어요",
                identifier: "terminalToastDislike"
            )
            feedbackButton(
                .liked,
                filled: "heart.fill",
                outline: "heart",
                color: .red,
                label: turn.feedback == .liked ? "좋아요 취소" : "좋아요",
                identifier: "terminalToastLike"
            )

            Button(action: dismiss) {
                Image(systemName: "xmark")
                    .font(.system(size: 9.5, weight: .bold))
                    .foregroundStyle(.secondary)
                    .frame(width: 16, height: 16)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(OfficeLocalization.string("닫기"))
            .accessibilityIdentifier("terminalToastClose")
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 4)
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(.regularMaterial)
                .shadow(
                    color: .black.opacity(colorScheme == .dark ? 0.45 : 0.18),
                    radius: 8,
                    y: 2
                )
        )
        .overlay(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .strokeBorder(DashboardPalette.accent.opacity(0.35), lineWidth: 1)
        )
        // 직원 선택 줄 위쪽 바깥에 붙으므로 버튼과의 간격만 둔다.
        .padding(.bottom, 6)
        // 터미널 위에 떠 있으므로 터미널의 I 빔 커서 영역 안이다. 토스트 위에서는
        // 화살표를 유지하고, 벗어나면 창의 커서 영역을 다시 계산하게 한다.
        .onContinuousHover { phase in
            switch phase {
            case .active:
                NSCursor.arrow.set()
            case .ended:
                NSApp.keyWindow?.resetCursorRects()
            }
        }
        .accessibilityIdentifier("terminalFeedbackToast-\(turn.characterId)")
    }

    private func feedbackButton(
        _ selection: TurnResponseFeedback,
        filled: String,
        outline: String,
        color: Color,
        label: String,
        identifier: String
    ) -> some View {
        Button {
            toggleFeedback(selection)
        } label: {
            Image(systemName: turn.feedback == selection ? filled : outline)
                .font(.system(size: 11.5, weight: .semibold))
                .foregroundStyle(
                    turn.feedback == selection ? color : Color.secondary
                )
                .contentTransition(.symbolEffect(.replace))
                .frame(width: 18, height: 18)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(isUpdatingFeedback)
        .accessibilityLabel(OfficeLocalization.string(label))
        .accessibilityIdentifier(identifier)
    }

    private func toggleFeedback(_ selection: TurnResponseFeedback) {
        guard !isUpdatingFeedback else {
            return
        }
        let nextFeedback = TurnResponseFeedback.toggled(
            current: turn.feedback,
            selection: selection
        )
        isUpdatingFeedback = true
        Task {
            await feedbackChanged(nextFeedback)
            isUpdatingFeedback = false
        }
    }
}
