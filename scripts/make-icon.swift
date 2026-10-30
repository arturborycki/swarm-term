// Draws the Swarm app icon: a terminal chevron and cursor bar on a blue
// rounded square, as in the UI mockups.
// Usage: swift scripts/make-icon.swift <output-dir>
// Writes 512x512.png and 512x512@2x.png (1024 px).
import AppKit

func draw(size: CGFloat) -> Data {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let s = size / 1024

    // macOS icon grid: 824pt body inset in a 1024pt canvas.
    let body = NSRect(x: 100 * s, y: 100 * s, width: 824 * s, height: 824 * s)
    let shape = NSBezierPath(roundedRect: body, xRadius: 185 * s, yRadius: 185 * s)
    NSGradient(colors: [
        NSColor(srgbRed: 0.20, green: 0.27, blue: 0.43, alpha: 1),
        NSColor(srgbRed: 0.11, green: 0.14, blue: 0.22, alpha: 1),
    ])!.draw(in: shape, angle: -90)
    NSColor(srgbRed: 0.31, green: 0.43, blue: 0.69, alpha: 1).setStroke()
    shape.lineWidth = 6 * s
    shape.stroke()

    let accent = NSColor(srgbRed: 0.61, green: 0.75, blue: 1.0, alpha: 1)
    accent.setStroke()
    let chevron = NSBezierPath()
    chevron.move(to: NSPoint(x: 300 * s, y: 650 * s))
    chevron.line(to: NSPoint(x: 460 * s, y: 512 * s))
    chevron.line(to: NSPoint(x: 300 * s, y: 374 * s))
    chevron.lineWidth = 64 * s
    chevron.lineCapStyle = .round
    chevron.lineJoinStyle = .round
    chevron.stroke()

    let cursor = NSBezierPath()
    cursor.move(to: NSPoint(x: 540 * s, y: 362 * s))
    cursor.line(to: NSPoint(x: 724 * s, y: 362 * s))
    cursor.lineWidth = 64 * s
    cursor.lineCapStyle = .round
    cursor.stroke()

    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

let out = URL(fileURLWithPath: CommandLine.arguments[1])
for size in [1024, 512] {
    try! draw(size: CGFloat(size)).write(to: out.appendingPathComponent(size == 1024 ? "512x512@2x.png" : "\(size)x\(size).png"))
}
