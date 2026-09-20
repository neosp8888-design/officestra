import AppKit
import OfficeCore
import SwiftUI

struct EmployeeMention: Identifiable, Equatable {
    let id: OfficeCharacter
    let name: String
    static let pasteboardType = NSPasteboard.PasteboardType("com.officestra.employee-mention")

    /// A fully typed @name also works when the user clicks Send instead of
    /// accepting autocomplete. Only explicit, unambiguous user-input tokens
    /// count; employee output is never passed through this parser.
    static func recipients(in prompt: String, candidates: [Self]) -> Set<OfficeCharacter> {
        let uniqueNames = Dictionary(grouping: candidates, by: \.name)
        let fullRange = NSRange(location: 0, length: (prompt as NSString).length)
        return Set(candidates.compactMap { candidate in
            guard uniqueNames[candidate.name]?.count == 1,
                  let pattern = try? NSRegularExpression(pattern: "(?<!\\S)@" + NSRegularExpression.escapedPattern(for: candidate.name) + "(?=$|\\s|[,.!?])"),
                  pattern.firstMatch(in: prompt, range: fullRange) != nil else { return nil }
            return candidate.id
        })
    }

    static func dragProvider(for character: OfficeCharacter) -> NSItemProvider {
        let provider = NSItemProvider()
        provider.registerDataRepresentation(forTypeIdentifier: pasteboardType.rawValue, visibility: .ownProcess) { completion in
            completion(Data(character.rawValue.utf8), nil)
            return nil
        }
        return provider
    }
}

struct EmployeeMentionQuery: Equatable {
    let range: NSRange
    let text: String

    static func atCaret(in text: String, selection: NSRange) -> Self? {
        let value = text as NSString
        guard selection.length == 0, selection.location <= value.length else { return nil }
        let prefix = value.substring(to: selection.location) as NSString
        let at = prefix.range(of: "@", options: .backwards)
        guard at.location != NSNotFound else { return nil }
        if at.location > 0 {
            let before = prefix.substring(with: NSRange(location: at.location - 1, length: 1))
            guard before.rangeOfCharacter(from: .whitespacesAndNewlines) != nil else { return nil }
        }
        let query = prefix.substring(from: at.location + 1)
        guard query.rangeOfCharacter(from: .whitespacesAndNewlines) == nil else { return nil }
        return Self(range: NSRange(location: at.location, length: selection.location - at.location), text: query)
    }
}

struct ReplyRoutingControl: View {
    @ObservedObject var director: AgentDirector
    @ObservedObject private var selection: CharacterSelectionStore
    @State private var isPresented = false

    init(director: AgentDirector) {
        self.director = director
        _selection = ObservedObject(wrappedValue: director.characterSelectionStore)
    }

