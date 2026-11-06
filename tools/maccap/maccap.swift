// Roblox Studio windows on macOS, for cli/maccap.mjs (compiled on first use; macOS 14 or later):
//   maccap windows [title substring]   on-screen Studio document windows as JSON, largest first:
//                                      [{ "id": 33735, "title": "MyPlace.rbxl - Roblox Studio", "x": 0, "y": 33,
//                                         "width": 1728, "height": 1037, "scale": 2, "pid": 4242 }]
//                                      (points, top-left origin; scale: its display's pixels per point)
//   maccap capture <window id> <out.png>
//                                      the window's own pixels at its backing scale, in sRGB
//
// The capture uses ScreenCaptureKit with an sRGB output, not `screencapture -l`: that one colour-matches the
// window into the display's colour space (sRGB 255,0,255 came back as 234,51,247 on a P3 display), while Studio
// draws sRGB values, the ones a comparison needs. Both need the Screen Recording permission for the app running
// this, which is also what makes other apps' window titles readable.
import AppKit
import CoreGraphics
import CoreMedia
import Foundation
import ImageIO
import ScreenCaptureKit
import UniformTypeIdentifiers

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("maccap: \(message)\n".utf8))
    exit(1)
}

func isStudio(_ owner: String?) -> Bool {
    guard let owner else { return false }
    return owner.hasPrefix("Roblox Studio") || owner == "RobloxStudio"
}

// Pixels per point of the display holding most of `rect` (global display coordinates, top-left origin)
func backingScale(_ rect: CGRect) -> Double {
    var displays = [CGDirectDisplayID](repeating: 0, count: 8)
    var count: UInt32 = 0
    guard CGGetDisplaysWithRect(rect, 8, &displays, &count) == .success, count > 0 else { return 1 }
    let overlap = { (id: CGDirectDisplayID) -> CGFloat in
        let shared = CGDisplayBounds(id).intersection(rect)
        return shared.isNull ? 0 : shared.width * shared.height
    }
    let display = displays[..<Int(count)].max { overlap($0) < overlap($1) }!
    guard let mode = CGDisplayCopyDisplayMode(display), mode.width > 0 else { return 1 }
    return Double(mode.pixelWidth) / Double(mode.width)
}

func listWindows(_ filter: String) {
    let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    var rows: [[String: Any]] = []
    for window in windows {
        guard isStudio(window[kCGWindowOwnerName as String] as? String) else { continue }
        guard (window[kCGWindowLayer as String] as? NSNumber)?.intValue == 0 else { continue }
        let title = window[kCGWindowName as String] as? String ?? ""
        if !filter.isEmpty && !title.lowercased().contains(filter.lowercased()) { continue }
        let bounds = window[kCGWindowBounds as String] as? [String: Any] ?? [:]
        let number = { (key: String) in (bounds[key] as? NSNumber)?.doubleValue ?? 0 }
        let rect = CGRect(x: number("X"), y: number("Y"), width: number("Width"), height: number("Height"))
        rows.append([
            "id": (window[kCGWindowNumber as String] as? NSNumber)?.intValue ?? 0,
            "title": title,
            "x": rect.minX, "y": rect.minY, "width": rect.width, "height": rect.height,
            "scale": backingScale(rect),
            "pid": (window[kCGWindowOwnerPID as String] as? NSNumber)?.intValue ?? 0,
        ])
    }
    let area = { (row: [String: Any]) in (row["width"] as? Double ?? 0) * (row["height"] as? Double ?? 0) }
    rows.sort { area($0) > area($1) }
    let data = try! JSONSerialization.data(withJSONObject: rows, options: [.sortedKeys])
    print(String(decoding: data, as: UTF8.self))
}

@MainActor
func capture(_ windowId: CGWindowID, _ out: String) async {
    // ScreenCaptureKit needs this process's window-server connection, which AppKit sets up
    _ = NSApplication.shared
    let content: SCShareableContent
    do {
        content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
    } catch {
        fail("listing windows for capture failed (is the Screen Recording permission granted?): \(error.localizedDescription)")
    }
    guard let window = content.windows.first(where: { $0.windowID == windowId }) else { fail("no on-screen window \(windowId)") }
    let filter = SCContentFilter(desktopIndependentWindow: window)
    let configuration = SCStreamConfiguration()
    let scale = CGFloat(filter.pointPixelScale)
    configuration.width = Int((filter.contentRect.width * scale).rounded())
    configuration.height = Int((filter.contentRect.height * scale).rounded())
    configuration.colorSpaceName = CGColorSpace.sRGB
    configuration.pixelFormat = kCVPixelFormatType_32BGRA
    configuration.showsCursor = false
    configuration.ignoreShadowsSingleWindow = true
    configuration.captureResolution = .best
    let image: CGImage
    do {
        image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: configuration)
    } catch {
        fail("capturing window \(windowId) failed: \(error.localizedDescription)")
    }
    let url = URL(fileURLWithPath: out)
    guard let destination = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else { fail("cannot write \(out)") }
    CGImageDestinationAddImage(destination, image, nil)
    if !CGImageDestinationFinalize(destination) { fail("writing \(out) failed") }
    print("{\"width\":\(image.width),\"height\":\(image.height),\"scale\":\(scale)}")
}

let arguments = Array(CommandLine.arguments.dropFirst())
switch arguments.first {
case "windows":
    listWindows(arguments.count > 1 ? arguments[1] : "")
case "capture":
    guard arguments.count == 3, let id = UInt32(arguments[1]) else { fail("usage: maccap capture <window id> <out.png>") }
    await capture(CGWindowID(id), arguments[2])
default:
    fail("usage: maccap windows [title] | maccap capture <window id> <out.png>")
}
