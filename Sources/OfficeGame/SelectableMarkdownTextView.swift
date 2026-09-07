// Markdown 본문을 하나의 AppKit 텍스트 저장소로 렌더링해 문단과 표를
// 가로질러 끊김 없이 선택할 수 있게 한다.

import AppKit
import OfficeCore
import SwiftUI

struct SelectableMarkdownTextView: NSViewRepresentable {
    let source: String
    let fontSize: CGFloat
    let fileBaseDirectory: String?
    let minimumLayoutWidth: CGFloat?

    @Environment(\.colorScheme) private var colorScheme

    init(
        source: String,
        fontSize: CGFloat,
        fileBaseDirectory: String?,
        minimumLayoutWidth: CGFloat? = nil
    ) {
        self.source = source
        self.fontSize = fontSize
        self.fileBaseDirectory = fileBaseDirectory
        self.minimumLayoutWidth = minimumLayoutWidth
    }

    func makeNSView(context: Context) -> SelectableMarkdownDocumentView {
        let view = SelectableMarkdownDocumentView(
            fontSize: fontSize,
            minimumLayoutWidth: minimumLayoutWidth
        )
        apply(to: view)
        return view
    }

    // 선택 영역 전환으로 이 뷰가 버려질 때 조판이 끝난 텍스트 스택을 풀에
    // 맡긴다. SwiftUI는 새 뷰의 make를 먼저 부르므로 새 뷰는 자기 layout()에서
    // 이어받는다.
    static func dismantleNSView(
        _ nsView: SelectableMarkdownDocumentView,
        coordinator: ()
    ) {
        nsView.releaseTextStackForReuse()
    }

    func updateNSView(
        _ nsView: SelectableMarkdownDocumentView,
        context: Context
    ) {
        apply(to: nsView)
    }

    func sizeThatFits(
        _ proposal: ProposedViewSize,
        nsView: SelectableMarkdownDocumentView,
        context: Context
    ) -> CGSize? {
        guard let width = proposal.width, width > 0 else {
            return nil
        }
        return CGSize(
            width: width,
            height: nsView.heightThatFits(width: width)
        )
    }

    private func apply(to view: SelectableMarkdownDocumentView) {
        view.setMinimumLayoutWidth(minimumLayoutWidth)
        let fallbackDirectory = fileBaseDirectory.flatMap {
            $0.isEmpty ? nil : URL(fileURLWithPath: $0)
        }
        view.apply(
            source: source,
            fallbackDirectory: fallbackDirectory,
            isDark: colorScheme == .dark
        )
    }
}

// Immutable attribute names are shared by rendering and layout code; they do
// not access UI state and must not require a main-actor hop.
extension NSAttributedString.Key {
    static let conversationInlineCodeBackground = NSAttributedString.Key("conversationInlineCodeBackground")
}

/// Paint only: keeps the original characters and TextKit selection intact.
final class ConversationInlineCodeLayoutManager: NSLayoutManager {
    func codeBackgroundRects(for range: NSRange, in container: NSTextContainer) -> [NSRect] {
        guard let storage = textStorage else { return [] }
        // Paragraph separators can inherit the final run's attributes. Their
        // glyph bounds cover the remaining line width; they are not code ink.
        let string = storage.string as NSString
        var ranges: [NSRange] = []
        var start = range.location
        for index in range.location..<NSMaxRange(range) {
            if (CharacterSet.newlines as NSCharacterSet).characterIsMember(string.character(at: index)) {
                if index > start { ranges.append(NSRange(location: start, length: index - start)) }
                start = index + 1
            }
        }
        if start < NSMaxRange(range) { ranges.append(NSRange(location: start, length: NSMaxRange(range) - start)) }
        var result: [NSRect] = []
        for range in ranges {
            let glyphs = glyphRange(forCharacterRange: range, actualCharacterRange: nil)
            enumerateLineFragments(forGlyphRange: glyphs) { _, _, _, lineRange, _ in
                let intersection = NSIntersectionRange(glyphs, lineRange)
                if intersection.length > 0 {
                    result.append(self.boundingRect(forGlyphRange: intersection, in: container))
                }
            }
        }
        return result
    }

    override func drawBackground(forGlyphRange glyphsToShow: NSRange, at origin: NSPoint) {
        if let storage = textStorage, let container = textContainers.first {
            let characters = characterRange(forGlyphRange: glyphsToShow, actualGlyphRange: nil)
            storage.enumerateAttribute(.conversationInlineCodeBackground, in: characters) { value, range, _ in
                guard let color = value as? NSColor else { return }
                for rect in self.codeBackgroundRects(for: range, in: container) {
                    let box = rect.offsetBy(dx: origin.x, dy: origin.y).insetBy(dx: -2, dy: 0)
                    color.setFill()
                    NSBezierPath(roundedRect: box, xRadius: 8, yRadius: 8).fill()
                }
            }
        }
        // Native selection highlighting remains above the code background.
        super.drawBackground(forGlyphRange: glyphsToShow, at: origin)
    }
}

final class SelectableMarkdownDocumentView: NSView, NSTextViewDelegate {
    private(set) var textView = NSTextView()
    var linkOpener: ((URL) -> Bool)?

    private let horizontalClipView = SelectableMarkdownHorizontalClipView()
    private let horizontalScroller = NSScroller()
    private let fontSize: CGFloat
    private var minimumLayoutWidth: CGFloat?
    private var source = ""
    private var fallbackDirectory: URL?
    private var isDark = false
    private var measuredHeight = CGFloat.zero
    private var measuredWidth = CGFloat.zero
    private var measuredUsedRect = NSRect.zero
    private var hasValidTextMeasurement = false
    private(set) var textLayoutMeasurementCount = 0
    private(set) var textLayoutCacheHitCount = 0
    // hover·선택 상태 변화가 이 뷰를 얼마나 다시 만들고 다시 배치하는지
    // 측정하기 위한 계수기다. 화면 동작에는 영향이 없다.
    private(set) var layoutPassCount = 0
    private(set) var heightRequestCount = 0
    private(set) static var createdCount = 0
    private(set) var textStackReuseCount = 0
    private var textStackAdoptionAttemptsLeft = 3

