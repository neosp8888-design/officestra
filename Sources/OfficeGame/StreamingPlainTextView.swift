// 이 파일은 실시간 응답을 AppKit 텍스트 저장소에 한 글자씩 추가해 SwiftUI 전체 재렌더링을 피한다.

import AppKit
import OfficeCore
import SwiftUI

enum StreamingPlainTextRevealMode: Equatable {
    case trailingCharacters
    case fullLine
}

struct StreamingPlainTextView: NSViewRepresentable {
    let source: String
    let animates: Bool
    let animatesInitialSource: Bool
    let fontSize: CGFloat
    let lineSpacing: CGFloat
    let revealMode: StreamingPlainTextRevealMode
    let onFinishedTyping: () -> Void

    func makeNSView(context: Context) -> IncrementalStreamingTextView {
        let view = IncrementalStreamingTextView(
            fontSize: fontSize,
            lineSpacing: lineSpacing
        )
        view.onFinishedTyping = onFinishedTyping
        view.apply(
            source: source,
            animates: animates && animatesInitialSource,
            revealMode: revealMode
        )
        return view
    }

    func updateNSView(
        _ nsView: IncrementalStreamingTextView,
        context: Context
    ) {
        nsView.onFinishedTyping = onFinishedTyping
        nsView.apply(
            source: source,
            animates: animates,
            revealMode: revealMode
        )
    }

    func sizeThatFits(
        _ proposal: ProposedViewSize,
        nsView: IncrementalStreamingTextView,
        context: Context
    ) -> CGSize? {
        guard let width = proposal.width, width.isFinite,
              width > 0, width < 100_000 else {
            return nil
        }
        return CGSize(
            width: width,
            height: nsView.heightThatFits(width: width)
        )
    }
}

final class IncrementalStreamingTextView: NSView {
    private let textView = NSTextView()
    private let fontSize: CGFloat
    private let lineSpacing: CGFloat
    private var renderedText = ""
    private var targetText = ""
    private var pendingCharacters: [Character] = []
    private var pendingIndex = 0
    private var charactersPerTick = 1
    private var animationStartedAt: TimeInterval?
    private var animationDuration: TimeInterval?
    private var revealMode = StreamingPlainTextRevealMode.trailingCharacters
    private var updateGeneration = 0
    private var finishedGeneration: Int?
    private var typingTimer: Timer?
    private var measuredHeight = CGFloat.zero
    private var measuredWidth = CGFloat.zero
    private weak var pausedLiveScrollView: NSScrollView?
    private var liveScrollPausedAt: TimeInterval?
    var onFinishedTyping: () -> Void = {}

