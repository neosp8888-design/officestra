import AppKit
import CoreVideo
import ImageIO
import UniformTypeIdentifiers
import Vision

struct CharacterSpec {
    let id: String
    let name: String
    let role: String
    let rect: CGRect
}

let specs: [CharacterSpec] = [
    CharacterSpec(id: "boss", name: "백부장", role: "보스 (상단)", rect: CGRect(x: 680, y: 140, width: 180, height: 210)),
    CharacterSpec(id: "left-man", name: "클대리", role: "좌측 하단", rect: CGRect(x: 240, y: 450, width: 200, height: 240)),
    CharacterSpec(id: "left-woman", name: "로과장", role: "좌측 상단", rect: CGRect(x: 395, y: 390, width: 190, height: 240)),
    CharacterSpec(id: "right-woman", name: "코대리", role: "우측 상단", rect: CGRect(x: 1020, y: 400, width: 180, height: 250)),
    CharacterSpec(id: "right-man", name: "안과장", role: "우측 하단", rect: CGRect(x: 1155, y: 475, width: 205, height: 250))
]

let fileManager = FileManager.default
let projectURL = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
let sourceImageURL = projectURL.appendingPathComponent("Sources/OfficeCore/Resources/office-2d-themes-v1/modernDay.png")
let outputDirURL = projectURL.appendingPathComponent("artifacts/krea-character-cutouts")

try? fileManager.createDirectory(at: outputDirURL, withIntermediateDirectories: true)

guard let imageSource = CGImageSourceCreateWithURL(sourceImageURL as CFURL, nil),
      let fullImage = CGImageSourceCreateImageAtIndex(imageSource, 0, nil) else {
    fputs("원본 이미지 로드 실패: \(sourceImageURL.path)\n", stderr)
    exit(1)
}

print("전체 캔버스: \(fullImage.width)x\(fullImage.height)")

for spec in specs {
    // 1. 원본에서 캐릭터 영역 크롭
    guard let cropped = fullImage.cropping(to: spec.rect) else {
        fputs("\(spec.id) 크롭 실패\n", stderr)
        continue
    }

    let cropWidth = cropped.width
    let cropHeight = cropped.height

    // 2. Vision 프레임워크로 사람 누끼(세그멘테이션) 마스크 생성
    let request = VNGeneratePersonSegmentationRequest()
    request.qualityLevel = .accurate
    request.outputPixelFormat = kCVPixelFormatType_OneComponent8

    let handler = VNImageRequestHandler(cgImage: cropped, options: [:])
    try handler.perform([request])

    guard let maskBuffer = request.results?.first?.pixelBuffer else {
        fputs("\(spec.id) Vision 마스크 생성 실패\n", stderr)
        continue
    }

    CVPixelBufferLockBaseAddress(maskBuffer, .readOnly)
    let maskWidth = CVPixelBufferGetWidth(maskBuffer)
    let maskHeight = CVPixelBufferGetHeight(maskBuffer)
    let maskBytesPerRow = CVPixelBufferGetBytesPerRow(maskBuffer)
    guard let maskBase = CVPixelBufferGetBaseAddress(maskBuffer) else {
        CVPixelBufferUnlockBaseAddress(maskBuffer, .readOnly)
        continue
    }

    let maskBytes = maskBase.assumingMemoryBound(to: UInt8.self)

    // 3. RGBA 비트맵 컨텍스트 생성 후 마스크 알파 적용
    let colorSpace = CGColorSpaceCreateDeviceRGB()
    let bytesPerPixel = 4
    let bytesPerRow = cropWidth * bytesPerPixel
    var rawData = [UInt8](repeating: 0, count: cropHeight * bytesPerRow)

    guard let context = CGContext(
        data: &rawData,
        width: cropWidth,
        height: cropHeight,
        bitsPerComponent: 8,
        bytesPerRow: bytesPerRow,
        space: colorSpace,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else {
        CVPixelBufferUnlockBaseAddress(maskBuffer, .readOnly)
        continue
    }

    context.draw(cropped, in: CGRect(x: 0, y: 0, width: cropWidth, height: cropHeight))

    // CGContext 좌표계(아래가 0)와 Vision/이미지 좌표계(위가 0) 보정
    for y in 0..<cropHeight {
        // CGContext는 아래부터 0이므로 y 뒤집기
        let srcY = cropHeight - 1 - y
        let maskY = Int(Double(srcY) / Double(cropHeight) * Double(maskHeight))

        for x in 0..<cropWidth {
            let maskX = Int(Double(x) / Double(cropWidth) * Double(maskWidth))
            let maskVal = maskBytes[maskY * maskBytesPerRow + maskX]

            // 마스크 임계치 및 페더링
            let alphaFactor: Double
            if maskVal < 30 {
                alphaFactor = 0.0
            } else if maskVal > 150 {
                alphaFactor = 1.0
            } else {
                alphaFactor = Double(maskVal - 30) / 120.0
            }

            let pixelIndex = y * bytesPerRow + x * bytesPerPixel
            let r = Double(rawData[pixelIndex]) * alphaFactor
            let g = Double(rawData[pixelIndex + 1]) * alphaFactor
            let b = Double(rawData[pixelIndex + 2]) * alphaFactor
            let a = Double(rawData[pixelIndex + 3]) * alphaFactor

            rawData[pixelIndex] = UInt8(clamping: Int(r))
            rawData[pixelIndex + 1] = UInt8(clamping: Int(g))
            rawData[pixelIndex + 2] = UInt8(clamping: Int(b))
            rawData[pixelIndex + 3] = UInt8(clamping: Int(a))
        }
    }

    CVPixelBufferUnlockBaseAddress(maskBuffer, .readOnly)

    // 원본 크롭(누끼 적용)과 마스크 없는 원본 크롭 두 가지 모두 저장
    guard let maskedImage = context.makeImage() else { continue }

    // 파일 저장
    let transparentURL = outputDirURL.appendingPathComponent("\(spec.id)-transparent.png")
    let rawCropURL = outputDirURL.appendingPathComponent("\(spec.id)-desk-crop.png")

    func savePNG(image: CGImage, to url: URL) {
        guard let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else { return }
        CGImageDestinationAddImage(dest, image, nil)
        CGImageDestinationFinalize(dest)
    }

    savePNG(image: maskedImage, to: transparentURL)
    savePNG(image: cropped, to: rawCropURL)

    print("생성 완료: [\(spec.name)(\(spec.id))] -> \(cropWidth)x\(cropHeight) (투명 누끼 및 배경 크롭)")
}

print("전체 추출 성공: \(outputDirURL.path)")
