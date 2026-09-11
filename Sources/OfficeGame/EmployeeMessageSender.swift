import SwiftUI
import OfficeCore

/// Display attribution, not an authenticated identity or an authorization grant.
struct EmployeeMessageSender: Decodable, Equatable, Sendable {
    let characterId: String
    let name: String

    static func resolve(_ explicit: Self?, prompt: String) -> Self? {
        if let explicit {
            return OfficeCharacter(rawValue: explicit.characterId) == nil ? nil : explicit
        }
        // Legacy messages have no sender field. Only accept an explicit
        // self-introduction at the beginning, never a name mentioned in prose.
        var prefix = String(prompt.prefix(240)).trimmingCharacters(in: .whitespacesAndNewlines)
        if prefix.hasPrefix("["), let end = prefix.firstIndex(of: "]") {
            prefix = String(prefix[prefix.index(after: end)...])
                .trimmingCharacters(in: .whitespacesAndNewlines)
        }
        let identities = [("boss", "백부장"), ("left-man", "클대리"),
                          ("left-woman", "로과장"), ("right-woman", "코대리"),
                          ("right-man", "안과장")]
        for (id, name) in identities {
            if prefix.hasPrefix(name + "입니다.") || prefix.hasPrefix(name + "입니다\n") {
                return Self(characterId: id, name: name)
            }
        }
        return nil
    }
}

struct EmployeeMessageSenderLabel: View {
    let sender: EmployeeMessageSender?

    var body: some View {
        if let sender {
            HStack(spacing: 6) {
                // A static avatar: no per-message hover observation or new requests.
                CharacterAvatar(name: sender.name, characterID: sender.characterId, size: 26)
                    .accessibilityHidden(true)
                Text(sender.name)
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(.secondary)
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("employeeMessageSender-\(sender.characterId)")
        }
    }
}
