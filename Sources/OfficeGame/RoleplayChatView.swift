// 롤플레이를 켠 직원의 대화 칸을 대신하는 롤플레이 대화 화면과 설정 패널.
import AppKit
import OfficeCore
import SwiftUI
import UniformTypeIdentifiers

/// 보이는 대화 칸 중 롤플레이를 켠 직원 칸만 덮는다.
struct RoleplayOverlays: View {
    @ObservedObject var director: AgentDirector
    @ObservedObject var roleplay: RoleplayStore
    @ObservedObject var layout: ConversationLayoutStore
    let size: CGSize

    var body: some View {
        let frames = layout.frames(in: CGRect(origin: .zero, size: size))
        let characters = layout.visible
            .filter { roleplay.isEnabled($0) }
            .sorted { $0.rawValue < $1.rawValue }
        ForEach(characters, id: \.self) { character in
            if let frame = frames[character] {
                RoleplayChatView(director: director, roleplay: roleplay, character: character)
                    .frame(width: frame.width, height: frame.height)
                    .offset(x: frame.minX, y: frame.minY)
            }
        }
    }
}

struct RoleplayChatView: View {
    @ObservedObject var director: AgentDirector
    @ObservedObject var roleplay: RoleplayStore
    let character: OfficeCharacter
    @State private var isShowingSettings = false
    /// 대화 끝 표시의 화면 안 위치. 지연 목록이 끝 표시를 내리면 기본값(무한대)이라 버튼이 보인다.
    @State private var bottomOffset = CGFloat.infinity
    @State private var viewportHeight = CGFloat.zero

    private var state: RoleplayState? { roleplay.states[character] }

    private var accent: Color {
        DashboardPalette.providerAccent(for: director.agentSettings(for: character).backend)
    }

