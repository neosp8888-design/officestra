// 실시간 대화 제목 줄에서 선택한 직원 응답의 음성 따라 읽기를 켜고 끄는 버튼이다.
import OfficeCore
import SwiftUI

struct VoiceFollowToggle: View {
    @ObservedObject var director: AgentDirector
    let character: OfficeCharacter
    @State private var isSending = false
    @State private var requestError: String?

    private var status: VoiceFollowStatus? { director.voiceFollowStatus }

    private var isOn: Bool { status?.isActive(for: character.rawValue) == true }

    private var isStarting: Bool { isOn && status?.state == "starting" }

    private var failure: String? {
        if let requestError { return requestError }
        guard status?.characterId == character.rawValue, status?.state == "failed" else { return nil }
        return status?.error
    }

    private var helpText: String {
        if let failure { return failure }
        if isStarting { return OfficeLocalization.string("음성 모델을 준비하는 중입니다.") }
        return OfficeLocalization.string(isOn ? "음성 지원을 끕니다." : "이 직원의 응답을 맥에서 음성으로 읽어 줍니다.")
    }

    var body: some View {
        Button { toggle() } label: {
            HStack(spacing: 5) {
                Image(systemName: isOn ? "speaker.wave.2.fill" : "speaker.slash")
                Text(OfficeLocalization.string("음성 지원"))
                if isSending || isStarting {
                    ProgressView().controlSize(.mini)
                } else if failure != nil {
                    Image(systemName: "exclamationmark.circle").foregroundStyle(.red)
                }
            }
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(isOn ? Color.white : Color.primary)
            .padding(.horizontal, 9).frame(height: 30)
            .background(isOn ? DashboardPalette.accent : DashboardPalette.accent.opacity(0.05), in: Capsule())
        }
        .buttonStyle(.plain)
        .disabled(isSending)
        .help(helpText)
        .accessibilityLabel(OfficeLocalization.string("음성 지원"))
        .accessibilityValue(OfficeLocalization.string(isOn ? "켜짐" : "꺼짐"))
        .accessibilityIdentifier("voiceFollowToggle")
    }

    private func toggle() {
        let enable = !isOn
        isSending = true
        requestError = nil
        Task {
            do { try await director.setVoiceFollow(enable, for: character) }
            catch { requestError = error.localizedDescription }
            isSending = false
        }
    }
}
