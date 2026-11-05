// 이 파일은 2D·3D 오피스 캔버스의 비율과 상호작용 좌표를 공통으로 제공한다.

import CoreGraphics

public enum OfficeCanvasGeometry {
    public static let designSize = CGSize(width: 1_536, height: 1_024)

    public static func fittedFrame(in container: CGSize) -> CGRect {
        guard container.width > 0, container.height > 0 else {
            return .zero
        }

        let scale = min(
            container.width / designSize.width,
            container.height / designSize.height
        )
        let fittedSize = CGSize(
            width: designSize.width * scale,
            height: designSize.height * scale
        )
        return CGRect(
            x: (container.width - fittedSize.width) / 2,
            y: (container.height - fittedSize.height) / 2,
            width: fittedSize.width,
            height: fittedSize.height
        )
    }
}

public enum OfficeInteractionGeometry {
    public static func faceBounds(
        for character: OfficeCharacter,
        artStyle: OfficeArtStyle
    ) -> CGRect? {
        guard character == .boss, artStyle == .twoD else {
            return nil
        }

        return CGRect(x: 758, y: 142, width: 68, height: 72)
    }

    public static func characterHitbox(
        for character: OfficeCharacter,
        artStyle: OfficeArtStyle,
        fallback: CGRect
    ) -> CGRect {
        guard artStyle == .twoD else {
            return fallback
        }

        switch character {
        case .boss:
            return CGRect(x: 720, y: 135, width: 155, height: 205)
        case .leftMan:
            return CGRect(x: 275, y: 485, width: 150, height: 205)
        case .leftWoman:
            return CGRect(x: 445, y: 425, width: 135, height: 180)
        case .rightWoman:
            return CGRect(x: 1_075, y: 425, width: 135, height: 190)
        case .rightMan:
            return CGRect(x: 1_205, y: 510, width: 145, height: 205)
        }
    }

    public static func monitorHitbox(
        for character: OfficeCharacter,
        artStyle: OfficeArtStyle,
        fallback: CGRect
    ) -> CGRect {
        guard artStyle == .twoD else {
            return fallback
        }

        switch character {
        case .boss:
            return CGRect(x: 790, y: 250, width: 105, height: 105)
        case .leftMan:
            return CGRect(x: 365, y: 600, width: 105, height: 115)
        case .leftWoman:
            return CGRect(x: 525, y: 505, width: 90, height: 110)
        case .rightWoman:
            return CGRect(x: 1_015, y: 505, width: 95, height: 115)
        case .rightMan:
            return CGRect(x: 1_165, y: 615, width: 95, height: 115)
        }
    }

    public static func bubbleAnchor(
        for character: OfficeCharacter,
        artStyle: OfficeArtStyle,
        fallback: CGPoint
    ) -> CGPoint {
        guard artStyle == .twoD else {
            return fallback
        }

        switch character {
        case .boss:
            return CGPoint(x: 900, y: 165)
        case .leftMan:
            return CGPoint(x: 340, y: 430)
        case .leftWoman:
            return CGPoint(x: 505, y: 380)
        case .rightWoman:
            return CGPoint(x: 1_125, y: 380)
        case .rightMan:
            return CGPoint(x: 1_275, y: 465)
        }
    }

    public static func archiveCabinetHitbox(
        artStyle: OfficeArtStyle,
        fallback: CGRect
    ) -> CGRect {
        artStyle == .twoD
            ? CGRect(x: 590, y: 95, width: 140, height: 290)
            : fallback
    }

    /// 바닥 화분 두 개(화이트보드 아래, 오른쪽 벽 큰 화분). 숨긴 기능 암호창을 여는 자리다.
    public static func plantHitboxes(artStyle: OfficeArtStyle) -> [CGRect] {
        switch artStyle {
        case .twoD:
            return [
                CGRect(x: 135, y: 565, width: 110, height: 145),
                CGRect(x: 1_240, y: 372, width: 150, height: 280)
            ]
        case .threeD:
            return [
                CGRect(x: 140, y: 565, width: 105, height: 135),
                CGRect(x: 1_243, y: 388, width: 125, height: 225)
            ]
        }
    }
}