    init(
        fontSize: CGFloat,
        minimumLayoutWidth: CGFloat? = nil
    ) {
        self.fontSize = fontSize
        self.minimumLayoutWidth = minimumLayoutWidth
        super.init(frame: .zero)
        Self.createdCount += 1
        configureTextView()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override var isFlipped: Bool {
        true
    }

    override var intrinsicContentSize: NSSize {
        NSSize(
            width: NSView.noIntrinsicMetric,
            height: max(measuredHeight, minimumTextHeight)
        )
    }

    override func layout() {
        super.layout()
        layoutPassCount += 1
        adoptPooledTextStackIfNeeded()
        guard bounds.width > 0 else {
            return
        }
        if usesHorizontalViewport {
            horizontalClipView.frame = bounds
        }
        updateTextContainerWidth(layoutWidth(for: bounds.width))
        updateMeasuredHeight()
        updateDocumentFrame()
        updateHorizontalScroller()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        let updatedIsDark = effectiveAppearance.bestMatch(
            from: [.darkAqua, .aqua]
        ) == .darkAqua
        guard updatedIsDark != isDark else {
            return
        }
        apply(
            source: source,
            fallbackDirectory: fallbackDirectory,
            isDark: updatedIsDark
        )
    }

    func apply(
        source: String,
        fallbackDirectory: URL?,
        isDark: Bool
    ) {
        let standardizedDirectory = fallbackDirectory?.standardizedFileURL
        guard
            self.source != source
                || self.fallbackDirectory != standardizedDirectory
                || self.isDark != isDark
        else {
            return
        }

        self.source = source
        self.fallbackDirectory = standardizedDirectory
        self.isDark = isDark
        invalidateTextMeasurement()

        let previousSelection = textView.selectedRange()
        textView.textStorage?.setAttributedString(
            SelectableMarkdownAttributedStringCache.shared.attributedString(
                source: source,
                fontSize: fontSize,
                fallbackDirectory: standardizedDirectory,
                isDark: isDark
            )
        )
        let length = textView.string.utf16.count
        textView.setSelectedRange(
            NSRange(
                location: min(previousSelection.location, length),
                length: min(
                    previousSelection.length,
                    max(0, length - min(previousSelection.location, length))
                )
            )
        )
        updateMeasuredHeight(force: true)
    }

    func setMinimumLayoutWidth(_ width: CGFloat?) {
        let normalized = width.flatMap { $0 > 0 ? $0 : nil }
        guard minimumLayoutWidth != normalized else {
            return
        }
        minimumLayoutWidth = normalized
        updateTextViewEmbedding()
        measuredWidth = 0
        invalidateTextMeasurement()
        invalidateIntrinsicContentSize()
        needsLayout = true
    }

    func heightThatFits(width: CGFloat) -> CGFloat {
        heightRequestCount += 1
        guard width > 0 else {
            return minimumTextHeight
        }
        updateTextContainerWidth(layoutWidth(for: width))
        if hasValidTextMeasurement {
            return max(measuredHeight, minimumTextHeight)
        }
        let height = measuredTextHeight()
        measuredHeight = height
        return height
    }

    // SwiftUI는 선택 영역이 바뀔 때 새 뷰의 make를 먼저 부르고 옛 뷰의
    // dismantle을 나중에 부른다. 그래서 옛 뷰는 글리프·레이아웃을 마친
    // NSTextView를 풀에 맡기고, 새 뷰는 트랜잭션이 끝난 뒤 도는 layout()에서
    // 같은 본문의 스택을 이어받는다. 같은 인스턴스가 이어지므로 표 전체를
    // 다시 조판·그리지 않고 선택 상태도 그대로 남는다.
    func releaseTextStackForReuse() {
        guard !source.isEmpty else {
            return
        }
        let stack = textView
        stack.delegate = nil
        if horizontalClipView.documentView === stack {
            horizontalClipView.documentView = nil
        }
        stack.removeFromSuperview()
        SelectableMarkdownTextStackPool.shared.store(stack, key: textStackKey)
    }

    private func adoptPooledTextStackIfNeeded() {
        guard textStackAdoptionAttemptsLeft > 0, !source.isEmpty else {
            return
        }
        textStackAdoptionAttemptsLeft -= 1
        guard
            let stack = SelectableMarkdownTextStackPool.shared.take(
                key: textStackKey
            )
        else {
            return
        }
        textStackAdoptionAttemptsLeft = 0
        let previous = textView
        previous.delegate = nil
        if horizontalClipView.documentView === previous {
            horizontalClipView.documentView = nil
        }
        previous.removeFromSuperview()
        textView = stack
        stack.delegate = self
        if usesHorizontalViewport {
            horizontalClipView.documentView = stack
        } else {
            addSubview(stack)
        }
        // 이어받은 스택이 이미 이 폭으로 조판돼 있으면 컨테이너를 건드리지
        // 않아 재조판이 일어나지 않는다.
        measuredWidth = stack.textContainer?.containerSize.width ?? 0
        textStackReuseCount += 1
    }

    private var textStackKey: String {
        [
            source,
            String(format: "%.2f", fontSize),
            fallbackDirectory?.path ?? "",
            isDark ? "dark" : "light",
        ].joined(separator: "\u{1F}")
    }

    private var minimumTextHeight: CGFloat {
        let font = NSFont.systemFont(ofSize: fontSize)
        return ceil(font.ascender) + ceil(abs(font.descender)) + 2
    }

    private func configureTextView() {
        textView.textContainer?.replaceLayoutManager(ConversationInlineCodeLayoutManager())
        wantsLayer = true
        textView.drawsBackground = false
        textView.backgroundColor = .clear
        textView.isEditable = false
        textView.isSelectable = true
        textView.isRichText = true
        textView.importsGraphics = false
        textView.allowsUndo = false
        textView.textContainerInset = .zero
        textView.textContainer?.lineFragmentPadding = 0
        textView.textContainer?.widthTracksTextView = false
        textView.textContainer?.heightTracksTextView = false
        // 가로 폭은 updateDocumentFrame이 정한다. 가로 자동 크기 조절을 켜 두면
        // 본문이 바뀔 때마다 NSTextView가 그 순간의 사용 폭으로 프레임을 줄이고,
        // 높이가 같아 layout()이 다시 돌지 않으면 긴 줄의 끝이 잘린 채 남는다.
        textView.isHorizontallyResizable = false
        textView.isVerticallyResizable = true
        textView.minSize = .zero
        textView.maxSize = NSSize(
            width: 100_000,
            height: 100_000
        )
        textView.linkTextAttributes = [
            .foregroundColor: NSColor.systemBlue,
            .underlineStyle: NSUnderlineStyle.single.rawValue,
        ]
        textView.delegate = self
        textView.setAccessibilityIdentifier(
            "conversationSelectableMarkdownText"
        )
        horizontalClipView.drawsBackground = false
        horizontalClipView.onHorizontalScroll = { [weak self] in
            self?.updateHorizontalScroller()
        }
        horizontalScroller.controlSize = .small
        horizontalScroller.scrollerStyle = .overlay
        horizontalScroller.target = self
        horizontalScroller.action = #selector(horizontalScrollerChanged(_:))
        horizontalScroller.isHidden = true
        updateTextViewEmbedding()
    }

    func textView(
        _ textView: NSTextView,
        clickedOnLink link: Any,
        at charIndex: Int
    ) -> Bool {
        guard let url = linkURL(from: link) else {
            return false
        }
        if
            let fileURL = LocalMarkdownResource.existingFileURL(
                from: url,
                fallbackDirectory: fallbackDirectory
            )
        {
            if let linkOpener {
                return linkOpener(fileURL)
            }
            if NSWorkspace.shared.open(fileURL) {
                return true
            }
            NSWorkspace.shared.activateFileViewerSelecting([fileURL])
            return true
        }
        if LocalMarkdownResource.fileURL(from: url) != nil {
            return true
        }
        return linkOpener?(url) ?? NSWorkspace.shared.open(url)
    }

    private func linkURL(from value: Any) -> URL? {
        if let url = value as? URL {
            return url
        }
        if let string = value as? String {
            return URL(string: string)
        }
        return nil
    }

    private var usesHorizontalViewport: Bool {
        minimumLayoutWidth != nil
    }

    private func updateTextViewEmbedding() {
        if usesHorizontalViewport {
            if horizontalClipView.superview !== self {
                textView.removeFromSuperview()
                horizontalClipView.documentView = textView
                addSubview(horizontalClipView)
                addSubview(horizontalScroller)
            }
        } else if textView.superview !== self {
            horizontalClipView.documentView = nil
            horizontalClipView.removeFromSuperview()
            horizontalScroller.removeFromSuperview()
            addSubview(textView)
        }
    }

    private func updateTextContainerWidth(_ width: CGFloat) {
        guard abs(measuredWidth - width) > 0.5 else {
            return
        }
        measuredWidth = width
        invalidateTextMeasurement()
        textView.textContainer?.containerSize = NSSize(
            width: width,
            height: 100_000
        )
    }

    private func layoutWidth(for viewportWidth: CGFloat) -> CGFloat {
        max(viewportWidth, minimumLayoutWidth ?? 0)
    }

    private func updateDocumentFrame() {
        let usedRect = measuredTextUsedRect()
        let layoutWidth = layoutWidth(for: bounds.width)
        textView.frame = NSRect(
            x: 0,
            y: 0,
            width: max(layoutWidth, ceil(usedRect.maxX)),
            height: max(bounds.height, ceil(usedRect.height))
        )
        if usesHorizontalViewport {
            let maximumX = max(
                0,
                textView.frame.width - horizontalClipView.bounds.width
            )
            let currentOrigin = horizontalClipView.bounds.origin
            if currentOrigin.x > maximumX || currentOrigin.x < 0 {
                horizontalClipView.scroll(
                    to: NSPoint(
                        x: min(max(0, currentOrigin.x), maximumX),
                        y: 0
                    )
                )
            }
        }
    }

    private func updateHorizontalScroller() {
        guard usesHorizontalViewport else {
            horizontalScroller.isHidden = true
            return
        }
        let viewportWidth = horizontalClipView.bounds.width
        let documentWidth = textView.frame.width
        let maximumX = max(0, documentWidth - viewportWidth)
        guard maximumX > 0.5, viewportWidth > 0 else {
            horizontalScroller.isHidden = true
            if horizontalClipView.bounds.origin.x != 0 {
                horizontalClipView.scroll(to: .zero)
            }
            return
        }

        let scrollerHeight = NSScroller.scrollerWidth(
            for: .small,
            scrollerStyle: .overlay
        )
        horizontalScroller.frame = NSRect(
            x: 4,
            y: max(0, bounds.height - scrollerHeight),
            width: max(0, bounds.width - 8),
            height: scrollerHeight
        )
        horizontalScroller.knobProportion = min(
            1,
            viewportWidth / documentWidth
        )
        horizontalScroller.doubleValue = Double(
            horizontalClipView.bounds.origin.x / maximumX
        )
        horizontalScroller.isHidden = false
    }

    @objc
    private func horizontalScrollerChanged(_ sender: NSScroller) {
        let maximumX = max(
            0,
            textView.frame.width - horizontalClipView.bounds.width
        )
        horizontalClipView.scroll(
            to: NSPoint(
                x: CGFloat(sender.doubleValue) * maximumX,
                y: 0
            )
        )
        horizontalClipView.onHorizontalScroll?()
    }

    private func measuredTextHeight() -> CGFloat {
        max(
            minimumTextHeight,
            ceil(measuredTextUsedRect().height)
        )
    }

    private func measuredTextUsedRect() -> NSRect {
        if hasValidTextMeasurement {
            return measuredUsedRect
        }
        if
            let cached = SelectableMarkdownLayoutMeasurementCache.shared
                .usedRect(
                    source: source,
                    fontSize: fontSize,
                    fallbackDirectory: fallbackDirectory,
                    isDark: isDark,
                    layoutWidth: measuredWidth
                )
        {
            measuredUsedRect = cached
            hasValidTextMeasurement = true
            textLayoutCacheHitCount += 1
            return cached
        }
        guard
            let layoutManager = textView.layoutManager,
            let textContainer = textView.textContainer
        else {
            return .zero
        }
        layoutManager.ensureLayout(for: textContainer)
        measuredUsedRect = layoutManager.usedRect(for: textContainer)
        hasValidTextMeasurement = true
        textLayoutMeasurementCount += 1
        SelectableMarkdownLayoutMeasurementCache.shared.store(
            measuredUsedRect,
            source: source,
            fontSize: fontSize,
            fallbackDirectory: fallbackDirectory,
            isDark: isDark,
            layoutWidth: measuredWidth
        )
        return measuredUsedRect
    }

    private func invalidateTextMeasurement() {
        hasValidTextMeasurement = false
        measuredUsedRect = .zero
    }

    private func updateMeasuredHeight(force: Bool = false) {
        guard measuredWidth > 0 else {
            return
        }
        if force {
            invalidateTextMeasurement()
        } else if hasValidTextMeasurement {
            return
        }
        let updatedHeight = measuredTextHeight()
        guard force || abs(measuredHeight - updatedHeight) > 0.5 else {
            return
        }
        measuredHeight = updatedHeight
        invalidateIntrinsicContentSize()
        superview?.needsLayout = true
    }
}

@MainActor
final class SelectableMarkdownLayoutMeasurementCache {
    private final class Entry {
        let usedRect: NSRect

