// 이 파일은 확인 질문을 팝업 대신 대화 카드 안에서 바로 답변하도록 표시한다.

import OfficeCore
import SwiftUI

/// 답변을 기다리는 턴 카드 아래에 붙는 선택지와 직접 입력 영역이다.
struct InlineQuestionAnswerView: View {
    @ObservedObject var director: AgentDirector
    let character: OfficeCharacter
    let turnID: String
    let needsInput: Bool
    let question: String

    @State private var answer = ""
    @FocusState private var answerIsFocused: Bool

    var body: some View {
        // 답변이 끝났거나 다른 턴의 질문이면 카드에서 사라진다.
        if InlineQuestionVisibility.showsAnswerComposer(
            turnNeedsInput: needsInput,
            turnID: turnID,
            pendingQuestionTurnID: director.pendingQuestionTurnID(
                for: character
            )
        ) {
            content
        }
    }

    private var content: some View {
        let presentation = AgentQuestionPresentation(text: question)
        let isSending = director.runningCharacters.contains(character)

        return VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 6) {
                Image(systemName: "questionmark.bubble.fill")
                    .font(.system(size: 11, weight: .bold))
                Text(
                    presentation.choices.isEmpty
                        ? OfficeLocalization.string("답변하기")
                        : OfficeLocalization.string("선택지에서 고르거나 직접 답변하기")
                )
                    .font(.system(size: 11, weight: .bold))

                Spacer(minLength: 6)

                if isSending {
                    ProgressView()
                        .controlSize(.mini)
                }
            }
            .foregroundStyle(Color.orange)

            if let error = director.questionSubmissionError(for: character) {
                Label(
                    OfficeLocalization.format("전송하지 못했습니다. %@", error),
                    systemImage: "exclamationmark.triangle.fill"
                )
                .font(.system(size: 10.5, weight: .semibold))
                .foregroundStyle(Color.red)
            }

            if !presentation.choices.isEmpty {
                VStack(spacing: 6) {
                    ForEach(
                        Array(presentation.choices.enumerated()),
                        id: \.offset
                    ) { index, choice in
                        choiceButton(
                            index: index,
                            choice: choice,
                            isSending: isSending
                        )
                    }
                }
            }

            answerField(isSending: isSending)
        }
        .padding(11)
        .officeGameSurface(accent: .orange, cornerRadius: 11)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(OfficeLocalization.string("확인 질문 답변"))
    }

    private func choiceButton(
        index: Int,
        choice: AgentQuestionChoice,
        isSending: Bool
    ) -> some View {
        Button {
            submit(choice.response)
        } label: {
            HStack(spacing: 9) {
                Text("\(index + 1)")
                    .font(
                        .system(size: 10, weight: .bold, design: .rounded)
                    )
                    .foregroundStyle(Color.orange)
                    .frame(width: 20, height: 20)
                    .background(
                        Circle().fill(Color.orange.opacity(0.14))
                    )

                Text(choice.title)
                    .font(.system(size: 12))
                    .multilineTextAlignment(.leading)
                    .frame(maxWidth: .infinity, alignment: .leading)

                Image(systemName: "arrow.right")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .contentShape(Rectangle())
        }
        .officeGameTool()
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(Color.primary.opacity(0.04))
        )
        .overlay {
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .stroke(Color.orange.opacity(0.22))
        }
        .disabled(isSending)
        .opacity(isSending ? 0.5 : 1)
        .accessibilityIdentifier("inlineNeedsInputChoice.\(index + 1)")
        .accessibilityLabel(
            OfficeLocalization.format("%d번 %@", index + 1, choice.title)
        )
    }

    private func answerField(isSending: Bool) -> some View {
        HStack(alignment: .bottom, spacing: 8) {
            TextEditor(text: $answer)
                .font(.system(size: 12))
                .scrollContentBackground(.hidden)
                .padding(7)
                .frame(minHeight: 54, maxHeight: 108)
                .background(
                    RoundedRectangle(cornerRadius: 9, style: .continuous)
                        .fill(Color(nsColor: .textBackgroundColor))
                )
                .overlay {
                    RoundedRectangle(cornerRadius: 9, style: .continuous)
                        .stroke(Color.primary.opacity(0.14))
                }
                .focused($answerIsFocused)
                .disabled(isSending)
                .accessibilityLabel(OfficeLocalization.string("답변 입력"))
                .accessibilityIdentifier("inlineNeedsInputAnswer")

            Button {
                submit(answer)
            } label: {
                Image(systemName: "paperplane.fill")
                    .font(.system(size: 12, weight: .bold))
                    .foregroundStyle(.white)
                    .frame(width: 32, height: 32)
                    .background(
                        Color.orange,
                        in: RoundedRectangle(
                            cornerRadius: 9,
                            style: .continuous
                        )
                    )
            }
            .officeGameTool()
            .disabled(isSending || trimmedAnswer.isEmpty)
            .opacity(isSending || trimmedAnswer.isEmpty ? 0.42 : 1)
            .help(OfficeLocalization.string("답변 보내기"))
            .accessibilityLabel(OfficeLocalization.string("답변 보내기"))
            .accessibilityIdentifier("inlineNeedsInputSend")
        }
    }

    private var trimmedAnswer: String {
        answer.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private func submit(_ response: String) {
        let value = response.trimmingCharacters(
            in: .whitespacesAndNewlines
        )
        guard !value.isEmpty else {
            return
        }
        answer = ""
        answerIsFocused = false
        director.submit(value, to: character)
    }
}
