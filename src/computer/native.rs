//! Validated desktop requests and native X11/AT-SPI and macOS operations. Other backends
//! fail explicitly; selecting a backend never selects canned results.

#[cfg(unix)]
use super::process::{self, strings, text};
#[cfg(any(unix, test))]
use super::{DisplayInfo, WindowInfo};
use super::{error, output};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::model::{ContentBlock, ImageContent};
use crate::tools::ToolOutput;
use base64::Engine as _;
#[cfg(any(unix, test))]
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
mod accessibility;
#[cfg(unix)]
mod input;
#[cfg(unix)]
mod macos;

pub(super) const HELPERS: &[&str] = &[
    "xrandr", "wmctrl", "xprop", "scrot", "xdotool", "xclip", "python3", "swift",
];
pub(super) const IMAGE_LIMIT: usize = 20 * 1024 * 1024;

#[derive(Default)]
pub(super) struct State {
    // A foreground selection owner must outlive clipboard_write. It remains
    // owned, and replacement or tool destruction kills/reaps it.
    #[cfg(unix)]
    clipboard: Option<process::Running>,
}

pub(super) fn check_owner(owner: &AgentCx) -> Result<()> {
    let caps = owner.capabilities();
    if !caps.io || !caps.spawn || !caps.time {
        return Err(error(
            "native desktop operations require I/O, spawn and timer capabilities",
        ));
    }
    owner
        .checkpoint()
        .map_err(|_| error("desktop operation cancelled"))
}

pub(super) fn string<'a>(args: &'a Value, field: &str) -> Result<Option<&'a str>> {
    args.get(field).map_or(Ok(None), |value| {
        value
            .as_str()
            .map(Some)
            .ok_or_else(|| error(format!("{field} must be a string")))
    })
}

pub(super) fn number(args: &Value, field: &str, min: i64, max: i64) -> Result<Option<i64>> {
    args.get(field).map_or(Ok(None), |value| {
        value
            .as_i64()
            .filter(|n| (min..=max).contains(n))
            .map(Some)
            .ok_or_else(|| error(format!("{field} must be an integer in {min}..={max}")))
    })
}

pub(super) fn validate(args: &Value) -> Result<Duration> {
    let object = args
        .as_object()
        .ok_or_else(|| error("computer arguments must be an object"))?;
    let action =
        string(args, "action")?.ok_or_else(|| error("missing required action parameter"))?;
    let fields: &[&str] = match action {
        "list_displays" | "list_windows" | "clipboard_read" => &[],
        "screenshot" => &["display_id", "window_id", "output_path"],
        "mouse_move" => &["x", "y", "window_id"],
        "mouse_drag" | "mouse_click" => &["x", "y", "button", "window_id"],
        "key_type" => &["text", "window_id"],
        "key_press" => &["key", "window_id"],
        "clipboard_write" => &["text"],
        "scroll" => &["direction", "amount", "window_id"],
        "ax_tree" | "focus_window" => &["window_id"],
        _ => return Err(error(format!("unknown action: {}", clean(action, 64)))),
    };
    for field in object.keys() {
        if field != "action" && field != "timeout_ms" && !fields.contains(&field.as_str()) {
            return Err(error(format!(
                "unsupported parameter for {action}: {}",
                clean(field, 64)
            )));
        }
    }
    let timeout = number(args, "timeout_ms", 1, 120_000)?.unwrap_or(30_000);
    let x = number(args, "x", -32768, 32767)?;
    let y = number(args, "y", -32768, 32767)?;
    if x.is_some() != y.is_some() || (matches!(action, "mouse_move" | "mouse_drag") && x.is_none())
    {
        return Err(error(format!("{action} requires both x and y coordinates")));
    }
    number(args, "window_id", 1, i64::from(u32::MAX))?;
    // macOS uses full-width CoreGraphics IDs, not XRandR list indices.
    // Each backend still admits at most 64 enumerated displays.
    number(args, "display_id", 1, i64::from(u32::MAX))?;
    if action == "focus_window" && args.get("window_id").is_none() {
        return Err(error("focus_window requires window_id"));
    }
    if args.get("window_id").is_some() && args.get("display_id").is_some() {
        return Err(error("choose window_id or display_id, not both"));
    }
    if let Some(button) = string(args, "button")?
        && !matches!(button, "left" | "middle" | "right")
    {
        return Err(error("button must be left, middle or right"));
    }
    if let Some(direction) = string(args, "direction")?
        && !matches!(direction, "up" | "down" | "left" | "right")
    {
        return Err(error("direction must be up, down, left or right"));
    }
    number(args, "amount", 1, 100)?;
    if matches!(action, "key_type" | "clipboard_write") {
        let value = string(args, "text")?
            .ok_or_else(|| error(format!("{action} requires text parameter")))?;
        if value.chars().count() > 4096 || value.contains('\0') {
            return Err(error(
                "text must contain at most 4096 Unicode characters and no NUL",
            ));
        }
        if action == "key_type"
            && value
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
        {
            return Err(error(
                "typed text may contain newline/tab but no other control characters",
            ));
        }
    }
    if action == "key_press" {
        key(string(args, "key")?.ok_or_else(|| error("key_press requires key parameter"))?)?;
    }
    if let Some(path) = string(args, "output_path")?
        && (path.is_empty() || path.len() > 4096 || path.contains('\0'))
    {
        return Err(error(
            "output_path must be nonempty, NUL-free and at most 4096 bytes",
        ));
    }
    Ok(Duration::from_millis(
        u64::try_from(timeout).expect("validated positive timeout"),
    ))
}

