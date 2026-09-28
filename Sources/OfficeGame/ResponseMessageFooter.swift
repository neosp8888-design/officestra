import OfficeCore
import SwiftUI

struct ResponseMessageFooter: View {
    static let timestampFontWeight: Font.Weight = .medium

    let occurredAt: Date
    let copied: Bool
    let accentColor: Color
    let accessibilityID: String
    let showsFeedback: Bool
    let feedback: TurnResponseFeedback?
    let feedbackAccessibilityIDPrefix: String
    let copy: () -> Void
    let feedbackChanged: (TurnResponseFeedback?) async -> Void

    @State private var isUpdatingFeedback = false

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(OfficeLocalization.date(occurredAt, dateStyle: .omitted, time: .standard))
                .font(.system(
                    size: 8.5,
                    weight: Self.timestampFontWeight,
                    design: .monospaced
                ))
                .foregroundStyle(.tertiary)

            HStack(spacing: 7) {
                if showsFeedback {
                    Button {
                        toggleFeedback(.disliked)
                    } label: {
                        Image(
                            systemName: feedback == .disliked
                                ? "hand.thumbsdown.fill"
                                : "hand.thumbsdown"
                        )
                        .officeToolFont(size: 11.5)
                        .foregroundStyle(
                            feedback == .disliked
                                ? Color.gray
                                : Color.secondary
                        )
                        .contentTransition(.symbolEffect(.replace))
                        .frame(width: 18, height: 18)
                        .contentShape(Rectangle())
                    }
                    .officeTextTool()
                    .disabled(isUpdatingFeedback)
                    .accessibilityLabel(
                        feedback == .disliked
                            ? OfficeLocalization.string("싫어요 취소")
                            : OfficeLocalization.string("싫어요")
                    )
                    .accessibilityIdentifier(
                        "dislikeMessage-\(feedbackAccessibilityIDPrefix)"
                    )

                    Button {
                        toggleFeedback(.liked)
                    } label: {
                        Image(
                            systemName: feedback == .liked
                                ? "heart.fill"
                                : "heart"
                        )
                        .officeToolFont(size: 11.5)
                        .foregroundStyle(
                            feedback == .liked ? Color.red : Color.secondary
                        )
                        .contentTransition(.symbolEffect(.replace))
                        .frame(width: 18, height: 18)
                        .contentShape(Rectangle())
                    }
                    .officeTextTool()
                    .disabled(isUpdatingFeedback)
                    .accessibilityLabel(
                        feedback == .liked
                            ? OfficeLocalization.string("좋아요 취소")
                            : OfficeLocalization.string("좋아요")
                    )
                    .accessibilityIdentifier(
                        "likeMessage-\(feedbackAccessibilityIDPrefix)"
                    )
                }

                Button(action: copy) {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        .officeToolFont(size: 11)
                        .foregroundStyle(
                            copied ? accentColor : Color.secondary
                        )
                        .contentTransition(.symbolEffect(.replace))
                        .frame(width: 18, height: 18)
                        .contentShape(Rectangle())
                }
                .officeTextTool()
                .accessibilityLabel(
                    copied
                        ? OfficeLocalization.string("복사됨")
                        : OfficeLocalization.string("복사")
                )
                .accessibilityIdentifier(accessibilityID)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .fixedSize(horizontal: false, vertical: true)
    }

    private func toggleFeedback(_ selection: TurnResponseFeedback) {
        guard !isUpdatingFeedback else {
            return
        }
        let nextFeedback = TurnResponseFeedback.toggled(
            current: feedback,
            selection: selection
        )
        isUpdatingFeedback = true
        Task {
            await feedbackChanged(nextFeedback)
            isUpdatingFeedback = false
        }
    }
}
