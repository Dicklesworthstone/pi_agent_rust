// One owned input transaction. stdin is a liveness lease after its JSON line.
// Only this process emits input; EOF stops new events before deferred releases.
import Foundation
#if os(macOS)
import Darwin
import CoreGraphics
import Carbon
#else
import Glibc
#endif

let ownedInputActions = ["mouse_click", "mouse_drag", "key_type", "key_press"]
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
    for _ in 0..<4 {
        var byte: UInt8 = 0
        let count = read(STDIN_FILENO, &byte, 1)
        // Exactly one frame is authorized. EOF and a second command revoke it.
        if count >= 0 { throw DesktopFailure("input_cancelled") }
        if errno == EAGAIN || errno == EWOULDBLOCK { return }
        if errno != EINTR { throw DesktopFailure("input_channel") }
    }
    // An interrupted observation is not affirmative proof of a live lease.
    throw DesktopFailure("input_channel")
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

// Keyboard names are logical tokens here; only the native boundary translates
// them using Carbon's named virtual-key constants. No guessed numeric keycodes.
struct KeyboardStroke {
    let key: String
    let down: Bool
    let modifiers: [String]
    var unicode: [UInt16] = []
}

func chordEventPlan(_ chord: String) throws -> [KeyboardStroke] {
    guard !chord.isEmpty, chord.utf8.count <= 128 else { throw DesktopFailure("invalid_request") }
    let parts = chord.split(separator: "+", omittingEmptySubsequences: false).map(String.init)
    guard (1...5).contains(parts.count) else { throw DesktopFailure("invalid_request") }
    var modifiers: [String] = []
    for part in parts.dropLast() {
        let modifier: String
        switch part.lowercased() {
        case "ctrl", "control": modifier = "ctrl"
        case "alt", "option": modifier = "alt"
        case "shift": modifier = "shift"
        case "super", "meta", "cmd", "command": modifier = "super"
        default: throw DesktopFailure("unsupported_key")
        }
        guard !modifiers.contains(modifier) else { throw DesktopFailure("invalid_request") }
        modifiers.append(modifier)
    }
    let last = parts.last!
    let key: String
    switch last.lowercased() {
    case "enter", "return": key = "return"
    case "escape", "esc": key = "escape"
    case "pageup", "prior": key = "pageup"
    case "pagedown", "next": key = "pagedown"
    case "arrowleft": key = "left"
    case "arrowright": key = "right"
    case "arrowup": key = "up"
    case "arrowdown": key = "down"
    case "tab", "space", "backspace", "delete", "insert", "home", "end", "left", "right", "up", "down":
        key = last.lowercased()
    default:
        if last.utf8.count == 1, let byte = last.utf8.first,
           (65...90).contains(byte) || (97...122).contains(byte) || (48...57).contains(byte) {
            key = last.lowercased()
            // Bare uppercase letters mean Shift+letter. Ctrl+C remains Ctrl+c;
            // callers must include Shift explicitly for a shifted shortcut.
            if modifiers.isEmpty && (65...90).contains(byte) { modifiers.append("shift") }
        } else if last.hasPrefix("F"), let number = Int(last.dropFirst()),
                  (1...20).contains(number), last == "F\(number)" {
            key = last.lowercased()
        } else { throw DesktopFailure("unsupported_key") }
    }
    var events: [KeyboardStroke] = []
    var active: [String] = []
    for modifier in modifiers {
        active.append(modifier)
        events.append(KeyboardStroke(key: modifier, down: true, modifiers: active))
    }
    events.append(KeyboardStroke(key: key, down: true, modifiers: active))
    events.append(KeyboardStroke(key: key, down: false, modifiers: active))
    for modifier in modifiers.reversed() {
        active.removeAll { $0 == modifier }
        events.append(KeyboardStroke(key: modifier, down: false, modifiers: active))
    }
    return events
}

func textEventPlan(_ text: String) throws -> [[KeyboardStroke]] {
    guard text.unicodeScalars.count <= 4096, !text.unicodeScalars.contains(where: {
        $0.properties.generalCategory == .control && $0.value != 10 && $0.value != 9
    }) else { throw DesktopFailure("invalid_request") }
    // Preserve scalar order, combining characters and complete surrogate pairs.
    // Newline and Tab are actual navigation keys; other text is literal Unicode.
    return text.unicodeScalars.map { scalar in
        let key = scalar.value == 10 ? "return" : scalar.value == 9 ? "tab" : "unicode"
        let units = key == "unicode" ? Array(String(scalar).utf16) : []
        return [KeyboardStroke(key: key, down: true, modifiers: [], unicode: units),
                KeyboardStroke(key: key, down: false, modifiers: [], unicode: units)]
    }
}

