// Rust supplies this literal source to /usr/bin/swift -e. All private request
// data arrives on anonymous-file stdin, never as source, paths or arguments.
// Requires macOS 14 and Xcode Command Line Tools with the macOS 14 SDK.
import Foundation
#if os(macOS)
import AppKit
import CoreGraphics
import ScreenCaptureKit
#endif

struct DesktopFailure: Error {
    let code: String
    init(_ code: String) { self.code = code }
}

struct Request: Decodable {
    let action: String
    let display_id: UInt32?
    let window_id: UInt32?
    let text: String?
    let x: Int32?
    let y: Int32?
    let direction: String?
    let amount: Int32?
}

let imageLimit = 20 * 1024 * 1024
let textLimit = 256 * 1024

func finish(_ reply: [String: Any], limit: Int = textLimit) -> Never {
    // Never serialize an NSError, request, or private OS diagnostic.
    guard let bytes = try? JSONSerialization.data(withJSONObject: reply), bytes.count <= limit else {
        FileHandle.standardOutput.write(Data("{\"status\":\"error\",\"code\":\"output_limit\"}".utf8))
        exit(0)
    }
    FileHandle.standardOutput.write(bytes)
    exit(0)
}

func fail(_ code: String) -> Never {
    finish(["status": "error", "code": code])
}

func succeed(_ request: Request, _ data: [String: Any], limit: Int = textLimit) -> Never {
    finish(["status": "ok", "action": request.action, "data": data], limit: limit)
}

func boundedLabel(_ value: String, limit: Int = 512) -> String {
    let kept = value.unicodeScalars.prefix(limit).filter {
        !CharacterSet.controlCharacters.contains($0)
            && !(0x202A...0x202E).contains($0.value)
            && !(0x2066...0x2069).contains($0.value)
    }
    return String(String.UnicodeScalarView(kept))
}

#if os(macOS)
func geometry(_ rect: CGRect) throws -> [String: Any] {
    guard rect.origin.x.isFinite, rect.origin.y.isFinite,
          rect.width.isFinite, rect.height.isFinite,
          rect.width > 0, rect.height > 0,
          abs(rect.origin.x) <= 1_000_000, abs(rect.origin.y) <= 1_000_000,
          rect.width <= 100_000, rect.height <= 100_000 else {
        throw DesktopFailure("invalid_geometry")
    }
    return ["x": Double(rect.origin.x), "y": Double(rect.origin.y),
            "width": Double(rect.width), "height": Double(rect.height)]
}

func requireScreenAccess() throws {
    guard CGPreflightScreenCaptureAccess() else {
        throw DesktopFailure("screen_permission")
    }
}

func displayList() throws -> [[String: Any]] {
    var count: UInt32 = 0
    guard CGGetActiveDisplayList(0, nil, &count) == .success,
          count > 0, count <= 64 else { throw DesktopFailure("display_unavailable") }
    var ids = [CGDirectDisplayID](repeating: 0, count: Int(count))
    guard CGGetActiveDisplayList(count, &ids, &count) == .success,
          count > 0, Int(count) <= ids.count else { throw DesktopFailure("display_unavailable") }
    return try ids.prefix(Int(count)).map { id in
        let bounds = CGDisplayBounds(id)
        let screen = NSScreen.screens.first {
            ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value == id
        }
        var result = try geometry(bounds)
        result["id"] = id
        result["name"] = boundedLabel(screen?.localizedName ?? "Display \(id)")
        result["is_primary"] = id == CGMainDisplayID()
        result["scale_factor"] = Double(screen?.backingScaleFactor ?? 1)
        result["pixel_width"] = CGDisplayPixelsWide(id)
        result["pixel_height"] = CGDisplayPixelsHigh(id)
        return result
    }
}

func windowBounds(_ window: [String: Any]) throws -> CGRect {
    guard let raw = window[kCGWindowBounds as String] as? [String: NSNumber],
          let x = raw["X"], let y = raw["Y"],
          let width = raw["Width"], let height = raw["Height"] else {
        throw DesktopFailure("window_unavailable")
    }
    let bounds = CGRect(x: x.doubleValue, y: y.doubleValue,
                        width: width.doubleValue, height: height.doubleValue)
    _ = try geometry(bounds)
    return bounds
}

func windowList() throws -> [[String: Any]] {
    try requireScreenAccess()
    guard let raw = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
            as? [[String: Any]] else { throw DesktopFailure("window_unavailable") }
    let frontPID = NSWorkspace.shared.frontmostApplication?.processIdentifier
    let normal = raw.filter { ($0[kCGWindowLayer as String] as? NSNumber)?.intValue == 0 }
    guard normal.count <= 256 else { throw DesktopFailure("output_limit") }
    // Application frontness does not prove which of its windows has focus.
    return try normal.map { window in
        guard let id = window[kCGWindowNumber as String] as? NSNumber,
              let pid = window[kCGWindowOwnerPID as String] as? NSNumber else {
            throw DesktopFailure("window_unavailable")
        }
        var result = try geometry(windowBounds(window))
        result["id"] = id.uint32Value
        result["pid"] = pid.int32Value
        result["title"] = boundedLabel(window[kCGWindowName as String] as? String ?? "")
        result["app_name"] = boundedLabel(window[kCGWindowOwnerName as String] as? String ?? "")
        result["frontmost_application"] = pid.int32Value == frontPID
        result["is_on_screen"] = true
        return result
    }
}