        init(usedRect: NSRect) {
            self.usedRect = usedRect
        }
    }

    static let shared = SelectableMarkdownLayoutMeasurementCache()

    private let storage = NSCache<NSString, Entry>()

    private init() {
        storage.countLimit = 160
        storage.totalCostLimit = 512 * 1_024
    }

    func usedRect(
        source: String,
        fontSize: CGFloat,
        fallbackDirectory: URL?,
        isDark: Bool,
        layoutWidth: CGFloat
    ) -> NSRect? {
        guard layoutWidth > 0 else {
            return nil
        }
        return storage.object(
            forKey: key(
                source: source,
                fontSize: fontSize,
                fallbackDirectory: fallbackDirectory,
                isDark: isDark,
                layoutWidth: layoutWidth
            )
        )?.usedRect
    }

    func store(
        _ usedRect: NSRect,
        source: String,
        fontSize: CGFloat,
        fallbackDirectory: URL?,
        isDark: Bool,
        layoutWidth: CGFloat
    ) {
        guard layoutWidth > 0 else {
            return
        }
        storage.setObject(
            Entry(usedRect: usedRect),
            forKey: key(
                source: source,
                fontSize: fontSize,
                fallbackDirectory: fallbackDirectory,
                isDark: isDark,
                layoutWidth: layoutWidth
            ),
            cost: max(1, source.utf8.count / 8)
        )
    }

