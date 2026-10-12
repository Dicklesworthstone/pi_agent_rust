//! macOS desktop operations through a bundled literal Swift helper. The host
//! keeps approval, capability, timeout, and create-only publication ownership.
//! No request text is interpolated into source, shell commands, or argv.

use super::{IMAGE_LIMIT, check_owner, destination, error, output, process, publish};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::tools::ToolOutput;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

const HELPER: &str = concat!(include_str!("macos_input.swift"), "\n", include_str!("macos.swift"));
const OWNED_INPUT_ACTIONS: &[&str] = &["mouse_click", "mouse_drag"];
const SCREENSHOT_REPLY_LIMIT: usize = IMAGE_LIMIT.div_ceil(3) * 4 + 8192;
const ACTIONS: &[&str] = &[
    "list_displays",
    "list_windows",
    "screenshot",
    "clipboard_read",
    "clipboard_write",
    "ax_tree",
    "focus_window",
    "mouse_move",
    "mouse_click",
    "mouse_drag",
    "scroll",
];

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Ok { action: String, data: Value },
    Error { code: String },
}

fn protocol() -> crate::error::Error {
    error("macOS desktop helper returned an invalid response")
}

// Unknown diagnostics may contain private desktop data. Only fixed messages
// are exposed, never an NSError, subprocess stderr, or server-provided code.
fn failure(code: &str) -> crate::error::Error {
    error(match code {
        "screen_permission" => "macOS Screen Recording permission is required; grant it to the hosting terminal/application in System Settings and restart the host",
        "accessibility_permission" => "macOS Accessibility permission is required; grant it to the hosting terminal/application in System Settings and restart the host",
        "accessibility_failed" => "the macOS application did not provide the requested accessibility data",
        "ambiguous_window" => "the macOS window could not be uniquely matched by PID, title and geometry; refresh the window list instead of guessing",
        "focus_unconfirmed" => "macOS window activation did not confirm the requested focus; focus may already have changed",
        "target_not_focused" => "the requested macOS window is not focused; use focus_window explicitly before input",
        "pointer_outside_displays" => "the requested point is outside the active macOS displays",
        "pointer_unconfirmed" => "the macOS pointer did not reach the requested position; effects may already have occurred",
        "input_failed" => "macOS could not create the requested input event",
        "input_cancelled" => "macOS input stopped because its owner or deadline ended; releases were attempted and effects may already have occurred",
        "input_channel" => "macOS input could not establish its owner channel",
        "input_conflict" => "macOS input refused while its button or a modifier is already held; release physical inputs before retrying",
        "os_version" => "the macOS computer backend requires macOS 14 or newer and Xcode Command Line Tools with a compatible macOS SDK",
        "display_unavailable" => "the requested macOS display is unavailable; list displays again",
        "window_unavailable" => "the requested macOS window is unavailable; list windows again",
        "clipboard_not_text" => "the macOS clipboard has no readable text representation",
        "clipboard_changed" => "the macOS clipboard changed while being read; retry the read",
        "clipboard_failed" => "the macOS clipboard write was not verified; its previous contents may already have been cleared",
        "capture_failed" => "macOS screen capture failed; check Screen Recording permission and the selected display/window",
        "output_limit" => "the macOS desktop result exceeds its output or image budget",
        "invalid_geometry" => "macOS returned invalid desktop geometry",
        "unsupported_action" => "this action is not available in the macOS desktop backend",
        "unsupported_platform" => "the macOS desktop helper cannot run on this operating system",
        _ => "macOS desktop operation failed without a valid result",
    })
}

fn request_bytes(args: &Value) -> Result<Vec<u8>> {
    let mut request = args.as_object().ok_or_else(protocol)?.clone();
    // The helper never receives filesystem destinations or chooses a timeout.
    request.remove("output_path");
    request.remove("timeout_ms");
    serde_json::to_vec(&request).map_err(|_| protocol())
}

