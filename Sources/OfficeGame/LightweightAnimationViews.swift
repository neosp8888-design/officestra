// 이 파일은 업무 상태의 반복 모션을 Core Animation으로 그려 SwiftUI 레이아웃 갱신을 막는다.

import AppKit
import QuartzCore
import SwiftUI

struct CoreAnimationFocusLine: NSViewRepresentable {
    let isAnimated: Bool

    func makeNSView(context: Context) -> CoreAnimationFocusLineNSView {
        let view = CoreAnimationFocusLineNSView()
        view.setAnimated(isAnimated)
        return view
    }

    func updateNSView(_ view: CoreAnimationFocusLineNSView, context: Context) {
        view.setAnimated(isAnimated)
    }

    static func dismantleNSView(_ view: CoreAnimationFocusLineNSView, coordinator: ()) {
        view.setAnimated(false)
    }
}

/// Only the two-pixel indicator animates; no timer or SwiftUI state drives frames.
final class CoreAnimationFocusLineNSView: NSView {
    private let track = CAGradientLayer()
    private let shimmer = CAGradientLayer()
    private var isAnimated = true
    private var laidOutSize = CGSize.zero
    private(set) var animationStarts = 0
    var isShimmering: Bool { shimmer.animation(forKey: "officestra.focusFlow") != nil }

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.masksToBounds = true
        let accent = NSColor(DashboardPalette.accent)
        track.colors = [accent.withAlphaComponent(0.65).cgColor,
                        NSColor.systemCyan.withAlphaComponent(0.75).cgColor,
                        NSColor.systemPurple.withAlphaComponent(0.55).cgColor,
                        accent.withAlphaComponent(0.65).cgColor]
        shimmer.colors = [NSColor.clear.cgColor, NSColor.systemCyan.withAlphaComponent(0.8).cgColor,
                          NSColor.white.cgColor, NSColor.systemCyan.withAlphaComponent(0.8).cgColor,
                          NSColor.clear.cgColor]
        for gradient in [track, shimmer] {
            gradient.startPoint = CGPoint(x: 0, y: 0.5)
            gradient.endPoint = CGPoint(x: 1, y: 0.5)
            layer?.addSublayer(gradient)
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }
    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    override func layout() {
        super.layout()
        guard laidOutSize != bounds.size else { return }
        laidOutSize = bounds.size
        let bandWidth = min(160, max(48, bounds.width * 0.23))
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        track.frame = bounds
        shimmer.frame = CGRect(x: -bandWidth, y: 0, width: bandWidth, height: bounds.height)
        CATransaction.commit()
        updateAnimation()
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        updateAnimation()
    }

    func setAnimated(_ animated: Bool) {
        guard isAnimated != animated else { return }
        isAnimated = animated
        updateAnimation()
    }

    private func updateAnimation() {
        shimmer.removeAllAnimations()
        guard isAnimated, window != nil, bounds.width > 0, shimmer.bounds.width > 0 else { return }
        let travel = CABasicAnimation(keyPath: "transform.translation.x")
        travel.fromValue = 0
        travel.toValue = bounds.width + shimmer.bounds.width
        let brightness = CAKeyframeAnimation(keyPath: "opacity")
        brightness.values = [0.25, 1, 0.25]
        brightness.keyTimes = [0, 0.5, 1]
        let flow = CAAnimationGroup()
        flow.duration = 2.4
        travel.duration = flow.duration
        brightness.duration = flow.duration
        flow.animations = [travel, brightness]
        flow.repeatCount = .infinity
        flow.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
        shimmer.add(flow, forKey: "officestra.focusFlow")
        animationStarts += 1
    }
}

struct CoreAnimationDotsView: NSViewRepresentable {
    let dotSize: CGFloat
    let spacing: CGFloat
    let travel: CGFloat
    let color: NSColor
    let isAnimated: Bool

    func makeNSView(context: Context) -> CoreAnimationDotsNSView {
        let view = CoreAnimationDotsNSView()
        view.configure(
            dotSize: dotSize,
            spacing: spacing,
            travel: travel,
            color: color,
            isAnimated: isAnimated
        )
        return view
    }

