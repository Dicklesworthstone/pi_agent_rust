struct DesktopFailure: Error { let code: String; init(_ code: String) { self.code = code } }
var cases = 0
func check(_ value: Bool) { precondition(value); cases += 1 }
for button in ["left", "right", "middle"] {
    let click = try mouseEventPlan("mouse_click", button: button, startX: -20, startY: 50, x: nil, y: nil)
    check(click.count == 2 && click[0].kind == .down && click[1].kind == .up)
    check(click[0].x == -20 && click[1].y == 50)
    let drag = try mouseEventPlan("mouse_drag", button: button, startX: -20, startY: 50, x: 200, y: 100)
    check(drag.count == 26 && drag.first!.kind == .down && drag.last!.kind == .up)
    check(drag.last!.x == 200 && drag.last!.y == 100)
    // Interrupt at every checkpoint and every post, including after a down
    // has reached a sink that then throws. No release may require checkpoint.
    for failureAt in 0...drag.count {
        var checkpointCount = 0
        var live: Set<UInt16> = []
        var posts = 0
        do {
            try dispatchOwnedEvents(drag, checkpoint: {
                defer { checkpointCount += 1 }
                if checkpointCount == failureAt { throw DesktopFailure("cancelled") }
            }, post: { _, event in
                posts += 1
                if event.kind == .down { live.insert(event.code) }
                if event.kind == .up { live.remove(event.code) }
            }, release: { code in live.remove(code) })
        } catch {}
        check(live.isEmpty)
        check(posts == min(failureAt, drag.count))
    }
    for failureAt in 0..<drag.count {
        var live: Set<UInt16> = []
        do {
            try dispatchOwnedEvents(drag, checkpoint: {}, post: { index, event in
                if event.kind == .down { live.insert(event.code) }
                if event.kind == .up { live.remove(event.code) }
                if index == failureAt { throw DesktopFailure("sink failure") }
            }, release: { code in live.remove(code) })
        } catch {}
        check(live.isEmpty)
    }
}
for (action, button, x, y) in [("mouse_drag", "left", nil, nil),
    ("mouse_click", "bad", nil, nil), ("mouse_click", "left", 1.0, nil),
    ("key_type", "left", nil, nil)] as [(String, String, Double?, Double?)] {
    do { _ = try mouseEventPlan(action, button: button, startX: 0, startY: 0, x: x, y: y); fatalError("accepted invalid plan") }
    catch { cases += 1 }
}
print("mouse plan and lifecycle assertions: \(cases)")

let mouseCases = cases
func checkKeyboardCancellation(_ strokes: [KeyboardStroke]) throws {
    let (names, events) = try keyboardLifecycle(strokes)
    for failureAt in 0...events.count {
        var checks = 0
        var live: [UInt16] = []
        var releaseOrder: [UInt16] = []
        var posted = 0
        do {
            try dispatchOwnedEvents(events, checkpoint: {
                defer { checks += 1 }
                if checks == failureAt { throw DesktopFailure("cancelled") }
            }, post: { _, event in
                posted += 1
                if event.kind == .down { live.append(event.code) }
                if event.kind == .up { live.removeAll { $0 == event.code } }
            }, release: { code in
                check(live.last == code)
                releaseOrder.append(code)
                live.removeAll { $0 == code }
            })
        } catch {}
        check(live.isEmpty)
        check(posted == min(failureAt, events.count))
        check(releaseOrder.allSatisfy { Int($0) < names.count })
    }
    for failureAt in events.indices {
        var live: Set<UInt16> = []
        do {
            try dispatchOwnedEvents(events, checkpoint: {}, post: { index, event in
                if event.kind == .down { live.insert(event.code) }
                if event.kind == .up { live.remove(event.code) }
                if index == failureAt { throw DesktopFailure("after post") }
            }, release: { live.remove($0) })
        } catch {}
        check(live.isEmpty)
    }
}

// Every modifier subset and every event boundary use the production lifecycle
// projection. Symbolic plans use the same parser that the native adapter uses.
let modifierNames = ["ctrl", "alt", "shift", "super"]
for mask in 0..<16 {
    let modifiers = modifierNames.enumerated().filter { mask & (1 << $0.offset) != 0 }.map { $0.element }
    let chord = (modifiers + ["c"]).joined(separator: "+")
    let strokes = try chordEventPlan(chord)
    check(strokes.count == modifiers.count * 2 + 2)
    check(strokes[modifiers.count].key == "c" && strokes[modifiers.count].down)
    check(strokes[modifiers.count].modifiers == modifiers)
    check(strokes.last!.modifiers.isEmpty)
    check(strokes.suffix(modifiers.count).map { $0.key } == Array(modifiers.reversed()))
    try checkKeyboardCancellation(strokes)
}
let uppercase = try chordEventPlan("C")
check(uppercase.count == 4 && uppercase.first!.key == "shift")
check(try chordEventPlan("Ctrl+C").count == 4)
check(try chordEventPlan("Control+Command+C")[2].modifiers == ["ctrl", "super"])
check(try chordEventPlan("Ctrl+Shift+C").count == 6)
for key in Array("abcdefghijklmnopqrstuvwxyz0123456789").map(String.init)
    + ["Return", "Tab", "Escape", "Space", "Backspace", "Delete", "Insert", "Home", "End",
       "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "PageUp", "PageDown"]
    + (1...20).map({ "F\($0)" }) {
    let strokes = try chordEventPlan(key)
    check(strokes.count == 2 && strokes[0].key == strokes[1].key)
}
for key in ["", "ctrl+ctrl+c", "Ctrl+Control+C", "ctrl+", "+a", "ctrl++c", "F21", "F24",
            "F01", "ctrl+exec", "exec", "cmd+ø", "a\n", "ctrl+alt+shift+super+ctrl+c", String(repeating: "a", count: 129)] {
    do { _ = try chordEventPlan(key); fatalError("accepted invalid chord") }
    catch { cases += 1 }
}
let literal = "a\u{0301}🦀👩‍💻\n\t$(literal)"
let plans = try textEventPlan(literal)
check(plans.count == literal.unicodeScalars.count)
var roundtrip = ""
for pair in plans {
    check(pair.count == 2 && pair[0].down && !pair[1].down)
    check(pair[0].key == pair[1].key && pair[0].unicode == pair[1].unicode)
    check(pair[0].modifiers.isEmpty && pair[1].modifiers.isEmpty)
    if pair[0].key == "unicode" {
        check((1...2).contains(pair[0].unicode.count))
        roundtrip += String(decoding: pair[0].unicode, as: UTF16.self)
    } else { roundtrip += pair[0].key == "return" ? "\n" : "\t" }
    try checkKeyboardCancellation(pair)
}
check(roundtrip == literal)
check(try textEventPlan("").isEmpty)
check(try textEventPlan(String(repeating: "🦀", count: 4096)).count == 4096)
for value in ["\0", "\r", "\u{1b}", "clean\u{7f}tail", String(repeating: "x", count: 4097)] {
    do { _ = try textEventPlan(value); fatalError("accepted invalid text") }
    catch { cases += 1 }
}
print("keyboard plan and lifecycle assertions: \(cases - mouseCases); total: \(cases)")
