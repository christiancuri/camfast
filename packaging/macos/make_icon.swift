// Generates icon-1024.png: dark rounded square with a 2x2 camera-tile grid.
// Deterministic; run with `swift packaging/macos/make_icon.swift [output.png]`.
import AppKit

let size: CGFloat = 1024
let output = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("icon-1024.png").path

guard let ctx = CGContext(
    data: nil, width: Int(size), height: Int(size), bitsPerComponent: 8, bytesPerRow: 0,
    space: CGColorSpace(name: CGColorSpace.sRGB)!,
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
) else { fatalError("CGContext") }

func rgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: CGColorSpace(name: CGColorSpace.sRGB)!, components: [r, g, b, a])!
}

// Background: Apple-style inset rounded square (transparent margins), dark gradient.
let inset: CGFloat = 100
let bg = CGRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let bgPath = CGPath(roundedRect: bg, cornerWidth: 184, cornerHeight: 184, transform: nil)

ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -12), blur: 40, color: rgb(0, 0, 0, 0.45))
ctx.addPath(bgPath)
ctx.setFillColor(rgb(0.09, 0.10, 0.13))
ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(bgPath)
ctx.clip()
let gradient = CGGradient(
    colorsSpace: CGColorSpace(name: CGColorSpace.sRGB)!,
    colors: [rgb(0.16, 0.18, 0.24), rgb(0.07, 0.08, 0.11)] as CFArray,
    locations: [0, 1]
)!
ctx.drawLinearGradient(gradient, start: CGPoint(x: 0, y: size), end: CGPoint(x: 0, y: 0), options: [])
ctx.restoreGState()

// 2x2 grid of camera tiles.
let gap: CGFloat = 40
let pad: CGFloat = 150
let tile = (bg.width - 2 * pad - gap) / 2
let tileColors: [CGColor] = [
    rgb(0.24, 0.55, 0.96), rgb(0.35, 0.62, 0.95),
    rgb(0.30, 0.58, 0.95), rgb(0.20, 0.50, 0.92),
]
var index = 0
for row in 0..<2 {
    for col in 0..<2 {
        let x = bg.minX + pad + CGFloat(col) * (tile + gap)
        let y = bg.maxY - pad - tile - CGFloat(row) * (tile + gap)
        let rect = CGRect(x: x, y: y, width: tile, height: tile)
        let path = CGPath(roundedRect: rect, cornerWidth: 56, cornerHeight: 56, transform: nil)
        ctx.addPath(path)
        ctx.setFillColor(tileColors[index])
        ctx.fillPath()

        // Lens: dark ring + light center.
        let lens: CGFloat = tile * 0.36
        let lensRect = CGRect(x: rect.midX - lens / 2, y: rect.midY - lens / 2, width: lens, height: lens)
        ctx.setFillColor(rgb(0.06, 0.08, 0.12))
        ctx.fillEllipse(in: lensRect)
        let core = lensRect.insetBy(dx: lens * 0.22, dy: lens * 0.22)
        ctx.setFillColor(rgb(0.55, 0.75, 1.0))
        ctx.fillEllipse(in: core)
        let glint = CGRect(x: core.minX + core.width * 0.18, y: core.maxY - core.height * 0.42,
                           width: core.width * 0.22, height: core.height * 0.22)
        ctx.setFillColor(rgb(1, 1, 1, 0.9))
        ctx.fillEllipse(in: glint)
        index += 1
    }
}

// "Live" dot on the first tile.
let dot: CGFloat = 44
let firstX = bg.minX + pad
let firstY = bg.maxY - pad - tile
ctx.setFillColor(rgb(0.95, 0.25, 0.25))
ctx.fillEllipse(in: CGRect(x: firstX + tile - dot - 28, y: firstY + tile - dot - 28, width: dot, height: dot))

guard let image = ctx.makeImage() else { fatalError("makeImage") }
let rep = NSBitmapImageRep(cgImage: image)
guard let png = rep.representation(using: .png, properties: [:]) else { fatalError("png") }
try! png.write(to: URL(fileURLWithPath: output))
print("wrote \(output)")