    func updateNSView(
        _ nsView: CoreAnimationDotsNSView,
        context: Context
    ) {
        nsView.configure(
            dotSize: dotSize,
            spacing: spacing,
            travel: travel,
            color: color,
            isAnimated: isAnimated
        )
    }
}

struct CoreAnimationRunningIndicator: NSViewRepresentable {
    let isAnimated: Bool

    func makeNSView(
        context: Context
    ) -> CoreAnimationRunningIndicatorNSView {
        let view = CoreAnimationRunningIndicatorNSView()
        view.setAnimated(isAnimated)
        return view
    }

    func updateNSView(
        _ nsView: CoreAnimationRunningIndicatorNSView,
        context: Context
    ) {
        nsView.setAnimated(isAnimated)
    }
}

struct CoreAnimationSelectionHighlight: NSViewRepresentable {
    let selectedIndex: Int?
    let itemCount: Int
    let spacing: CGFloat
    let reduceMotion: Bool

    func makeNSView(context: Context) -> CoreAnimationSelectionHighlightNSView {
        let view = CoreAnimationSelectionHighlightNSView()
        view.configure(
            selectedIndex: selectedIndex,
            itemCount: itemCount,
            spacing: spacing,
            animated: false
        )
        return view
    }

    func updateNSView(
        _ nsView: CoreAnimationSelectionHighlightNSView,
        context: Context
    ) {
        nsView.configure(
            selectedIndex: selectedIndex,
            itemCount: itemCount,
            spacing: spacing,
            animated: !reduceMotion
        )
    }
}

enum CharacterSelectionHighlightGeometry {
    static func frame(
        in bounds: CGRect,
        selectedIndex: Int?,
        itemCount: Int,
        spacing: CGFloat
    ) -> CGRect? {
        guard
            let selectedIndex,
            itemCount > 0,
            selectedIndex >= 0,
            selectedIndex < itemCount,
            bounds.width > 0,
            bounds.height > 0
        else {
            return nil
        }

        let totalSpacing = spacing * CGFloat(itemCount - 1)
        let itemWidth = max(0, bounds.width - totalSpacing)
            / CGFloat(itemCount)
        return CGRect(
            x: CGFloat(selectedIndex) * (itemWidth + spacing),
            y: 0,
            width: itemWidth,
            height: bounds.height
        )
    }
}

final class CoreAnimationSelectionHighlightNSView: NSView {
    private let highlightLayer = CALayer()
    private var selectedIndex: Int?
    private var itemCount = 0
    private var spacing = CGFloat.zero
    private var hasPositionedHighlight = false

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.masksToBounds = false
        configureHighlightLayer()
        layer?.addSublayer(highlightLayer)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override func layout() {
        super.layout()
        updateHighlightGeometry(
            animated: false,
            preserveCurrentAnimation: true
        )
    }

    func configure(
        selectedIndex: Int?,
        itemCount: Int,
        spacing: CGFloat,
        animated: Bool
    ) {
        let selectionChanged = self.selectedIndex != selectedIndex
        let geometryChanged = self.itemCount != itemCount
            || self.spacing != spacing
        self.selectedIndex = selectedIndex
        self.itemCount = itemCount
        self.spacing = spacing

        if selectionChanged || geometryChanged {
            updateHighlightGeometry(
                animated: animated && selectionChanged,
                preserveCurrentAnimation: false
            )
        }
    }

    private func configureHighlightLayer() {
        highlightLayer.backgroundColor = NSColor.controlBackgroundColor
            .withAlphaComponent(0.94)
            .cgColor
        highlightLayer.borderColor = NSColor(
            calibratedRed: 0.13,
            green: 0.55,
            blue: 0.52,
            alpha: 0.18
        ).cgColor
        highlightLayer.borderWidth = 1
        highlightLayer.cornerRadius = 10
        highlightLayer.cornerCurve = .continuous
        highlightLayer.shadowColor = NSColor.black.cgColor
        highlightLayer.shadowOpacity = 0.10
        highlightLayer.shadowRadius = 6
        highlightLayer.shadowOffset = CGSize(width: 0, height: -2)
    }