    func removeAll() {
        storage.removeAllObjects()
    }

    private func key(
        source: String,
        fontSize: CGFloat,
        fallbackDirectory: URL?,
        isDark: Bool,
        layoutWidth: CGFloat
    ) -> NSString {
        [
            source,
            String(format: "%.2f", fontSize),
            fallbackDirectory?.standardizedFileURL.path ?? "",
            isDark ? "dark" : "light",
            String(format: "%.2f", layoutWidth),
        ].joined(separator: "\u{1F}") as NSString
    }
}

// 선택 영역 전환으로 버려지는 문서 뷰의 NSTextView를 잠깐 맡아 두는 풀이다.
// 같은 본문을 그리는 새 뷰가 곧바로 이어받지 않으면 전환이 아니므로
// 짧은 수명 뒤 버린다.
@MainActor
final class SelectableMarkdownTextStackPool {
    static let shared = SelectableMarkdownTextStackPool()
    static let lifetime: TimeInterval = 1.5
    static let capacity = 4

    private var entries: [(key: String, textView: NSTextView, storedAt: Date)] = []

    init() {}

    var count: Int {
        entries.count
    }

    func store(_ textView: NSTextView, key: String, now: Date = Date()) {
        purge(now: now)
        entries.removeAll { $0.key == key }
        entries.append((key: key, textView: textView, storedAt: now))
        if entries.count > Self.capacity {
            entries.removeFirst(entries.count - Self.capacity)
        }
    }

    func take(key: String, now: Date = Date()) -> NSTextView? {
        purge(now: now)
        guard let index = entries.firstIndex(where: { $0.key == key }) else {
            return nil
        }
        return entries.remove(at: index).textView
    }

    func removeAll() {
        entries.removeAll()
    }

    private func purge(now: Date) {
        entries.removeAll {
            now.timeIntervalSince($0.storedAt) > Self.lifetime
        }
    }
}

@MainActor
private final class SelectableMarkdownHorizontalClipView: NSClipView {
    var onHorizontalScroll: (() -> Void)?