    private var presetName: String? {
        guard let id = state?.profile.presetId else { return nil }
        return roleplay.assets.first { $0.id == id }?.name
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            HStack(spacing: 0) {
                transcript
                if isShowingSettings {
                    Divider()
                    RoleplaySettingsPanel(
                        roleplay: roleplay,
                        character: character,
                        accent: accent,
                        localOptions: director.localModelOptions.filter { $0.enabled && $0.backend == .claude }
                    )
                        .frame(width: 330)
                }
            }
        }
        .officeGameSurface(emphasis: .conversation, cornerRadius: 0)
        .environment(\.officeHoverEffectsEnabled, false)
        .task(id: character) {
            if roleplay.assets.isEmpty { await roleplay.refreshAssets() }
            if state?.profile.presetId == nil { isShowingSettings = true }
        }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Label(
                OfficeLocalization.format("롤플레이 · %@", director.displayName(for: character)),
                systemImage: "theatermasks.fill"
            )
            .font(.system(size: 12, weight: .bold))
            .foregroundStyle(accent)
            if let presetName {
                Text(presetName)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            Button {
                Task { await roleplay.newChat(character) }
            } label: {
                Label(OfficeLocalization.string("새 대화"), systemImage: "arrow.counterclockwise")
                    .font(.system(size: 11, weight: .semibold))
                    .padding(.horizontal, 7)
                    .padding(.vertical, 4)
            }
            .officeGameTool(accent: accent)
            .disabled(state?.generating == true)
            Button {
                isShowingSettings.toggle()
            } label: {
                Label(OfficeLocalization.string("설정"), systemImage: "slider.horizontal.3")
                    .font(.system(size: 11, weight: .semibold))
                    .padding(.horizontal, 7)
                    .padding(.vertical, 4)
            }
            .officeGameTool(accent: accent, selected: isShowingSettings)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private var transcript: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    if state?.profile.presetId == nil {
                        Text(OfficeLocalization.string("설정에서 리스 프리셋(.risup)을 가져와 고르면 대화를 시작할 수 있어요."))
                            .font(.system(size: 12))
                            .foregroundStyle(.secondary)
                    }
                    let first = state?.profile.firstMessage ?? ""
                    let messages = state?.messages ?? []
                    let fallbacks = fallbackSpeakers(first: first, messages: messages)
                    if !first.isEmpty {
                        RoleplayBubble(role: "char", text: first, fallbackSpeaker: fallbacks.first)
                    }
                    ForEach(messages) { message in
                        RoleplayBubble(role: message.role, text: message.content, fallbackSpeaker: fallbacks.byMessage[message.id])
                    }
                    if state?.generating == true {
                        HStack(spacing: 6) {
                            ProgressView().controlSize(.small)
                            Text(OfficeLocalization.string("답을 쓰는 중"))
                                .font(.system(size: 11))
                                .foregroundStyle(.secondary)
                        }
                    }
                    if let error = roleplay.errors[character] ?? state?.lastError {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .font(.system(size: 11, weight: .semibold))
                            .foregroundStyle(.red)
                            .textSelection(.enabled)
                    }
                    Color.clear.frame(height: 1).id("roleplay-bottom")
                        .background(GeometryReader { geometry in
                            Color.clear.preference(
                                key: RoleplayBottomOffsetKey.self,
                                value: geometry.frame(in: .named("roleplay-scroll")).minY
                            )
                        })
                }
                .padding(14)
            }
            .coordinateSpace(name: "roleplay-scroll")
            .background(GeometryReader { geometry in
                Color.clear.preference(key: RoleplayViewportHeightKey.self, value: geometry.size.height)
            })
            .onPreferenceChange(RoleplayBottomOffsetKey.self) { bottomOffset = $0 }
            .onPreferenceChange(RoleplayViewportHeightKey.self) { viewportHeight = $0 }
            .onAppear { proxy.scrollTo("roleplay-bottom", anchor: .bottom) }
            // 새 답변은 따라 내려가지 않는다. 내가 보낸 메시지만 맨 위로 올려 보이고 나머지는 버튼으로 내린다(2026-10-28 요청).
            .onChange(of: state?.messages.count) { old, new in
                guard let old, let new, new > old else {
                    proxy.scrollTo("roleplay-bottom", anchor: .bottom)
                    return
                }
                let added = (state?.messages ?? []).suffix(new - old)
                if let sent = added.last(where: { $0.role == "user" }) {
                    withAnimation { proxy.scrollTo(sent.id, anchor: .top) }
                }
            }
            .overlay(alignment: .bottomTrailing) {
                if bottomOffset > viewportHeight + 24 {
                    Button {
                        withAnimation { proxy.scrollTo("roleplay-bottom", anchor: .bottom) }
                    } label: {
                        Image(systemName: "chevron.down")
                            .font(.system(size: 11, weight: .bold))
                            .padding(.horizontal, 9)
                            .padding(.vertical, 6)
                    }
                    .officeGameTool(accent: accent)
                    .padding(12)
                    .accessibilityLabel(OfficeLocalization.string("맨 아래로 이동"))
                    .help(OfficeLocalization.string("맨 아래로 이동"))
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    /// 이름표 없는 답변에 붙일 화자. 앞 답변에서 가장 많이 나온 이름표, 없으면 직원 이름이다.
    private func fallbackSpeakers(
        first: String,
        messages: [RoleplayMessage]
    ) -> (first: RoleplayTranscript.Speaker?, byMessage: [String: RoleplayTranscript.Speaker]) {
        let replies = messages.filter { $0.role != "user" }
        let texts = (first.isEmpty ? [] : [first]) + replies.map(\.content)
        var speakers = RoleplayTranscript.fallbackSpeakers(for: texts, defaultName: director.displayName(for: character))
        let firstSpeaker = first.isEmpty ? nil : speakers.removeFirst()
        return (firstSpeaker, Dictionary(uniqueKeysWithValues: zip(replies.map(\.id), speakers)))
    }
}

private struct RoleplayBottomOffsetKey: PreferenceKey {
    static let defaultValue = CGFloat.infinity
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}

private struct RoleplayViewportHeightKey: PreferenceKey {
    static let defaultValue = CGFloat.zero
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}

private struct RoleplayBubble: View {
    let role: String
    let text: String
    var fallbackSpeaker: RoleplayTranscript.Speaker?

    var body: some View {
        if role == "user" {
            HStack {
                Spacer(minLength: 40)
                Text(text)
                    .font(.system(size: 13))
                    .textSelection(.enabled)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
                    .officeGameSurface(accent: LiveTurnPromptOrigin.user.accent, emphasis: .prompt, cornerRadius: 12)
            }
        } else {
            VStack(alignment: .leading, spacing: 8) {
                ForEach(Array(RoleplayTranscript.segments(text, fallbackSpeaker: fallbackSpeaker).enumerated()), id: \.offset) { _, segment in
                    RoleplaySegmentView(segment: segment)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// 대사는 화자색 상자로, 속마음은 흐린 기울임으로, 서술은 기존 마크다운으로 보여 준다.
private struct RoleplaySegmentView: View {
    let segment: RoleplayTranscript.Segment
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        switch segment {
        case .narration(let source):
            ConversationMarkdownView(source: source, fontSize: 13)
        case .dialogue(let speaker, let line):
            let color = RoleplayTranscript.speakerColor(for: speaker?.name, isDark: colorScheme == .dark)
            VStack(alignment: .leading, spacing: 3) {
                if let speaker { speakerLabel(speaker, color: color) }
                Text(inlineMarkdown(line))
                    .font(.system(size: 14, weight: .medium))
                    .textSelection(.enabled)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .officeGameSurface(accent: color, emphasis: .prompt, cornerRadius: 10)
        case .thought(let speaker, let line):
            let color = RoleplayTranscript.speakerColor(for: speaker?.name, isDark: colorScheme == .dark)
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                if let speaker { speakerLabel(speaker, color: color) }
                Text(inlineMarkdown(line))
                    .font(.system(size: 13).italic())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            .padding(.leading, 4)
        }
    }

    private func speakerLabel(_ speaker: RoleplayTranscript.Speaker, color: Color) -> some View {
        Text([speaker.emoji, speaker.name].compactMap { $0 }.joined(separator: " "))
            .font(.system(size: 11, weight: .bold))
            .foregroundStyle(color)
    }

    private func inlineMarkdown(_ line: String) -> AttributedString {
        let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        return (try? AttributedString(markdown: line, options: options)) ?? AttributedString(line)
    }
}

private struct RoleplaySettingsPanel: View {
    @ObservedObject var roleplay: RoleplayStore
    let character: OfficeCharacter
    let accent: Color
    /// 로컬 브리지가 Messages 형식으로 받는 프로필만 롤플레이에 연결할 수 있다.
    let localOptions: [LocalModelOption]
    @State private var characterDescription = ""
    @State private var firstMessage = ""
    @State private var userName = ""
    @State private var persona = ""
    @State private var model = ""
    @State private var loadedProfile: String?

    private var state: RoleplayState? { roleplay.states[character] }
    private var presets: [RoleplayAsset] { roleplay.assets.filter { $0.kind == "preset" } }
    private var modules: [RoleplayAsset] { roleplay.assets.filter { $0.kind == "module" } }

    private var isDirty: Bool {
        guard let profile = state?.profile else { return false }
        return characterDescription != profile.characterDescription
            || firstMessage != profile.firstMessage
            || userName != profile.userName
            || persona != profile.persona
            || model != profile.model
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                presetSection
                Divider()
                characterSection
                Divider()
                toggleSection
            }
            .padding(12)
        }
        .onAppear(perform: loadDraft)
        .task { await roleplay.refreshProxyModels() }
        .onChange(of: character) { _, _ in loadedProfile = nil; loadDraft() }
        .onChange(of: state?.profile.characterId) { _, _ in loadDraft() }
    }

    private func loadDraft() {
        guard let profile = state?.profile, loadedProfile != profile.characterId else { return }
        loadedProfile = profile.characterId
        characterDescription = profile.characterDescription
        firstMessage = profile.firstMessage
        userName = profile.userName
        persona = profile.persona
        model = profile.model
    }

    private var presetSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            sectionTitle("프리셋")
            Picker(OfficeLocalization.string("프리셋"), selection: Binding(
                get: { state?.profile.presetId ?? "" },
                set: { id in
                    guard !id.isEmpty else { return }
                    Task { await roleplay.update(character, RoleplayProfilePatch(presetId: id)) }
                }
            )) {
                Text(OfficeLocalization.string("고르지 않음")).tag("")
                ForEach(presets) { preset in
                    Text(preset.name).tag(preset.id)
                }
            }
            .labelsHidden()
            if let id = state?.profile.presetId, let preset = presets.first(where: { $0.id == id }) {
                Text(OfficeLocalization.format(
                    "블록 %d개 · 토글 %d개 · 정규식 %d개",
                    preset.promptCount, preset.toggleCount, preset.regexCount
                ))
                .font(.system(size: 10.5))
                .foregroundStyle(.secondary)
            }
            if !modules.isEmpty {
                ForEach(modules) { module in
                    moduleRow(module)
                }
            }
            Button {
                chooseFiles()
            } label: {
                Label(OfficeLocalization.string("리스 파일 가져오기"), systemImage: "square.and.arrow.down")
                    .font(.system(size: 11, weight: .semibold))
                    .padding(.horizontal, 7)
                    .padding(.vertical, 4)
            }
            .officeGameTool(accent: accent)
            if let error = roleplay.assetError {
                Text(error).font(.system(size: 10.5)).foregroundStyle(.red)
            }
        }
    }

    private func moduleRow(_ module: RoleplayAsset) -> some View {
        let explicit = state?.profile.moduleIds.contains(module.id) == true
        let integrated = !explicit && state?.activeModuleIds.contains(module.id) == true
        return Toggle(isOn: Binding(
            get: { explicit || integrated },
            set: { on in
                var ids = state?.profile.moduleIds ?? []
                ids.removeAll { $0 == module.id }
                if on { ids.append(module.id) }
                Task { await roleplay.update(character, RoleplayProfilePatch(moduleIds: ids)) }
            }
        )) {
            VStack(alignment: .leading, spacing: 1) {
                Text(module.name).font(.system(size: 11.5))
                if integrated {
                    Text(OfficeLocalization.string("프리셋이 함께 켬"))
                        .font(.system(size: 10))
                        .foregroundStyle(.secondary)
                }
            }
        }
        .disabled(integrated)
    }

    private var characterSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            sectionTitle("캐릭터")
            field("캐릭터 설명", text: $characterDescription, height: 120)
            field("첫 메시지", text: $firstMessage, height: 70)
            VStack(alignment: .leading, spacing: 3) {
                fieldTitle("내 이름")
                TextField("", text: $userName).textFieldStyle(.roundedBorder)
            }
            field("내 페르소나", text: $persona, height: 60)
            VStack(alignment: .leading, spacing: 3) {
                fieldTitle("모델 연결")
                Picker(OfficeLocalization.string("모델 연결"), selection: Binding(
                    get: { state?.profile.localProfileId ?? "" },
                    set: { id in
                        Task { await roleplay.update(character, RoleplayProfilePatch(localProfileId: id)) }
                    }
                )) {
                    Text(OfficeLocalization.string("안티그래비티 프록시")).tag("")
                    ForEach(localOptions) { option in
                        Text("\(OfficeLocalization.string("로컬 AI")) · \(option.displayTitle)").tag(option.id)
                    }
                    // 꺼졌거나 목록에 없는 프로필도 현재 값은 보여 준다.
                    if let id = state?.profile.localProfileId, !localOptions.contains(where: { $0.id == id }) {
                        Text(id).tag(id)
                    }
                }
                .labelsHidden()
            }
            VStack(alignment: .leading, spacing: 3) {
                fieldTitle("모델")
                if roleplay.proxyModels.isEmpty {
                    TextField("", text: $model).textFieldStyle(.roundedBorder)
                        .disabled(state?.profile.localProfileId != nil)
                    if state?.profile.localProfileId == nil, roleplay.proxyModelsError != nil {
                        Text(OfficeLocalization.string("모델 목록을 받지 못해 직접 입력합니다"))
                            .font(.system(size: 10))
                            .foregroundStyle(.secondary)
                    }
                } else {
                    Picker(OfficeLocalization.string("모델"), selection: $model) {
                        ForEach(roleplay.proxyModels) { option in
                            Text(option.title).tag(option.id)
                        }
                        // 목록에 없는 모델을 저장해 둔 경우에도 현재 값은 보여 준다.
                        if !roleplay.proxyModels.contains(where: { $0.id == model }) {
                            Text(model.isEmpty ? OfficeLocalization.string("고르지 않음") : model).tag(model)
                        }
                    }
                    .labelsHidden()
                    .disabled(state?.profile.localProfileId != nil)
                }
                if state?.profile.localProfileId != nil {
                    Text(OfficeLocalization.string("로컬 AI는 프로필 모델을 씁니다"))
                        .font(.system(size: 10))
                        .foregroundStyle(.secondary)
                }
            }
            Button {
                Task {
                    await roleplay.update(character, RoleplayProfilePatch(
                        characterDescription: characterDescription,
                        firstMessage: firstMessage,
                        userName: userName,
                        persona: persona,
                        model: model
                    ))
                }
            } label: {
                Label(OfficeLocalization.string("저장"), systemImage: "checkmark")
                    .font(.system(size: 11, weight: .semibold))
                    .padding(.horizontal, 7)
                    .padding(.vertical, 4)
            }
            .officeGameTool(accent: accent, selected: isDirty)
            .disabled(!isDirty)
        }
    }

    @ViewBuilder
    private var toggleSection: some View {
        let definitions = state?.toggleDefinitions ?? []
        VStack(alignment: .leading, spacing: 6) {
            sectionTitle("토글")
            Toggle(OfficeLocalization.string("대사에 화자 이름표"), isOn: Binding(
                get: { state?.profile.speakerCue == true },
                set: { on in Task { await roleplay.update(character, RoleplayProfilePatch(speakerCue: on)) } }
            ))
            .font(.system(size: 11.5))
            Text(OfficeLocalization.string(
                // 토글 값은 프리셋을 바꿔도 남으므로 지금 프리셋에 cueinput이 있을 때만 본다.
                state?.profile.toggles["cueinput"] == "1" && definitions.contains { $0.key == "cueinput" }
                    ? "프리셋의 화자 표시가 켜져 있어 따로 넣지 않아요."
                    : "새 답변부터 대사 앞에 이모지와 이름을 쓰게 해요."
            ))
            .font(.system(size: 10))
            .foregroundStyle(.secondary)
            if definitions.isEmpty {
                Text(OfficeLocalization.string("프리셋에 토글이 없어요."))
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
            }
            ForEach(Array(definitions.enumerated()), id: \.offset) { _, item in
                toggleRow(item)
            }
        }
    }

    @ViewBuilder
    private func toggleRow(_ item: RoleplayToggleDefinition) -> some View {
        let values = state?.profile.toggles ?? [:]
        switch item.type {
        case "group":
            Text(item.value)
                .font(.system(size: 11.5, weight: .bold))
                .padding(.top, 6)
        case "groupEnd":
            EmptyView()
        case "divider":
            VStack(alignment: .leading, spacing: 2) {
                Divider()
                if !item.value.isEmpty {
                    Text(item.value).font(.system(size: 10.5, weight: .semibold)).foregroundStyle(.secondary)
                }
            }
        case "caption":
            Text(item.value).font(.system(size: 10.5)).foregroundStyle(.secondary)
        case "select":
            Picker(item.value, selection: Binding(
                get: { values[item.key] ?? "0" },
                set: { value in Task { await roleplay.setToggle(item.key, value: value, for: character) } }
            )) {
                ForEach(Array(item.options.enumerated()), id: \.offset) { index, option in
                    Text(option).tag(String(index))
                }
            }
            .font(.system(size: 11.5))
        case "text", "textarea":
            RoleplayToggleTextField(title: item.value, value: values[item.key] ?? "") { value in
                Task { await roleplay.setToggle(item.key, value: value, for: character) }
            }
        default:
            Toggle(item.value, isOn: Binding(
                get: { values[item.key] == "1" },
                set: { on in Task { await roleplay.setToggle(item.key, value: on ? "1" : "0", for: character) } }
            ))
            .font(.system(size: 11.5))
        }
    }

    private func sectionTitle(_ key: String) -> some View {
        Text(OfficeLocalization.string(key))
            .font(.system(size: 12, weight: .bold))
            .foregroundStyle(accent)
    }

    private func fieldTitle(_ key: String) -> some View {
        Text(OfficeLocalization.string(key))
            .font(.system(size: 10.5, weight: .semibold))
            .foregroundStyle(.secondary)
    }

    private func field(_ key: String, text: Binding<String>, height: CGFloat) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            fieldTitle(key)
            TextEditor(text: text)
                .font(.system(size: 12))
                .frame(height: height)
                .scrollContentBackground(.hidden)
                .padding(4)
                .officeGameSurface(emphasis: .panel, cornerRadius: 6)
        }
    }

    private func chooseFiles() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = true
        panel.allowedContentTypes = ["risup", "risum"].compactMap { UTType(filenameExtension: $0) }
        panel.prompt = OfficeLocalization.string("가져오기")
        panel.message = OfficeLocalization.string("리스 프리셋(.risup)이나 모듈(.risum)을 고르세요.")
        guard panel.runModal() == .OK else { return }
        let urls = panel.urls
        Task {
            for url in urls {
                let asset = await roleplay.importAsset(from: url)
                if let asset, asset.kind == "preset", state?.profile.presetId == nil {
                    await roleplay.update(character, RoleplayProfilePatch(presetId: asset.id))
                }
            }
        }
    }
}

/// 글자 토글은 입력을 마칠 때(엔터) 한 번만 저장한다.
private struct RoleplayToggleTextField: View {
    let title: String
    let value: String
    let onCommit: (String) -> Void
    @State private var text = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title).font(.system(size: 10.5, weight: .semibold)).foregroundStyle(.secondary)
            TextField("", text: $text)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 11.5))
                .onSubmit { onCommit(text) }
        }
        .onAppear { text = value }
        .onChange(of: value) { _, newValue in text = newValue }
    }
}
