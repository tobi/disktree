#!/usr/bin/env swift

// The Dock icon a run outside the app bundle shows, in both the looks a Mac
// can give it. `make dock-icons` runs this; the app embeds what it writes
// (`crates/disktree-app/src/dock_icon.rs`).
//
//   packaging/macos/DockIcon.png        AppIcon.svg as it is: the tile on
//                                       Apple's 824-in-1024 legacy canvas,
//                                       drawn literally by macOS 15 and
//                                       earlier.
//   packaging/macos/DockIcon-glass.png  The same icon as macOS 26 draws it
//                                       from AppIcon.icon: its own shape,
//                                       the glass edge and the mark's shadow.
//
// macOS 26 gives Liquid Glass only to an app bundle's Icon Composer icon,
// never to an image set at runtime, and has no API to ask for it. So the
// glass is rendered here, by the system itself: AppIcon.icon is compiled with
// actool into a throwaway bundle, and what Finder and the Dock would draw for
// that bundle is read back through NSWorkspace. That draws in the icon style
// this Mac is set to, so the script refuses to run unless the Default style
// is on, and refuses on a Mac older than 26 or with an older actool.
//
// Both files are 512 px: the Dock draws at most 128 pt, twice that on a Retina
// display, and the glass render carries gradients that would make a 1024 px
// file four times the size for nothing.

import AppKit
import Foundation

let macosDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let tileSVG = macosDir.appendingPathComponent("AppIcon.svg")
let glyphSVG = macosDir.appendingPathComponent("AppGlyph.svg")
let package = macosDir.appendingPathComponent("AppIcon.icon", isDirectory: true)
let flatOut = macosDir.appendingPathComponent("DockIcon.png")
let glassOut = macosDir.appendingPathComponent("DockIcon-glass.png")

/// The icon is rendered on the canvas the bundle's icons use, then scaled.
let canvas = 1024
let output = 512
/// Where the legacy tile sits on that canvas, as AppIcon.svg draws it. The
/// glass render must land there too, or the two files would not line up
/// between one macOS and the next.
let tileOrigin = 100
let tileSize = 824

func fail(_ message: String) -> Never {
    FileHandle.standardError.write("render-dock-icons: \(message)\n".data(using: .utf8)!)
    exit(1)
}

func run(_ executable: String, _ arguments: [String]) -> (status: Int32, output: Data) {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: executable)
    process.arguments = arguments
    let pipe = Pipe()
    process.standardOutput = pipe
    process.standardError = pipe
    do { try process.run() } catch { fail("cannot run \(executable): \(error)") }
    let output = pipe.fileHandleForReading.readDataToEndOfFile()
    process.waitUntilExit()
    return (process.terminationStatus, output)
}

// MARK: - Preconditions, before anything is written

// The bundle's LSMinimumSystemVersion, which `cargo xtask bundle` compiles
// the package for too, so both compiles make the same catalog. Read from
// the template by hand: its placeholders keep it from parsing as a plist.
let plistTemplate = macosDir.appendingPathComponent("Info.plist.in")
guard let template = try? String(contentsOf: plistTemplate, encoding: .utf8),
    let pattern = try? Regex(
        #"<key>LSMinimumSystemVersion</key>\s*<string>([^<]+)</string>"#),
    let match = template.firstMatch(of: pattern),
    let version = match.output[1].substring
else {
    fail("cannot read LSMinimumSystemVersion from \(plistTemplate.path)")
}
let deploymentTarget = String(version)

let os = ProcessInfo.processInfo.operatingSystemVersion
guard os.majorVersion >= 26 else {
    fail("the glass icon is drawn by macOS 26 or later; this is \(os.majorVersion).\(os.minorVersion)")
}
if NSWorkspace.shared.accessibilityDisplayShouldReduceTransparency
    || NSWorkspace.shared.accessibilityDisplayShouldIncreaseContrast
{
    fail("turn off Reduce Transparency and Increase Contrast (Accessibility > Display) first: icons render with them")
}
let actoolVersion = run("/usr/bin/xcrun", ["actool", "--version"])
guard actoolVersion.status == 0,
    let plist = try? PropertyListSerialization.propertyList(from: actoolVersion.output, format: nil) as? [String: Any],
    let info = plist["com.apple.actool.version"] as? [String: Any],
    let short = info["short-bundle-version"] as? String,
    let major = Int(short.split(separator: ".").first ?? ""), major >= 26
else {
    fail("needs Xcode 26's actool; select it with xcode-select or DEVELOPER_DIR")
}

// MARK: - Helpers

func rasterize(_ svg: URL, to png: URL, pixels: Int) {
    // The same tool `cargo xtask bundle` draws the iconset with, so the flat
    // Dock icon is the 512 px rep of the shipped AppIcon.icns, pixel for pixel.
    let result = run("/usr/bin/sips", ["-s", "format", "png", "-z", "\(pixels)", "\(pixels)", svg.path, "--out", png.path])
    guard result.status == 0 else {
        fail("sips could not draw \(svg.lastPathComponent):\n\(String(decoding: result.output, as: UTF8.self))")
    }
}

func bitmap(_ pixels: Int) -> NSBitmapImageRep {
    guard let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
    ) else { fail("cannot create a \(pixels) px bitmap") }
    rep.size = NSSize(width: pixels, height: pixels)
    return rep
}