    override func scrollWheel(with event: NSEvent) {
        guard abs(event.scrollingDeltaX) > 0.1 else {
            nextResponder?.scrollWheel(with: event)
            return
        }
        guard let documentView else {
            return
        }
        let maximumX = max(0, documentView.frame.width - bounds.width)
        let nextX = min(
            max(0, bounds.origin.x - event.scrollingDeltaX),
            maximumX
        )
        scroll(to: NSPoint(x: nextX, y: 0))
        onHorizontalScroll?()
    }
}

struct SelectableMarkdownSegment: Equatable {
    enum Kind: Equatable {
        case prose
        case table(columnCount: Int)
        case codeBlock
    }

    let source: String
    let kind: Kind

    var minimumLayoutWidth: CGFloat? {
        guard case let .table(columnCount) = kind else {
            return nil
        }
        return max(280, CGFloat(columnCount) * 110)
    }
}

enum SelectableMarkdownSegmenter {
    // Foundation은 텍스트가 없는 표 셀의 presentation intent를 생략한다.
    // 파싱 동안만 자리표시자를 넣어 빈 셀·행도 같은 열 수를 유지한다.
    static func preservingEmptyTableCells(
        in source: String,
        placeholder: String
    ) -> String {
        guard source.contains("|") else { return source }
        var lines = source.components(separatedBy: "\n")
        var index = 0
        while index < lines.count {
            if let openingFence = fence(in: lines[index]) {
                index += 1
                while index < lines.count {
                    let closes = isClosingFence(lines[index], matching: openingFence)
                    index += 1
                    if closes { break }
                }
                continue
            }
            guard
                !lines[index].hasPrefix("    "),
                !lines[index].hasPrefix("\t"),
                lines.indices.contains(index + 1),
                let columnCount = tableColumnCount(
                    header: lines[index],
                    delimiter: lines[index + 1]
                )
            else {
                index += 1
                continue
            }

            let delimiterIndex = index + 1
            while index < lines.count {
                if index == delimiterIndex {
                    index += 1
                    continue
                }
                guard let cells = tableCells(in: lines[index]) else { break }
                if cells.count < columnCount || cells.prefix(columnCount).contains("") {
                    let padded = (0..<columnCount).map { column in
                        cells.indices.contains(column) && !cells[column].isEmpty
                            ? cells[column] : placeholder
                    }
                    lines[index] = "| " + padded.joined(separator: " | ") + " |"
                }
                index += 1
            }
        }
        return lines.joined(separator: "\n")
    }

    static func split(_ source: String) -> [SelectableMarkdownSegment] {
        let lines = source.split(
            separator: "\n",
            omittingEmptySubsequences: false
        ).map(String.init)
        var segments: [SelectableMarkdownSegment] = []
        var proseStart = 0
        var index = 0

        while index < lines.count {
            guard let openingFence = fence(in: lines[index]) else {
                index += 1
                continue
            }

            appendProse(
                lines[proseStart..<index],
                to: &segments
            )

            var codeEnd = index + 1
            while codeEnd < lines.count {
                if isClosingFence(
                    lines[codeEnd],
                    matching: openingFence
                ) {
                    codeEnd += 1
                    break
                }
                codeEnd += 1
            }

            segments.append(
                SelectableMarkdownSegment(
                    source: lines[index..<codeEnd].joined(separator: "\n"),
                    kind: .codeBlock
                )
            )
            proseStart = codeEnd
            index = codeEnd
        }

        appendProse(lines[proseStart...], to: &segments)
        if segments.isEmpty {
            return [SelectableMarkdownSegment(source: source, kind: .prose)]
        }
        return segments
    }

    private struct Fence: Equatable {
        let character: Character
        let length: Int
    }

    private static func appendProse(
        _ lines: ArraySlice<String>,
        to segments: inout [SelectableMarkdownSegment]
    ) {
        let source = lines.joined(separator: "\n")
        guard !source.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else {
            return
        }
        segments.append(
            SelectableMarkdownSegment(
                source: source,
                kind: proseKind(in: source)
            )
        )
    }

    private static func proseKind(
        in source: String
    ) -> SelectableMarkdownSegment.Kind {
        let lines = source.split(
            separator: "\n",
            omittingEmptySubsequences: false
        ).map(String.init)
        var maximumTableColumnCount: Int?
        var index = 0

        while index < lines.count {
            guard
                lines.indices.contains(index + 1),
                let columnCount = tableColumnCount(
                    header: lines[index],
                    delimiter: lines[index + 1]
                )
            else {
                index += 1
                continue
            }

            maximumTableColumnCount = max(
                maximumTableColumnCount ?? 0,
                columnCount
            )
            var tableEnd = index + 2
            while tableEnd < lines.count {
                let line = lines[tableEnd]
                guard
                    !line.trimmingCharacters(in: .whitespaces).isEmpty,
                    tableCells(in: line) != nil
                else {
                    break
                }
                tableEnd += 1
            }
            index = tableEnd
        }

        let kind = maximumTableColumnCount.map {
            SelectableMarkdownSegment.Kind.table(columnCount: $0)
        } ?? .prose
        return kind
    }

    private static func tableColumnCount(
        header: String,
        delimiter: String
    ) -> Int? {
        guard
            let headerCells = tableCells(in: header),
            let delimiterCells = tableCells(in: delimiter),
            !headerCells.isEmpty,
            headerCells.count == delimiterCells.count,
            delimiterCells.allSatisfy({ cell in
                cell.range(
                    of: #"^:?-{3,}:?$"#,
                    options: .regularExpression
                ) != nil
            })
        else {
            return nil
        }
        return headerCells.count
    }