fn decode_reply(bytes: &[u8], action: &str) -> Result<Value> {
    let limit = if action == "screenshot" {
        SCREENSHOT_REPLY_LIMIT
    } else {
        process::TEXT_LIMIT
    };
    if bytes.len() > limit {
        return Err(failure("output_limit"));
    }
    let reply: Reply = serde_json::from_slice(bytes).map_err(|_| protocol())?;
    match reply {
        Reply::Error { code } => Err(failure(&code)),
        Reply::Ok { action: actual, data } if actual == action && data.is_object() => Ok(data),
        Reply::Ok { .. } => Err(protocol()),
    }
}

fn fields(value: &Value, allowed: &[&str]) -> Result<()> {
    let object = value.as_object().ok_or_else(protocol)?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(protocol());
    }
    Ok(())
}

fn positive_id(value: &Value) -> Result<u64> {
    value.as_u64().filter(|id| *id > 0 && *id <= u64::from(u32::MAX)).ok_or_else(protocol)
}

fn geometry(value: &Value) -> Result<()> {
    for field in ["x", "y", "width", "height"] {
        let number = value[field].as_f64().ok_or_else(protocol)?;
        let extent = matches!(field, "width" | "height");
        if !number.is_finite() || number.abs() > 1_000_000.0
            || (extent && (number <= 0.0 || number > 100_000.0))
        {
            return Err(protocol());
        }
    }
    Ok(())
}

