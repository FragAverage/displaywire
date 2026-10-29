// Renders client/assets/Streamy.icns: run `swift scripts/make-icon.swift` from the repo root.
import AppKit

func render(_ size: CGFloat) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
                               bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let s = size / 1024
    // macOS icon grid: 824pt rounded square centred in 1024.
    let tile = NSRect(x: 100 * s, y: 100 * s, width: 824 * s, height: 824 * s)
    let path = NSBezierPath(roundedRect: tile, xRadius: 185 * s, yRadius: 185 * s)
    NSGradient(starting: NSColor(red: 0.07, green: 0.09, blue: 0.16, alpha: 1),
               ending: NSColor(red: 0.10, green: 0.32, blue: 0.45, alpha: 1))!.draw(in: path, angle: 90)
    // Three speed lines and a play chevron.
    NSColor(red: 0.35, green: 0.95, blue: 0.85, alpha: 1).setFill()
    for (i, w) in [220.0, 300.0, 220.0].enumerated() {
        let y = (420 + Double(i) * 80) * s
        NSBezierPath(roundedRect: NSRect(x: 230 * s, y: y, width: w * s, height: 40 * s), xRadius: 20 * s, yRadius: 20 * s).fill()
    }
    NSColor.white.setFill()
    let play = NSBezierPath()
    play.move(to: NSPoint(x: 520 * s, y: 320 * s))
    play.line(to: NSPoint(x: 800 * s, y: 512 * s))
    play.line(to: NSPoint(x: 520 * s, y: 704 * s))
    play.close()
    play.lineJoinStyle = .round
    play.lineWidth = 40 * s
    NSColor.white.setStroke()
    play.fill()
    play.stroke()
    NSGraphicsContext.restoreGraphicsState()
    return rep
}

let iconset = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("Streamy.iconset")
try? FileManager.default.removeItem(at: iconset)
try! FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
for base in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let name = scale == 1 ? "icon_\(base)x\(base).png" : "icon_\(base)x\(base)@2x.png"
        let png = render(CGFloat(base * scale)).representation(using: .png, properties: [:])!
        try! png.write(to: iconset.appendingPathComponent(name))
    }
}
let task = Process()
task.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
task.arguments = ["-c", "icns", iconset.path, "-o", "client/assets/Streamy.icns"]
try! task.run()
task.waitUntilExit()
try! render(1024).representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: NSTemporaryDirectory() + "streamy-icon.png"))
print("wrote client/assets/Streamy.icns")
