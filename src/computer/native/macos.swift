// Rust supplies this literal source to /usr/bin/swift -e. All private request
// data arrives on anonymous-file stdin, never as source, paths or arguments.
// Requires macOS 14 and Xcode Command Line Tools with the macOS 14 SDK.
import Foundation
#if os(macOS)
import AppKit
import CoreGraphics
import ApplicationServices
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

func requireAccessibility() throws {
    guard AXIsProcessTrusted() else { throw DesktopFailure("accessibility_permission") }
    guard AXUIElementSetMessagingTimeout(AXUIElementCreateSystemWide(), 0.15) == .success else {
        throw DesktopFailure("accessibility_failed")
    }
}

func attribute(_ element: AXUIElement, _ name: String) throws -> CFTypeRef? {
    var value: CFTypeRef?
    let status = AXUIElementCopyAttributeValue(element, name as CFString, &value)
    switch status {
    case .success: return value
    case .attributeUnsupported, .noValue: return nil
    default: throw DesktopFailure("accessibility_failed")
    }
}

func elementAttribute(_ element: AXUIElement, _ name: String) throws -> AXUIElement {
    guard let value = try attribute(element, name), CFGetTypeID(value) == AXUIElementGetTypeID() else {
        throw DesktopFailure("accessibility_failed")
    }
    return value as! AXUIElement // Type checked above; no private AX API.
}

func accessibilityBounds(_ element: AXUIElement) throws -> CGRect? {
    guard let position = try attribute(element, kAXPositionAttribute),
          let size = try attribute(element, kAXSizeAttribute),
          CFGetTypeID(position) == AXValueGetTypeID(), CFGetTypeID(size) == AXValueGetTypeID() else {
        return nil
    }
    var origin = CGPoint.zero
    var extent = CGSize.zero
    let p = position as! AXValue
    let s = size as! AXValue
    guard AXValueGetType(p) == .cgPoint, AXValueGetType(s) == .cgSize,
          AXValueGetValue(p, .cgPoint, &origin), AXValueGetValue(s, .cgSize, &extent) else { return nil }
    return CGRect(origin: origin, size: extent)
}

func children(_ element: AXUIElement, _ name: String, maximum: Int) throws -> ([AXUIElement], Bool) {
    var count = 0
    let status = AXUIElementGetAttributeValueCount(element, name as CFString, &count)
    if status == .attributeUnsupported || status == .noValue { return ([], false) }
    guard status == .success, count >= 0 else { throw DesktopFailure("accessibility_failed") }
    let admitted = min(count, maximum)
    if admitted == 0 { return ([], count > 0) }
    var values: CFArray?
    guard AXUIElementCopyAttributeValues(element, name as CFString, 0, admitted, &values) == .success,
          let array = values as? [AnyObject], array.count <= admitted else {
        throw DesktopFailure("accessibility_failed")
    }
    let elements = try array.map { value -> AXUIElement in
        guard CFGetTypeID(value) == AXUIElementGetTypeID() else {
            throw DesktopFailure("accessibility_failed")
        }
        return value as! AXUIElement
    }
    return (elements, count > elements.count)
}

func focusedWindow() throws -> (pid_t, AXUIElement) {
    guard let application = NSWorkspace.shared.frontmostApplication else {
        throw DesktopFailure("window_unavailable")
    }
    let root = AXUIElementCreateApplication(application.processIdentifier)
    return (application.processIdentifier, try elementAttribute(root, kAXFocusedWindowAttribute))
}

func sameBounds(_ a: CGRect, _ b: CGRect) -> Bool {
    abs(a.origin.x - b.origin.x) <= 0.5 && abs(a.origin.y - b.origin.y) <= 0.5
        && abs(a.width - b.width) <= 0.5 && abs(a.height - b.height) <= 0.5
}