    private func updateHighlightGeometry(
        animated: Bool,
        preserveCurrentAnimation: Bool
    ) {
        guard
            let targetFrame = CharacterSelectionHighlightGeometry.frame(
                in: bounds,
                selectedIndex: selectedIndex,
                itemCount: itemCount,
                spacing: spacing
            )
        else {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            highlightLayer.isHidden = true
            CATransaction.commit()
            hasPositionedHighlight = false
            return
        }

        let previousPositionX = highlightLayer.presentation()?.position.x
            ?? highlightLayer.position.x
        let targetPosition = CGPoint(
            x: targetFrame.midX,
            y: targetFrame.midY
        )

        CATransaction.begin()
        CATransaction.setDisableActions(true)
        highlightLayer.isHidden = false
        highlightLayer.bounds = CGRect(
            origin: .zero,
            size: targetFrame.size
        )
        highlightLayer.position = targetPosition
        highlightLayer.shadowPath = CGPath(
            roundedRect: CGRect(origin: .zero, size: targetFrame.size),
            cornerWidth: 10,
            cornerHeight: 10,
            transform: nil
        )
        CATransaction.commit()

        guard
            animated,
            hasPositionedHighlight,
            abs(previousPositionX - targetPosition.x) > 0.5
        else {
            if !preserveCurrentAnimation {
                highlightLayer.removeAnimation(
                    forKey: "officestra.selection"
                )
            }
            hasPositionedHighlight = true
            return
        }

        let spring = CASpringAnimation(keyPath: "position.x")
        spring.fromValue = previousPositionX
        spring.toValue = targetPosition.x
        spring.mass = 1
        spring.stiffness = 500
        spring.damping = 38
        spring.initialVelocity = 0
        spring.duration = spring.settlingDuration
        highlightLayer.add(spring, forKey: "officestra.selection")
        hasPositionedHighlight = true
    }
}

final class CoreAnimationDotsNSView: NSView {
    private let dotLayers = (0..<3).map { _ in CAShapeLayer() }
    private var dotSize = CGFloat(4)
    private var spacing = CGFloat(2.5)
    private var travel = CGFloat(2.5)
    private var dotColor = NSColor(
        calibratedRed: 0.13,
        green: 0.55,
        blue: 0.52,
        alpha: 1
    )
    private var isAnimated = true

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.masksToBounds = false
        for dotLayer in dotLayers {
            layer?.addSublayer(dotLayer)
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override func layout() {
        super.layout()
        let totalWidth =
            dotSize * CGFloat(dotLayers.count)
            + spacing * CGFloat(dotLayers.count - 1)
        let startX = (bounds.width - totalWidth) / 2
        let y = (bounds.height - dotSize) / 2

        CATransaction.begin()
        CATransaction.setDisableActions(true)
        for (index, dotLayer) in dotLayers.enumerated() {
            dotLayer.frame = CGRect(
                x: startX + CGFloat(index) * (dotSize + spacing),
                y: y,
                width: dotSize,
                height: dotSize
            )
            dotLayer.path = CGPath(
                ellipseIn: CGRect(
                    origin: .zero,
                    size: CGSize(width: dotSize, height: dotSize)
                ),
                transform: nil
            )
        }
        CATransaction.commit()
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        updateAnimations()
    }

    func configure(
        dotSize: CGFloat,
        spacing: CGFloat,
        travel: CGFloat,
        color: NSColor,
        isAnimated: Bool
    ) {
        let geometryChanged =
            self.dotSize != dotSize
                || self.spacing != spacing
                || self.travel != travel
        let animationChanged = self.isAnimated != isAnimated
        self.dotSize = dotSize
        self.spacing = spacing
        self.travel = travel
        self.dotColor = color
        self.isAnimated = isAnimated

        CATransaction.begin()
        CATransaction.setDisableActions(true)
        for dotLayer in dotLayers {
            dotLayer.fillColor = color.cgColor
        }
        CATransaction.commit()

        if geometryChanged {
            needsLayout = true
        }
        if geometryChanged || animationChanged {
            updateAnimations()
        }
    }

    private func updateAnimations() {
        for dotLayer in dotLayers {
            dotLayer.removeAnimation(forKey: "officestra.dots")
            dotLayer.opacity = 1
            dotLayer.transform = CATransform3DIdentity
        }
        guard isAnimated, window != nil else {
            return
        }

        let startTime = CACurrentMediaTime()
        for (index, dotLayer) in dotLayers.enumerated() {
            let bounce = CAKeyframeAnimation(
                keyPath: "transform.translation.y"
            )
            bounce.values = [0, travel, 0, 0]
            bounce.keyTimes = [0, 0.20, 0.42, 1]

            let opacity = CAKeyframeAnimation(keyPath: "opacity")
            opacity.values = [0.42, 1, 0.42, 0.42]
            opacity.keyTimes = bounce.keyTimes

            let group = CAAnimationGroup()
            group.animations = [bounce, opacity]
            group.duration = 0.84
            group.beginTime = startTime + Double(index) * 0.16
            group.repeatCount = .infinity
            group.isRemovedOnCompletion = false
            dotLayer.add(group, forKey: "officestra.dots")
        }
    }
}

final class CoreAnimationRunningIndicatorNSView: NSView {
    private let ringLayer = CAShapeLayer()
    private let dotLayer = CAShapeLayer()
    private let runningColor = NSColor(
        calibratedRed: 0.18,
        green: 0.73,
        blue: 0.42,
        alpha: 1
    )
    private var isAnimated = true

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.masksToBounds = false
        configureLayers()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override func layout() {
        super.layout()
        let center = CGPoint(x: bounds.midX, y: bounds.midY)
        update(
            ringLayer,
            size: 18,
            center: center
        )
        update(
            dotLayer,
            size: 12,
            center: center
        )
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        updateAnimations()
    }

