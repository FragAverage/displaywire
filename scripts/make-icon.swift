// Renders client/assets/DisplayWire.icns: run `swift scripts/make-icon.swift [preview.png]` from the repo root.
import AppKit

let cyan = NSColor(red: 0.30, green: 0.93, blue: 0.86, alpha: 1)
let blue = NSColor(red: 0.22, green: 0.55, blue: 1.00, alpha: 1)

func render(_ size: CGFloat) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
                               bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let s = size / 1024
    func p(_ x: CGFloat, _ y: CGFloat) -> NSPoint { NSPoint(x: x * s, y: y * s) }

    // macOS icon grid: 824pt rounded square centred in 1024.
    let tile = NSBezierPath(roundedRect: NSRect(x: 100 * s, y: 100 * s, width: 824 * s, height: 824 * s), xRadius: 185 * s, yRadius: 185 * s)
    NSGradient(starting: NSColor(red: 0.05, green: 0.06, blue: 0.13, alpha: 1),
               ending: NSColor(red: 0.10, green: 0.14, blue: 0.30, alpha: 1))!.draw(in: tile, angle: 90)
    tile.addClip()

    // The wire: out of the display's stand and off the right edge, with a pulse travelling along it.
    let wire = NSBezierPath()
    wire.move(to: p(512, 400))
    wire.line(to: p(512, 330))
    wire.curve(to: p(760, 205), controlPoint1: p(512, 240), controlPoint2: p(640, 205))
    wire.line(to: p(960, 205))
    wire.lineWidth = 34 * s
    wire.lineCapStyle = .round
    wire.lineJoinStyle = .round
    let glow = NSShadow()
    glow.shadowColor = cyan.withAlphaComponent(0.8)
    glow.shadowBlurRadius = 30 * s
    NSGraphicsContext.saveGraphicsState()
    glow.set()
    cyan.setStroke()
    wire.stroke()
    NSColor.white.setFill()
    NSBezierPath(ovalIn: NSRect(x: 676 * s, y: 182 * s, width: 50 * s, height: 50 * s)).fill()
    NSGraphicsContext.restoreGraphicsState()

    // The display: a glowing screen with a play mark.
    let screen = NSRect(x: 205 * s, y: 400 * s, width: 614 * s, height: 390 * s)
    let bezel = NSBezierPath(roundedRect: screen, xRadius: 48 * s, yRadius: 48 * s)
    NSGraphicsContext.saveGraphicsState()
    let halo = NSShadow()
    halo.shadowColor = blue.withAlphaComponent(0.55)
    halo.shadowBlurRadius = 60 * s
    halo.set()
    NSGradient(starting: NSColor(red: 0.09, green: 0.20, blue: 0.42, alpha: 1),
               ending: NSColor(red: 0.07, green: 0.42, blue: 0.62, alpha: 1))!.draw(in: bezel, angle: 90)
    NSGraphicsContext.restoreGraphicsState()
    bezel.lineWidth = 30 * s
    NSColor.white.setStroke()
    bezel.stroke()

    let play = NSBezierPath()
    play.move(to: p(458, 515))
    play.line(to: p(600, 595))
    play.line(to: p(458, 675))
    play.close()
    play.lineJoinStyle = .round
    play.lineWidth = 34 * s
    NSColor.white.setFill()
    NSColor.white.setStroke()
    play.fill()
    play.stroke()

    NSGraphicsContext.restoreGraphicsState()
    return rep
}

let iconset = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("DisplayWire.iconset")
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
task.arguments = ["-c", "icns", iconset.path, "-o", "client/assets/DisplayWire.icns"]
try! task.run()
task.waitUntilExit()
let preview = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : NSTemporaryDirectory() + "displaywire-icon.png"
try! render(1024).representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: preview))
print("wrote client/assets/DisplayWire.icns and \(preview)")
