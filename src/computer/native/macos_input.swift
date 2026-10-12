// One owned input transaction. stdin is a liveness lease after its JSON line.
// Only this process emits input; EOF stops new events before deferred releases.
import Foundation
#if os(macOS)
import Darwin
import CoreGraphics
#else
import Glibc
#endif

let ownedInputActions = ["mouse_click", "mouse_drag"]
let ownedInputInvocation = ProcessInfo.processInfo.environment["PI_COMPUTER_OWNED_INPUT"] == "1"

func readOwnedInputFrame() throws -> Data {
    let flags = fcntl(STDIN_FILENO, F_GETFL)
    guard flags >= 0, fcntl(STDIN_FILENO, F_SETFL, flags | O_NONBLOCK) == 0 else {
        throw DesktopFailure("input_channel")
    }
    var data = Data()
    var buffer = [UInt8](repeating: 0, count: 4096)
    let deadline = ProcessInfo.processInfo.systemUptime + 30
    while ProcessInfo.processInfo.systemUptime < deadline {
        let count = read(STDIN_FILENO, &buffer, buffer.count)
        if count > 0 {
            let chunk = buffer.prefix(count)
            if let end = chunk.firstIndex(of: 10) {
                guard end == count - 1, data.count + end <= 32 * 1024 else {
                    throw DesktopFailure("invalid_request")
                }
                data.append(contentsOf: chunk.prefix(end))
                return data
            }
            guard data.count + count <= 32 * 1024 else { throw DesktopFailure("invalid_request") }
            data.append(contentsOf: chunk)
        } else if count == 0 {
            throw DesktopFailure("input_cancelled")
        } else if errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR {
            throw DesktopFailure("input_channel")
        }
        Thread.sleep(forTimeInterval: 0.002)
    }
    throw DesktopFailure("input_cancelled")
}

func checkInputLease(_ deadline: Double) throws {
    guard ProcessInfo.processInfo.systemUptime < deadline else { throw DesktopFailure("input_cancelled") }
    var byte: UInt8 = 0
    let count = read(STDIN_FILENO, &byte, 1)
    // Exactly one frame is authorized. EOF and any second command revoke it.
    guard count < 0, errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR else {
        throw DesktopFailure("input_cancelled")
    }
}

enum OwnedEventKind { case move, down, drag, up }
struct OwnedEvent {
    let kind: OwnedEventKind
    let code: UInt16
    let x: Double
    let y: Double
}

// The same lifecycle engine is exercised with recording sinks in portable
// tests. Cleanup is armed BEFORE a potentially failing press, removed AFTER
// its release, and never requires the cancelled lease to become live again.
func dispatchOwnedEvents(
    _ events: [OwnedEvent],
    checkpoint: () throws -> Void,
    post: (Int, OwnedEvent) throws -> Void,
    release: (UInt16) -> Void
) throws {
    guard events.count <= 64 else { throw DesktopFailure("invalid_request") }
    var held: [UInt16] = []
    defer { for code in held.reversed() { release(code) } }
    for (index, event) in events.enumerated() {
        try checkpoint()
        if event.kind == .down {
            guard !held.contains(event.code), held.count < 8 else { throw DesktopFailure("invalid_request") }
            held.append(event.code)
        }
        try post(index, event)
        if event.kind == .up {
            guard let index = held.lastIndex(of: event.code) else { throw DesktopFailure("invalid_request") }
            held.remove(at: index)
        }
    }
    guard held.isEmpty else { throw DesktopFailure("input_failed") }
    try checkpoint()
}

func mouseEventPlan(_ action: String, button: String?, startX: Double, startY: Double,
                    x: Double?, y: Double?) throws -> [OwnedEvent] {
    let code: UInt16
    switch button ?? "left" {
    case "left": code = 0
    case "right": code = 1
    case "middle": code = 2
    default: throw DesktopFailure("invalid_request")
    }
    guard startX.isFinite, startY.isFinite, (x == nil) == (y == nil),
          x?.isFinite != false, y?.isFinite != false else { throw DesktopFailure("invalid_request") }
    let endX = x ?? startX, endY = y ?? startY
    func event(_ kind: OwnedEventKind, _ x: Double, _ y: Double) -> OwnedEvent {
        OwnedEvent(kind: kind, code: code, x: x, y: y)
    }
    switch action {
    case "mouse_click":
        var events: [OwnedEvent] = []
        if x != nil { events.append(event(.move, endX, endY)) }
        events.append(event(.down, endX, endY))
        events.append(event(.up, endX, endY))
        return events
    case "mouse_drag":
        guard x != nil else { throw DesktopFailure("invalid_request") }
        var events = [event(.down, startX, startY)]
        for step in 1...24 {
            let progress = Double(step) / 24
            events.append(event(.drag, startX + (endX - startX) * progress,
                                 startY + (endY - startY) * progress))
        }
        events.append(event(.up, endX, endY))
        return events
    default: throw DesktopFailure("unsupported_action")
    }
}