    init(fontSize: CGFloat, lineSpacing: CGFloat) {
        self.fontSize = fontSize
        self.lineSpacing = lineSpacing
        super.init(frame: .zero)
        configureTextView()
        observeLiveScroll()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    deinit {
        typingTimer?.invalidate()
        NotificationCenter.default.removeObserver(self)
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
        guard bounds.width > 0 else {
            return
        }
        textView.frame = bounds
        updateTextContainerWidth(bounds.width)
        if revealMode == .trailingCharacters {
            updateMeasuredHeight()
        }
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if window == nil {
            stopTyping()
        } else {
            startTypingIfNeeded()
        }
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        replaceRenderedText(renderedText)
    }

    func apply(
        source: String,
        animates: Bool,
        revealMode: StreamingPlainTextRevealMode = .trailingCharacters
    ) {
        guard source != targetText else {
            // 원문이 그대로여도 아직 이번 revision의 완료를 알리지 않았다면
            // 여기서 알린다. 코드 펜스 안의 빈 줄은 원문이 ""이라 초기
            // targetText와 같아 이 경로로 들어오고, 통보가 없으면 줄 단위
            // 타자가 그 줄에서 영영 멈춘다.
            if finishedGeneration != updateGeneration,
               pendingCharacters.isEmpty {
                scheduleImmediateCompletion(
                    generation: updateGeneration,
                    target: targetText
                )
            }
            return
        }

        stopTyping(resetTiming: true)
        updateGeneration += 1
        let generation = updateGeneration
        finishedGeneration = nil
        targetText = source
        self.revealMode = revealMode
        let plan: StreamingTextUpdatePlan
        switch revealMode {
        case .trailingCharacters:
            plan = StreamingTextPacer.updatePlan(
                current: renderedText,
                target: source,
                animates: animates
            )
        case .fullLine:
            plan = StreamingTextPacer.fullLineUpdatePlan(
                current: renderedText,
                target: source,
                animates: animates
            )
        }
        replaceRenderedText(plan.immediateText)
        pendingCharacters = plan.animatedCharacters
        pendingIndex = 0
        charactersPerTick = plan.charactersPerTick
        animationDuration = plan.animationDuration
        if pendingCharacters.isEmpty {
            scheduleImmediateCompletion(
                generation: generation,
                target: source
            )
        } else {
            startTypingIfNeeded()
        }
    }

    func heightThatFits(width: CGFloat) -> CGFloat {
        guard width.isFinite, width > 0, width < 100_000 else {
            return max(measuredHeight, minimumTextHeight)
        }
        updateTextContainerWidth(width)
        let height = measuredTextHeight()
        measuredWidth = width
        measuredHeight = height
        return height
    }

    private var minimumTextHeight: CGFloat {
        ceil(NSFont.systemFont(ofSize: fontSize).ascender)
            + ceil(abs(NSFont.systemFont(ofSize: fontSize).descender))
            + lineSpacing
    }

    private var typingAttributes: [NSAttributedString.Key: Any] {
        let paragraphStyle = NSMutableParagraphStyle()
        paragraphStyle.lineSpacing = lineSpacing
        return [
            .font: NSFont.systemFont(ofSize: fontSize),
            .foregroundColor: NSColor.labelColor,
            .paragraphStyle: paragraphStyle,
        ]
    }

    private func configureTextView() {
        wantsLayer = true
        textView.drawsBackground = false
        textView.backgroundColor = .clear
        textView.isEditable = false
        textView.isSelectable = true
        textView.isRichText = false
        textView.allowsUndo = false
        textView.textContainerInset = .zero
        textView.textContainer?.lineFragmentPadding = 0
        textView.textContainer?.widthTracksTextView = true
        textView.textContainer?.heightTracksTextView = false
        textView.isHorizontallyResizable = false
        textView.isVerticallyResizable = true
        textView.minSize = .zero
        textView.maxSize = NSSize(
            width: CGFloat.greatestFiniteMagnitude,
            height: CGFloat.greatestFiniteMagnitude
        )
        addSubview(textView)
    }

    private func replaceRenderedText(_ text: String) {
        renderedText = text
        textView.textStorage?.setAttributedString(
            NSAttributedString(
                string: text,
                attributes: typingAttributes
            )
        )
        updateMeasuredHeight(force: true)
    }

    private func startTypingIfNeeded() {
        guard
            window != nil,
            typingTimer == nil,
            pausedLiveScrollView == nil,
            pendingIndex < pendingCharacters.count
        else {
            return
        }

        if animationStartedAt == nil {
            animationStartedAt = ProcessInfo.processInfo.systemUptime
        }

        let timer = Timer(
            timeInterval: StreamingTextPacer.animationTickInterval,
            repeats: true
        ) { [weak self] _ in
            self?.appendNextCharacter()
        }
        // live-scroll의 event-tracking 중에는 타이핑을 잠시 멈춘다. 강한 휠
        // 입력과 60Hz 텍스트 갱신이 같은 SwiftUI 레이아웃을 경쟁하지 않는다.
        RunLoop.main.add(timer, forMode: .default)
        typingTimer = timer
    }

    private func stopTyping(resetTiming: Bool = false) {
        typingTimer?.invalidate()
        typingTimer = nil
        if resetTiming {
            animationStartedAt = nil
            animationDuration = nil
        }
    }

    private func observeLiveScroll() {
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(liveScrollDidBeginOrUpdate(_:)),
            name: NSScrollView.willStartLiveScrollNotification,
            object: nil
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(liveScrollDidBeginOrUpdate(_:)),
            name: NSScrollView.didLiveScrollNotification,
            object: nil
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(liveScrollDidEnd(_:)),
            name: NSScrollView.didEndLiveScrollNotification,
            object: nil
        )
    }

