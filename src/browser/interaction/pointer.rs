//! Native pointer sequences on one resolved node, never JavaScript clicks.
//!
//! Double-click steps retain the original document and node. Every admitted
//! press gets one release attempt; failures never replay an earlier click.

use super::{Cdp, Document, References, document, element_call, frame_input, resolve, selectors};
use crate::agent_cx::AgentCx;
use crate::browser::{output, required};
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    const fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Middle => "middle",
            Self::Right => "right",
        }
    }

    const fn mask(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 4,
            Self::Right => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Options {
    button: Button,
    count: u8,
    modifiers: u8,
}

fn usage() -> Error {
    Error::tool(
        "browser",
        "BROWSER_POINTER_USAGE: click accepts button (left/middle/right), click_count (1 or 2; double-click requires left), and distinct modifiers (Alt/Control/Meta/Shift)",
    )
}

impl Options {
    fn parse(args: &Value) -> Result<Self> {
        let button = match args.get("button") {
            None => Button::Left,
            Some(Value::String(value)) => match value.as_str() {
                "left" => Button::Left,
                "middle" => Button::Middle,
                "right" => Button::Right,
                _ => return Err(usage()),
            },
            _ => return Err(usage()),
        };
        let count = match args.get("click_count") {
            None => 1,
            Some(value) => match value.as_u64() {
                Some(1) => 1,
                Some(2) => 2,
                _ => return Err(usage()),
            },
        };
        if count == 2 && button != Button::Left {
            return Err(usage());
        }
        let mut modifiers = 0;
        if let Some(value) = args.get("modifiers") {
            let keys = value
                .as_array()
                .filter(|keys| keys.len() <= 4)
                .ok_or_else(usage)?;
            for key in keys {
                let bit = match key.as_str() {
                    Some("Alt") => 1,
                    Some("Control") => 2,
                    Some("Meta") => 4,
                    Some("Shift") => 8,
                    _ => return Err(usage()),
                };
                if modifiers & bit != 0 {
                    return Err(usage());
                }
                modifiers |= bit;
            }
        }
        Ok(Self {
            button,
            count,
            modifiers,
        })
    }

    fn event(self, x: f64, y: f64, number: u8, pressed: bool) -> Value {
        json!({
            "type":if pressed { "mousePressed" } else { "mouseReleased" },
            "x":x,"y":y,"button":self.button.name(),
            "buttons":if pressed { self.button.mask() } else { 0 },
            "clickCount":number,"modifiers":self.modifiers,
        })
    }
}

/// Public preflight precedes launch, connection and the explicit mock backend.
/// Pointer options on any other action are errors, never silently ignored.
pub(in crate::browser) fn validate_input(args: &Value) -> Result<bool> {
    if args.get("action").and_then(Value::as_str) == Some("hover") {
        let fields = args.as_object().ok_or_else(hover_usage)?;
        if fields.keys().any(|key| {
            !matches!(
                key.as_str(),
                "action" | "tab" | "selector" | "modifiers" | "timeout_ms" | "wait_after"
            )
        }) || args
            .get("tab")
            .and_then(Value::as_str)
            .is_none_or(|tab| tab.is_empty() || tab.len() > 256)
            || args
                .get("selector")
                .and_then(Value::as_str)
                .is_none_or(|selector| {
                    selector.is_empty() || selector.len() > 64 * 1024 || selector.contains('\0')
                })
        {
            return Err(hover_usage());
        }
        Options::parse(args)?;
        return Ok(true);
    }
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if is_gesture(action) {
        Gesture::parse(args)?;
        return Ok(true);
    }
    if ["target_selector", "drag_steps", "duration_ms"]
        .iter()
        .any(|field| args.get(*field).is_some())
    {
        return Err(gesture_usage());
    }
    let explicit = ["button", "click_count", "modifiers"]
        .iter()
        .any(|name| args.get(*name).is_some());
    if explicit {
        if args.get("action").and_then(Value::as_str) != Some("click") {
            return Err(usage());
        }
        Options::parse(args)?;
    }
    Ok(explicit)
}

fn hover_usage() -> Error {
    Error::tool(
        "browser",
        "BROWSER_HOVER_USAGE: hover requires an explicit tab and selector; optional modifiers, timeout_ms and same-document wait_after only; frames and button presses are not supported",
    )
}

/// A real pointer move can reveal CSS menus and trigger trusted pointer/mouse
/// enter handlers. It does not press a button, synthesize a DOM event, or retry.
pub(super) async fn hover(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    id: u64,
    args: &Value,
) -> Result<ToolOutput> {
    validate_input(args)?;
    if cdp.frame_selected() {
        return Err(hover_usage());
    }
    let options = Options::parse(args)?;
    let expected = document(owner, cdp).await?;
    cdp.command(owner, "Page.bringToFront", json!({})).await?;
    verify_document(owner, cdp, &expected).await?;
    let (x, y) = point(owner, cdp, id).await?;
    verify_document(owner, cdp, &expected).await?;
    cdp.command(
        owner,
        "Input.dispatchMouseEvent",
        hover_event(x, y, options.modifiers),
    )
    .await?;
    // Do not resolve the selector again after page handlers have run. Optional
    // wait_after inspects the outcome without replaying this pointer movement.
    Ok(output(
        format!("Dispatched native hover in tab {tab}; application outcome is not verified"),
        json!({"action":"hover", "tab":tab, "selector":required(args, "selector")?,
            "backend":"cdp", "input_method":"native-pointer", "modifier_mask":options.modifiers,
            "dispatch_acknowledged":true, "delivery_verified":false}),
    ))
}

fn hover_event(x: f64, y: f64, modifiers: u8) -> Value {
    json!({"type":"mouseMoved", "x":x, "y":y,
        "button":"none", "buttons":0, "modifiers":modifiers})
}

async fn verify_document(owner: &AgentCx, cdp: &mut Cdp, expected: &Document) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| Error::tool("browser", "BROWSER_POINTER_CANCELLED"))?;
    if document(owner, cdp).await? != *expected {
        return Err(Error::tool(
            "browser",
            "BROWSER_POINTER_STALE: page changed during pointer sequence",
        ));
    }
    Ok(())
}