func keyboardLifecycle(_ strokes: [KeyboardStroke]) throws -> ([String], [OwnedEvent]) {
    var names: [String] = []
    for stroke in strokes where !names.contains(stroke.key) { names.append(stroke.key) }
    guard strokes.count <= 10, names.count <= 5 else { throw DesktopFailure("invalid_request") }
    let events = strokes.map { stroke in
        OwnedEvent(kind: stroke.down ? .down : .up, code: UInt16(names.firstIndex(of: stroke.key)!), x: 0, y: 0)
    }
    return (names, events)
}

#if os(macOS) && !PI_INPUT_TESTS
func inputCheckpoint(_ request: Request, deadline: Double) throws -> () throws -> Void {
    let target = try request.window_id.map(resolveWindow)
    return {
        try checkInputLease(deadline)
        if let (pid, window) = target {
            let (actualPID, actualWindow) = try focusedWindow()
            guard pid == actualPID, CFEqual(window, actualWindow) else {
                throw DesktopFailure("target_not_focused")
            }
        }
        // Focus queries can block. Cancellation is checked again before input.
        try checkInputLease(deadline)
    }
}

func performOwnedMouse(_ request: Request) throws -> [String: Any] {
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
    let checkpoint = try inputCheckpoint(request, deadline: deadline)
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

// Carbon identifiers describe physical ANSI-US key positions, not the user's
// active keyboard layout. Build the lookup once, not for every typed scalar.
let keyboardKeyCodes: [String: Int] = [
        "unicode": kVK_ANSI_A,
        "a": kVK_ANSI_A, "b": kVK_ANSI_B, "c": kVK_ANSI_C, "d": kVK_ANSI_D,
        "e": kVK_ANSI_E, "f": kVK_ANSI_F, "g": kVK_ANSI_G, "h": kVK_ANSI_H,
        "i": kVK_ANSI_I, "j": kVK_ANSI_J, "k": kVK_ANSI_K, "l": kVK_ANSI_L,
        "m": kVK_ANSI_M, "n": kVK_ANSI_N, "o": kVK_ANSI_O, "p": kVK_ANSI_P,
        "q": kVK_ANSI_Q, "r": kVK_ANSI_R, "s": kVK_ANSI_S, "t": kVK_ANSI_T,
        "u": kVK_ANSI_U, "v": kVK_ANSI_V, "w": kVK_ANSI_W, "x": kVK_ANSI_X,
        "y": kVK_ANSI_Y, "z": kVK_ANSI_Z,
        "0": kVK_ANSI_0, "1": kVK_ANSI_1, "2": kVK_ANSI_2, "3": kVK_ANSI_3,
        "4": kVK_ANSI_4, "5": kVK_ANSI_5, "6": kVK_ANSI_6, "7": kVK_ANSI_7,
        "8": kVK_ANSI_8, "9": kVK_ANSI_9,
        "return": kVK_Return, "escape": kVK_Escape, "tab": kVK_Tab, "space": kVK_Space,
        "backspace": kVK_Delete, "delete": kVK_ForwardDelete, "insert": kVK_Help,
        "home": kVK_Home, "end": kVK_End, "left": kVK_LeftArrow, "right": kVK_RightArrow,
        "up": kVK_UpArrow, "down": kVK_DownArrow, "pageup": kVK_PageUp, "pagedown": kVK_PageDown,
        "shift": kVK_Shift, "ctrl": kVK_Control, "alt": kVK_Option, "super": kVK_Command,
        "f1": kVK_F1, "f2": kVK_F2, "f3": kVK_F3, "f4": kVK_F4, "f5": kVK_F5,
        "f6": kVK_F6, "f7": kVK_F7, "f8": kVK_F8, "f9": kVK_F9, "f10": kVK_F10,
        "f11": kVK_F11, "f12": kVK_F12, "f13": kVK_F13, "f14": kVK_F14, "f15": kVK_F15,
        "f16": kVK_F16, "f17": kVK_F17, "f18": kVK_F18, "f19": kVK_F19, "f20": kVK_F20,
]

func virtualKey(_ key: String) throws -> CGKeyCode {
    guard let code = keyboardKeyCodes[key], let narrowed = CGKeyCode(exactly: code) else {
        throw DesktopFailure("unsupported_key")
    }
    return narrowed
}

func keyboardFlags(_ modifiers: [String]) throws -> CGEventFlags {
    var flags: CGEventFlags = []
    for modifier in modifiers {
        switch modifier {
        case "shift": flags.insert(.maskShift)
        case "ctrl": flags.insert(.maskControl)
        case "alt": flags.insert(.maskAlternate)
        case "super": flags.insert(.maskCommand)
        default: throw DesktopFailure("unsupported_key")
        }
    }
    return flags
}

func postKeyboardTransaction(_ strokes: [KeyboardStroke], source: CGEventSource,
                             checkpoint: () throws -> Void) throws {
    let (names, events) = try keyboardLifecycle(strokes)
    func makeEvent(_ stroke: KeyboardStroke) throws -> CGEvent {
        guard let event = CGEvent(keyboardEventSource: source, virtualKey: try virtualKey(stroke.key),
                                  keyDown: stroke.down) else { throw DesktopFailure("input_failed") }
        event.flags = try keyboardFlags(stroke.modifiers)
        if ["ctrl", "alt", "shift", "super"].contains(stroke.key) { event.type = .flagsChanged }
        event.setIntegerValueField(.keyboardEventAutorepeat, value: 0)
        if !stroke.unicode.isEmpty {
            stroke.unicode.withUnsafeBufferPointer { units in
                event.keyboardSetUnicodeString(stringLength: units.count, unicodeString: units.baseAddress)
            }
        }
        return event
    }
    // Allocate every event and emergency release before the transaction's
    // first down. Text uses one scalar transaction at a time, not 8192 CGEvents.
    let prepared = try strokes.map(makeEvent)
    let emergency = try names.map { try makeEvent(KeyboardStroke(key: $0, down: false, modifiers: [])) }
    try dispatchOwnedEvents(events, checkpoint: checkpoint, post: { index, _ in
        prepared[index].post(tap: .cghidEventTap)
        Thread.sleep(forTimeInterval: 0.001)
    }, release: { code in
        emergency[Int(code)].post(tap: .cghidEventTap)
    })
}

func performOwnedKeyboard(_ request: Request) throws -> [String: Any] {
    let transactions: [[KeyboardStroke]]
    if request.action == "key_type" {
        guard let text = request.text else { throw DesktopFailure("invalid_request") }
        transactions = try textEventPlan(text)
    } else {
        guard request.action == "key_press", let chord = request.key else { throw DesktopFailure("invalid_request") }
        transactions = [try chordEventPlan(chord)]
    }
    try requireAccessibility()
    let deadline = ProcessInfo.processInfo.systemUptime + 20
    let checkpoint = try inputCheckpoint(request, deadline: deadline)
    try checkpoint()
    guard let source = CGEventSource(stateID: .combinedSessionState) else { throw DesktopFailure("input_failed") }
    let keys = Set(transactions.flatMap { $0.map { $0.key } })
    if !keys.isEmpty {
        guard CGEventSource.flagsState(.combinedSessionState)
            .intersection([.maskShift, .maskControl, .maskAlternate, .maskCommand, .maskAlphaShift, .maskSecondaryFn]).isEmpty else {
            throw DesktopFailure("input_conflict")
        }
        for key in keys {
            guard !CGEventSource.keyState(.combinedSessionState, key: try virtualKey(key)) else {
                throw DesktopFailure("input_conflict")
            }
        }
    }
    for transaction in transactions {
        try checkpoint()
        try postKeyboardTransaction(transaction, source: source, checkpoint: checkpoint)
    }
    try checkpoint()
    var result: [String: Any] = ["dispatched": true, "released": true, "application_effect": "not_verified"]
    if request.action == "key_type" { result["char_count"] = transactions.count }
    else { result["key"] = request.key! }
    if let id = request.window_id { result["window_id"] = id }
    return result
}

func performOwnedInput(_ request: Request) throws -> [String: Any] {
    if request.action == "key_type" || request.action == "key_press" {
        return try performOwnedKeyboard(request)
    }
    return try performOwnedMouse(request)
}
#endif