// xdotool supports command chaining. A free-form 'key' argument can therefore
// be a command, not a keysym. Only one explicitly parsed key/chord is admitted.
pub(super) fn key(value: &str) -> Result<String> {
    if value.len() > 128 {
        return Err(error("key chord is too long"));
    }
    let parts: Vec<_> = value.split('+').collect();
    if parts.is_empty() || parts.len() > 5 {
        return Err(error("invalid key chord"));
    }
    let mut result = Vec::new();
    for modifier in &parts[..parts.len() - 1] {
        let normalized = match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "ctrl",
            "alt" => "alt",
            "shift" => "shift",
            "super" | "meta" | "cmd" | "command" => "super",
            _ => return Err(error("unknown key modifier")),
        };
        if result.contains(&normalized.to_string()) {
            return Err(error("duplicate key modifier"));
        }
        result.push(normalized.to_string());
    }
    let last = parts[parts.len() - 1];
    let normalized = match last.to_ascii_lowercase().as_str() {
        "return" | "enter" => "Return",
        "escape" | "esc" => "Escape",
        "tab" => "Tab",
        "space" => "space",
        "backspace" => "BackSpace",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "left" | "arrowleft" => "Left",
        "right" | "arrowright" => "Right",
        "up" | "arrowup" => "Up",
        "down" | "arrowdown" => "Down",
        "pageup" | "prior" => "Prior",
        "pagedown" | "next" => "Next",
        _ if last.len() == 1 && last.as_bytes()[0].is_ascii_alphanumeric() => last,
        _ if last.starts_with('F')
            && last[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) =>
        {
            last
        }
        _ => {
            return Err(error(
                "unsupported key; use one alphanumeric key, F1..F24, or a named navigation key",
            ));
        }
    };
    // Conventional Ctrl+C means ctrl+c, not ctrl+shift+c. An explicit Shift
    // modifier remains in result; bare uppercase letters still request case.
    if !result.is_empty() && normalized.len() == 1 {
        result.push(normalized.to_ascii_lowercase());
    } else {
        result.push(normalized.to_string());
    }
    Ok(result.join("+"))
}