pub(in crate::browser) fn is_gesture(action: &str) -> bool {
    action == "drag"
}

fn gesture_usage() -> Error {
    Error::tool(
        "browser",
        "BROWSER_GESTURE_USAGE: hover/drag require an explicit main-document tab and selector; drag also requires target_selector, with optional drag_steps 1..64 and duration_ms 0..5000; only modifiers, wait_after and timeout_ms are shared options",
    )
}

#[derive(Debug)]
struct Gesture<'a> {
    target: Option<&'a str>,
    steps: u32,
    duration_ms: u64,
    modifiers: u8,
}

impl<'a> Gesture<'a> {
    fn parse(args: &'a Value) -> Result<Self> {
        let fields = args.as_object().ok_or_else(gesture_usage)?;
        let drag = match args["action"].as_str() {
            Some("hover") => false,
            Some("drag") => true,
            _ => return Err(gesture_usage()),
        };
        if fields.keys().any(|field| {
            !matches!(
                field.as_str(),
                "action" | "tab" | "selector" | "modifiers" | "wait_after" | "timeout_ms"
            ) && !(drag
                && matches!(
                    field.as_str(),
                    "target_selector" | "drag_steps" | "duration_ms"
                ))
        }) {
            return Err(gesture_usage());
        }
        args["tab"]
            .as_str()
            .filter(|tab| !tab.is_empty() && tab.len() <= 256 && !tab.chars().any(char::is_control))
            .ok_or_else(gesture_usage)?;
        let selector = |key: &str| -> Result<&'a str> {
            let value = args[key]
                .as_str()
                .filter(|value| {
                    !value.is_empty() && value.len() <= 64 * 1024 && !value.contains('\0')
                })
                .ok_or_else(gesture_usage)?;
            selectors::parse(value)?;
            Ok(value)
        };
        selector("selector")?;
        let target = if drag {
            Some(selector("target_selector")?)
        } else {
            None
        };
        let steps = match args.get("drag_steps") {
            None => 12,
            Some(value) => value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| (1..=64).contains(value))
                .ok_or_else(gesture_usage)?,
        };
        let duration_ms = match args.get("duration_ms") {
            None => 300,
            Some(value) => value
                .as_u64()
                .filter(|value| *value <= 5000)
                .ok_or_else(gesture_usage)?,
        };
        // The strict field admission above prevents button/count overrides.
        // Reuse exactly the click modifier vocabulary and duplicate checks.
        let modifiers = Options::parse(args)?.modifiers;
        Ok(Self {
            target,
            steps,
            duration_ms,
            modifiers,
        })
    }
}