func draw(_ image: NSImage, at pixels: Int) -> NSBitmapImageRep {
    let rep = bitmap(pixels)
    NSGraphicsContext.saveGraphicsState()
    guard let context = NSGraphicsContext(bitmapImageRep: rep) else { fail("cannot draw") }
    NSGraphicsContext.current = context
    context.imageInterpolation = .high
    let frame = NSRect(x: 0, y: 0, width: pixels, height: pixels)
    NSColor.clear.setFill()
    frame.fill()
    image.draw(in: frame, from: .zero, operation: .sourceOver, fraction: 1)
    NSGraphicsContext.restoreGraphicsState()
    return rep
}

/// The opaque bounds, top-down: (minX, minY, maxX, maxY).
func opaqueBounds(_ rep: NSBitmapImageRep) -> (Int, Int, Int, Int) {
    var minX = rep.pixelsWide, minY = rep.pixelsHigh, maxX = -1, maxY = -1
    for y in 0..<rep.pixelsHigh {
        for x in 0..<rep.pixelsWide where (rep.colorAt(x: x, y: y)?.alphaComponent ?? 0) > 0.5 {
            minX = min(minX, x); maxX = max(maxX, x); minY = min(minY, y); maxY = max(maxY, y)
        }
    }
    return (minX, minY, maxX, maxY)
}

func srgb(_ rep: NSBitmapImageRep, x: Int, y: Int) -> [Int] {
    guard let c = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB) else { return [] }
    return [c.redComponent, c.greenComponent, c.blueComponent, c.alphaComponent].map { Int(($0 * 255).rounded()) }
}

func write(_ rep: NSBitmapImageRep, to url: URL) {
    guard let data = rep.representation(using: .png, properties: [:]) else { fail("cannot encode \(url.lastPathComponent)") }
    do { try data.write(to: url, options: .atomic) } catch { fail("cannot write \(url.path): \(error)") }
    print("  \(url.lastPathComponent) (\(rep.pixelsWide) px, \(data.count) bytes)")
}

// MARK: - The glass render

let work = FileManager.default.temporaryDirectory
    .appendingPathComponent("disktree-dock-icons-\(UUID().uuidString)", isDirectory: true)
defer { try? FileManager.default.removeItem(at: work) }

