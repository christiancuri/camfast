// Renders an IP-camera style OSD (on-screen display) as a sequence of transparent PNGs,
// one per second of video: the date/time in the top-right corner and the camera name in
// the bottom-left corner, white text with a soft dark shadow like Dahua/Intelbras cameras.
//
// fetch.sh overlays the sequence onto the footage with ffmpeg's `overlay` filter. This is
// used instead of ffmpeg's `drawtext` because Homebrew's ffmpeg is built without libfreetype.
//
// Usage: osd <out_dir> <width> <height> <camera name> <start epoch (UTC)> <seconds>
// Writes <out_dir>/osd_0000.png ... osd_<seconds-1>.png.

import AppKit

let args = CommandLine.arguments
guard args.count == 7,
      let width = Int(args[2]), let height = Int(args[3]),
      let start = Int(args[5]), let seconds = Int(args[6]), seconds > 0
else {
    FileHandle.standardError.write("usage: osd <out_dir> <width> <height> <name> <start_epoch> <seconds>\n".data(using: .utf8)!)
    exit(2)
}
let outDir = URL(fileURLWithPath: args[1], isDirectory: true)
let name = args[4]

// Text size scales with the frame height (~38 px at 1080p, ~51 px at 1440p).
let fontSize = CGFloat(height) * 0.0355
let margin = CGFloat(height) * 0.03
let font = NSFont(name: "Arial", size: fontSize) ?? NSFont.systemFont(ofSize: fontSize)

let shadow = NSShadow()
shadow.shadowColor = NSColor.black.withAlphaComponent(0.85)
shadow.shadowOffset = NSSize(width: fontSize * 0.05, height: -fontSize * 0.05)
shadow.shadowBlurRadius = fontSize * 0.08

let attributes: [NSAttributedString.Key: Any] = [
    .font: font,
    .foregroundColor: NSColor.white,
    .shadow: shadow,
    // A thin dark outline keeps the text readable over bright sky or headlights.
    .strokeColor: NSColor.black.withAlphaComponent(0.55),
    .strokeWidth: -3.0, // negative = fill and stroke; percent of the font size
    .kern: fontSize * 0.02,
]

let formatter = DateFormatter()
formatter.locale = Locale(identifier: "en_US_POSIX")
formatter.timeZone = TimeZone(identifier: "UTC")
formatter.dateFormat = "dd-MM-yyyy EEE HH:mm:ss"

func render(second: Int) throws {
    guard let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)
    else { throw NSError(domain: "osd", code: 1) }
    rep.size = NSSize(width: width, height: height) // 1 point == 1 pixel

    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSGraphicsContext.current?.shouldAntialias = true

    // AppKit's origin is bottom-left.
    let stamp = NSAttributedString(
        string: formatter.string(from: Date(timeIntervalSince1970: TimeInterval(start + second))),
        attributes: attributes)
    let stampSize = stamp.size()
    stamp.draw(at: NSPoint(x: CGFloat(width) - margin - stampSize.width,
                           y: CGFloat(height) - margin - stampSize.height))

    let title = NSAttributedString(string: name, attributes: attributes)
    title.draw(at: NSPoint(x: margin, y: margin))

    NSGraphicsContext.restoreGraphicsState()

    guard let png = rep.representation(using: .png, properties: [:]) else {
        throw NSError(domain: "osd", code: 2)
    }
    try png.write(to: outDir.appendingPathComponent(String(format: "osd_%04d.png", second)))
}

do {
    try FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)
    for second in 0..<seconds {
        try render(second: second)
    }
} catch {
    FileHandle.standardError.write("osd: \(error)\n".data(using: .utf8)!)
    exit(1)
}