    var body: some View {
        if let source = selection.selectedCharacterID {
            let recipients = director.replyRecipients[source] ?? []
            let paused = director.pausedReplyRoutes.contains(source)
            HStack(spacing: 4) {
                Button { isPresented.toggle() } label: {
                    HStack(spacing: 5) {
                        Image(systemName: "at")
                        Text(title(for: recipients))
                            .lineLimit(1)
                            .frame(maxWidth: 110)
                        if director.replyRouteError != nil {
                            Image(systemName: "exclamationmark.circle").foregroundStyle(.red)
                        }
                        if director.savingReplyRoutes.contains(source) {
                            ProgressView().controlSize(.mini)
                        } else {
                            Image(systemName: "chevron.down").font(.system(size: 8, weight: .bold))
                        }
                    }
                    .font(.system(size: 11, weight: .semibold))
                    .padding(.horizontal, 9).frame(height: 30)
                    .background(DashboardPalette.accent.opacity(recipients.isEmpty ? 0.05 : 0.12), in: Capsule())
                }
                .buttonStyle(.plain)
                .accessibilityLabel(OfficeLocalization.string("자동 전달 대상 설정"))
                .help(OfficeLocalization.string("입력창에서 @로 선택하거나 직원을 끌어 넣으세요. 선택은 계속 유지됩니다."))
                .popover(isPresented: $isPresented, arrowEdge: .bottom) {
                    VStack(alignment: .leading, spacing: 12) {
                        Text(OfficeLocalization.format("%@의 자동 전달", director.displayName(for: source)))
                            .font(.headline)
                        Text(OfficeLocalization.string("답변을 받을 직원을 선택하세요. 선택은 계속 유지됩니다."))
                            .font(.caption).foregroundStyle(.secondary)
                        ForEach(director.characters.filter { $0.id != source }) { character in
                            Toggle(isOn: Binding(
                                get: { director.replyRecipients[source]?.contains(character.id) == true },
                                set: { director.setReplyRecipient(character.id, selected: $0, for: source) }
                            )) {
                                HStack(spacing: 8) {
                                    CharacterAvatar(name: director.displayName(for: character.id), characterID: character.id.rawValue, size: 24)
                                    Text(director.displayName(for: character.id))
                                }
                            }
                            .toggleStyle(.checkbox)
                            .onDrag { EmployeeMention.dragProvider(for: character.id) }
                        }
                        if !recipients.isEmpty {
                            Divider()
                            Button(paused ? OfficeLocalization.string("자동 전달 재개") : OfficeLocalization.string("자동 전달 일시정지")) {
                                director.toggleReplyRoutePause(for: source)
                            }
                            Text(OfficeLocalization.string("일시정지는 대기 중인 전달을 멈춥니다. 이미 시작한 답변은 계속됩니다."))
                                .font(.caption).foregroundStyle(.secondary)
                        }
                        ReplyDeliveryStatusView(store: director.liveFeedStore, character: source, onCancel: director.cancelReplyDelivery)
                        if let error = director.replyRouteError {
                            Text(error).font(.caption).foregroundStyle(.red)
                            Button(OfficeLocalization.string("다시 불러오기")) { Task { await director.refreshReplyRoutes() } }
                        }
                    }
                    .padding(16).frame(width: 300)
                }
                if !recipients.isEmpty {
                    Button { director.toggleReplyRoutePause(for: source) } label: {
                        Image(systemName: paused ? "play.fill" : "pause.fill")
                            .font(.system(size: 11)).frame(width: 28, height: 30)
                    }
                    .buttonStyle(.plain)
                    .help(paused ? OfficeLocalization.string("자동 전달 재개") : OfficeLocalization.string("자동 전달 일시정지"))
                    .accessibilityLabel(paused ? OfficeLocalization.string("자동 전달 재개") : OfficeLocalization.string("자동 전달 일시정지"))
                }
            }
            .foregroundStyle(paused ? Color.secondary : DashboardPalette.accent)
        }
    }

    private func title(for recipients: Set<OfficeCharacter>) -> String {
        if let source = director.selectedCharacterID, director.pausedReplyRoutes.contains(source), !recipients.isEmpty {
            return OfficeLocalization.format("전달 정지 · %d명", recipients.count)
        }
        if recipients.count == 1, let recipient = recipients.first { return director.displayName(for: recipient) }
        return recipients.isEmpty ? OfficeLocalization.string("자동 전달") : OfficeLocalization.format("자동 전달 · %d명", recipients.count)
    }
}