    @objc private func liveScrollDidBeginOrUpdate(
        _ notification: Notification
    ) {
        // 직원 전환이 willStart 알림 뒤에 끝나면 새 텍스트 뷰는 그 알림을
        // 받지 못한다. 이어지는 didLiveScroll에서도 상태를 복구해야 전환
        // 직후 짧은 휠 입력과 타자 tick이 같은 레이아웃을 동시에 바꾸지 않는다.
        guard
            pausedLiveScrollView == nil,
            pendingIndex < pendingCharacters.count,
            let scrollView = notification.object as? NSScrollView,
            belongs(to: scrollView)
        else {
            return
        }
        pausedLiveScrollView = scrollView
        liveScrollPausedAt = ProcessInfo.processInfo.systemUptime
        stopTyping()
    }

    @objc private func liveScrollDidEnd(_ notification: Notification) {
        guard
            let scrollView = notification.object as? NSScrollView,
            pausedLiveScrollView === scrollView
        else {
            return
        }
        if
            let pausedAt = liveScrollPausedAt,
            let startedAt = animationStartedAt
        {
            let pausedDuration = max(
                0,
                ProcessInfo.processInfo.systemUptime - pausedAt
            )
            animationStartedAt = startedAt + pausedDuration
        }
        pausedLiveScrollView = nil
        liveScrollPausedAt = nil
        startTypingIfNeeded()
    }

    private func belongs(to scrollView: NSScrollView) -> Bool {
        guard let documentView = scrollView.documentView else {
            return false
        }
        return self === documentView || isDescendant(of: documentView)
    }

    private func appendNextCharacter() {
        guard pendingIndex < pendingCharacters.count else {
            pendingCharacters = []
            pendingIndex = 0
            stopTyping(resetTiming: true)
            return
        }

        var endIndex = min(
            pendingCharacters.count,
            pendingIndex + charactersPerTick
        )
        if
            let animationStartedAt,
            let animationDuration,
            animationDuration > 0
        {
            let elapsed = max(
                0,
                ProcessInfo.processInfo.systemUptime - animationStartedAt
            )
            endIndex = StreamingTextPacer.elapsedRevealCharacterCount(
                totalCharacterCount: pendingCharacters.count,
                minimumCharacterCount: endIndex,
                elapsed: elapsed,
                duration: animationDuration
            )
        }
        let text = String(pendingCharacters[pendingIndex..<endIndex])
        pendingIndex = endIndex
        renderedText.append(text)
        textView.textStorage?.append(
            NSAttributedString(
                string: text,
                attributes: typingAttributes
            )
        )
        textView.needsDisplay = true
        // 완성 응답의 한 줄은 SwiftUI 쪽에서 최종 Markdown 높이를 먼저
        // 예약한다. 매 tick 고유 높이를 다시 계산하면 외부 ScrollView의
        // LazyVStack 측정까지 연쇄적으로 무효화된다.
        if revealMode == .trailingCharacters {
            updateMeasuredHeight()
        }

        if pendingIndex >= pendingCharacters.count {
            pendingCharacters = []
            pendingIndex = 0
            stopTyping(resetTiming: true)
            finishCurrentUpdate()
        }
    }

    private func scheduleImmediateCompletion(
        generation: Int,
        target: String
    ) {
        Task { @MainActor [weak self] in
            await Task.yield()
            guard
                let self,
                self.updateGeneration == generation,
                self.targetText == target,
                self.pendingCharacters.isEmpty,
                self.renderedText == target
            else {
                return
            }
            self.finishCurrentUpdate()
        }
    }

    private func finishCurrentUpdate() {
        guard finishedGeneration != updateGeneration else {
            return
        }
        finishedGeneration = updateGeneration
        onFinishedTyping()
    }

    private func updateTextContainerWidth(_ width: CGFloat) {
        guard abs(measuredWidth - width) > 0.5 else {
            return
        }
        measuredWidth = width
        textView.frame.size.width = width
        textView.textContainer?.containerSize = NSSize(
            width: width,
            height: CGFloat.greatestFiniteMagnitude
        )
    }

    private func measuredTextHeight() -> CGFloat {
        guard
            let layoutManager = textView.layoutManager,
            let textContainer = textView.textContainer
        else {
            return minimumTextHeight
        }
        layoutManager.ensureLayout(for: textContainer)
        return max(
            minimumTextHeight,
            ceil(layoutManager.usedRect(for: textContainer).height)
        )
    }

    private func updateMeasuredHeight(force: Bool = false) {
        guard measuredWidth > 0 else {
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