fn motion(point: (f64, f64), held: bool, modifiers: u8) -> Value {
    json!({
        "type":"mouseMoved", "x":point.0, "y":point.1,
        // Chromium needs the pressed button as well as its mask for native
        // HTML drag initiation. `none` with buttons:1 only drives pointer events.
        "button":if held { "left" } else { "none" },
        "buttons":if held { 1 } else { 0 }, "modifiers":modifiers,
    })
}

fn interpolate(from: (f64, f64), to: (f64, f64), step: u32, steps: u32) -> (f64, f64) {
    let fraction = f64::from(step) / f64::from(steps);
    (
        from.0 + (to.0 - from.0) * fraction,
        from.1 + (to.1 - from.1) * fraction,
    )
}

fn unchanged_point(before: (f64, f64), now: (f64, f64)) -> bool {
    (before.0 - now.0).abs() <= 0.5 && (before.1 - now.1).abs() <= 0.5
}

fn separated_points(from: (f64, f64), to: (f64, f64)) -> bool {
    // Distinct nested nodes can have the same center. Never turn a requested
    // drag into an intentional stationary click. This is a minimum movement,
    // not proof that a platform or application will accept the drag.
    (from.0 - to.0).abs().max((from.1 - to.1).abs()) >= 8.0
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Bounds {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl Bounds {
    fn parse(value: Value) -> Result<Self> {
        let value: Self = serde_json::from_value(value).map_err(|_| {
            Error::tool("browser", "BROWSER_GESTURE_GEOMETRY: invalid target bounds")
        })?;
        if [value.left, value.top, value.right, value.bottom]
            .iter()
            .any(|coordinate| !coordinate.is_finite() || !(0.0..=1_000_000.0).contains(coordinate))
            || value.right <= value.left
            || value.bottom <= value.top
        {
            return Err(Error::tool(
                "browser",
                "BROWSER_GESTURE_GEOMETRY: invalid target bounds",
            ));
        }
        Ok(value)
    }

    fn preserves(&self, other: &Self) -> bool {
        unchanged_point((self.left, self.top), (other.left, other.top))
            && unchanged_point((self.right, self.bottom), (other.right, other.bottom))
    }
}

async fn bounds(owner: &AgentCx, cdp: &mut Cdp, id: u64) -> Result<Bounds> {
    Bounds::parse(element_call(owner, cdp, id, "gesture_bounds", json!({})).await?)
}

async fn verify_drag(
    owner: &AgentCx,
    cdp: &mut Cdp,
    expected: &Document,
    source: u64,
    target: u64,
    before: &Bounds,
) -> Result<()> {
    verify_document(owner, cdp, expected).await?;
    // A source may hide or move after dragstart, but a replacement is not the
    // original gesture. Inspect identity/availability without requiring a box.
    if element_call(owner, cdp, source, "gesture_alive", json!({})).await? != true {
        return Err(Error::tool(
            "browser",
            "BROWSER_GESTURE_STALE: original source is unavailable",
        ));
    }
    if !before.preserves(&bounds(owner, cdp, target).await?) {
        return Err(Error::tool(
            "browser",
            "BROWSER_GESTURE_GEOMETRY: target moved during drag",
        ));
    }
    verify_document(owner, cdp, expected).await
}

#[derive(Default, serde::Serialize)]
struct GestureProgress {
    moves_acknowledged: u32,
    press_attempted: bool,
    press_acknowledged: bool,
    release_attempted: bool,
    release_acknowledged: bool,
    cancel_attempted: bool,
    cancel_acknowledged: bool,
}

fn gesture_result(
    action: &str,
    tab: &str,
    progress: &GestureProgress,
    result: Result<()>,
) -> ToolOutput {
    let failed = result.is_err();
    // Do not reflect arbitrary page or transport exception payloads. Native
    // counters remain available even when the gesture stopped after input.
    let error_kind = result.err().map(|error| {
        let message = error.to_string();
        if message.contains("CANCELLED") || message.contains("cancelled") {
            "cancelled"
        } else if message.contains("BROWSER_POINTER_STALE") {
            "document_changed"
        } else if message.contains("BROWSER_GESTURE_GEOMETRY") {
            "target_geometry"
        } else {
            "gesture_interrupted"
        }
    });
    let mut result = output(
        if failed {
            "BROWSER_GESTURE_INTERRUPTED: stopped without replay. Pointer or page effects may already have occurred; inspect the page before another action."
        } else {
            "Native pointer gesture acknowledged. Application outcome is not verified; use an explicit wait_after condition or inspect the result."
        },
        json!({"action":action,"tab":tab,"backend":"cdp","input_method":"native-pointer",
            "dispatch_acknowledged":!failed,"delivery_verified":false,"action_replayed":false,
            "error_kind":error_kind,"gesture":progress}),
    );
    result.is_error = failed;
    result
}

/// Main-document gestures retain both nodes and their original connection.
/// No synthetic DragEvents, invented DataTransfer payload, retries or scrolling
/// while held. The session's existing outer watchdog owns total time and drop.
pub(super) async fn gesture(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    refs: Option<&References>,
    args: &Value,
) -> Result<ToolOutput> {
    let options = Gesture::parse(args)?;
    if cdp.frame_selected() {
        return Err(gesture_usage());
    }
    let action = required(args, "action")?;
    let expected = document(owner, cdp).await?;
    let original_page = cdp.page_session_id()?.to_owned();
    cdp.command(owner, "Page.bringToFront", json!({})).await?;
    verify_document(owner, cdp, &expected).await?;
    let source = resolve(owner, cdp, required(args, "selector")?, refs)
        .await?
        .ok_or_else(|| Error::tool("browser", "BROWSER_GESTURE_MISSING: source did not match"))?;
    let target = if let Some(selector) = options.target {
        let target = resolve(owner, cdp, selector, refs).await?.ok_or_else(|| {
            Error::tool("browser", "BROWSER_GESTURE_MISSING: target did not match")
        })?;
        if target == source {
            return Err(Error::tool(
                "browser",
                "BROWSER_GESTURE_USAGE: source and target must be different elements",
            ));
        }
        Some(target)
    } else {
        None
    };
    cdp.command(
        owner,
        "DOM.scrollIntoViewIfNeeded",
        json!({"backendNodeId":source}),
    )
    .await?;
    let start = point(owner, cdp, source).await?;
    // Check the destination before any mouse input. It must be visible in the
    // source viewport already; dragging never scrolls to a different target.
    let destination = if let Some(target) = target {
        let end = point(owner, cdp, target).await?;
        if !separated_points(start, end) {
            return Err(Error::tool(
                "browser",
                "BROWSER_GESTURE_GEOMETRY: source and target points must be at least 8 CSS pixels apart",
            ));
        }
        Some((end, bounds(owner, cdp, target).await?))
    } else {
        None
    };
    verify_document(owner, cdp, &expected).await?;
    let mut progress = GestureProgress::default();
    let click = Options {
        button: Button::Left,
        count: 1,
        modifiers: options.modifiers,
    };
    let mut last_attempted = start;
    let result: Result<()> = async {
        cdp.command(
            owner,
            "Input.dispatchMouseEvent",
            motion(start, false, options.modifiers),
        )
        .await?;
        progress.moves_acknowledged += 1;
        verify_document(owner, cdp, &expected).await?;
        let Some((end, target_bounds)) = destination.as_ref() else {
            return Ok(());
        };
        let target = target.expect("destination came from the resolved target");
        // Hover handlers may rearrange the DOM before the press. Never press
        // at an old point or bless a new target position by resampling it.
        if !unchanged_point(start, point(owner, cdp, source).await?) {
            return Err(Error::tool(
                "browser",
                "BROWSER_GESTURE_GEOMETRY: source moved before press",
            ));
        }
        verify_drag(owner, cdp, &expected, source, target, target_bounds).await?;
        if !unchanged_point(*end, point(owner, cdp, target).await?) {
            return Err(Error::tool(
                "browser",
                "BROWSER_GESTURE_GEOMETRY: target lost its original hit point before press",
            ));
        }
        verify_document(owner, cdp, &expected).await?;
        progress.press_attempted = true;
        cdp.command(
            owner,
            "Input.dispatchMouseEvent",
            click.event(start.0, start.1, 1, true),
        )
        .await?;
        progress.press_acknowledged = true;
        let started = std::time::Instant::now();
        for step in 1..=options.steps {
            let due = started
                + std::time::Duration::from_millis(
                    options.duration_ms * u64::from(step) / u64::from(options.steps),
                );
            let delay = due.saturating_duration_since(std::time::Instant::now());
            if !delay.is_zero() {
                owner.time().sleep(delay).await;
            }
            verify_drag(owner, cdp, &expected, source, target, target_bounds).await?;
            last_attempted = interpolate(start, *end, step, options.steps);
            cdp.command(
                owner,
                "Input.dispatchMouseEvent",
                motion(last_attempted, true, options.modifiers),
            )
            .await?;
            progress.moves_acknowledged += 1;
        }
        // A first arrival can produce dragenter without dragover. A second
        // movement at the same point lets the page opt into the native drop;
        // it does not replay the press, create data or force drop acceptance.
        verify_drag(owner, cdp, &expected, source, target, target_bounds).await?;
        cdp.command(
            owner,
            "Input.dispatchMouseEvent",
            motion(*end, true, options.modifiers),
        )
        .await?;
        progress.moves_acknowledged += 1;
        verify_drag(owner, cdp, &expected, source, target, target_bounds).await?;
        if !unchanged_point(*end, point(owner, cdp, target).await?) {
            return Err(Error::tool(
                "browser",
                "BROWSER_GESTURE_GEOMETRY: target no longer has its original hit point",
            ));
        }
        verify_document(owner, cdp, &expected).await?;
        progress.release_attempted = true;
        cdp.command(
            owner,
            "Input.dispatchMouseEvent",
            click.event(end.0, end.1, 1, false),
        )
        .await?;
        progress.release_acknowledged = true;
        Ok(())
    }
    .await;
    if result.is_err() && progress.press_attempted && !progress.release_attempted {
        // Cancelling a browser-native drag avoids intentionally dropping after
        // observed drift. A custom pointer handler can still react to release.
        // Keep the original page and original cancellation/deadline authority.
        progress.cancel_attempted = true;
        progress.cancel_acknowledged = cdp
            .frame_command(owner, &original_page, "Input.cancelDragging", json!({}))
            .await
            .is_ok();
        progress.release_attempted = true;
        progress.release_acknowledged = cdp
            .frame_command(
                owner,
                &original_page,
                "Input.dispatchMouseEvent",
                click.event(last_attempted.0, last_attempted.1, 1, false),
            )
            .await
            .is_ok();
    }
    Ok(gesture_result(action, tab, &progress, result))
}

async fn point(owner: &AgentCx, cdp: &mut Cdp, id: u64) -> Result<(f64, f64)> {
    if cdp.frame_selected() {
        // Includes the existing transformed/OOP-frame projection and hit tests.
        return frame_input::click_point(owner, cdp, id).await;
    }
    let value = element_call(owner, cdp, id, "point", json!({})).await?;
    let coordinate = |key: &str| {
        value[key]
            .as_f64()
            .filter(|value| value.is_finite() && (0.0..=1_000_000.0).contains(value))
            .ok_or_else(|| {
                Error::tool(
                    "browser",
                    "BROWSER_POINTER_GEOMETRY: invalid clickable point",
                )
            })
    };
    Ok((coordinate("x")?, coordinate("y")?))
}

pub(super) async fn click(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    id: u64,
    args: &Value,
) -> Result<ToolOutput> {
    let options = Options::parse(args)?;
    let selector = required(args, "selector")?;
    let expected = document(owner, cdp).await?;
    let original_page = cdp.page_session_id()?.to_owned();
    let mut completed = 0_u8;
    let sequence: Result<()> = async {
        // Bring ordinary main-document clicks to the front too. Background
        // tabs must not rely on incidental focus left by an earlier call.
        cdp.command(owner, "Page.bringToFront", json!({})).await?;
        verify_document(owner, cdp, &expected).await?;
        for number in 1..=options.count {
            // Never re-resolve a selector after page handlers run. If the
            // original node disappeared, stop rather than clicking its replacement.
            let (x, y) = point(owner, cdp, id).await?;
            verify_document(owner, cdp, &expected).await?;
            let pressed = cdp
                .command(
                    owner,
                    "Input.dispatchMouseEvent",
                    options.event(x, y, number, true),
                )
                .await;
            let released = if pressed.is_ok() {
                // The ordinary route must keep its explicit expected-dialog
                // handling. It is still this connection's original page session.
                cdp.command(
                    owner,
                    "Input.dispatchMouseEvent",
                    options.event(x, y, number, false),
                )
                .await
            } else {
                // A lost acknowledgement does not prove the press was rejected.
                // Attempt one release on the original page, not a newly selected
                // frame. The same owner/deadline applies: cleanup is best effort.
                cdp.frame_command(
                    owner,
                    &original_page,
                    "Input.dispatchMouseEvent",
                    options.event(x, y, number, false),
                )
                .await
            };
            pressed?;
            released?;
            completed += 1;
            if number < options.count {
                verify_document(owner, cdp, &expected).await?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = sequence {
        return Err(Error::tool(
            "browser",
            format!(
                "BROWSER_POINTER_INTERRUPTED: {completed} complete click pairs acknowledged; side effects may already have occurred. No click was retried. {error}"
            ),
        ));
    }
    Ok(output(
        format!(
            "Dispatched {} {} click(s) to {selector} in tab {tab}; application outcome is not verified",
            options.count,
            options.button.name()
        ),
        json!({
            "action":"click","selector":selector,"tab":tab,"backend":"cdp",
            "button":options.button.name(),"click_count":options.count,
            "modifier_mask":options.modifiers,"input_method":"native-pointer",
            "dispatch_acknowledged":true,"delivery_verified":false,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(case: &str) {
        eprintln!(
            "{}",
            json!({"suite":"browser_pointer","case":case,"phase":"start"})
        );
    }

    #[test]
    fn hover_admission_is_explicit_and_never_accepts_ignored_input_or_frame_authority() {
        log("hover-admission");
        let args = json!({"action":"hover", "tab":"app", "selector":"role/button[name=\"Menu\" exact]", "modifiers":["Shift"]});
        assert!(validate_input(&args).unwrap());
        for field in [
            "frame",
            "button",
            "click_count",
            "text",
            "dialog_response",
            "diagnostics",
        ] {
            let mut invalid = args.clone();
            invalid[field] = json!("PRIVATE-CANARY");
            let failure = validate_input(&invalid).unwrap_err().to_string();
            assert!(failure.contains("BROWSER_HOVER_USAGE"));
            assert!(!failure.contains("PRIVATE-CANARY"));
        }
        for invalid in [
            json!({"action":"hover","selector":"#menu"}),
            json!({"action":"hover","tab":"app","selector":""}),
            json!({"action":"hover","tab":"app","selector":"#menu","modifiers":["Shift","Shift"]}),
        ] {
            assert!(validate_input(&invalid).is_err());
        }
    }

    #[test]
    fn hover_events_have_no_pressed_buttons_or_click_count() {
        log("hover-events");
        assert_eq!(
            hover_event(12.5, 48.0, 8),
            json!({
                "type":"mouseMoved", "x":12.5, "y":48.0, "button":"none", "buttons":0, "modifiers":8
            })
        );
        assert!(hover_event(0.0, 0.0, 0).get("clickCount").is_none());
    }

    #[test]
    fn defaults_and_all_three_native_buttons_have_the_correct_masks() {
        log("button-masks");
        assert_eq!(
            Options::parse(&json!({})).unwrap(),
            Options {
                button: Button::Left,
                count: 1,
                modifiers: 0
            }
        );
        for (name, mask) in [("left", 1), ("middle", 4), ("right", 2)] {
            let options = Options::parse(&json!({"button":name})).unwrap();
            assert_eq!(
                options.event(10.0, 20.0, 1, true),
                json!({
                    "type":"mousePressed","x":10.0,"y":20.0,"button":name,
                    "buttons":mask,"clickCount":1,"modifiers":0,
                })
            );
            assert_eq!(options.event(10.0, 20.0, 1, false)["buttons"], 0);
        }
    }

    #[test]
    fn double_click_counts_advance_but_are_not_an_implicit_retry() {
        log("double-click-events");
        let options = Options::parse(&json!({"click_count":2,"modifiers":["Shift"]})).unwrap();
        let events: Vec<_> = (1..=options.count)
            .flat_map(|n| {
                [
                    options.event(1.0, 2.0, n, true),
                    options.event(1.0, 2.0, n, false),
                ]
            })
            .collect();
        assert_eq!(
            events
                .iter()
                .map(|event| event["clickCount"].clone())
                .collect::<Vec<_>>(),
            vec![json!(1), json!(1), json!(2), json!(2)]
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event["buttons"].clone())
                .collect::<Vec<_>>(),
            vec![json!(1), json!(0), json!(1), json!(0)]
        );
        assert!(events.iter().all(|event| event["modifiers"] == 8));
    }

    #[test]
    fn modifiers_are_distinct_typed_and_never_insert_shortcut_text() {
        log("modifier-validation");
        let options =
            Options::parse(&json!({"modifiers":["Alt","Control","Meta","Shift"]})).unwrap();
        assert_eq!(options.modifiers, 15);
        for value in [
            json!(["Shift", "Shift"]),
            json!(["ctrl"]),
            json!([1]),
            json!("Shift"),
            Value::Null,
        ] {
            assert!(Options::parse(&json!({"modifiers":value})).is_err());
        }
        assert!(options.event(1.0, 2.0, 1, true).get("text").is_none());
    }

    #[test]
    fn malformed_and_unsupported_sequences_fail_without_echoing_values() {
        log("invalid-sequences");
        for args in [
            json!({"button":"PRIVATE-CANARY"}),
            json!({"button":null}),
            json!({"click_count":0}),
            json!({"click_count":3}),
            json!({"click_count":1.5}),
            json!({"click_count":"2"}),
            json!({"click_count":null}),
            json!({"button":"right","click_count":2}),
            json!({"button":"middle","click_count":2}),
        ] {
            let error = Options::parse(&args).unwrap_err().to_string();
            assert!(error.contains("BROWSER_POINTER_USAGE"));
            assert!(!error.contains("PRIVATE-CANARY"));
        }
    }

    #[test]
    fn preflight_never_ignores_pointer_options_on_other_actions() {
        log("preflight-actions");
        assert!(!validate_input(&json!({"action":"click","selector":"#a"})).unwrap());
        assert!(validate_input(&json!({"action":"click","button":"left"})).unwrap());
        for action in [
            "press",
            "fill",
            "set_checked",
            "evaluate",
            "snapshot",
            "scroll",
        ] {
            for field in ["button", "click_count", "modifiers"] {
                let mut args = json!({"action":action});
                args[field] = Value::Null;
                assert!(validate_input(&args).is_err());
            }
        }
    }

    #[test]
    fn gesture_options_are_typed_bounded_and_distinct_from_batch_steps() {
        log("gesture-options");
        let args = json!({"action":"drag","tab":"board","selector":"#a","target_selector":"testid/雪",
            "drag_steps":64,"duration_ms":5000,"modifiers":["Control","Shift"]});
        let parsed = Gesture::parse(&args).unwrap();
        assert_eq!(parsed.target, Some("testid/雪"));
        assert_eq!(parsed.steps, 64);
        assert_eq!(parsed.duration_ms, 5000);
        assert_eq!(parsed.modifiers, 10);
        for (key, bad) in [
            ("drag_steps", json!(0)),
            ("drag_steps", json!(65)),
            ("drag_steps", json!(1.5)),
            ("drag_steps", json!("12")),
            ("duration_ms", json!(5001)),
            ("duration_ms", Value::Null),
            ("target_selector", json!("")),
            ("tab", json!("")),
        ] {
            let mut invalid = args.clone();
            invalid[key] = bad;
            assert!(Gesture::parse(&invalid).is_err(), "field {key}");
        }
        let mut invalid = args;
        invalid["steps"] = json!(12);
        assert!(Gesture::parse(&invalid).is_err());
    }

    #[test]
    fn gestures_never_ignore_frames_dialog_consent_or_foreign_input_fields() {
        log("gesture-field-admission");
        for action in ["hover", "drag"] {
            let mut base = json!({"action":action,"tab":"board","selector":"#a"});
            if action == "drag" {
                base["target_selector"] = json!("#b");
            }
            assert!(validate_input(&base).unwrap());
            for key in [
                "frame",
                "button",
                "click_count",
                "dialog_response",
                "diagnostics",
                "text",
                "files",
                "target_tab",
            ] {
                let mut invalid = base.clone();
                invalid[key] = json!("PRIVATE-CANARY");
                let error = validate_input(&invalid).unwrap_err().to_string();
                assert!(error.contains(if action == "hover" {
                    "BROWSER_HOVER_USAGE"
                } else {
                    "BROWSER_GESTURE_USAGE"
                }));
                assert!(!error.contains("PRIVATE-CANARY"));
            }
        }
        for action in ["hover", "click", "evaluate", "scroll"] {
            let args =
                json!({"action":action,"tab":"board","selector":"#a","target_selector":"#b"});
            assert!(validate_input(&args).is_err());
        }
    }

    #[test]
    fn native_drag_motion_has_a_held_button_and_exact_bounded_endpoints() {
        log("gesture-motion");
        assert!(!separated_points((10.0, 20.0), (10.0, 20.0)));
        assert!(!separated_points((10.0, 20.0), (17.9, 27.9)));
        assert!(separated_points((10.0, 20.0), (18.0, 20.0)));
        assert!(separated_points((10.0, 20.0), (10.0, 12.0)));
        assert_eq!(
            motion((20.0, 30.0), false, 8),
            json!({"type":"mouseMoved","x":20.0,"y":30.0,
            "button":"none","buttons":0,"modifiers":8})
        );
        assert_eq!(
            motion((80.0, 30.0), true, 8),
            json!({"type":"mouseMoved","x":80.0,"y":30.0,
            "button":"left","buttons":1,"modifiers":8})
        );
        let points: Vec<_> = (1..=3)
            .map(|step| interpolate((20.0, 30.0), (140.0, 30.0), step, 3))
            .collect();
        assert_eq!(points, vec![(60.0, 30.0), (100.0, 30.0), (140.0, 30.0)]);
        assert!(motion(points[0], true, 0).get("data").is_none());
    }

    #[test]
    fn target_geometry_never_accepts_invalid_or_moved_boxes() {
        log("gesture-geometry");
        let initial = Bounds::parse(json!({"left":10,"top":20,"right":50,"bottom":60})).unwrap();
        let shifted = Bounds::parse(json!({"left":11,"top":20,"right":51,"bottom":60})).unwrap();
        assert!(!initial.preserves(&shifted));
        for bad in [
            json!({"left":10,"top":20,"right":10,"bottom":60}),
            json!({"left":-1,"top":20,"right":50,"bottom":60}),
            json!({"left":10,"top":20,"right":50,"bottom":1000001}),
            json!({"left":"10","top":20,"right":50,"bottom":60}),
        ] {
            assert!(Bounds::parse(bad).is_err());
        }
    }

    #[test]
    fn interrupted_gesture_retains_native_progress_without_page_diagnostics() {
        log("gesture-progress");
        let progress = GestureProgress {
            moves_acknowledged: 2,
            press_attempted: true,
            press_acknowledged: true,
            release_attempted: true,
            release_acknowledged: false,
            cancel_attempted: true,
            cancel_acknowledged: true,
        };
        let result = gesture_result(
            "drag",
            "board",
            &progress,
            Err(Error::tool("browser", "PRIVATE-CANARY")),
        );
        assert!(result.is_error);
        let details = result.details.unwrap();
        assert_eq!(details["gesture"]["moves_acknowledged"], 2);
        assert_eq!(details["gesture"]["release_acknowledged"], false);
        assert_eq!(details["dispatch_acknowledged"], false);
        assert_eq!(details["action_replayed"], false);
        assert!(!details.to_string().contains("PRIVATE-CANARY"));
        assert_eq!(
            gesture_result("hover", "board", &GestureProgress::default(), Ok(()))
                .details
                .unwrap()["delivery_verified"],
            false
        );
    }
}