func resolveWindow(_ id: UInt32) throws -> (pid_t, AXUIElement) {
    try requireScreenAccess()
    guard let raw = CGWindowListCopyWindowInfo(.optionIncludingWindow, id) as? [[String: Any]],
          raw.count == 1, let record = raw.first,
          (record[kCGWindowNumber as String] as? NSNumber)?.uint32Value == id,
          let pid = (record[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value, pid > 0,
          let title = record[kCGWindowName as String] as? String else {
        throw DesktopFailure("window_unavailable")
    }
    let bounds = try windowBounds(record)
    let root = AXUIElementCreateApplication(pid)
    let (windows, incomplete) = try children(root, kAXWindowsAttribute, maximum: 256)
    guard !incomplete else { throw DesktopFailure("ambiguous_window") }
    var matches: [AXUIElement] = []
    for window in windows {
        guard let candidate = try accessibilityBounds(window), sameBounds(bounds, candidate),
              (try attribute(window, kAXTitleAttribute) as? String ?? "") == title else { continue }
        matches.append(window)
    }
    // No public CGWindowID -> AXUIElement conversion exists. Correlate PID,
    // exact unsanitized title and geometry; never guess or use a private API.
    guard matches.count == 1 else { throw DesktopFailure("ambiguous_window") }
    return (pid, matches[0])
}

func focusWindow(_ request: Request) throws -> Never {
    try requireAccessibility()
    guard let id = request.window_id else { throw DesktopFailure("invalid_request") }
    let (pid, window) = try resolveWindow(id)
    guard let application = NSRunningApplication(processIdentifier: pid),
          application.activate(options: []),
          AXUIElementPerformAction(window, kAXRaiseAction as CFString) == .success else {
        throw DesktopFailure("focus_unconfirmed")
    }
    let until = ProcessInfo.processInfo.systemUptime + 2
    while ProcessInfo.processInfo.systemUptime < until {
        if let (actualPID, actualWindow) = try? focusedWindow(), actualPID == pid, CFEqual(actualWindow, window) {
            let (confirmedPID, confirmedWindow) = try resolveWindow(id)
            guard confirmedPID == pid, CFEqual(confirmedWindow, window) else {
                throw DesktopFailure("focus_unconfirmed")
            }
            succeed(request, ["window_id": id, "pid": pid, "focused": true])
        }
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.02))
    }
    throw DesktopFailure("focus_unconfirmed")
}

struct TreeReader {
    var remaining = 256
    var textBytes = 64 * 1024
    var truncated = false
    var visited: [AXUIElement] = []
    let deadline = ProcessInfo.processInfo.systemUptime + 8

    mutating func label(_ value: String) -> String {
        let cap = min(512, textBytes / 4)
        let limited = boundedLabel(value, limit: cap)
        if value.unicodeScalars.prefix(cap + 1).count > cap { truncated = true }
        textBytes -= limited.utf8.count
        return limited
    }

    mutating func read(_ element: AXUIElement, depth: Int = 0) throws -> [String: Any]? {
        guard remaining > 0, depth <= 12, ProcessInfo.processInfo.systemUptime < deadline,
              !visited.contains(where: { CFEqual($0, element) }) else {
            truncated = true
            return nil
        }
        remaining -= 1
        visited.append(element)
        guard let role = try attribute(element, kAXRoleAttribute) as? String else {
            throw DesktopFailure("accessibility_failed")
        }
        let subrole = try attribute(element, kAXSubroleAttribute) as? String ?? ""
        var node: [String: Any] = ["role": label(role)]
        if role == "AXSecureTextField" || subrole == "AXSecureTextField" {
            // Do not even query names or children of secure fields. Value/Text
            // interfaces are never queried for any node, secure or otherwise.
            node["value_redacted"] = true
            node["children"] = [[String: Any]]()
            return node
        }
        if let title = try attribute(element, kAXTitleAttribute) as? String { node["title"] = label(title) }
        if let enabled = try attribute(element, kAXEnabledAttribute) as? Bool { node["enabled"] = enabled }
        if let focused = try attribute(element, kAXFocusedAttribute) as? Bool { node["focused"] = focused }
        if let bounds = try accessibilityBounds(element), bounds.width > 0, bounds.height > 0 {
            node["bounds"] = try geometry(bounds)
        }
        let (items, omitted) = try children(element, kAXChildrenAttribute, maximum: min(128, remaining))
        truncated = truncated || omitted
        var nodes: [[String: Any]] = []
        for item in items {
            if let child = try read(item, depth: depth + 1) { nodes.append(child) }
        }
        node["children"] = nodes
        return node
    }
}

