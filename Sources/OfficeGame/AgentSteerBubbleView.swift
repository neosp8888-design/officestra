// 이 파일은 실행 중 업무에 끼워 넣은 사용자 입력을 대화록의 말풍선으로 보여준다.

import SwiftUI

/// 활동 kind "steer"를 대화록 항목으로 옮긴 값이다.
struct AgentSteerNote: Identifiable, Equatable {
    let activityID: String
    let text: String
    let status: LiveFeedActivityStatus
    let occurredAt: Date

    var id: String { "steer:\(activityID)" }

    static let activityKind = "steer"

    init?(_ activity: LiveFeedActivity) {
        guard activity.kind == Self.activityKind else {
            return nil
        }
        activityID = activity.id
        text = activity.text
        status = activity.status
        occurredAt = activity.occurredAt
    }

    var statusLabel: String {
        switch status {
        case .running: OfficeLocalization.string("전달 대기")
        case .completed: OfficeLocalization.string("반영됨")
        case .failed: OfficeLocalization.string("전달 못 함")
        }
    }

    var statusSymbol: String {
        switch status {
        case .running: "clock"
        case .completed: "checkmark.circle.fill"
        case .failed: "exclamationmark.triangle.fill"
        }
    }
}

/// 질문 말풍선과 같은 사용자 입력 색으로, 오른쪽에 붙여 보여준다.
struct AgentSteerBubbleView: View {
    let note: AgentSteerNote

    private static let minimumLeadingGap = CGFloat(96)

    var body: some View {
        HStack(spacing: 0) {
            Spacer(minLength: Self.minimumLeadingGap)

            VStack(alignment: .trailing, spacing: 4) {
                VStack(alignment: .leading, spacing: 6) {
                    Label(OfficeLocalization.string("끼어들기"), systemImage: "text.bubble.fill")
                        .font(.system(size: 11, weight: .bold))
                        .foregroundStyle(LiveTurnPromptOrigin.user.accent)

                    Text(note.text)
                        .font(.system(size: 13, weight: .semibold))
                        .multilineTextAlignment(.leading)
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 9)
                .officeGameSurface(
                    accent: LiveTurnPromptOrigin.user.accent,
                    emphasis: .prompt,
                    cornerRadius: 13
                )

                Label(note.statusLabel, systemImage: note.statusSymbol)
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(note.status == .failed ? Color.red : Color.secondary)
                    .help(
                        note.status == .failed
                            ? OfficeLocalization.string("이 끼어들기는 업무에 전달되지 않았습니다. 다음 턴으로 다시 보내세요.")
                            : OfficeLocalization.date(note.occurredAt, dateStyle: .omitted, time: .shortened)
                    )
            }
        }
        .frame(maxWidth: .infinity, alignment: .trailing)
        .fixedSize(horizontal: false, vertical: true)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("agentSteerBubble")
    }
}
