import AppKit
import CoreGraphics
import UniformTypeIdentifiers

let fileManager = FileManager.default
let currentDir = URL(fileURLWithPath: fileManager.currentDirectoryPath)
let inputDir = currentDir.appendingPathComponent("artifacts/krea-character-cutouts")
let output512Dir = inputDir.appendingPathComponent("krea_input_512")

try? fileManager.createDirectory(at: output512Dir, withIntermediateDirectories: true)

let characters = [
    ("boss", "boss-desk-crop.png"),
    ("left-man", "left-man-desk-crop.png"),
    ("left-woman", "left-woman-desk-crop.png"),
    ("right-woman", "right-woman-desk-crop.png"),
    ("right-man", "right-man-desk-crop.png")
]

for (id, filename) in characters {
    let sourceURL = inputDir.appendingPathComponent(filename)
    guard let img = NSImage(contentsOf: sourceURL) else { continue }

    let targetSize = CGSize(width: 512, height: 512)
    let outputImage = NSImage(size: targetSize)

    outputImage.lockFocus()

    // 512x512 캔버스 중앙에 비율 유지 확대 (가로/세로 중 큰 쪽에 맞춰 여백 40px 남김)
    let srcW = img.size.width
    let srcH = img.size.height
    let scale = min((targetSize.width - 60) / srcW, (targetSize.height - 60) / srcH)
    let drawW = srcW * scale
    let drawH = srcH * scale
    let drawX = (targetSize.width - drawW) / 2.0
    let drawY = (targetSize.height - drawH) / 2.0

    NSGraphicsContext.current?.imageInterpolation = .high
    img.draw(in: CGRect(x: drawX, y: drawY, width: drawW, height: drawH))

    outputImage.unlockFocus()

    if let tiff = outputImage.tiffRepresentation,
       let bitmap = NSBitmapImageRep(data: tiff),
       let png = bitmap.representation(using: .png, properties: [:]) {
        let destURL = output512Dir.appendingPathComponent("\(id)-512.png")
        try? png.write(to: destURL)
        print("Krea 입력용 512x512 생성 완료: \(destURL.lastPathComponent)")
    }
}