public enum OfficeWhiteboardGeometry {
    public static let usageOrigin = CGPoint(x: 200, y: 415)
    public static let usageSize = CGSize(width: 128, height: 78)
    public static let horizontalShear: CGFloat = -70.0 / 128.0
    public static let twoDUsageCorners = (
        topLeft: CGPoint(x: 194, y: 423),
        topRight: CGPoint(x: 326, y: 339),
        bottomRight: CGPoint(x: 326, y: 447),
        bottomLeft: CGPoint(x: 194, y: 531)
    )
    public static let interactionRect = CGRect(
        x: 188,
        y: 338,
        width: 158,
        height: 170
    )

    public static func usageOrigin(
        for artStyle: OfficeArtStyle
    ) -> CGPoint {
        artStyle == .twoD
            ? twoDUsageCorners.topLeft
            : usageOrigin
    }

    public static func horizontalShear(
        for artStyle: OfficeArtStyle
    ) -> CGFloat {
        artStyle == .twoD ? -69.0 / 132.0 : horizontalShear
    }

    public static func interactionRect(
        for artStyle: OfficeArtStyle
    ) -> CGRect {
        artStyle == .twoD
            ? CGRect(x: 168, y: 300, width: 180, height: 250)
            : interactionRect
    }

    public static func usagePoint(
        for artStyle: OfficeArtStyle,
        x: CGFloat,
        y: CGFloat
    ) -> CGPoint {
        if artStyle == .twoD {
            let horizontal = x / usageSize.width
            let vertical = y / usageSize.height
            let top = interpolatedPoint(
                from: twoDUsageCorners.topLeft,
                to: twoDUsageCorners.topRight,
                amount: horizontal
            )
            let bottom = interpolatedPoint(
                from: twoDUsageCorners.bottomLeft,
                to: twoDUsageCorners.bottomRight,
                amount: horizontal
            )
            return interpolatedPoint(
                from: top,
                to: bottom,
                amount: vertical
            )
        }

        let origin = usageOrigin(for: artStyle)
        let shear = horizontalShear(for: artStyle)
        return CGPoint(
            x: origin.x + x,
            y: origin.y + shear * x + y
        )
    }

    public static func usageTransform(
        for artStyle: OfficeArtStyle,
        at point: CGPoint
    ) -> CGAffineTransform {
        let origin = usagePoint(
            for: artStyle,
            x: point.x,
            y: point.y
        )
        let xNeighbor = usagePoint(
            for: artStyle,
            x: point.x + 1,
            y: point.y
        )
        let yNeighbor = usagePoint(
            for: artStyle,
            x: point.x,
            y: point.y + 1
        )
        return CGAffineTransform(
            a: xNeighbor.x - origin.x,
            b: xNeighbor.y - origin.y,
            c: yNeighbor.x - origin.x,
            d: yNeighbor.y - origin.y,
            tx: origin.x,
            ty: origin.y
        )
    }

    public static func usagePoint(
        x: CGFloat,
        y: CGFloat
    ) -> CGPoint {
        CGPoint(
            x: usageOrigin.x + x,
            y: usageOrigin.y + horizontalShear * x + y
        )
    }

    private static func interpolatedPoint(
        from start: CGPoint,
        to end: CGPoint,
        amount: CGFloat
    ) -> CGPoint {
        CGPoint(
            x: start.x + (end.x - start.x) * amount,
            y: start.y + (end.y - start.y) * amount
        )
    }
}

public enum OfficeAnalogClockGeometry {
    // SpriteKit 좌표계에서 2D 벽시계의 실제 안쪽 타원을 따른다.
    public static let twoDCenter = CGPoint(x: 1_194, y: 687)
    public static let twoDHorizontalScale: CGFloat = 1
    public static let twoDVerticalScale: CGFloat = 1.35
    public static let twoDRotation: CGFloat = 0
}
