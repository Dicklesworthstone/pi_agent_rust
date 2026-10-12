struct DesktopFailure: Error { let code: String; init(_ code: String) { self.code = code } }
var cases = 0
func check(_ value: @autoclosure () -> Bool) { precondition(value()); cases += 1 }
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
