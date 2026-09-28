// 이 파일은 대화 원문의 Markdown 블록을 선택 가능한 SwiftUI 콘텐츠로 표시한다.

import AppKit
import AVFoundation
import Combine
import ImageIO
import MarkdownUI
import OfficeCore
import SwiftUI

private struct LiveWorkspaceFeedPresentationStoreKey: EnvironmentKey {
    static let defaultValue: LiveWorkspaceFeedPresentationStore? = nil
}

extension EnvironmentValues {
    var liveWorkspaceFeedPresentationStore:
        LiveWorkspaceFeedPresentationStore?
    {
        get { self[LiveWorkspaceFeedPresentationStoreKey.self] }
        set { self[LiveWorkspaceFeedPresentationStoreKey.self] = newValue }
    }
}

struct ConversationMarkdownView: View {
    let source: String
    let fontSize: CGFloat
    let fileBaseDirectory: String?
    let compactsChangedFileLists: Bool

    init(
        source: String,
        fontSize: CGFloat = 12,
        fileBaseDirectory: String? = nil,
        compactsChangedFileLists: Bool = false
    ) {
        self.source = source
        self.fontSize = fontSize
        self.fileBaseDirectory = fileBaseDirectory
        self.compactsChangedFileLists = compactsChangedFileLists
    }

    var body: some View {
        ConversationMarkdownContent(
            source: source,
            fontSize: fontSize,
            fileBaseDirectory: fileBaseDirectory,
            compactsChangedFileLists: compactsChangedFileLists
        )
        .equatable()
    }
}

private struct ConversationMarkdownContent: View, Equatable {
    let source: String
    let fontSize: CGFloat
    let fileBaseDirectory: String?
    let compactsChangedFileLists: Bool

    var body: some View {
        let fallbackDirectory = fileBaseDirectory.flatMap {
            $0.isEmpty ? nil : URL(fileURLWithPath: $0)
        }
        let presentation = compactsChangedFileLists
            ? ConversationChangedFileListPresentation(source: source)
            : ConversationChangedFileListPresentation.unmodified(source)

        let content = VStack(alignment: .leading, spacing: 9) {
            if !presentation.leadingMarkdown.isEmpty {
                ConversationMarkdownSection(
                    source: presentation.leadingMarkdown,
                    fontSize: fontSize,
                    fallbackDirectory: fallbackDirectory
                )
            }

            if !presentation.files.isEmpty {
                ConversationChangedFilesView(
                    files: presentation.files,
                    workspaceDirectory: fallbackDirectory?.path ?? ""
                )
            }

            if !presentation.trailingMarkdown.isEmpty {
                ConversationMarkdownSection(
                    source: presentation.trailingMarkdown,
                    fontSize: fontSize,
                    fallbackDirectory: fallbackDirectory
                )
            }
        }
            .environment(
                \.openURL,
                OpenURLAction { url in
                    guard
                        let fileURL = LocalMarkdownResource.existingFileURL(
                            from: url,
                            fallbackDirectory: fallbackDirectory
                        )
                    else {
                        return LocalMarkdownResource.fileURL(from: url) == nil
                            ? .systemAction
                            : .discarded
                    }

                    if !NSWorkspace.shared.open(fileURL) {
                        NSWorkspace.shared.activateFileViewerSelecting(
                            [fileURL]
                        )
                    }
                    return .handled
                }
            )

        content
            .frame(maxWidth: .infinity, alignment: .leading)
            .buttonStyle(.plain)
    }
}

private struct ConversationMarkdownSection: View {
    let source: String
    let fontSize: CGFloat
    let fallbackDirectory: URL?

