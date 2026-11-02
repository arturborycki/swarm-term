// Draws the Swarm icons: the app icon (a hive of session cells around the
// prompt cell, on a graphite rounded square) and the menu bar template (one
// cell with the prompt, black on transparent; macOS tints it).
// Usage: swift scripts/make-icon.swift <app-icon-dir> [<status-icon-dir>]
// Writes 512x512.png and 512x512@2x.png (1024 px) to the first directory, and
// SwarmStatus.png (18 px) and SwarmStatus@2x.png (36 px) to the second.
import AppKit

func srgb(_ hex: UInt32) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: 1)
}

func render(pixels: Int, _ body: (CGFloat) -> Void) -> Data {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSGraphicsContext.current!.imageInterpolation = .high
    body(CGFloat(pixels))
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

/// A pointy-top hexagon centered on `center` with circumradius `r`.
func hexagon(_ center: NSPoint, _ r: CGFloat) -> NSBezierPath {
    let path = NSBezierPath()
    for i in 0..<6 {
        let a = (CGFloat(i) * 60 + 30) * .pi / 180
        let p = NSPoint(x: center.x + r * cos(a), y: center.y + r * sin(a))
        i == 0 ? path.move(to: p) : path.line(to: p)
    }
    path.close()
    path.lineJoinStyle = .round
    return path
}

func chevron(_ center: NSPoint, _ size: CGFloat, width: CGFloat) -> NSBezierPath {
    let path = NSBezierPath()
    path.move(to: NSPoint(x: center.x - 0.6 * size, y: center.y + 0.82 * size))
    path.line(to: NSPoint(x: center.x + 0.4 * size, y: center.y))
    path.line(to: NSPoint(x: center.x - 0.6 * size, y: center.y - 0.82 * size))
    path.lineWidth = width
    path.lineCapStyle = .round
    path.lineJoinStyle = .round
    return path
}

func appIcon(size: CGFloat) {
    let s = size / 1024
    func pt(_ x: CGFloat, _ y: CGFloat) -> NSPoint { NSPoint(x: x * s, y: y * s) }

    // macOS icon grid: 824pt body inset in a 1024pt canvas.
    let body = NSBezierPath(roundedRect: NSRect(x: 100 * s, y: 100 * s, width: 824 * s, height: 824 * s),
                            xRadius: 185 * s, yRadius: 185 * s)
    NSGradient(colors: [srgb(0x1D2230), srgb(0x13161E)])!.draw(in: body, angle: -90)
    srgb(0x2C3A5A).setStroke()
    body.lineWidth = 8 * s
    body.stroke()

    // Cells on a 130pt grid, drawn at 118pt so a gap shows between them.
    // Filled cells are sessions at work, outlined ones idle.
    let r = 118 * s
    let dx: CGFloat = 225, dy: CGFloat = 195
    let filledDark = [pt(512 + dx, 512), pt(512 - dx / 2, 512 + dy)]
    let filledMid = [pt(512 - dx / 2, 512 - dy)]
    let outlined = [pt(512 - dx, 512), pt(512 + dx / 2, 512 + dy), pt(512 + dx / 2, 512 - dy)]
    srgb(0x2B3D63).setFill()
    filledDark.forEach { hexagon($0, r).fill() }
    srgb(0x4F74B8).setFill()
    filledMid.forEach { hexagon($0, r).fill() }
    srgb(0x3B4D75).setStroke()
    for c in outlined {
        let cell = hexagon(c, r - 8 * s)
        cell.lineWidth = 16 * s
        cell.stroke()
    }

    let center = pt(512, 512)
    NSGradient(colors: [srgb(0x9CC2FF), srgb(0x6E9FF2)])!.draw(in: hexagon(center, r), angle: -90)
    srgb(0x141821).setStroke()
    chevron(NSPoint(x: center.x + 6 * s, y: center.y), 56 * s, width: 30 * s).stroke()
}

/// Menu bar template, in points on an 18pt canvas.
func statusIcon(size: CGFloat) {
    let s = size / 18
    let center = NSPoint(x: 9 * s, y: 9 * s)
    NSColor.black.setStroke()
    let cell = hexagon(center, 7.9 * s)
    cell.lineWidth = 1.5 * s
    cell.stroke()
    chevron(NSPoint(x: center.x + 0.4 * s, y: center.y), 3.1 * s, width: 1.6 * s).stroke()
}

let args = CommandLine.arguments
let appDir = URL(fileURLWithPath: args[1])
for (pixels, name) in [(1024, "512x512@2x.png"), (512, "512x512.png")] {
    try! render(pixels: pixels) { appIcon(size: $0) }.write(to: appDir.appendingPathComponent(name))
}
if args.count > 2 {
    let statusDir = URL(fileURLWithPath: args[2])
    for (pixels, name) in [(36, "SwarmStatus@2x.png"), (18, "SwarmStatus.png")] {
        try! render(pixels: pixels) { statusIcon(size: $0) }.write(to: statusDir.appendingPathComponent(name))
    }
}