fn clean(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|ch| {
            !ch.is_control() && !matches!(*ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(limit)
        .collect()
}

#[cfg(unix)]
pub(super) async fn execute(
    owner: &AgentCx,
    cwd: &Path,
    helpers: &BTreeMap<String, PathBuf>,
    state: &mut State,
    args: &Value,
) -> Result<ToolOutput> {
    check_owner(owner)?;
    if cfg!(target_os = "macos") {
        return macos::execute(owner, cwd, helpers, args).await;
    }
    if let Some(clipboard) = state.clipboard.as_mut()
        && !matches!(clipboard.poll(4096), Ok(None))
    {
        state.clipboard = None;
    }
    if !cfg!(target_os = "linux") {
        return Err(error(
            "native computer backend is currently available for Linux X11 only",
        ));
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
        || std::env::var("XDG_SESSION_TYPE")
            .is_ok_and(|value| value.eq_ignore_ascii_case("wayland"))
    {
        return Err(error(
            "Wayland desktop is not an X11 desktop; refusing a misleading XWayland fallback",
        ));
    }
    if std::env::var_os("DISPLAY").is_none_or(|value| value.is_empty()) {
        return Err(error(
            "native computer operations need the authorized X11 DISPLAY and XAUTHORITY",
        ));
    }
    x11(owner, cwd, helpers, state, args).await
}

#[cfg(not(unix))]
pub(super) async fn execute(
    _owner: &AgentCx,
    _cwd: &Path,
    _helpers: &BTreeMap<String, PathBuf>,
    _state: &mut State,
    _args: &Value,
) -> Result<ToolOutput> {
    Err(error(
        "native computer backend is not implemented for this operating system",
    ))
}

#[cfg(unix)]
async fn query(
    owner: &AgentCx,
    cwd: &Path,
    helpers: &BTreeMap<String, PathBuf>,
    program: &str,
    args: &[&str],
) -> Result<String> {
    text(
        process::run(
            owner,
            cwd,
            helpers,
            program,
            &strings(args),
            b"",
            process::TEXT_LIMIT,
        )
        .await?,
    )
}

#[cfg(unix)]
async fn x11(
    owner: &AgentCx,
    cwd: &Path,
    helpers: &BTreeMap<String, PathBuf>,
    state: &mut State,
    args: &Value,
) -> Result<ToolOutput> {
    match args["action"].as_str().expect("validated action") {
        "list_displays" => {
            let monitors = monitors(owner, cwd, helpers).await?;
            Ok(output(
                format!("Found {} display(s)", monitors.len()),
                json!({
                    "displays":monitors,"backend":"x11","coordinate_space":"desktop_pixels"
                }),
                false,
            ))
        }
        "list_windows" => {
            let active = active_window(owner, cwd, helpers).await?;
            let raw = query(owner, cwd, helpers, "wmctrl", &["-u", "-lpGx"]).await?;
            let mut windows = parse_windows(&raw, active)?;
            for window in &mut windows {
                let id = format!("0x{:x}", window.info.id);
                let window_state = query(
                    owner,
                    cwd,
                    helpers,
                    "xprop",
                    &["-id", &id, "-notype", "_NET_WM_STATE"],
                )
                .await?;
                window.info.is_minimized = window_state
                    .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                    .any(|token| token == "_NET_WM_STATE_HIDDEN");
            }
            Ok(output(
                format!("Found {} window(s)", windows.len()),
                json!({
                    "windows":windows,"active_window":active,"backend":"x11"
                }),
                false,
            ))
        }
        "screenshot" => {
            let target = destination(cwd, args)?;
            let mut command = strings(&["--silent", "--format", "png"]);
            if let Some(window) = args.get("window_id").and_then(Value::as_u64) {
                command.extend(["--window".into(), window.to_string()]);
            } else if let Some(display) = args.get("display_id").and_then(Value::as_u64) {
                let monitors = monitors(owner, cwd, helpers).await?;
                let monitor = monitors
                    .iter()
                    .find(|m| u64::from(m.info.id) == display)
                    .ok_or_else(|| {
                        error("display_id is not present in the current monitor list")
                    })?;
                command.extend([
                    "--autoselect".into(),
                    format!(
                        "{},{},{},{}",
                        monitor.x, monitor.y, monitor.info.width, monitor.info.height
                    ),
                ]);
            }
            command.extend(strings(&["--file", "-"]));
            let bytes =
                process::run(owner, cwd, helpers, "scrot", &command, b"", IMAGE_LIMIT).await?;
            check_owner(owner)?;
            publish(&target, args, &bytes, false)
        }
        "clipboard_read" => {
            let bytes = process::run(
                owner,
                cwd,
                helpers,
                "xclip",
                &strings(&["-selection", "clipboard", "-out", "-target", "UTF8_STRING"]),
                b"",
                64 * 1024,
            )
            .await?;
            let value = text(bytes)?;
            Ok(output(
                format!(
                    "Clipboard content ({} chars):\n{value}",
                    value.chars().count()
                ),
                json!({
                    "text":value,"char_count":value.chars().count(),"backend":"x11"
                }),
                false,
            ))
        }
        "ax_tree" => accessibility::execute(owner, cwd, helpers, args).await,
        _ => input::execute(owner, cwd, helpers, state, args).await,
    }
}

#[cfg(unix)]
async fn active_window(
    owner: &AgentCx,
    cwd: &Path,
    helpers: &BTreeMap<String, PathBuf>,
) -> Result<Option<u32>> {
    let value = query(
        owner,
        cwd,
        helpers,
        "xprop",
        &["-root", "_NET_ACTIVE_WINDOW"],
    )
    .await?;
    match value.split_once("0x") {
        Some((_, tail)) => {
            let digits: String = tail.chars().take_while(char::is_ascii_hexdigit).collect();
            let id =
                u32::from_str_radix(&digits, 16).map_err(|_| error("invalid active window ID"))?;
            Ok((id != 0).then_some(id))
        }
        None if value.contains("not found") || value.contains("no such atom") => Ok(None),
        None => Err(error(
            "could not parse the desktop's active window property",
        )),
    }
}

#[cfg(any(unix, test))]
#[derive(Serialize)]
struct Monitor {
    #[serde(flatten)]
    info: DisplayInfo,
    x: i32,
    y: i32,
}

#[cfg(unix)]
async fn monitors(
    owner: &AgentCx,
    cwd: &Path,
    helpers: &BTreeMap<String, PathBuf>,
) -> Result<Vec<Monitor>> {
    parse_monitors(&query(owner, cwd, helpers, "xrandr", &["--listactivemonitors"]).await?)
}

#[cfg(any(unix, test))]
fn parse_monitors(raw: &str) -> Result<Vec<Monitor>> {
    let mut lines = raw.lines();
    let expected = lines
        .next()
        .and_then(|line| line.strip_prefix("Monitors:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count <= 64)
        .ok_or_else(|| error("invalid xrandr monitor header"))?;
    let geometry = regex::Regex::new(r"^(\d+)(?:/\d+)?x(\d+)(?:/\d+)?([+-]\d+)([+-]\d+)$")
        .expect("constant monitor geometry expression");
    let mut monitors = Vec::new();
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 3 || monitors.len() >= 64 {
            return Err(error("invalid xrandr monitor entry"));
        }
        let index = fields[0]
            .trim_end_matches(':')
            .parse::<u32>()
            .ok()
            .filter(|index| *index < 64)
            .ok_or_else(|| error("invalid monitor index"))?;
        let caps = geometry
            .captures(fields[2])
            .ok_or_else(|| error("invalid monitor geometry"))?;
        let width = caps[1]
            .parse::<u32>()
            .map_err(|_| error("invalid monitor width"))?;
        let height = caps[2]
            .parse::<u32>()
            .map_err(|_| error("invalid monitor height"))?;
        if width == 0 || height == 0 {
            return Err(error("empty monitor geometry"));
        }
        let x = caps[3]
            .parse::<i32>()
            .map_err(|_| error("invalid monitor x"))?;
        let y = caps[4]
            .parse::<i32>()
            .map_err(|_| error("invalid monitor y"))?;
        if monitors
            .iter()
            .any(|monitor: &Monitor| monitor.info.id == index + 1)
        {
            return Err(error("duplicate monitor index"));
        }
        monitors.push(Monitor {
            info: DisplayInfo {
                id: index + 1,
                name: clean(fields[1].trim_start_matches(['+', '*']), 256),
                width,
                height,
                is_primary: fields[1].contains('*'),
                scale_factor: 1,
            },
            x,
            y,
        });
    }
    if monitors.len() != expected {
        return Err(error("incomplete xrandr monitor list"));
    }
    Ok(monitors)
}

#[cfg(any(unix, test))]
#[derive(Serialize)]
struct Window {
    #[serde(flatten)]
    info: WindowInfo,
    pid: u32,
    // Keep identity separate from display sanitization, and never serialize it
    // into tool output or an audit record.
    #[serde(skip)]
    raw_title: String,
}

#[cfg(any(unix, test))]
fn parse_windows(raw: &str, active: Option<u32>) -> Result<Vec<Window>> {
    let mut windows = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        if windows.len() >= 256 {
            return Err(error("window list exceeds 256 entries"));
        }
        // Nine fixed fields, followed by a title that may contain spaces.
        let mut rest = line;
        let mut fields = Vec::new();
        for _ in 0..9 {
            rest = rest.trim_start();
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            if end == 0 {
                return Err(error("malformed wmctrl window entry"));
            }
            fields.push(&rest[..end]);
            rest = &rest[end..];
        }
        let id = fields[0]
            .strip_prefix("0x")
            .and_then(|id| u32::from_str_radix(id, 16).ok())
            .filter(|id| *id != 0)
            .ok_or_else(|| error("invalid window ID"))?;
        let parse = |field: &str| {
            field
                .parse::<i32>()
                .map_err(|_| error("invalid window geometry"))
        };
        let width = fields[5]
            .parse::<u32>()
            .map_err(|_| error("invalid window width"))?;
        let height = fields[6]
            .parse::<u32>()
            .map_err(|_| error("invalid window height"))?;
        if windows.iter().any(|window: &Window| window.info.id == id) {
            return Err(error("duplicate window ID"));
        }
        // wmctrl emits one delimiter after the right-aligned hostname. Any
        // further leading spaces belong to the title and affect identity.
        let raw_title = rest
            .strip_prefix(' ')
            .ok_or_else(|| error("malformed wmctrl title delimiter"))?
            .to_string();
        windows.push(Window {
            info: WindowInfo {
                id,
                title: clean(&raw_title, 2048),
                app_name: clean(fields[7], 256),
                x: parse(fields[3])?,
                y: parse(fields[4])?,
                width,
                height,
                is_minimized: false,
                is_focused: active == Some(id),
            },
            pid: fields[2]
                .parse::<u32>()
                .map_err(|_| error("invalid window process ID"))?,
            raw_title,
        });
    }
    windows.sort_by_key(|window| window.info.id);
    Ok(windows)
}

pub(super) fn destination(
    cwd: &Path,
    args: &Value,
) -> Result<crate::artifact_output::OutputTarget> {
    let requested = string(args, "output_path")?.map_or_else(
        || format!("screenshots/desktop_{}.png", uuid::Uuid::new_v4().simple()),
        ToString::to_string,
    );
    if !Path::new(&requested)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
    {
        return Err(error("screenshot output_path must have a .png extension"));
    }
    crate::artifact_output::resolve_new(cwd, &requested, "computer")
}

pub(super) fn publish(
    target: &crate::artifact_output::OutputTarget,
    args: &Value,
    bytes: &[u8],
    mock: bool,
) -> Result<ToolOutput> {
    if bytes.len() < 45
        || bytes.len() > IMAGE_LIMIT
        || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.get(12..16) != Some(b"IHDR".as_slice())
        || !bytes.ends_with(b"\0\0\0\0IEND\xaeB`\x82")
    {
        return Err(error(
            "screenshot helper did not return a complete bounded PNG container",
        ));
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("checked PNG length"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("checked PNG length"));
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 128 * 1024 * 1024 {
        return Err(error(
            "screenshot dimensions are empty or exceed 128 megapixels",
        ));
    }
    crate::artifact_output::publish(target, bytes, "computer")?;
    let path = target.path();
    let preview = bytes.len() <= crate::tools::IMAGE_MAX_BYTES;
    let mut result = output(
        format!(
            "Screenshot captured to {} ({}x{}, {} bytes){}",
            path.display(),
            width,
            height,
            bytes.len(),
            if preview {
                ""
            } else {
                "; preview exceeds the inline image budget; inspect the saved file"
            }
        ),
        json!({
            "saved_path":path.display().to_string(),"size_bytes":bytes.len(),"width":width,"height":height,
            "display_id":args.get("display_id"),"window_id":args.get("window_id"),"preview_included":preview
        }),
        mock,
    );
    if preview {
        result.content.push(ContentBlock::Image(ImageContent {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            mime_type: "image/png".into(),
        }));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_real_wmctrl_columns_and_preserves_spaced_titles() {
        let windows = parse_windows("0x00200022  0 42 30 40 360 120 PiDesktopProbe.Xmessage localhost Pi desktop protocol fixture\n", Some(0x0020_0022)).unwrap();
        assert_eq!(windows[0].info.title, "Pi desktop protocol fixture");
        assert_eq!(windows[0].info.app_name, "PiDesktopProbe.Xmessage");
        assert!(windows[0].info.is_focused);
        assert_eq!(windows[0].pid, 42);
        assert!(parse_windows("bad window", None).is_err());
    }
    #[test]
    fn display_sanitization_does_not_change_accessibility_target_identity() {
        let windows = parse_windows(
            "0x00200022 0 42 0 0 80 60 Test.App host  A\u{202e}B\n",
            None,
        )
        .unwrap();
        assert_eq!(windows[0].raw_title, " A\u{202e}B");
        assert_eq!(windows[0].info.title, " AB");
        let serialized = serde_json::to_string(&windows).unwrap();
        assert!(!serialized.contains("raw_title"));
        assert!(!serialized.contains('\u{202e}'));
    }
    #[test]
    fn monitor_ids_geometry_and_completeness_are_checked() {
        let monitors = parse_monitors("Monitors: 2\n 0: +*DP-1 1920/518x1080/324+0+0 DP-1\n 1: +HDMI-1 1280/300x720/170+1920+0 HDMI-1\n").unwrap();
        assert_eq!(monitors[0].info.id, 1);
        assert!(monitors[0].info.is_primary);
        assert_eq!(monitors[1].x, 1920);
        assert!(parse_monitors("Monitors: 1\n").is_err());
    }
    #[test]
    fn command_chains_and_wrong_typed_inputs_are_rejected() {
        for value in [
            "exec",
            "key Return",
            "ctrl+exec",
            "--window",
            "a\nexec",
            "ctrl+ctrl+a",
            "F25",
        ] {
            assert!(key(value).is_err(), "{value}");
        }
        assert_eq!(key("Ctrl+C").unwrap(), "ctrl+c");
        assert_eq!(key("Ctrl+Shift+C").unwrap(), "ctrl+shift+c");
        assert!(validate(&json!({"action":"mouse_move","x":"1","y":2})).is_err());
        assert!(validate(&json!({"action":"mouse_click","x":1})).is_err());
        assert!(validate(&json!({"action":"screenshot","display_id":1,"window_id":2})).is_err());
        assert!(validate(&json!({"action":"screenshot","timeout_ms":0})).is_err());
    }
    #[test]
    fn screenshot_paths_cannot_escape_workspace() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "../escape.png",
            "/tmp/escape.png",
            "a/../../escape.png",
            "a\\escape.png",
        ] {
            assert!(
                destination(
                    dir.path(),
                    &json!({"action":"screenshot","output_path":path})
                )
                .is_err(),
                "{path}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), dir.path().join("linked")).unwrap();
            assert!(
                destination(
                    dir.path(),
                    &json!({"action":"screenshot","output_path":"linked/capture.png"})
                )
                .is_err()
            );
        }
    }

    #[test]
    fn screenshots_are_no_clobber_and_contain_the_actual_payload() {
        let dir = tempfile::tempdir().unwrap();
        let args = json!({"action":"screenshot","output_path":"capture.png"});
        let target = destination(dir.path(), &args).unwrap();
        let bytes = super::super::mock::PNG;
        let result = publish(&target, &args, bytes, false).unwrap();
        assert_eq!(std::fs::read(target.path()).unwrap(), bytes);
        assert!(
            result
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image(_)))
        );
        assert!(destination(dir.path(), &args).is_err());
        assert!(publish(&target, &args, bytes, false).is_err());
        assert_eq!(std::fs::read(target.path()).unwrap(), bytes);
    }
}