private struct EmployeeMentionSuggestions: View {
    let mentions: [EmployeeMention]
    let selectedIndex: Int
    let select: (EmployeeMention) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(OfficeLocalization.string("답변을 전달할 직원"))
                .font(.caption).foregroundStyle(.secondary).padding(6)
            ForEach(Array(mentions.enumerated()), id: \.element.id) { index, mention in
                Button { select(mention) } label: {
                    HStack(spacing: 8) {
                        CharacterAvatar(name: mention.name, characterID: mention.id.rawValue, size: 26)
                        Text("@" + mention.name).font(.system(size: 13, weight: .medium))
                        Spacer()
                        if index == selectedIndex { Image(systemName: "return").foregroundStyle(.secondary) }
                    }
                    .padding(8)
                    .background(index == selectedIndex ? DashboardPalette.accent.opacity(0.12) : Color.clear,
                                in: RoundedRectangle(cornerRadius: 8))
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
        .padding(6).frame(width: 250)
    }
}

extension CommandComposerTextView {
    func refreshMentionSuggestions() {
        guard isEditable, !hasMarkedText(),
              let query = EmployeeMentionQuery.atCaret(in: string, selection: selectedRange()) else {
            dismissMentionSuggestions()
            return
        }
        let matches = mentionCandidates.filter { query.text.isEmpty || $0.name.localizedCaseInsensitiveContains(query.text) }
        if mentionRange != query.range || mentionMatches != matches { mentionIndex = 0 }
        mentionMatches = matches
        mentionRange = query.range
        guard !matches.isEmpty else { dismissMentionSuggestions(); return }
        showMentionSuggestions()
    }

    private func showMentionSuggestions() {
        guard window != nil, !mentionMatches.isEmpty else { return }
        let popover = mentionPopover ?? NSPopover()
        popover.behavior = .semitransient
        popover.animates = false
        popover.contentViewController = NSHostingController(rootView: EmployeeMentionSuggestions(
            mentions: mentionMatches, selectedIndex: mentionIndex,
            select: { [weak self] mention in
                guard let self, let range = mentionRange else { return }
                insertMention(mention, replacing: range)
            }
        ))
        mentionPopover = popover
        if !popover.isShown {
            let screenRect = firstRect(forCharacterRange: selectedRange(), actualRange: nil)
            if let window {
                let localRect = convert(window.convertFromScreen(screenRect), from: nil)
                popover.show(relativeTo: localRect, of: self, preferredEdge: .maxY)
                window.makeFirstResponder(self)
            }
        }
    }

    func dismissMentionSuggestions() {
        mentionPopover?.close()
        mentionPopover = nil
        mentionMatches = []
        mentionRange = nil
        mentionIndex = 0
    }

    func handleMentionKey(_ event: NSEvent) -> Bool {
        guard !event.modifierFlags.contains(.shift), !event.modifierFlags.contains(.command) else { return false }
        let isReturn = event.keyCode == 36 || event.keyCode == 76
        if isReturn, hasMarkedText(), EmployeeMentionQuery.atCaret(in: string, selection: selectedRange()) != nil {
            unmarkText()
            DispatchQueue.main.async { [weak self] in
                self?.refreshMentionSuggestions()
                self?.acceptSelectedMention()
            }
            return true
        }
        guard !hasMarkedText(), !mentionMatches.isEmpty else { return false }
        switch event.keyCode {
        case 125, 126:
            mentionIndex = (mentionIndex + (event.keyCode == 125 ? 1 : mentionMatches.count - 1)) % mentionMatches.count
            showMentionSuggestions()
            return true
        case 36, 76, 48:
            acceptSelectedMention()
            return true
        case 53:
            dismissMentionSuggestions()
            return true
        default: return false
        }
    }

    func acceptSelectedMention() {
        guard mentionMatches.indices.contains(mentionIndex), let range = mentionRange else { return }
        insertMention(mentionMatches[mentionIndex], replacing: range)
    }

    func insertMention(_ mention: EmployeeMention, replacing range: NSRange) {
        guard isEditable, mentionCandidates.contains(where: { $0.id == mention.id }) else { return }
        dismissMentionSuggestions()
        let prefix = range.location > 0 && !(string as NSString).substring(to: range.location).hasSuffix(" ")
            && range.length == 0 ? " " : ""
        insertText(prefix + "@" + mention.name + " ", replacementRange: range)
        onMention?(mention.id)
        reportMeasuredHeight()
    }

    func droppedMention(from pasteboard: NSPasteboard) -> EmployeeMention? {
        guard let data = pasteboard.data(forType: EmployeeMention.pasteboardType),
              let raw = String(data: data, encoding: .utf8) else { return nil }
        return mentionCandidates.first { $0.id.rawValue == raw }
    }
}