    private static func tableCells(in line: String) -> [String]? {
        var cells = [""]
        var foundPipe = false
        var isEscaped = false

        for character in line {
            if isEscaped {
                cells[cells.count - 1].append(character)
                isEscaped = false
            } else if character == "\\" {
                cells[cells.count - 1].append(character)
                isEscaped = true
            } else if character == "|" {
                foundPipe = true
                cells.append("")
            } else {
                cells[cells.count - 1].append(character)
            }
        }

        guard foundPipe else {
            return nil
        }
        if cells.first?.trimmingCharacters(in: .whitespaces).isEmpty == true {
            cells.removeFirst()
        }
        if cells.last?.trimmingCharacters(in: .whitespaces).isEmpty == true {
            cells.removeLast()
        }
        return cells.map {
            $0.trimmingCharacters(in: .whitespaces)
        }
    }

    private static func fence(in line: String) -> Fence? {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard let first = trimmed.first, first == "`" || first == "~" else {
            return nil
        }
        let prefixLength = trimmed.prefix { $0 == first }.count
        guard prefixLength >= 3 else {
            return nil
        }
        return Fence(character: first, length: prefixLength)
    }

    private static func isClosingFence(
        _ line: String,
        matching openingFence: Fence
    ) -> Bool {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        let prefixLength = trimmed.prefix {
            $0 == openingFence.character
        }.count
        guard prefixLength >= openingFence.length else {
            return false
        }
        return trimmed.dropFirst(prefixLength)
            .trimmingCharacters(in: .whitespaces)
            .isEmpty
    }
}

@MainActor
final class SelectableMarkdownAttributedStringCache {
    private final class Entry {
        let attributedString: NSAttributedString

        init(attributedString: NSAttributedString) {
            self.attributedString = attributedString
        }
    }

    static let shared = SelectableMarkdownAttributedStringCache()

    private let storage = NSCache<NSString, Entry>()

    private init() {
        storage.countLimit = 80
        storage.totalCostLimit = 12 * 1_024 * 1_024
    }

    func attributedString(
        source: String,
        fontSize: CGFloat,
        fallbackDirectory: URL?,
        isDark: Bool
    ) -> NSAttributedString {
        let key = [
            source,
            String(format: "%.2f", fontSize),
            fallbackDirectory?.path ?? "",
            isDark ? "dark" : "light",
        ].joined(separator: "\u{1F}") as NSString
        if let cached = storage.object(forKey: key) {
            return cached.attributedString
        }

        let attributedString = SelectableMarkdownAttributedRenderer.render(
            source: source,
            fontSize: fontSize,
            fallbackDirectory: fallbackDirectory,
            isDark: isDark
        )
        storage.setObject(
            Entry(attributedString: attributedString),
            forKey: key,
            cost: max(1, attributedString.length * 4)
        )
        return attributedString
    }
}

enum SelectableMarkdownAttributedRenderer {
    private struct TableCellDescriptor {
        let tableIdentity: Int
        let columnCount: Int
        let rowIndex: Int
        let columnIndex: Int
        let isHeader: Bool
        let alignment: PresentationIntent.TableColumn.Alignment
    }

    private struct BlockDescriptor {
        let identity: Int
        let headerLevel: Int?
        let codeLanguage: String?
        let isCodeBlock: Bool
        let isBlockQuote: Bool
        let listItemIdentity: Int?
        let listItemOrdinal: Int?
        let isOrderedList: Bool
        let indentationLevel: Int
        let tableCell: TableCellDescriptor?

        init(_ intent: PresentationIntent?) {
            let components = intent?.components ?? []
            identity = components.first?.identity ?? 0
            indentationLevel = intent?.indentationLevel ?? 0

            var headerLevel: Int?
            var codeLanguage: String?
            var isCodeBlock = false
            var isBlockQuote = false
            var listItemIdentity: Int?
            var listItemOrdinal: Int?
            var isOrderedList = false
            var listKindResolved = false
            var tableIdentity: Int?
            var tableColumns: [PresentationIntent.TableColumn] = []
            var tableRowIndex: Int?
            var tableColumnIndex: Int?
            var isTableHeader = false

            for component in components {
                switch component.kind {
                case let .header(level):
                    headerLevel = level
                case let .codeBlock(languageHint):
                    isCodeBlock = true
                    codeLanguage = languageHint
                case .blockQuote:
                    isBlockQuote = true
                // 구성 요소는 안쪽부터 바깥쪽 순서다. 중첩 목록에서는 가장
                // 안쪽 항목과 그 목록 종류만 쓴다. 바깥 항목을 잡으면 이미
                // 글머리표를 붙인 항목으로 보고 안쪽 항목의 글머리표를 건너뛴다.
                case .orderedList:
                    if listItemIdentity != nil, !listKindResolved {
                        isOrderedList = true
                    }
                    listKindResolved = true
                case .unorderedList:
                    listKindResolved = true
                case let .listItem(ordinal):
                    guard listItemIdentity == nil else { break }
                    listItemIdentity = component.identity
                    listItemOrdinal = ordinal
                case let .table(columns):
                    tableIdentity = component.identity
                    tableColumns = columns
                case .tableHeaderRow:
                    tableRowIndex = 0
                    isTableHeader = true
                case let .tableRow(rowIndex):
                    tableRowIndex = rowIndex
                case let .tableCell(columnIndex):
                    tableColumnIndex = columnIndex
                case .paragraph, .thematicBreak:
                    break
                @unknown default:
                    break
                }
            }

            self.headerLevel = headerLevel
            self.codeLanguage = codeLanguage
            self.isCodeBlock = isCodeBlock
            self.isBlockQuote = isBlockQuote
            self.listItemIdentity = listItemIdentity
            self.listItemOrdinal = listItemOrdinal
            self.isOrderedList = isOrderedList

            if
                let tableIdentity,
                let tableRowIndex,
                let tableColumnIndex
            {
                let alignment = tableColumns.indices.contains(tableColumnIndex)
                    ? tableColumns[tableColumnIndex].alignment
                    : .left
                tableCell = TableCellDescriptor(
                    tableIdentity: tableIdentity,
                    columnCount: max(1, tableColumns.count),
                    rowIndex: tableRowIndex,
                    columnIndex: tableColumnIndex,
                    isHeader: isTableHeader,
                    alignment: alignment
                )
            } else {
                tableCell = nil
            }
        }
    }