    var body: some View {
        let videoURLs = LocalMarkdownResource.videoFileURLs(
            in: source,
            fallbackDirectory: fallbackDirectory
        )
        let imageURLs = LocalMarkdownResource.imageFileURLs(
            in: source,
            fallbackDirectory: fallbackDirectory
        )
        let visibleSource = LocalMarkdownResource
            .removingGeneratedImagePreviews(from: source)
        let markdownSegments = visibleSource.isEmpty
            ? []
            : SelectableMarkdownSegmenter.split(visibleSource)

        VStack(alignment: .leading, spacing: 9) {
            ForEach(Array(markdownSegments.enumerated()), id: \.offset) {
                _, segment in
                switch segment.kind {
                case .codeBlock:
                    ConversationCodeBlockSegmentView(
                        source: segment.source,
                        fontSize: fontSize,
                        fallbackDirectory: fallbackDirectory
                    )
                case .prose, .table:
                    SelectableMarkdownTextView(
                        source: segment.source,
                        fontSize: fontSize,
                        fileBaseDirectory: fallbackDirectory?.path,
                        minimumLayoutWidth: segment.minimumLayoutWidth
                    )
                }
            }
            ForEach(Array(imageURLs.prefix(8)), id: \.self) { imageURL in
                LocalMarkdownFileImage(url: imageURL)
            }
            ForEach(Array(videoURLs.prefix(4)), id: \.self) { videoURL in
                LocalMarkdownFileVideo(url: videoURL)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct ConversationCodeBlockSegmentView: View {
    let source: String
    let fontSize: CGFloat
    let fallbackDirectory: URL?

    var body: some View {
        Markdown(
            source,
            baseURL: fallbackDirectory,
            imageBaseURL: fallbackDirectory
        )
        .markdownCodeSyntaxHighlighter(
            ConversationCodeSyntaxHighlighter()
        )
        .markdownBlockStyle(\.codeBlock) { configuration in
            ConversationCodeBlockView(
                configuration: configuration,
                fontSize: fontSize
            )
        }
        .markdownTheme(.gitHub)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

struct ConversationChangedFileReference: Identifiable, Equatable {
    let id: Int
    let title: String
    let path: String?
}

struct ConversationChangedFileListPresentation: Equatable {
    let leadingMarkdown: String
    let files: [ConversationChangedFileReference]
    let trailingMarkdown: String

    init(source: String) {
        let lines = source.split(
            separator: "\n",
            omittingEmptySubsequences: false
        ).map(String.init)
        if
            let header = lines.enumerated().compactMap({ index, line in
                Self.headerValue(from: line).map { value in
                    (index: index, value: value)
                }
            }).first
        {
            let headerIndex = header.index

            if !header.value.isEmpty {
                guard
                    let inlineValues = Self.inlineFileValues(from: header.value),
                    !inlineValues.isEmpty
                else {
                    self = Self.unmodified(source)
                    return
                }
                let parsedFiles = inlineValues.enumerated().compactMap {
                    index, value in
                    Self.fileReference(fromContent: value, id: index)
                }
                guard parsedFiles.count == inlineValues.count else {
                    self = Self.unmodified(source)
                    return
                }

                leadingMarkdown = Self.joined(lines[..<headerIndex])
                files = parsedFiles
                trailingMarkdown = Self.joined(lines[(headerIndex + 1)...])
                return
            }

            var cursor = headerIndex + 1
            var parsedFiles: [ConversationChangedFileReference] = []
            var suffixIndex = cursor
            while cursor < lines.count {
                if lines[cursor].trimmingCharacters(in: .whitespaces).isEmpty {
                    cursor += 1
                    continue
                }
                guard
                    let file = Self.fileReference(
                        from: lines[cursor],
                        id: parsedFiles.count
                    )
                else {
                    break
                }
                parsedFiles.append(file)
                cursor += 1
                suffixIndex = cursor
            }

            guard !parsedFiles.isEmpty else {
                self = Self.unmodified(source)
                return
            }
            while suffixIndex < lines.count,
                lines[suffixIndex]
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                    .isEmpty
            {
                suffixIndex += 1
            }

            leadingMarkdown = Self.joined(lines[..<headerIndex])
            files = parsedFiles
            trailingMarkdown = Self.joined(lines[suffixIndex...])
            return
        }

        guard
            let report = lines.enumerated().compactMap({ index, line in
                Self.proseFileReferences(from: line).map { files in
                    (index: index, files: files)
                }
            }).first
        else {
            self = Self.unmodified(source)
            return
        }

        leadingMarkdown = Self.joined(lines[..<report.index])
        files = report.files
        trailingMarkdown = Self.joined(lines[(report.index + 1)...])
    }

    static func unmodified(_ source: String)
        -> ConversationChangedFileListPresentation
    {
        ConversationChangedFileListPresentation(
            leadingMarkdown: source,
            files: [],
            trailingMarkdown: ""
        )
    }

    private init(
        leadingMarkdown: String,
        files: [ConversationChangedFileReference],
        trailingMarkdown: String
    ) {
        self.leadingMarkdown = leadingMarkdown
        self.files = files
        self.trailingMarkdown = trailingMarkdown
    }

    private static func headerValue(from line: String) -> String? {
        var normalized = line.trimmingCharacters(in: .whitespaces)
        while normalized.hasPrefix("#") {
            normalized.removeFirst()
            normalized = normalized.trimmingCharacters(in: .whitespaces)
        }
        let lowered = normalized.lowercased()
        let labels = [
            "변경 파일",
            "수정 파일",
            "changed files",
            "files changed",
        ]
        for label in labels where lowered.hasPrefix(label) {
            var remainder = String(normalized.dropFirst(label.count))
                .trimmingCharacters(in: .whitespaces)
            if remainder.isEmpty {
                return ""
            }
            guard remainder.hasPrefix(":") || remainder.hasPrefix("：") else {
                continue
            }
            remainder.removeFirst()
            return remainder.trimmingCharacters(in: .whitespaces)
        }
        return nil
    }

    private static func inlineFileValues(from value: String) -> [String]? {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        let noChangedFiles = [
            "없음",
            "없습니다",
            "없습니다.",
            "해당 없음",
            "변경 없음",
            "none",
            "no changes",
        ]
        guard !noChangedFiles.contains(trimmed.lowercased()) else {
            return nil
        }

        let values = trimmed.components(
            separatedBy: CharacterSet(charactersIn: "·•")
        ).map {
            $0.trimmingCharacters(in: .whitespacesAndNewlines)
        }.filter {
            !$0.isEmpty
        }
        return values.isEmpty ? nil : values
    }

    private static func proseFileReferences(
        from line: String
    ) -> [ConversationChangedFileReference]? {
        let lowered = line.lowercased()
        let reportCues = [
            "변경 파일",
            "수정 파일",
            "보완 파일",
            "손댄 곳",
            "changed files",
            "files changed",
            "modified files",
        ]
        guard reportCues.contains(where: lowered.contains) else {
            return nil
        }

        let pattern = #"\[([^\]]+)\]\((<?[^)\n]+>?)\)"#
        guard let expression = try? NSRegularExpression(pattern: pattern) else {
            return nil
        }
        let sourceRange = NSRange(line.startIndex..<line.endIndex, in: line)
        let matches = expression.matches(in: line, range: sourceRange)
        guard !matches.isEmpty else {
            return nil
        }

        let references: [ConversationChangedFileReference] = matches
            .enumerated().compactMap { index, match in
            guard
                let titleRange = Range(match.range(at: 1), in: line),
                let targetRange = Range(match.range(at: 2), in: line)
            else {
                return nil
            }
            let title = cleanLabel(String(line[titleRange]))
            let target = String(line[targetRange])
            guard let path = localPath(from: target) else {
                return nil
            }
            return ConversationChangedFileReference(
                id: index,
                title: title.isEmpty ? cleanLabel(target) : title,
                path: path
            )
        }
        guard references.count == matches.count else {
            return nil
        }
        return references
    }

    private static func fileReference(
        from line: String,
        id: Int
    ) -> ConversationChangedFileReference? {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        let bulletPrefixes = ["- ", "* ", "+ ", "• "]
        guard let prefix = bulletPrefixes.first(where: trimmed.hasPrefix) else {
            return nil
        }
        let content = String(trimmed.dropFirst(prefix.count))
            .trimmingCharacters(in: .whitespaces)
        return fileReference(fromContent: content, id: id)
    }

    private static func fileReference(
        fromContent content: String,
        id: Int
    ) -> ConversationChangedFileReference? {
        let content = content.trimmingCharacters(in: .whitespaces)
        guard !content.isEmpty else {
            return nil
        }

        if
            content.hasPrefix("["),
            content.hasSuffix(")"),
            let separator = content.range(of: "](")
        {
            let titleStart = content.index(after: content.startIndex)
            let title = cleanLabel(String(content[titleStart..<separator.lowerBound]))
            let target = String(content[separator.upperBound..<content.index(before: content.endIndex)])
            return ConversationChangedFileReference(
                id: id,
                title: title.isEmpty ? cleanLabel(target) : title,
                path: localPath(from: target)
            )
        }

        let title = cleanLabel(content)
        return ConversationChangedFileReference(
            id: id,
            title: title,
            path: localPath(from: title)
        )
    }

    private static func cleanLabel(_ value: String) -> String {
        value.trimmingCharacters(
            in: CharacterSet.whitespacesAndNewlines.union(
                CharacterSet(charactersIn: "`<>")
            )
        )
    }

    private static func localPath(from value: String) -> String? {
        var target = cleanLabel(value)
        guard !target.isEmpty else {
            return nil
        }
        if let decoded = target.removingPercentEncoding {
            target = decoded
        }
        if let url = URL(string: target), url.isFileURL {
            target = url.path
        } else if target.contains("://") {
            return nil
        }
        if let range = target.range(
            of: #":\d+(?:-\d+)?$"#,
            options: .regularExpression
        ) {
            target.removeSubrange(range)
        }
        return target.isEmpty ? nil : target
    }

    private static func joined<S>(_ lines: S) -> String
    where S: Collection, S.Element == String {
        lines.joined(separator: "\n")
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

private struct ConversationChangedFilesView: View {
    let files: [ConversationChangedFileReference]
    let workspaceDirectory: String

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var isExpanded = false
    @State private var copied = false
    @State private var copyResetTask: Task<Void, Never>?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Button {
                    withAnimation(
                        reduceMotion ? nil : .easeInOut(duration: 0.16)
                    ) {
                        isExpanded.toggle()
                    }
                } label: {
                    HStack(spacing: 8) {
                        Image(systemName: "doc.badge.gearshape")
                            .font(.system(size: 11, weight: .semibold))
                            .foregroundStyle(Color.accentColor)
                            .frame(width: 24, height: 24)
                            .background(
                                Color.accentColor.opacity(0.10),
                                in: RoundedRectangle(
                                    cornerRadius: 7,
                                    style: .continuous
                                )
                            )

                        VStack(alignment: .leading, spacing: 1) {
                            Text(
                                OfficeLocalization.format(
                                    "변경 파일 %d개",
                                    files.count
                                )
                            )
                                .font(.system(size: 11.5, weight: .semibold))
                                .foregroundStyle(.primary)
                            Text(previewText)
                                .font(.system(size: 9.5, design: .monospaced))
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }

                        Spacer(minLength: 6)

                        Image(systemName: "chevron.right")
                            .font(.system(size: 9, weight: .bold))
                            .foregroundStyle(.tertiary)
                            .rotationEffect(.degrees(isExpanded ? 90 : 0))
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel(
                    isExpanded
                        ? OfficeLocalization.string("변경 파일 목록 접기")
                        : OfficeLocalization.string("변경 파일 목록 펼치기")
                )

                Button(action: copyFiles) {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        .officeToolFont(size: 10.5)
                        .foregroundStyle(
                            copied ? Color.accentColor : Color.secondary
                        )
                        .frame(width: 24, height: 24)
                }
                .officeTextTool()
                .help(
                    copied
                        ? OfficeLocalization.string("파일 목록 복사됨")
                        : OfficeLocalization.string("파일 목록 복사")
                )
                .accessibilityLabel(
                    copied
                        ? OfficeLocalization.string("파일 목록 복사됨")
                        : OfficeLocalization.string("파일 목록 복사")
                )
            }

            if isExpanded {
                VStack(alignment: .leading, spacing: 5) {
                    ForEach(files) { file in
                        WorkspaceFileRevealButton(
                            title: file.title,
                            path: file.path,
                            workspaceDirectory: workspaceDirectory,
                            foregroundColor: .secondary,
                            accessibilityIdentifier:
                                "responseChangedFile-\(file.id)"
                        )
                    }
                }
                .padding(.leading, 32)
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .padding(9)
        .background(
            Color.primary.opacity(0.035),
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
        .onDisappear {
            copyResetTask?.cancel()
        }
    }

    private var previewText: String {
        let visible = files.prefix(2).map(\.title).joined(separator: " · ")
        let remaining = files.count - min(files.count, 2)
        return remaining > 0
            ? OfficeLocalization.format("%@ 외 %d개", visible, remaining)
            : visible
    }

    private func copyFiles() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(
            files.map { $0.path ?? $0.title }.joined(separator: "\n"),
            forType: .string
        )
        copied = true
        copyResetTask?.cancel()
        copyResetTask = Task {
            try? await Task.sleep(for: .seconds(1.4))
            if !Task.isCancelled {
                copied = false
            }
        }
    }
}

@MainActor
private final class ConversationMarkdownCache {
    private final class Entry {
        let content: MarkdownContent

        init(content: MarkdownContent) {
            self.content = content
        }
    }

    static let shared = ConversationMarkdownCache()

    private let storage = NSCache<NSString, Entry>()

    private init() {
        storage.countLimit = 64
        storage.totalCostLimit = 4 * 1_024 * 1_024
    }

    func content(
        for source: String,
        fallbackDirectory: URL?
    ) -> MarkdownContent {
        let trimmed = source.trimmingCharacters(
            in: .whitespacesAndNewlines
        )
        let rendered = trimmed.isEmpty
            ? OfficeLocalization.string("내용 없음")
            : LocalMarkdownResource.addingLinkedImagePreviews(
                to: trimmed,
                fallbackDirectory: fallbackDirectory
            )
        let key = rendered as NSString
        if let entry = storage.object(forKey: key) {
            return entry.content
        }

        let content = MarkdownContent(rendered)
        storage.setObject(
            Entry(content: content),
            forKey: key,
            cost: max(1, rendered.utf8.count)
        )
        return content
    }
}

private struct LocalMarkdownImageProvider: ImageProvider {
    let fallbackDirectory: URL?

    @ViewBuilder
    func makeImage(url: URL?) -> some View {
        if
            let url,
            let fileURL = LocalMarkdownResource.imageFileURL(
                from: url,
                fallbackDirectory: fallbackDirectory
            )
        {
            LocalMarkdownFileImage(url: fileURL)
        } else if
            let url,
            url.scheme == nil,
            !url.path.hasPrefix("/"),
            let image = NSImage(named: url.lastPathComponent)
        {
            LocalMarkdownThumbnail(image: image)
        }
    }
}

private struct LocalMarkdownFileVideo: View {
    let url: URL
    @Environment(\.liveWorkspaceFeedPresentationStore)
    private var feedPresentationStore
    @State private var isPlaying = false
    @State private var hasPreparedPlayer = false
    @State private var aspectRatio =
        ConversationMarkdownVideoLayout.fallbackAspectRatio

    var body: some View {
        ZStack {
            Group {
                if hasPreparedPlayer {
                    LocalMarkdownVideoSurface(
                        url: url,
                        isPlaying: isPlaying
                    )
                } else {
                    RoundedRectangle(
                        cornerRadius: 10,
                        style: .continuous
                    )
                    .fill(.black.opacity(0.82))
                    .overlay {
                        Image(systemName: "film")
                            .font(.system(size: 24, weight: .medium))
                            .foregroundStyle(.white.opacity(0.48))
                    }
                }
            }
            .aspectRatio(aspectRatio, contentMode: .fit)
            .frame(maxWidth: ConversationMarkdownVideoLayout.maximumWidth)
            .clipShape(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
            )

            Button {
                if isPlaying {
                    isPlaying = false
                } else {
                    hasPreparedPlayer = true
                    isPlaying = true
                }
            } label: {
                Image(systemName: isPlaying ? "pause.fill" : "play.fill")
                    .font(.system(size: 15, weight: .bold))
                    .foregroundStyle(.white)
                    .frame(width: 38, height: 38)
                    .background(.black.opacity(0.58), in: Circle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(
                isPlaying
                    ? OfficeLocalization.string("영상 일시 정지")
                    : OfficeLocalization.string("영상 재생")
            )
        }
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(Color.primary.opacity(0.10))
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .task(id: url) {
            await loadAspectRatio()
        }
        .onDisappear {
            isPlaying = false
        }
        .onReceive(feedPresentationPublisher) { isPresented in
            if !isPresented {
                isPlaying = false
            }
        }
    }

    private var feedPresentationPublisher: AnyPublisher<Bool, Never> {
        if let feedPresentationStore {
            return feedPresentationStore.$isPresented.eraseToAnyPublisher()
        }
        return Just(true).eraseToAnyPublisher()
    }

    @MainActor
    private func loadAspectRatio() async {
        if
            let cached = ConversationMarkdownVideoAspectRatioCache.shared
                .aspectRatio(for: url)
        {
            aspectRatio = cached
            return
        }

        let asset = AVURLAsset(url: url)
        guard
            let tracks = try? await asset.loadTracks(withMediaType: .video),
            let track = tracks.first,
            let naturalSize = try? await track.load(.naturalSize),
            let preferredTransform = try? await track.load(.preferredTransform)
        else {
            return
        }

        let loadedAspectRatio = ConversationMarkdownVideoLayout.aspectRatio(
            naturalSize: naturalSize,
            preferredTransform: preferredTransform
        )
        ConversationMarkdownVideoAspectRatioCache.shared.store(
            loadedAspectRatio,
            for: url
        )
        aspectRatio = loadedAspectRatio
    }
}

@MainActor
final class ConversationMarkdownVideoAspectRatioCache {
    static let shared = ConversationMarkdownVideoAspectRatioCache()

    private let storage = NSCache<NSURL, NSNumber>()

    init(countLimit: Int = 32) {
        storage.countLimit = countLimit
    }

    func aspectRatio(for url: URL) -> CGFloat? {
        guard let value = storage.object(forKey: url as NSURL) else {
            return nil
        }
        return CGFloat(truncating: value)
    }

    func store(_ aspectRatio: CGFloat, for url: URL) {
        storage.setObject(NSNumber(value: aspectRatio), forKey: url as NSURL)
    }
}

enum ConversationMarkdownVideoLayout {
    static let maximumWidth: CGFloat = 294
    static let fallbackAspectRatio: CGFloat = 9 / 16

    static func aspectRatio(
        naturalSize: CGSize,
        preferredTransform: CGAffineTransform
    ) -> CGFloat {
        let displayedSize = naturalSize.applying(preferredTransform)
        let width = abs(displayedSize.width)
        let height = abs(displayedSize.height)
        guard width > 0, height > 0 else {
            return fallbackAspectRatio
        }
        return width / height
    }
}

private struct LocalMarkdownVideoSurface: NSViewRepresentable {
    let url: URL
    let isPlaying: Bool

    func makeCoordinator() -> Coordinator {
        Coordinator()
    }

    func makeNSView(context: Context) -> PlayerLayerView {
        let player = AVPlayer(url: url)
        player.isMuted = true
        let view = PlayerLayerView(player: player)
        view.setAccessibilityLabel(OfficeLocalization.string("대화 로컬 동영상"))
        context.coordinator.startLooping(player)
        return view
    }

    func updateNSView(
        _ nsView: PlayerLayerView,
        context: Context
    ) {
        if isPlaying {
            nsView.player.play()
        } else {
            nsView.player.pause()
        }
    }

    static func dismantleNSView(
        _ nsView: PlayerLayerView,
        coordinator: Coordinator
    ) {
        coordinator.stopLooping()
        nsView.player.pause()
    }

    final class Coordinator {
        private var endObserver: NSObjectProtocol?

        func startLooping(_ player: AVPlayer) {
            endObserver = NotificationCenter.default.addObserver(
                forName: .AVPlayerItemDidPlayToEndTime,
                object: player.currentItem,
                queue: .main
            ) { [weak player] _ in
                player?.seek(to: .zero)
                player?.play()
            }
        }

        func stopLooping() {
            if let endObserver {
                NotificationCenter.default.removeObserver(endObserver)
            }
            endObserver = nil
        }

        deinit {
            stopLooping()
        }
    }

    final class PlayerLayerView: NSView {
        let player: AVPlayer
        private let playerLayer: AVPlayerLayer

        init(player: AVPlayer) {
            self.player = player
            playerLayer = AVPlayerLayer(player: player)
            super.init(frame: .zero)
            wantsLayer = true
            playerLayer.videoGravity = .resizeAspect
            layer?.addSublayer(playerLayer)
        }

        required init?(coder: NSCoder) {
            nil
        }

        override func layout() {
            super.layout()
            playerLayer.frame = bounds
        }
    }
}

private struct LocalMarkdownFileImage: View {
    let url: URL
    @State private var image: NSImage?

    init(url: URL) {
        self.url = url
        _image = State(initialValue: nil)
    }

    var body: some View {
        Button {
            if !NSWorkspace.shared.open(url) {
                NSWorkspace.shared.activateFileViewerSelecting([url])
            }
        } label: {
            Group {
                if let image {
                    LocalMarkdownThumbnail(image: image)
                } else {
                    RoundedRectangle(cornerRadius: 10, style: .continuous)
                        .fill(Color.primary.opacity(0.035))
                        .frame(width: 240, height: 160)
                        .overlay {
                            ProgressView()
                                .controlSize(.small)
                        }
                }
            }
        }
        .buttonStyle(.plain)
        .help(OfficeLocalization.string("이미지 열기"))
        .accessibilityLabel(OfficeLocalization.string("이미지 열기"))
        .task(id: url) {
            await loadImage()
        }
    }

    @MainActor
    private func loadImage() async {
        if let cached = LocalMarkdownImageCache.shared.image(at: url) {
            image = cached
            return
        }

        let thumbnail = await Task.detached(priority: .utility) {
            LocalMarkdownThumbnailLoader.loadCGImage(at: url)
        }.value
        guard
            !Task.isCancelled,
            let thumbnail
        else {
            return
        }
        let loadedImage = NSImage(
            cgImage: thumbnail,
            size: NSSize(
                width: thumbnail.width,
                height: thumbnail.height
            )
        )
        LocalMarkdownImageCache.shared.store(loadedImage, at: url)
        image = loadedImage
    }
}

enum LocalMarkdownThumbnailLoader {
    static let maximumPixelDimension = 640

    nonisolated static func loadCGImage(at url: URL) -> CGImage? {
        guard
            let source = CGImageSourceCreateWithURL(url as CFURL, nil)
        else {
            return nil
        }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceShouldCacheImmediately: true,
            kCGImageSourceThumbnailMaxPixelSize: maximumPixelDimension,
        ]
        return CGImageSourceCreateThumbnailAtIndex(
            source,
            0,
            options as CFDictionary
        )
    }
}

@MainActor
final class LocalMarkdownImageCache {
    static let shared = LocalMarkdownImageCache()

    private let storage = NSCache<NSString, NSImage>()

    private init() {
        storage.countLimit = 32
        storage.totalCostLimit = 64 * 1_024 * 1_024
    }

    func image(at url: URL) -> NSImage? {
        storage.object(forKey: cacheKey(for: url))
    }

    func store(_ image: NSImage, at url: URL) {
        let estimatedCost = max(
            1,
            image.representations.reduce(0) { current, representation in
                max(
                    current,
                    representation.pixelsWide
                        * representation.pixelsHigh
                        * 4
                )
            }
        )
        storage.setObject(
            image,
            forKey: cacheKey(for: url),
            cost: estimatedCost
        )
    }

    private func cacheKey(for url: URL) -> NSString {
        let values = try? url.resourceValues(
            forKeys: [
                .contentModificationDateKey,
                .fileSizeKey,
            ]
        )
        let modifiedAt =
            values?.contentModificationDate?.timeIntervalSince1970 ?? 0
        let fileSize = values?.fileSize ?? 0
        return "\(url.path)|\(modifiedAt)|\(fileSize)" as NSString
    }
}

private struct LocalMarkdownThumbnail: View {
    let image: NSImage

    var body: some View {
        let size = fittedSize

        Image(nsImage: image)
            .resizable()
            .frame(width: size.width, height: size.height)
            .clipShape(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
            )
            .overlay {
                RoundedRectangle(
                    cornerRadius: 10,
                    style: .continuous
                )
                .stroke(Color.primary.opacity(0.10))
            }
    }

    private var fittedSize: CGSize {
        let original = image.size
        guard original.width > 0, original.height > 0 else {
            return CGSize(width: 240, height: 180)
        }

        let scale = min(
            1,
            320 / original.width,
            240 / original.height
        )
        return CGSize(
            width: max(1, floor(original.width * scale)),
            height: max(1, floor(original.height * scale))
        )
    }
}
