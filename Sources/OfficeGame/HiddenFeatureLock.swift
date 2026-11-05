// 사무실 화분을 두 번 누르면 뜨는 암호창과, 숨긴 기능(음성 지원·롤플레이)의 암호 확인을 맡는다.
import AppKit
import CryptoKit
import SwiftUI

enum HiddenFeatureLock {
    /// 암호 원문은 저장소에 두지 않고 SHA-256 값만 비교한다.
    private static let passwordDigest = "50f389694bcfbe019bf832a1e3aa890e4e501226dd244285f244c7034538f870"

    static func matches(_ password: String) -> Bool {
        digest(password) == passwordDigest
    }

    static func digest(_ text: String) -> String {
        SHA256.hash(data: Data(text.utf8)).map { String(format: "%02x", $0) }.joined()
    }
}

/// 암호가 맞으면 숨긴 기능을 열고, 틀리면 소리만 내고 닫는다.
struct HiddenFeatureUnlockPrompt: ViewModifier {
    @Binding var isPresented: Bool
    let director: AgentDirector
    @State private var password = ""

    func body(content: Content) -> some View {
        content.alert(OfficeLocalization.string("암호 입력"), isPresented: $isPresented) {
            SecureField(OfficeLocalization.string("암호"), text: $password)
            Button(OfficeLocalization.string("확인")) {
                if !director.unlockHiddenFeatures(password: password) { NSSound.beep() }
                password = ""
            }
            .keyboardShortcut(.defaultAction)
            Button(OfficeLocalization.string("취소"), role: .cancel) { password = "" }
        }
    }
}