    static func render(
        source: String,
        fontSize: CGFloat,
        fallbackDirectory: URL?,
        isDark: Bool
    ) -> NSAttributedString {
        let trimmed = source.trimmingCharacters(
            in: .whitespacesAndNewlines
        )
        let markdown = trimmed.isEmpty
            ? OfficeLocalization.string("내용 없음")
            : trimmed
        let markdownWithVisibleLineBreaks = preservingVisibleLineBreaks(
            in: markdown
        )
        // 원문에 없는 기호를 쓰고 렌더링할 때 제거해 복사 내용은 보존한다.
        var emptyCellPlaceholder = "\u{2060}"
        while markdown.contains(emptyCellPlaceholder) {
            emptyCellPlaceholder += "\u{2060}"
        }
        let markdownWithTableCells = SelectableMarkdownSegmenter.preservingEmptyTableCells(
            in: markdownWithVisibleLineBreaks,
            placeholder: emptyCellPlaceholder
        )
        guard
            let parsed = try? AttributedString(
                markdown: markdownWithTableCells,
                options: .init(interpretedSyntax: .full),
                baseURL: fallbackDirectory
            )
        else {
            return fallback(
                markdown,
                fontSize: fontSize,
                isDark: isDark
            )
        }

        let output = NSMutableAttributedString()
        var previousBlockIdentity: Int?
        var previousAttributes: [NSAttributedString.Key: Any]?
        var previousWasTableCell = false
        var prefixedListItems = Set<Int>()
        var textTables: [Int: NSTextTable] = [:]

        for run in parsed.runs {
            let descriptor = BlockDescriptor(run.presentationIntent)
            let attributes = attributes(
                descriptor: descriptor,
                inlineIntent: run.inlinePresentationIntent,
                link: run.link,
                imageURL: run.imageURL,
                fontSize: fontSize,
                isDark: isDark,
                textTables: &textTables
            )

            if previousBlockIdentity != descriptor.identity {
                appendParagraphBoundary(
                    to: output,
                    attributes: previousAttributes ?? attributes,
                    force: previousWasTableCell
                )
                if
                    let listItemIdentity = descriptor.listItemIdentity,
                    prefixedListItems.insert(listItemIdentity).inserted
                {
                    let marker = descriptor.isOrderedList
                        ? "\(descriptor.listItemOrdinal ?? 1).\t"
                        : "•\t"
                    // 항목이 링크로 시작해도 글머리표는 링크가 아니다. 첫 run의
                    // 링크·밑줄·링크 색을 글머리표에 그대로 옮기지 않는다.
                    var markerAttributes = attributes
                    markerAttributes[.link] = nil
                    markerAttributes[.underlineStyle] = nil
                    markerAttributes[.foregroundColor] = baseAttributes(
                        fontSize: fontSize,
                        isDark: isDark
                    )[.foregroundColor]
                    output.append(
                        NSAttributedString(
                            string: marker,
                            attributes: markerAttributes
                        )
                    )
                }
            }

            let text = String(parsed.characters[run.range])
            output.append(
                NSAttributedString(
                    string: text.replacingOccurrences(of: emptyCellPlaceholder, with: ""),
                    attributes: attributes
                )
            )
            previousBlockIdentity = descriptor.identity
            previousAttributes = attributes
            previousWasTableCell = descriptor.tableCell != nil
        }

        appendParagraphBoundary(
            to: output,
            attributes: previousAttributes ?? baseAttributes(
                fontSize: fontSize,
                isDark: isDark
            ),
            force: previousWasTableCell
        )
        return output
    }

    // Foundation Markdown은 같은 문단 안의 단일 줄바꿈을 공백으로
    // 접는다. 대화 원문은 사용자가 입력한 줄 구분 자체가 의미가 있으므로
    // fenced code를 제외한 줄 끝을 Markdown hard break로 보존한다.
    private static func preservingVisibleLineBreaks(in source: String) -> String {
        let lines = source.split(
            separator: "\n",
            omittingEmptySubsequences: false
        ).map(String.init)
        guard lines.count > 1 else {
            return source
        }

        var output: [String] = []
        output.reserveCapacity(lines.count)
        var openFence: Character?

        for (index, line) in lines.enumerated() {
            let fence = markdownFenceCharacter(in: line)
            let wasInsideFence = openFence != nil

            if let fence {
                if openFence == fence {
                    openFence = nil
                } else if openFence == nil {
                    openFence = fence
                }
            }

            guard index < lines.count - 1 else {
                output.append(line)
                continue
            }

            let trimmed = line.trimmingCharacters(in: .whitespaces)
            let alreadyHardBreak = line.hasSuffix("  ") || line.hasSuffix("\\")
            if
                !wasInsideFence,
                fence == nil,
                !trimmed.isEmpty,
                !alreadyHardBreak
            {
                output.append(line + "  ")
            } else {
                output.append(line)
            }
        }

        return output.joined(separator: "\n")
    }

    private static func markdownFenceCharacter(in line: String) -> Character? {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard let first = trimmed.first, first == "`" || first == "~" else {
            return nil
        }
        return trimmed.prefix { $0 == first }.count >= 3 ? first : nil
    }

    private static func appendParagraphBoundary(
        to output: NSMutableAttributedString,
        attributes: [NSAttributedString.Key: Any],
        force: Bool = false
    ) {
        guard force || (output.length > 0 && !output.string.hasSuffix("\n")) else {
            return
        }
        output.append(
            NSAttributedString(string: "\n", attributes: attributes)
        )
    }