/// What macOS draws for an app that ships AppIcon.icon.
///
/// The bundle path and identifier are new for every run: icon services caches
/// by path, and a reused path returns whatever it drew there before. The
/// bundle gets a real executable, because one without is drawn with the
/// "cannot open" badge.
func renderGlass() -> NSImage {
    let fm = FileManager.default
    // actool resolves `--app-icon Icon` by the package's basename: under any
    // other name it exits 0 and writes no icon at all.
    let icon = work.appendingPathComponent("Icon.icon", isDirectory: true)
    let compiled = work.appendingPathComponent("compiled", isDirectory: true)
    let bundle = work.appendingPathComponent("disktree-\(UUID().uuidString).app", isDirectory: true)
    let contents = bundle.appendingPathComponent("Contents", isDirectory: true)
    let resources = contents.appendingPathComponent("Resources", isDirectory: true)
    let executables = contents.appendingPathComponent("MacOS", isDirectory: true)
    do {
        try fm.createDirectory(at: work, withIntermediateDirectories: true)
        try fm.copyItem(at: package, to: icon)
        try fm.createDirectory(at: icon.appendingPathComponent("Assets"), withIntermediateDirectories: true)
        for dir in [compiled, resources, executables] {
            try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        }
    } catch { fail("cannot stage the render bundle: \(error)") }
    rasterize(glyphSVG, to: icon.appendingPathComponent("Assets/glyph.png"), pixels: canvas)

    // The invocation `cargo xtask bundle` uses, so this is the catalog the
    // shipped app carries.
    let compile = run("/usr/bin/xcrun", [
        "actool", icon.path,
        "--compile", compiled.path,
        "--output-format", "human-readable-text",
        "--notices", "--warnings",
        "--output-partial-info-plist", compiled.appendingPathComponent("Info.plist").path,
        "--app-icon", "Icon",
        "--include-all-app-icons",
        "--enable-on-demand-resources", "NO",
        "--development-region", "en",
        "--target-device", "mac",
        "--minimum-deployment-target", deploymentTarget,
        "--platform", "macosx",
    ])
    let catalog = compiled.appendingPathComponent("Assets.car")
    guard compile.status == 0, fm.fileExists(atPath: catalog.path) else {
        fail("actool did not compile AppIcon.icon:\n\(String(decoding: compile.output, as: UTF8.self))")
    }
    let info: [String: Any] = [
        "CFBundleExecutable": "stub",
        "CFBundleIconName": "Icon",
        "CFBundleIdentifier": "io.github.tobi.disktree.dock-icon-render.\(UUID().uuidString)",
        "CFBundleName": "disktree",
        "CFBundlePackageType": "APPL",
    ]
    do {
        try fm.copyItem(at: catalog, to: resources.appendingPathComponent("Assets.car"))
        try fm.copyItem(at: URL(fileURLWithPath: "/usr/bin/true"), to: executables.appendingPathComponent("stub"))
        let plist = try PropertyListSerialization.data(fromPropertyList: info, format: .xml, options: 0)
        try plist.write(to: contents.appendingPathComponent("Info.plist"))
    } catch { fail("cannot assemble the render bundle: \(error)") }
    return NSWorkspace.shared.icon(forFile: bundle.path)
}

// MARK: - Render, check, then write

let flatPNG = work.appendingPathComponent("flat.png")
do { try FileManager.default.createDirectory(at: work, withIntermediateDirectories: true) } catch { fail("\(error)") }
rasterize(tileSVG, to: flatPNG, pixels: canvas)
guard let flatImage = NSImage(contentsOf: flatPNG) else { fail("cannot read the flat render") }
let flat = draw(flatImage, at: canvas)
let glass = draw(renderGlass(), at: canvas)

// The glass tile must sit where the flat one does, within a pixel of
// antialiasing, so the Dock icon does not jump between macOS versions.
let expected = (tileOrigin, tileOrigin, tileOrigin + tileSize - 1, tileOrigin + tileSize - 1)
for (name, rep) in [("AppIcon.svg", flat), ("the glass render", glass)] {
    let b = opaqueBounds(rep)
    let drift = [b.0 - expected.0, b.1 - expected.1, b.2 - expected.2, b.3 - expected.3]
    guard drift.allSatisfy({ abs($0) <= 1 }) else {
        fail("\(name) covers \(b), not the \(expected) tile the Dock expects")
    }
}

// The Default icon style keeps the mark's own colours; Clear and Tinted
// replace them, and Dark darkens the tile. Two probes that hold whatever
// size AppGlyph.svg draws the mark at, as long as it is centred: the
// canvas centre, which is the mark's dark green block, and the tile 50 px
// in from its left edge, outside the mark and clear of the glass rim.
// Lighting shifts them a little; a style change replaces them.
for probe in [(x: 512, y: 512, label: "green block"), (x: 150, y: 512, label: "tile")] {
    let authored = srgb(flat, x: probe.x, y: probe.y)
    let rendered = srgb(glass, x: probe.x, y: probe.y)
    guard authored.count == 4, rendered.count == 4,
        zip(authored, rendered).allSatisfy({ abs($0 - $1) <= 32 })
    else {
        fail("the glass \(probe.label) renders \(rendered), not \(authored): set System Settings > Appearance > Icon & widget style to Default")
    }
}

// sips at the output size, as the bundle's iconset is drawn, rather than a
// resample of the 1024 px check render.
rasterize(tileSVG, to: flatOut, pixels: output)
print("  \(flatOut.lastPathComponent) (\(output) px)")
let glassImage = NSImage(size: NSSize(width: canvas, height: canvas))
glassImage.addRepresentation(glass)
write(draw(glassImage, at: output), to: glassOut)