fn label(value: &Value) -> Result<()> {
    let value = value.as_str().ok_or_else(protocol)?;
    if value.chars().count() > 512 || value.chars().any(|ch| {
        ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        return Err(protocol());
    }
    Ok(())
}

fn catalog(data: &Value, displays: bool) -> Result<()> {
    let field = if displays { "displays" } else { "windows" };
    fields(data, if displays { &["displays"] } else { &["windows", "scope", "order"] })?;
    let entries = data[field].as_array().ok_or_else(protocol)?;
    if entries.len() > if displays { 64 } else { 256 } || (displays && entries.is_empty()) {
        return Err(protocol());
    }
    let mut ids = HashSet::new();
    for entry in entries {
        geometry(entry)?;
        if !ids.insert(positive_id(&entry["id"])?) {
            return Err(protocol());
        }
        if displays {
            fields(entry, &["id", "name", "x", "y", "width", "height", "is_primary",
                "scale_factor", "pixel_width", "pixel_height"])?;
            label(&entry["name"])?;
            positive_id(&entry["pixel_width"])?;
            positive_id(&entry["pixel_height"])?;
            let scale = entry["scale_factor"].as_f64().ok_or_else(protocol)?;
            if !entry["is_primary"].is_boolean() || !scale.is_finite() || scale <= 0.0 || scale > 16.0 {
                return Err(protocol());
            }
        } else {
            fields(entry, &["id", "pid", "title", "app_name", "x", "y", "width", "height",
                "frontmost_application", "is_on_screen"])?;
            positive_id(&entry["pid"])?;
            label(&entry["title"])?;
            label(&entry["app_name"])?;
            if !entry["frontmost_application"].is_boolean() || entry["is_on_screen"] != true {
                return Err(protocol());
            }
        }
    }
    if !displays && (data["scope"] != "on_screen_normal_windows" || data["order"] != "front_to_back") {
        return Err(protocol());
    }
    Ok(())
}

fn admit_result(action: &str, args: &Value, data: &Value) -> Result<()> {
    match action {
        "list_displays" => catalog(data, true),
        "list_windows" => catalog(data, false),
        "ax_tree" => accessibility_result(args, data),
        "mouse_click" | "mouse_drag" => input_result(args, data),
        "focus_window" | "mouse_move" | "scroll" => targeting_result(action, args, data),
        "clipboard_read" => {
            fields(data, &["text", "char_count"])?;
            let text = data["text"].as_str().ok_or_else(protocol)?;
            if text.len() > 64 * 1024 || data["char_count"].as_u64() != Some(text.chars().count() as u64) {
                return Err(protocol());
            }
            Ok(())
        }
        "clipboard_write" => {
            fields(data, &["written", "char_count"])?;
            let text = args["text"].as_str().ok_or_else(protocol)?;
            if data["written"] != true || data["char_count"].as_u64() != Some(text.chars().count() as u64) {
                return Err(protocol());
            }
            Ok(())
        }
        _ => Err(failure("unsupported_action")),
    }
}

fn input_result(args: &Value, data: &Value) -> Result<()> {
    fields(data, &["dispatched", "released", "button", "position", "application_effect", "window_id"])?;
    fields(&data["position"], &["x", "y"])?;
    if data["dispatched"] != true || data["released"] != true
        || data["application_effect"] != "not_verified"
        || data.get("window_id") != args.get("window_id")
        || data["button"].as_str() != Some(args["button"].as_str().unwrap_or("left"))
    {
        return Err(protocol());
    }
    for axis in ["x", "y"] {
        let actual = data["position"][axis].as_f64().ok_or_else(protocol)?;
        if !actual.is_finite() || actual.abs() > 1_000_000.0
            || args.get(axis).is_some_and(|expected| expected.as_f64()
                .is_none_or(|expected| !expected.is_finite() || (actual - expected).abs() > 0.5))
        {
            return Err(protocol());
        }
    }
    Ok(())
}

fn targeting_result(action: &str, args: &Value, data: &Value) -> Result<()> {
    match action {
        "focus_window" => {
            fields(data, &["focused", "window_id", "pid"])?;
            positive_id(&data["window_id"])?;
            positive_id(&data["pid"])?;
            if data["focused"] != true || data.get("window_id") != args.get("window_id") {
                return Err(protocol());
            }
        }
        "mouse_move" => {
            fields(data, &["dispatched", "position"])?;
            fields(&data["position"], &["x", "y"])?;
            if data["dispatched"] != true {
                return Err(protocol());
            }
            for axis in ["x", "y"] {
                let actual = data["position"][axis].as_f64().ok_or_else(protocol)?;
                let expected = args[axis].as_f64().ok_or_else(protocol)?;
                if !actual.is_finite() || !expected.is_finite() || (actual - expected).abs() > 0.5 {
                    return Err(protocol());
                }
            }
        }
        "scroll" => {
            fields(data, &["dispatched", "direction", "amount", "application_effect"])?;
            if data["dispatched"] != true
                || data["direction"].as_str() != Some(args["direction"].as_str().unwrap_or("down"))
                || data["amount"].as_u64() != Some(args["amount"].as_u64().unwrap_or(3))
                || data["application_effect"] != "not_verified"
            {
                return Err(protocol());
            }
        }
        _ => return Err(failure("unsupported_action")),
    }
    Ok(())
}

// Independently validate the helper's tree before exposing application data.
// There is no Value/Text interface in this protocol. A redacted field may not
// smuggle a title, geometry or descendants alongside its redaction marker.
fn admit_tree(node: &Value, depth: usize, nodes: &mut usize, text_bytes: &mut usize) -> Result<()> {
    if depth > 12 || *nodes >= 256 {
        return Err(protocol());
    }
    *nodes += 1;
    fields(node, &["role", "title", "enabled", "focused", "bounds", "children", "value_redacted"])?;
    label(&node["role"])?;
    let children = node["children"].as_array().ok_or_else(protocol)?;
    if children.len() > 128 {
        return Err(protocol());
    }
    if node.get("value_redacted").is_some() || node["role"] == "AXSecureTextField" {
        fields(node, &["role", "value_redacted", "children"])?;
        if node["value_redacted"] != true || !children.is_empty() {
            return Err(protocol());
        }
    }
    for field in ["role", "title"] {
        if let Some(value) = node.get(field) {
            label(value)?;
            *text_bytes = (*text_bytes).checked_add(value.as_str().ok_or_else(protocol)?.len())
                .filter(|bytes| *bytes <= 64 * 1024).ok_or_else(protocol)?;
        }
    }
    for field in ["enabled", "focused"] {
        if node.get(field).is_some_and(|value| !value.is_boolean()) {
            return Err(protocol());
        }
    }
    if let Some(bounds) = node.get("bounds") {
        fields(bounds, &["x", "y", "width", "height"])?;
        geometry(bounds)?;
    }
    for child in children {
        admit_tree(child, depth + 1, nodes, text_bytes)?;
    }
    Ok(())
}

fn accessibility_result(args: &Value, data: &Value) -> Result<()> {
    fields(data, &["pid", "window_id", "tree", "truncated", "node_count", "scope"])?;
    positive_id(&data["pid"])?;
    if data.get("window_id") != args.get("window_id") || !data["truncated"].is_boolean()
        || data["scope"] != "requested_or_focused_window"
    {
        return Err(protocol());
    }
    if let Some(id) = data.get("window_id") {
        positive_id(id)?;
    }
    let mut nodes = 0;
    admit_tree(&data["tree"], 0, &mut nodes, &mut 0)?;
    if data["node_count"].as_u64() != Some(nodes as u64) {
        return Err(protocol());
    }
    Ok(())
}

pub(super) async fn execute(
    owner: &AgentCx,
    cwd: &Path,
    helpers: &BTreeMap<String, PathBuf>,
    args: &Value,
) -> Result<ToolOutput> {
    check_owner(owner)?;
    super::validate(args)?;
    let action = args["action"].as_str().ok_or_else(protocol)?;
    if !ACTIONS.contains(&action) {
        return Err(failure("unsupported_action"));
    }
    let target = (action == "screenshot").then(|| destination(cwd, args)).transpose()?;
    let input = request_bytes(args)?;
    let mut helpers = helpers.clone();
    helpers.entry("swift".into()).or_insert_with(|| "/usr/bin/swift".into());
    let bytes = if OWNED_INPUT_ACTIONS.contains(&action) {
        process::input::run(owner, &helpers,
            &process::strings(&["-swift-version", "5", "-e", HELPER]),
            &input).await?
    } else { process::run(
        owner,
        // Neither a workspace executable nor a workspace Swift module may
        // shadow the system helper. SDK overrides remain trusted host policy.
        Path::new("/"),
        &helpers,
        "swift",
        &process::strings(&["-swift-version", "5", "-e", HELPER]),
        &input,
        if target.is_some() { SCREENSHOT_REPLY_LIMIT } else { process::TEXT_LIMIT },
    ).await? };
    check_owner(owner)?;
    let mut data = decode_reply(&bytes, action)?;
    if let Some(target) = target {
        return screenshot_output(&target, args, data);
    }
    admit_result(action, args, &data)?;
    data["backend"] = json!("macos");
    data["coordinate_space"] = json!("desktop_points");
    if action == "list_displays" {
        data["supported_actions"] = json!(ACTIONS);
    }
    Ok(output(format!("macOS {action}:\n{data}"), data, false))
}

fn screenshot_output(
    target: &crate::artifact_output::OutputTarget,
    args: &Value,
    mut data: Value,
) -> Result<ToolOutput> {
    fields(&data, &["png", "bounds", "display_id", "window_id", "pixel_width", "pixel_height"])?;
    let encoded = data.as_object_mut().ok_or_else(protocol)?.remove("png").ok_or_else(protocol)?;
    let encoded = encoded.as_str().ok_or_else(protocol)?;
    if encoded.len() > IMAGE_LIMIT.div_ceil(3) * 4 {
        return Err(failure("output_limit"));
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_| protocol())?;
    let bounds = &data["bounds"];
    fields(bounds, &["x", "y", "width", "height"])?;
    geometry(bounds)?;
    let (selected, other) = if args.get("window_id").is_some() {
        ("window_id", "display_id")
    } else {
        ("display_id", "window_id")
    };
    positive_id(&data[selected])?;
    if data.get(other).is_some() || args.get(selected).is_some_and(|expected| data.get(selected) != Some(expected)) {
        return Err(protocol());
    }
    let dimension = |range: std::ops::Range<usize>| -> Result<u32> {
        let raw = bytes.get(range).ok_or_else(protocol)?.try_into().map_err(|_| protocol())?;
        Ok(u32::from_be_bytes(raw))
    };
    let width = dimension(16..20)?;
    let height = dimension(20..24)?;
    if data["pixel_width"].as_u64() != Some(u64::from(width))
        || data["pixel_height"].as_u64() != Some(u64::from(height))
    {
        return Err(protocol());
    }
    let scale_x = f64::from(width) / bounds["width"].as_f64().ok_or_else(protocol)?;
    let scale_y = f64::from(height) / bounds["height"].as_f64().ok_or_else(protocol)?;
    if !scale_x.is_finite() || !scale_y.is_finite() || scale_x <= 0.0 || scale_y <= 0.0 {
        return Err(protocol());
    }
    data["pixels_per_point_x"] = json!(scale_x);
    data["pixels_per_point_y"] = json!(scale_y);
    data["backend"] = json!("macos");
    data["coordinate_space"] = json!("desktop_points");
    let mut result = publish(target, args, &bytes, false)?;
    if let Some(details) = result.details.as_mut() {
        details["desktop"] = data.clone();
    }
    result.content.insert(1, crate::model::ContentBlock::Text(
        crate::model::TextContent::new(format!("macOS screenshot mapping (desktop points to PNG pixels): {data}")),
    ));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_requests_are_data_and_publication_paths_stay_in_rust() {
        let args = json!({"action":"clipboard_write", "text":"$(touch /tmp/never)\n\"🦀\"",
            "output_path":"private/path.png", "timeout_ms":100});
        let request: Value = serde_json::from_slice(&request_bytes(&args).unwrap()).unwrap();
        assert_eq!(request["text"], args["text"]);
        assert!(request.get("output_path").is_none());
        assert!(request.get("timeout_ms").is_none());
    }

    #[test]
    fn errors_and_wrong_action_results_cannot_become_success() {
        for bytes in [
            br#"{"status":"ok","action":"clipboard_write","data":{}}"#.as_slice(),
            br#"{"status":"ok","action":"list_displays","data":[]}"#,
            br#"{"status":"error","code":"PRIVATE-CANARY"}"#,
            br#"{"status":"ok","action":"list_displays","data":{},"extra":"PRIVATE-CANARY"}"#,
            br#"{"status":"error","code":"screen_permission"}"#,
            b"PRIVATE-CANARY",
        ] {
            let failure = decode_reply(bytes, "list_displays").unwrap_err().to_string();
            assert!(!failure.contains("PRIVATE-CANARY"));
        }
    }

    fn screenshot_data() -> Value {
        json!({"png":base64::engine::general_purpose::STANDARD.encode(super::super::super::mock::PNG),
            "bounds":{"x":-100.0,"y":0.0,"width":1.0,"height":1.0},
            "display_id":73,"pixel_width":1,"pixel_height":1})
    }

    #[test]
    fn actual_png_is_published_once_with_mapping_but_without_encoded_details() {
        let dir = tempfile::tempdir().unwrap();
        let args = json!({"action":"screenshot","display_id":73,"output_path":"capture.png"});
        let target = destination(dir.path(), &args).unwrap();
        let result = screenshot_output(&target, &args, screenshot_data()).unwrap();
        assert_eq!(std::fs::read(target.path()).unwrap(), super::super::super::mock::PNG);
        let details = result.details.unwrap();
        assert_eq!(details["desktop"]["bounds"]["x"], -100.0);
        assert_eq!(details["desktop"]["pixels_per_point_x"], 1.0);
        assert!(details["desktop"].get("png").is_none());
        assert!(screenshot_output(&target, &args, screenshot_data()).is_err());
    }

    #[test]
    fn invalid_screenshot_identity_geometry_and_dimensions_never_publish() {
        let dir = tempfile::tempdir().unwrap();
        let args = json!({"action":"screenshot","display_id":73,"output_path":"capture.png"});
        let target = destination(dir.path(), &args).unwrap();
        for field in ["png", "bounds", "display_id", "pixel_width", "pixel_height"] {
            let mut data = screenshot_data();
            data[field] = Value::Null;
            assert!(screenshot_output(&target, &args, data).is_err(), "{field}");
            assert!(!target.path().exists());
        }
        let mut wrong = screenshot_data();
        wrong["display_id"] = json!(74);
        assert!(screenshot_output(&target, &args, wrong).is_err());
        let mut ambiguous = screenshot_data();
        ambiguous["window_id"] = json!(73);
        assert!(screenshot_output(&target, &args, ambiguous).is_err());
        let mut empty = screenshot_data();
        empty["bounds"]["width"] = json!(0);
        assert!(screenshot_output(&target, &args, empty).is_err());
        assert!(!target.path().exists());
    }

    #[test]
    fn native_display_ids_use_the_full_u32_range() {
        assert!(super::super::validate(&json!({"action":"screenshot","display_id":u32::MAX})).is_ok());
        for id in [0, u64::MAX] {
            assert!(super::super::validate(&json!({"action":"screenshot","display_id":id})).is_err());
        }
    }

    #[test]
    fn display_catalog_preserves_negative_points_and_rejects_ambiguous_ids() {
        let display = json!({"id":73,"name":"Retina","x":-1280,"y":0,"width":1280,
            "height":800,"pixel_width":2560,"pixel_height":1600,"scale_factor":2,"is_primary":true});
        assert!(catalog(&json!({"displays":[display.clone()]}), true).is_ok());
        assert!(catalog(&json!({"displays":[display.clone(),display.clone()]}), true).is_err());
        for field in ["id", "name", "x", "width", "pixel_width", "scale_factor", "is_primary"] {
            let mut invalid = display.clone();
            invalid[field] = Value::Null;
            assert!(catalog(&json!({"displays":[invalid]}), true).is_err(), "{field}");
        }
        assert!(catalog(&json!({"displays":[]}), true).is_err());
    }

    #[test]
    fn clipboard_replies_bind_exact_text_counts_and_write_confirmation() {
        let args = json!({"action":"clipboard_write","text":"a🦀"});
        assert!(admit_result("clipboard_write", &args, &json!({"written":true,"char_count":2})).is_ok());
        for data in [json!({"written":false,"char_count":2}), json!({"written":true,"char_count":5})] {
            assert!(admit_result("clipboard_write", &args, &data).is_err());
        }
        let text = "a\n🦀\u{202e}";
        assert!(admit_result("clipboard_read", &json!({}), &json!({"text":text,"char_count":4})).is_ok());
        assert!(admit_result("clipboard_read", &json!({}), &json!({"text":text,"char_count":3})).is_err());
    }

    #[test]
    fn failed_helpers_never_publish_or_fall_back_to_fixture_results() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let args = json!({"action":"screenshot","output_path":"capture.png"});
        let helpers = BTreeMap::from([("swift".to_string(), PathBuf::from("/bin/false"))]);
        let failure = runtime.block_on(async {
            execute(&AgentCx::for_current_or_request(), dir.path(), &helpers, &args).await.unwrap_err()
        });
        assert!(failure.to_string().contains("desktop helper swift failed"));
        assert!(!dir.path().join("capture.png").exists());
    }

    #[test]
    fn focus_pointer_and_scroll_confirmations_bind_the_requested_operation() {
        let focus = json!({"focused":true,"window_id":73,"pid":123});
        let args = json!({"window_id":73});
        assert!(admit_result("focus_window", &args, &focus).is_ok());
        assert!(admit_result("focus_window", &json!({"window_id":74}), &focus).is_err());
        let point = json!({"dispatched":true,"position":{"x":-12.0,"y":300.25}});
        assert!(admit_result("mouse_move", &json!({"x":-12,"y":300}), &point).is_ok());
        assert!(admit_result("mouse_move", &json!({"x":12,"y":300}), &point).is_err());
        let scroll = json!({"dispatched":true,"direction":"down","amount":3,"application_effect":"not_verified"});
        assert!(admit_result("scroll", &json!({}), &scroll).is_ok());
        assert!(admit_result("scroll", &json!({"direction":"up"}), &scroll).is_err());
        assert!(admit_result("scroll", &json!({"amount":4}), &scroll).is_err());
        let mut unverified = focus;
        unverified["focused"] = json!(false);
        assert!(admit_result("focus_window", &args, &unverified).is_err());
    }

    fn tree_result(tree: Value, count: usize) -> Value {
        json!({"pid":123,"window_id":73,"tree":tree,"truncated":false,
            "node_count":count,"scope":"requested_or_focused_window"})
    }

    #[test]
    fn accessibility_tree_keeps_structure_but_never_accepts_secure_field_payloads() {
        let secure = json!({"role":"AXTextField","value_redacted":true,"children":[]});
        let tree = json!({"role":"AXWindow","title":"Window","children":[secure.clone()]});
        assert!(accessibility_result(&json!({"window_id":73}), &tree_result(tree, 2)).is_ok());
        for field in ["title", "value", "bounds", "children"] {
            let mut invalid = secure.clone();
            invalid[field] = if field == "children" {
                json!([{"role":"AXStaticText","title":"PRIVATE-CANARY","children":[]}])
            } else {
                json!("PRIVATE-CANARY")
            };
            let error = accessibility_result(&json!({"window_id":73}), &tree_result(invalid, 1)).unwrap_err();
            assert!(!error.to_string().contains("PRIVATE-CANARY"));
        }
        let value = json!({"role":"AXTextField","value":"PRIVATE-CANARY","children":[]});
        assert!(accessibility_result(&json!({"window_id":73}), &tree_result(value, 1)).is_err());
    }

    #[test]
    fn accessibility_shape_counts_depth_and_identity_are_independently_bounded() {
        let leaf = json!({"role":"AXButton","children":[]});
        let args = json!({"window_id":73});
        assert!(accessibility_result(&args, &tree_result(leaf.clone(), 2)).is_err());
        assert!(accessibility_result(&json!({"window_id":74}), &tree_result(leaf.clone(), 1)).is_err());
        let wide = json!({"role":"AXWindow","children":vec![leaf.clone();129]});
        assert!(accessibility_result(&args, &tree_result(wide, 130)).is_err());
        let mut deep = leaf.clone();
        for _ in 0..12 {
            deep = json!({"role":"AXGroup","children":[deep]});
        }
        assert!(accessibility_result(&args, &tree_result(deep.clone(), 13)).is_ok());
        deep = json!({"role":"AXGroup","children":[deep]});
        assert!(accessibility_result(&args, &tree_result(deep, 14)).is_err());
        let branch = json!({"role":"AXGroup","children":vec![leaf;127]});
        let oversized = json!({"role":"AXWindow","children":[branch.clone(),branch]});
        assert!(accessibility_result(&args, &tree_result(oversized, 257)).is_err());
    }

    #[test]
    fn accessibility_labels_cannot_exceed_the_aggregate_text_budget() {
        let leaf = json!({"role":"AXStaticText","title":"x".repeat(512),"children":[]});
        let tree = json!({"role":"AXWindow","children":vec![leaf;128]});
        assert!(accessibility_result(&json!({"window_id":73}), &tree_result(tree,129)).is_err());
    }

    #[test]
    fn new_targeting_operations_keep_the_existing_host_approval_boundary() {
        use crate::tools::Tool;
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let tool = crate::computer::ComputerTool::new(dir.path()).with_mock(false);
        for args in [json!({"action":"focus_window","window_id":73}),
            json!({"action":"mouse_move","x":-12,"y":300}),
            json!({"action":"scroll","direction":"down"}),
            json!({"action":"mouse_click","button":"right"}),
            json!({"action":"mouse_drag","x":-12,"y":300})]
        {
            let failure = runtime.block_on(tool.execute("desktop-consent", args, None)).unwrap_err();
            assert!(failure.to_string().contains("requires host approval"));
        }
    }

    #[test]
    fn mouse_input_receipts_bind_release_target_button_and_position() {
        let args = json!({"action":"mouse_drag","window_id":73,"x":-12,"y":300,"button":"right"});
        let receipt = json!({"dispatched":true,"released":true,"window_id":73,"button":"right",
            "position":{"x":-12.0,"y":300.0},"application_effect":"not_verified"});
        assert!(admit_result("mouse_drag", &args, &receipt).is_ok());
        for field in ["dispatched", "released", "window_id", "button", "position", "application_effect"] {
            let mut invalid = receipt.clone();
            invalid[field] = Value::Null;
            assert!(admit_result("mouse_drag", &args, &invalid).is_err(), "{field}");
        }
        let mut wrong = receipt.clone();
        wrong["position"]["x"] = json!(12);
        assert!(admit_result("mouse_drag", &args, &wrong).is_err());
        wrong = receipt.clone();
        wrong["window_id"] = json!(74);
        assert!(admit_result("mouse_drag", &args, &wrong).is_err());
        wrong = receipt;
        wrong["private"] = json!("PRIVATE-CANARY");
        let failure = admit_result("mouse_drag", &args, &wrong).unwrap_err();
        assert!(!failure.to_string().contains("PRIVATE-CANARY"));
    }

    #[test]
    fn mouse_input_reaches_the_owned_transport_and_validates_its_reply() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("swift-fixture");
        std::fs::write(&helper, concat!(
            "#!/bin/sh\n[ \"$PI_COMPUTER_OWNED_INPUT\" = 1 ] || exit 7\n",
            "IFS= read -r frame || exit 8\n",
            "printf '%s' '{\"status\":\"ok\",\"action\":\"mouse_click\",\"data\":{",
            "\"dispatched\":true,\"released\":true,\"button\":\"right\",",
            "\"position\":{\"x\":-12,\"y\":300},\"application_effect\":\"not_verified\"}}'\n",
        )).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let helpers = BTreeMap::from([("swift".to_string(), helper)]);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let args = json!({"action":"mouse_click","x":-12,"y":300,"button":"right"});
        let output = runtime.block_on(async {
            execute(&AgentCx::for_current_or_request(), dir.path(), &helpers, &args).await.unwrap()
        });
        assert!(!output.is_error);
        assert_eq!(output.details.unwrap()["backend"], "macos");
        let wrong = json!({"action":"mouse_drag","x":-12,"y":300,"button":"right"});
        assert!(runtime.block_on(async {
            execute(&AgentCx::for_current_or_request(), dir.path(), &helpers, &wrong).await
        }).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_dispatch_reaches_macos_without_an_x11_display() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let helpers = BTreeMap::from([("swift".to_string(), PathBuf::from("/bin/false"))]);
        let failure = runtime.block_on(async {
            super::super::execute(&AgentCx::for_current_or_request(), dir.path(), &helpers,
                &mut super::super::State::default(), &json!({"action":"list_displays"})).await.unwrap_err()
        });
        assert!(failure.to_string().contains("desktop helper swift failed"));
    }
}