    private static func attributes(
        descriptor: BlockDescriptor,
        inlineIntent: InlinePresentationIntent?,
        link: URL?,
        imageURL: URL?,
        fontSize: CGFloat,
        isDark: Bool,
        textTables: inout [Int: NSTextTable]
    ) -> [NSAttributedString.Key: Any] {
        var attributes = baseAttributes(
            fontSize: fontSize,
            isDark: isDark
        )
        let paragraphStyle = NSMutableParagraphStyle()
        paragraphStyle.lineSpacing = 2
        paragraphStyle.paragraphSpacing = descriptor.tableCell == nil ? 9 : 0
        paragraphStyle.lineBreakMode = .byWordWrapping

        var font = NSFont.systemFont(ofSize: fontSize)
        if let headerLevel = descriptor.headerLevel {
            let scale: CGFloat
            switch headerLevel {
            case 1:
                scale = 1.65
            case 2:
                scale = 1.42
            case 3:
                scale = 1.22
            default:
                scale = 1.08
            }
            font = NSFont.systemFont(
                ofSize: fontSize * scale,
                weight: .bold
            )
            paragraphStyle.paragraphSpacingBefore = 5
            paragraphStyle.paragraphSpacing = 7
        }

        if descriptor.isCodeBlock || inlineIntent?.contains(.code) == true {
            font = NSFont.monospacedSystemFont(
                ofSize: fontSize,
                weight: .regular
            )
            attributes[descriptor.isCodeBlock ? .backgroundColor : .conversationInlineCodeBackground] = isDark
                ? NSColor.white.withAlphaComponent(0.12)
                : NSColor.black.withAlphaComponent(0.08)
            if descriptor.isCodeBlock {
                paragraphStyle.paragraphSpacing = 12
            }
        }

        if
            inlineIntent?.contains(.stronglyEmphasized) == true
                || descriptor.tableCell?.isHeader == true
        {
            font = NSFontManager.shared.convert(
                font,
                toHaveTrait: .boldFontMask
            )
        }
        if inlineIntent?.contains(.emphasized) == true {
            font = NSFontManager.shared.convert(
                font,
                toHaveTrait: .italicFontMask
            )
        }
        if inlineIntent?.contains(.strikethrough) == true {
            attributes[.strikethroughStyle] =
                NSUnderlineStyle.single.rawValue
        }

        if let listItemIdentity = descriptor.listItemIdentity {
            _ = listItemIdentity
            let indent = CGFloat(max(1, descriptor.indentationLevel)) * 18
            paragraphStyle.firstLineHeadIndent = max(0, indent - 18)
            paragraphStyle.headIndent = indent
            paragraphStyle.tabStops = [
                NSTextTab(
                    textAlignment: .left,
                    location: indent,
                    options: [:]
                )
            ]
        }

        if descriptor.isBlockQuote {
            let indent = CGFloat(max(1, descriptor.indentationLevel)) * 12
            paragraphStyle.firstLineHeadIndent = indent
            paragraphStyle.headIndent = indent
            attributes[.foregroundColor] = isDark
                ? NSColor.white.withAlphaComponent(0.68)
                : NSColor.secondaryLabelColor
        }

        if let tableCell = descriptor.tableCell {
            let table: NSTextTable
            if let existing = textTables[tableCell.tableIdentity] {
                table = existing
            } else {
                let created = NSTextTable()
                created.numberOfColumns = tableCell.columnCount
                created.collapsesBorders = true
                created.hidesEmptyCells = false
                created.setContentWidth(
                    100,
                    type: .percentageValueType
                )
                textTables[tableCell.tableIdentity] = created
                table = created
            }
            let block = NSTextTableBlock(
                table: table,
                startingRow: tableCell.rowIndex,
                rowSpan: 1,
                startingColumn: tableCell.columnIndex,
                columnSpan: 1
            )
            block.setWidth(1, type: .absoluteValueType, for: .border)
            block.setBorderColor(
                isDark
                    ? NSColor.white.withAlphaComponent(0.20)
                    : NSColor.black.withAlphaComponent(0.18)
            )
            block.setWidth(6, type: .absoluteValueType, for: .padding)
            if tableCell.isHeader || tableCell.rowIndex.isMultiple(of: 2) {
                block.backgroundColor = isDark
                    ? NSColor.white.withAlphaComponent(0.055)
                    : NSColor.black.withAlphaComponent(0.035)
            }
            paragraphStyle.textBlocks = [block]
            switch tableCell.alignment {
            case .left:
                paragraphStyle.alignment = .left
            case .center:
                paragraphStyle.alignment = .center
            case .right:
                paragraphStyle.alignment = .right
            @unknown default:
                paragraphStyle.alignment = .left
            }
        }

        attributes[.font] = font
        attributes[.paragraphStyle] = paragraphStyle
        if let link {
            attributes[.link] = link
            attributes[.foregroundColor] = NSColor.systemBlue
            attributes[.underlineStyle] = NSUnderlineStyle.single.rawValue
        }
        if imageURL != nil {
            attributes[.foregroundColor] = NSColor.secondaryLabelColor
        }
        return attributes
    }

    private static func baseAttributes(
        fontSize: CGFloat,
        isDark: Bool
    ) -> [NSAttributedString.Key: Any] {
        [
            .font: NSFont.systemFont(ofSize: fontSize),
            .foregroundColor: isDark
                ? NSColor.white.withAlphaComponent(0.92)
                : NSColor.labelColor,
        ]
    }

    private static func fallback(
        _ markdown: String,
        fontSize: CGFloat,
        isDark: Bool
    ) -> NSAttributedString {
        let paragraphStyle = NSMutableParagraphStyle()
        paragraphStyle.lineSpacing = 2
        paragraphStyle.paragraphSpacing = 9
        var attributes = baseAttributes(
            fontSize: fontSize,
            isDark: isDark
        )
        attributes[.paragraphStyle] = paragraphStyle
        return NSAttributedString(string: markdown, attributes: attributes)
    }
}