    func setAnimated(_ isAnimated: Bool) {
        guard self.isAnimated != isAnimated else {
            return
        }
        self.isAnimated = isAnimated
        updateAnimations()
    }

    private func configureLayers() {
        ringLayer.fillColor = NSColor.clear.cgColor
        ringLayer.strokeColor = runningColor.withAlphaComponent(0.58).cgColor
        ringLayer.lineWidth = 1.6
        dotLayer.fillColor = runningColor.cgColor
        dotLayer.shadowColor = runningColor.withAlphaComponent(0.34).cgColor
        dotLayer.shadowRadius = 4
        dotLayer.shadowOffset = CGSize(width: 0, height: -2)
        dotLayer.shadowOpacity = 1
        layer?.addSublayer(ringLayer)
        layer?.addSublayer(dotLayer)
    }

    private func update(
        _ shapeLayer: CAShapeLayer,
        size: CGFloat,
        center: CGPoint
    ) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        shapeLayer.bounds = CGRect(
            origin: .zero,
            size: CGSize(width: size, height: size)
        )
        shapeLayer.position = center
        shapeLayer.path = CGPath(
            ellipseIn: shapeLayer.bounds,
            transform: nil
        )
        CATransaction.commit()
    }

    private func updateAnimations() {
        ringLayer.removeAllAnimations()
        dotLayer.removeAllAnimations()
        ringLayer.opacity = 1
        dotLayer.opacity = 1
        ringLayer.transform = CATransform3DIdentity
        dotLayer.transform = CATransform3DIdentity
        guard isAnimated, window != nil else {
            return
        }

        addPulse(
            to: ringLayer,
            fromScale: 0.76,
            toScale: 1.26,
            fromOpacity: 0.96,
            toOpacity: 0.10
        )
        addPulse(
            to: dotLayer,
            fromScale: 0.84,
            toScale: 1,
            fromOpacity: 1,
            toOpacity: 0.72
        )
    }

    private func addPulse(
        to shapeLayer: CAShapeLayer,
        fromScale: CGFloat,
        toScale: CGFloat,
        fromOpacity: Float,
        toOpacity: Float
    ) {
        let scale = CABasicAnimation(keyPath: "transform.scale")
        scale.fromValue = fromScale
        scale.toValue = toScale

        let opacity = CABasicAnimation(keyPath: "opacity")
        opacity.fromValue = fromOpacity
        opacity.toValue = toOpacity

        let group = CAAnimationGroup()
        group.animations = [scale, opacity]
        group.duration = 0.62
        group.autoreverses = true
        group.repeatCount = .infinity
        group.isRemovedOnCompletion = false
        group.timingFunction = CAMediaTimingFunction(
            name: .easeInEaseOut
        )
        shapeLayer.add(group, forKey: "officestra.pulse")
    }
}