func accessibilityTree(_ request: Request) throws -> Never {
    try requireAccessibility()
    let target: (pid_t, AXUIElement)
    if let id = request.window_id { target = try resolveWindow(id) }
    else { target = try focusedWindow() }
    var reader = TreeReader()
    guard let tree = try reader.read(target.1) else { throw DesktopFailure("accessibility_failed") }
    var data: [String: Any] = ["pid": target.0, "tree": tree, "truncated": reader.truncated,
                               "node_count": 256 - reader.remaining,
                               "scope": "requested_or_focused_window"]
    if let id = request.window_id { data["window_id"] = id }
    succeed(request, data)
}

func requireFocusedTarget(_ request: Request) throws {
    if let id = request.window_id {
        let (pid, window) = try resolveWindow(id)
        let (actualPID, actualWindow) = try focusedWindow()
        guard pid == actualPID, CFEqual(window, actualWindow) else {
            throw DesktopFailure("target_not_focused")
        }
    }
}

func movePointer(_ request: Request) throws -> Never {
    try requireAccessibility()
    guard let x = request.x, let y = request.y else { throw DesktopFailure("invalid_request") }
    let point = CGPoint(x: Int(x), y: Int(y))
    let visible = try displayList().contains { display in
        guard let left = display["x"] as? Double, let top = display["y"] as? Double,
              let width = display["width"] as? Double, let height = display["height"] as? Double else { return false }
        return CGRect(x: left, y: top, width: width, height: height).contains(point)
    }
    guard visible else { throw DesktopFailure("pointer_outside_displays") }
    try requireFocusedTarget(request)
    guard let event = CGEvent(mouseEventSource: nil, mouseType: .mouseMoved,
                              mouseCursorPosition: point, mouseButton: .left) else {
        throw DesktopFailure("input_failed")
    }
    event.post(tap: .cghidEventTap)
    let until = ProcessInfo.processInfo.systemUptime + 0.5
    while ProcessInfo.processInfo.systemUptime < until {
        if let actual = CGEvent(source: nil)?.location,
           abs(actual.x - point.x) <= 0.5, abs(actual.y - point.y) <= 0.5 {
            succeed(request, ["position": ["x": Double(actual.x), "y": Double(actual.y)],
                              "dispatched": true])
        }
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.01))
    }
    throw DesktopFailure("pointer_unconfirmed")
}

func scroll(_ request: Request) throws -> Never {
    try requireAccessibility()
    let amount = request.amount ?? 3
    guard (1...100).contains(amount) else { throw DesktopFailure("invalid_request") }
    let direction = request.direction ?? "down"
    let vertical: Int32
    let horizontal: Int32
    switch direction {
    case "up": (vertical, horizontal) = (amount, 0)
    case "down": (vertical, horizontal) = (-amount, 0)
    case "left": (vertical, horizontal) = (0, amount)
    case "right": (vertical, horizontal) = (0, -amount)
    default: throw DesktopFailure("invalid_request")
    }
    try requireFocusedTarget(request)
    guard let event = CGEvent(scrollWheelEvent2Source: nil, units: .line, wheelCount: 2,
                              wheel1: vertical, wheel2: horizontal, wheel3: 0) else {
        throw DesktopFailure("input_failed")
    }
    event.post(tap: .cghidEventTap)
    succeed(request, ["dispatched": true, "direction": direction, "amount": amount,
                      "application_effect": "not_verified"])
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
    case "ax_tree":
        try accessibilityTree(request)
    case "focus_window":
        try focusWindow(request)
    case "mouse_move":
        try movePointer(request)
    case "scroll":
        try scroll(request)
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