#if os(macOS) && !PI_INPUT_TESTS
func performOwnedInput(_ request: Request) throws -> [String: Any] {
    try requireAccessibility()
    let deadline = ProcessInfo.processInfo.systemUptime + 20
    try checkInputLease(deadline)
    guard let start = CGEvent(source: nil)?.location else { throw DesktopFailure("input_failed") }
    let events = try mouseEventPlan(request.action, button: request.button,
        startX: Double(start.x), startY: Double(start.y),
        x: request.x.map { Double($0) }, y: request.y.map { Double($0) })
    let displays = try displayList()
    // Validate the entire path before a press, including gaps between displays.
    for event in events {
        guard displays.contains(where: { display in
            guard let x = display["x"] as? Double, let y = display["y"] as? Double,
                  let w = display["width"] as? Double, let h = display["height"] as? Double else { return false }
            return event.x >= x && event.x < x + w && event.y >= y && event.y < y + h
        }) else { throw DesktopFailure("pointer_outside_displays") }
    }
    let button = CGMouseButton(rawValue: UInt32(events[0].code))!
    guard !CGEventSource.buttonState(.combinedSessionState, button: button),
          CGEventSource.flagsState(.combinedSessionState)
            .intersection([.maskShift, .maskControl, .maskAlternate, .maskCommand]).isEmpty else {
        throw DesktopFailure("input_conflict")
    }
    let target = try request.window_id.map(resolveWindow)
    func checkpoint() throws {
        try checkInputLease(deadline)
        if let (pid, window) = target {
            let (actualPID, actualWindow) = try focusedWindow()
            guard pid == actualPID, CFEqual(window, actualWindow) else {
                throw DesktopFailure("target_not_focused")
            }
        }
        // The focus query may block. Recheck cancellation before each event.
        try checkInputLease(deadline)
    }
    func nativeEvent(_ event: OwnedEvent) throws -> CGEvent {
        let kind: CGEventType
        switch event.kind {
        case .move: kind = .mouseMoved
        case .down: kind = button == .left ? .leftMouseDown : button == .right ? .rightMouseDown : .otherMouseDown
        case .up: kind = button == .left ? .leftMouseUp : button == .right ? .rightMouseUp : .otherMouseUp
        case .drag: kind = button == .left ? .leftMouseDragged : button == .right ? .rightMouseDragged : .otherMouseDragged
        }
        guard let result = CGEvent(mouseEventSource: nil, mouseType: kind,
            mouseCursorPosition: CGPoint(x: event.x, y: event.y), mouseButton: button) else {
            throw DesktopFailure("input_failed")
        }
        result.flags = []
        if event.kind == .down || event.kind == .up {
            result.setIntegerValueField(.mouseEventClickState, value: 1)
        }
        return result
    }
    // Allocate normal and emergency release events BEFORE the first press.
    let prepared = try events.map(nativeEvent)
    let emergency = try nativeEvent(OwnedEvent(kind: .up, code: events[0].code,
        x: Double(start.x), y: Double(start.y)))
    try dispatchOwnedEvents(events, checkpoint: checkpoint, post: { index, event in
        prepared[index].post(tap: .cghidEventTap)
        if event.kind == .drag || event.kind == .down { Thread.sleep(forTimeInterval: 0.01) }
    }, release: { _ in
        // Use the actual pointer on abort, never jump back to the drag origin.
        if let position = CGEvent(source: nil)?.location { emergency.location = position }
        emergency.post(tap: .cghidEventTap)
    })
    guard let end = events.last, let actual = CGEvent(source: nil)?.location,
          abs(Double(actual.x) - end.x) <= 0.5, abs(Double(actual.y) - end.y) <= 0.5 else {
        throw DesktopFailure("pointer_unconfirmed")
    }
    var result: [String: Any] = ["dispatched": true, "released": true, "button": request.button ?? "left",
            "position": ["x": Double(actual.x), "y": Double(actual.y)],
            "application_effect": "not_verified"]
    if let id = request.window_id { result["window_id"] = id }
    return result
}
#endif