@available(macOS 14.0, *)
func screenshot(_ request: Request) throws {
    try requireScreenAccess()
    SCShareableContent.getExcludingDesktopWindows(true, onScreenWindowsOnly: true) { content, failure in
        guard failure == nil, let content = content else { fail("capture_failed") }
        do {
            let filter: SCContentFilter
            let bounds: CGRect
            var result: [String: Any] = [:]
            if let id = request.window_id {
                guard let window = content.windows.first(where: { $0.windowID == id }) else {
                    throw DesktopFailure("window_unavailable")
                }
                filter = SCContentFilter(desktopIndependentWindow: window)
                bounds = window.frame
                result["window_id"] = id
            } else {
                let id = request.display_id ?? CGMainDisplayID()
                guard let display = content.displays.first(where: { $0.displayID == id }) else {
                    throw DesktopFailure("display_unavailable")
                }
                filter = SCContentFilter(display: display, excludingWindows: [])
                bounds = CGDisplayBounds(id)
                result["display_id"] = id
            }
            result["bounds"] = try geometry(bounds)
            let scale = Double(filter.pointPixelScale)
            let width = (Double(bounds.width) * scale).rounded(.up)
            let height = (Double(bounds.height) * scale).rounded(.up)
            guard scale.isFinite, scale > 0, width.isFinite, height.isFinite,
                  width > 0, height > 0, width * height <= 128 * 1024 * 1024 else {
                throw DesktopFailure("output_limit")
            }
            let configuration = SCStreamConfiguration()
            configuration.width = Int(width)
            configuration.height = Int(height)
            configuration.showsCursor = true
            configuration.ignoreShadowsSingleWindow = true
            let metadata = result
            SCScreenshotManager.captureImage(contentFilter: filter, configuration: configuration) { image, failure in
                guard failure == nil, let image = image,
                      let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:]),
                      png.count <= imageLimit else { fail("capture_failed") }
                var captured = metadata
                captured["png"] = png.base64EncodedString()
                captured["pixel_width"] = image.width
                captured["pixel_height"] = image.height
                succeed(request, captured, limit: ((imageLimit + 2) / 3) * 4 + 8192)
            }
        } catch let failure as DesktopFailure {
            fail(failure.code)
        } catch {
            fail("capture_failed")
        }
    }
}

@available(macOS 14.0, *)
func perform(_ request: Request) throws {
    switch request.action {
    case "list_displays":
        succeed(request, ["displays": try displayList()])
    case "list_windows":
        succeed(request, ["windows": try windowList(), "scope": "on_screen_normal_windows",
                          "order": "front_to_back"])
    case "screenshot":
        try screenshot(request)
    case "clipboard_read":
        let board = NSPasteboard.general
        let generation = board.changeCount
        guard let value = board.string(forType: .string) else {
            throw DesktopFailure("clipboard_not_text")
        }
        guard value.utf8.count <= 64 * 1024 else { throw DesktopFailure("output_limit") }
        guard generation == board.changeCount else { throw DesktopFailure("clipboard_changed") }
        succeed(request, ["text": value, "char_count": value.unicodeScalars.count])
    case "clipboard_write":
        guard let value = request.text, value.unicodeScalars.count <= 4096,
              !value.contains("\0") else { throw DesktopFailure("invalid_request") }
        let board = NSPasteboard.general
        board.clearContents()
        guard board.setString(value, forType: .string) else { throw DesktopFailure("clipboard_failed") }
        let generation = board.changeCount
        guard board.string(forType: .string) == value, generation == board.changeCount else {
            throw DesktopFailure("clipboard_failed")
        }
        succeed(request, ["written": true, "char_count": value.unicodeScalars.count])
    default:
        throw DesktopFailure("unsupported_action")
    }
}
#endif

do {
    let bytes = FileHandle.standardInput.readData(ofLength: 128 * 1024 + 1)
    guard bytes.count <= 128 * 1024 else { throw DesktopFailure("invalid_request") }
    let request = try JSONDecoder().decode(Request.self, from: bytes)
    guard request.display_id == nil || request.window_id == nil else {
        throw DesktopFailure("invalid_request")
    }
    #if os(macOS)
    guard #available(macOS 14.0, *) else { throw DesktopFailure("os_version") }
    try perform(request)
    // Only capture is asynchronous. Rust owns its deadline and subprocess.
    RunLoop.main.run()
    throw DesktopFailure("capture_failed")
    #else
    throw DesktopFailure("unsupported_platform")
    #endif
} catch let failure as DesktopFailure {
    fail(failure.code)
} catch {
    fail("invalid_request")
}
