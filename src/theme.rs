//! JSON theme file format and loader.
//!
//! This module defines a Pi-specific theme schema and discovery rules:
//! - Global themes: `~/.pi/agent/themes/*.json`
//! - Project themes: `<cwd>/.pi/themes/*.json`

use crate::config::Config;
use crate::error::{Error, Result};
#[cfg(feature = "tui")]
use glamour::{Style as GlamourStyle, StyleConfig as GlamourStyleConfig};
#[cfg(feature = "tui")]
use lipgloss::Style as LipglossStyle;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// `TuiStyles` and the `tui_styles`/`glamour_style_config` helpers below build
// concrete lipgloss/glamour render styles and are only needed by the
// interactive front-end. The `Theme` data struct itself (colors, schema,
// discovery) stays unconditional because non-TUI code (resource loading,
// config) depends on it.
#[cfg(feature = "tui")]
#[derive(Debug, Clone)]
pub struct TuiStyles {
    pub title: LipglossStyle,
    pub muted: LipglossStyle,
    pub muted_bold: LipglossStyle,
    pub muted_italic: LipglossStyle,
    pub accent: LipglossStyle,
    pub accent_bold: LipglossStyle,
    pub success_bold: LipglossStyle,
    pub warning: LipglossStyle,
    pub warning_bold: LipglossStyle,
    pub error_bold: LipglossStyle,
    pub border: LipglossStyle,
    pub selection: LipglossStyle,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Theme {
    pub name: String,
    pub version: String,
    pub colors: ThemeColors,
    pub syntax: SyntaxColors,
    pub ui: UiColors,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThemeColors {
    pub foreground: String,
    pub background: String,
    pub accent: String,
    pub success: String,
    pub warning: String,
    pub error: String,
    pub muted: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SyntaxColors {
    pub keyword: String,
    pub string: String,
    pub number: String,
    pub comment: String,
    pub function: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UiColors {
    pub border: String,
    pub selection: String,
    pub cursor: String,
}

/// Terminal background classification used for theme auto-detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalBackground {
    Dark,
    Light,
}

/// Classify a `COLORFGBG` value into a terminal background.
///
/// `COLORFGBG` is a passive environment hint set by some terminals
/// (rxvt, konsole, and others) with the format `"<fg>;<bg>"` or
/// `"<fg>;default;<bg>"`. The last segment is the background color
/// index: values below 8 are dark colors, 8 and above are light.
///
/// Mirrors original Pi's `detectTerminalBackground()`: missing or
/// unparseable values default to dark.
#[must_use]
pub fn classify_colorfgbg(value: Option<&str>) -> TerminalBackground {
    let Some(value) = value else {
        return TerminalBackground::Dark;
    };
    let value = value.trim();
    if value.is_empty() {
        return TerminalBackground::Dark;
    }
    let mut parts = value.split(';');
    let (Some(_fg), Some(second)) = (parts.next(), parts.next()) else {
        return TerminalBackground::Dark;
    };
    // "fg;default;bg" form: the background index is the last segment.
    let bg = parts.next_back().unwrap_or(second);
    bg.trim()
        .parse::<u32>()
        .map_or(TerminalBackground::Dark, |index| {
            if index < 8 {
                TerminalBackground::Dark
            } else {
                TerminalBackground::Light
            }
        })
}

/// Detect the terminal background from the `COLORFGBG` environment variable.
///
/// This is intentionally passive (no OSC/tty queries, which can hang or
/// raise SIGTTIN in background processes). Defaults to dark when the
/// variable is missing or unparseable.
#[must_use]
pub fn detect_terminal_background() -> TerminalBackground {
    classify_colorfgbg(std::env::var("COLORFGBG").ok().as_deref())
}

/// Explicit roots for theme discovery.
#[derive(Debug, Clone)]
pub struct ThemeRoots {
    pub global_dir: PathBuf,
    pub project_dir: PathBuf,
}

impl ThemeRoots {
    #[must_use]
    pub fn from_cwd(cwd: &Path) -> Self {
        Self {
            global_dir: Config::global_dir(),
            project_dir: cwd.join(Config::project_dir()),
        }
    }
}

impl Theme {
    /// Resolve the active theme for the given config/cwd.
    ///
    /// - If `config.theme` is unset/empty, auto-detects dark/light from the
    ///   terminal background (`COLORFGBG`), defaulting to [`Theme::dark`]
    ///   when detection is inconclusive — matching original Pi.
    /// - If set to `dark`, `light`, or `solarized`, uses built-in defaults.
    /// - If set to `light/dark`, `auto`, or `system`, auto-detects as above.
    /// - Otherwise, attempts to resolve a theme spec:
    ///   - discovered theme name (from user/project theme dirs)
    ///   - theme JSON file path (absolute or cwd-relative, supports `~/...`)
    ///
    /// Falls back to dark on error.
    #[must_use]
    pub fn resolve(config: &Config, cwd: &Path) -> Self {
        let Some(spec) = config.theme.as_deref() else {
            return Self::detected();
        };
        let spec = spec.trim();
        if spec.is_empty() {
            return Self::detected();
        }

        match Self::resolve_spec(spec, cwd) {
            Ok(theme) => theme,
            Err(err) => {
                tracing::warn!("Failed to load theme '{spec}': {err}");
                Self::dark()
            }
        }
    }

    /// Resolve a theme spec into a theme.
    ///
    /// Supported specs:
    /// - Built-ins: `dark`, `light`, `solarized`
    /// - Auto-detection: `light/dark`, `auto`, `system` (via `COLORFGBG`)
    /// - Theme name: resolves via [`Self::load_by_name`]
    /// - File path: resolves via [`Self::load`] (absolute or cwd-relative, supports `~/...`)
    pub fn resolve_spec(spec: &str, cwd: &Path) -> Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(Error::validation("Theme spec is empty"));
        }
        if spec.eq_ignore_ascii_case("dark") {
            return Ok(Self::dark());
        }
        if spec.eq_ignore_ascii_case("light") {
            return Ok(Self::light());
        }
        if spec.eq_ignore_ascii_case("solarized") {
            return Ok(Self::solarized());
        }
        if spec.eq_ignore_ascii_case("light/dark")
            || spec.eq_ignore_ascii_case("auto")
            || spec.eq_ignore_ascii_case("system")
        {
            return Ok(Self::detected());
        }

        if looks_like_theme_path(spec) {
            let path = resolve_theme_path(spec, cwd);
            if !path.exists() {
                return Err(Error::config(format!(
                    "Theme file not found: {}",
                    path.display()
                )));
            }
            return Self::load(&path);
        }

        Self::load_by_name(spec, cwd)
    }

    #[must_use]
    pub fn is_light(&self) -> bool {
        let Some((r, g, b)) = parse_hex_color(&self.colors.background) else {
            return false;
        };
        // Relative luminance (sRGB) without gamma correction is sufficient here.
        // Treat anything above mid-gray as light.
        let r = f64::from(r);
        let g = f64::from(g);
        let b = f64::from(b);
        let luma = 0.0722_f64.mul_add(b, 0.2126_f64.mul_add(r, 0.7152 * g));
        luma >= 128.0
    }

    #[cfg(feature = "tui")]
    #[must_use]
    pub fn tui_styles(&self) -> TuiStyles {
        let title = LipglossStyle::new()
            .bold()
            .foreground(self.colors.accent.as_str());
        let muted = LipglossStyle::new().foreground(self.colors.muted.as_str());
        let muted_bold = muted.clone().bold();
        let muted_italic = muted.clone().italic();

        TuiStyles {
            title,
            muted,
            muted_bold,
            muted_italic,
            accent: LipglossStyle::new().foreground(self.colors.accent.as_str()),
            accent_bold: LipglossStyle::new()
                .foreground(self.colors.accent.as_str())
                .bold(),
            success_bold: LipglossStyle::new()
                .foreground(self.colors.success.as_str())
                .bold(),
            warning: LipglossStyle::new().foreground(self.colors.warning.as_str()),
            warning_bold: LipglossStyle::new()
                .foreground(self.colors.warning.as_str())
                .bold(),
            error_bold: LipglossStyle::new()
                .foreground(self.colors.error.as_str())
                .bold(),
            border: LipglossStyle::new().foreground(self.ui.border.as_str()),
            selection: LipglossStyle::new()
                .foreground(self.colors.foreground.as_str())
                .background(self.ui.selection.as_str())
                .bold(),
        }
    }

    #[cfg(feature = "tui")]
    #[must_use]
    pub fn glamour_style_config(&self) -> GlamourStyleConfig {
        let mut config = if self.is_light() {
            GlamourStyle::Light.config()
        } else {
            GlamourStyle::Dark.config()
        };

        config.document.style.color = Some(self.colors.foreground.clone());

        // Headings use accent color
        let accent = Some(self.colors.accent.clone());
        config.heading.style.color.clone_from(&accent);
        config.h1.style.color.clone_from(&accent);
        config.h2.style.color.clone_from(&accent);
        config.h3.style.color.clone_from(&accent);
        config.h4.style.color.clone_from(&accent);
        config.h5.style.color.clone_from(&accent);
        config.h6.style.color.clone_from(&accent);

        // Links
        config.link.color.clone_from(&accent);
        config.link_text.color = accent;

        // Emphasis (bold/italic) uses foreground
        config.strong.color = Some(self.colors.foreground.clone());
        config.emph.color = Some(self.colors.foreground.clone());

        // Basic code styling (syntax-highlighting is controlled by glamour feature flags).
        let code_color = Some(self.syntax.string.clone());
        config.code.style.color.clone_from(&code_color);
        config.code_block.block.style.color = code_color;

        // Blockquotes use muted color
        config.block_quote.style.color = Some(self.colors.muted.clone());

        // Horizontal rules use muted color
        config.horizontal_rule.color = Some(self.colors.muted.clone());

        // Lists use foreground
        config.item.color = Some(self.colors.foreground.clone());
        config.enumeration.color = Some(self.colors.foreground.clone());

        config
    }

    /// Discover available theme JSON files.
    #[must_use]
    pub fn discover_themes(cwd: &Path) -> Vec<PathBuf> {
        Self::discover_themes_with_roots(&ThemeRoots::from_cwd(cwd))
    }

    /// Discover available theme JSON files using explicit roots.
    #[must_use]
    pub fn discover_themes_with_roots(roots: &ThemeRoots) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        paths.extend(glob_json(&roots.global_dir.join("themes")));
        paths.extend(glob_json(&roots.project_dir.join("themes")));
        paths.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
        paths
    }

    /// Load a theme from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let theme: Self = serde_json::from_str(&content)?;
        theme.validate()?;
        Ok(theme)
    }

    /// Load a theme by name, searching global and project theme directories.
    pub fn load_by_name(name: &str, cwd: &Path) -> Result<Self> {
        Self::load_by_name_with_roots(name, &ThemeRoots::from_cwd(cwd))
    }

    /// Load a theme by name using explicit roots.
    ///
    /// Project themes take precedence over global themes with the same name,
    /// consistent with the project-over-global convention used elsewhere
    /// (config, extension policies, resources).
    pub fn load_by_name_with_roots(name: &str, roots: &ThemeRoots) -> Result<Self> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::validation("Theme name is empty"));
        }

        // Search project themes first so they override global themes.
        if let Some(theme) =
            Self::load_named_theme_from_dir(&roots.project_dir.join("themes"), name)?
        {
            return Ok(theme);
        }

        if let Some(theme) =
            Self::load_named_theme_from_dir(&roots.global_dir.join("themes"), name)?
        {
            return Ok(theme);
        }

        Err(Error::config(format!("Theme not found: {name}")))
    }

    fn load_named_theme_from_dir(dir: &Path, name: &str) -> Result<Option<Self>> {
        for path in glob_json(dir) {
            match Self::load(&path) {
                Ok(theme) => {
                    if theme.name.eq_ignore_ascii_case(name) {
                        return Ok(Some(theme));
                    }
                }
                Err(err) if theme_path_stem_matches(&path, name) => {
                    return Err(Error::config(format!(
                        "Failed to load theme '{name}' from {}: {err}",
                        path.display()
                    )));
                }
                Err(_) => {}
            }
        }

        Ok(None)
    }

    /// Built-in theme matching a detected terminal background.
    #[must_use]
    pub fn from_background(background: TerminalBackground) -> Self {
        match background {
            TerminalBackground::Dark => Self::dark(),
            TerminalBackground::Light => Self::light(),
        }
    }

    /// Built-in theme auto-detected from the terminal background
    /// (`COLORFGBG`; dark when inconclusive).
    #[must_use]
    pub fn detected() -> Self {
        Self::from_background(detect_terminal_background())
    }

    /// Default dark theme.
    #[must_use]
    pub fn dark() -> Self {
        Self {
            name: "dark".to_string(),
            version: "1.0".to_string(),
            colors: ThemeColors {
                foreground: "#d4d4d4".to_string(),
                background: "#1e1e1e".to_string(),
                accent: "#007acc".to_string(),
                success: "#4ec9b0".to_string(),
                warning: "#ce9178".to_string(),
                error: "#f44747".to_string(),
                muted: "#6a6a6a".to_string(),
            },
            syntax: SyntaxColors {
                keyword: "#569cd6".to_string(),
                string: "#ce9178".to_string(),
                number: "#b5cea8".to_string(),
                comment: "#6a9955".to_string(),
                function: "#dcdcaa".to_string(),
            },
            ui: UiColors {
                border: "#3c3c3c".to_string(),
                selection: "#264f78".to_string(),
                cursor: "#aeafad".to_string(),
            },
        }
    }

    /// Default light theme.
    #[must_use]
    pub fn light() -> Self {
        Self {
            name: "light".to_string(),
            version: "1.0".to_string(),
            colors: ThemeColors {
                foreground: "#2d2d2d".to_string(),
                background: "#ffffff".to_string(),
                accent: "#0066bf".to_string(),
                success: "#2e8b57".to_string(),
                warning: "#b36200".to_string(),
                error: "#c62828".to_string(),
                muted: "#7a7a7a".to_string(),
            },
            syntax: SyntaxColors {
                keyword: "#0000ff".to_string(),
                string: "#a31515".to_string(),
                number: "#098658".to_string(),
                comment: "#008000".to_string(),
                function: "#795e26".to_string(),
            },
            ui: UiColors {
                border: "#c8c8c8".to_string(),
                selection: "#cce7ff".to_string(),
                cursor: "#000000".to_string(),
            },
        }
    }

    /// Default solarized dark theme.
    #[must_use]
    pub fn solarized() -> Self {
        Self {
            name: "solarized".to_string(),
            version: "1.0".to_string(),
            colors: ThemeColors {
                foreground: "#839496".to_string(),
                background: "#002b36".to_string(),
                accent: "#268bd2".to_string(),
                success: "#859900".to_string(),
                warning: "#b58900".to_string(),
                error: "#dc322f".to_string(),
                muted: "#586e75".to_string(),
            },
            syntax: SyntaxColors {
                keyword: "#268bd2".to_string(),
                string: "#2aa198".to_string(),
                number: "#d33682".to_string(),
                comment: "#586e75".to_string(),
                function: "#b58900".to_string(),
            },
            ui: UiColors {
                border: "#073642".to_string(),
                selection: "#073642".to_string(),
                cursor: "#93a1a1".to_string(),
            },
        }
    }

    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::validation("Theme name is empty"));
        }
        if self.version.trim().is_empty() {
            return Err(Error::validation("Theme version is empty"));
        }

        Self::validate_color("colors.foreground", &self.colors.foreground)?;
        Self::validate_color("colors.background", &self.colors.background)?;
        Self::validate_color("colors.accent", &self.colors.accent)?;
        Self::validate_color("colors.success", &self.colors.success)?;
        Self::validate_color("colors.warning", &self.colors.warning)?;
        Self::validate_color("colors.error", &self.colors.error)?;
        Self::validate_color("colors.muted", &self.colors.muted)?;

        Self::validate_color("syntax.keyword", &self.syntax.keyword)?;
        Self::validate_color("syntax.string", &self.syntax.string)?;
        Self::validate_color("syntax.number", &self.syntax.number)?;
        Self::validate_color("syntax.comment", &self.syntax.comment)?;
        Self::validate_color("syntax.function", &self.syntax.function)?;

        Self::validate_color("ui.border", &self.ui.border)?;
        Self::validate_color("ui.selection", &self.ui.selection)?;
        Self::validate_color("ui.cursor", &self.ui.cursor)?;

        Ok(())
    }

    fn validate_color(field: &str, value: &str) -> Result<()> {
        let value = value.trim();
        if !value.starts_with('#') || value.len() != 7 {
            return Err(Error::validation(format!(
                "Invalid color for {field}: {value}"
            )));
        }
        if !value[1..].chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::validation(format!(
                "Invalid color for {field}: {value}"
            )));
        }
        Ok(())
    }
}

fn glob_json(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            out.push(path);
        }
    }
    out
}

/// Returns true if the theme spec looks like a file path rather than a theme name.
/// Path-like specs: start with ~, have .json extension, or contain / or \.
#[must_use]
pub fn looks_like_theme_path(spec: &str) -> bool {
    let spec = spec.trim();
    if spec.starts_with('~') {
        return true;
    }
    if Path::new(spec)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        return true;
    }
    spec.contains('/') || spec.contains('\\')
}

fn theme_path_stem_matches(path: &Path, name: &str) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case(name))
}

fn resolve_theme_path(spec: &str, cwd: &Path) -> PathBuf {
    let trimmed = spec.trim();

    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| cwd.to_path_buf());
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return dirs::home_dir()
            .unwrap_or_else(|| cwd.to_path_buf())
            .join(rest);
    }
    if let Some(rest) = trimmed.strip_prefix('~') {
        return dirs::home_dir()
            .unwrap_or_else(|| cwd.to_path_buf())
            .join(rest);
    }

    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

pub(crate) fn parse_hex_color(value: &str) -> Option<(u8, u8, u8)> {
    let value = value.trim();
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }

    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_valid_theme_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dark.json");
        let json = serde_json::json!({
            "name": "test-dark",
            "version": "1.0",
            "colors": {
                "foreground": "#ffffff",
                "background": "#000000",
                "accent": "#123456",
                "success": "#00ff00",
                "warning": "#ffcc00",
                "error": "#ff0000",
                "muted": "#888888"
            },
            "syntax": {
                "keyword": "#111111",
                "string": "#222222",
                "number": "#333333",
                "comment": "#444444",
                "function": "#555555"
            },
            "ui": {
                "border": "#666666",
                "selection": "#777777",
                "cursor": "#888888"
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let theme = Theme::load(&path).expect("load theme");
        assert_eq!(theme.name, "test-dark");
        assert_eq!(theme.version, "1.0");
    }

    #[test]
    fn rejects_invalid_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.json");
        fs::write(&path, "{this is not json").unwrap();
        let err = Theme::load(&path).unwrap_err();
        assert!(
            matches!(&err, Error::Json(_)),
            "expected json error, got {err:?}"
        );
    }

    #[test]
    fn rejects_invalid_colors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bad.json");
        let json = serde_json::json!({
            "name": "bad",
            "version": "1.0",
            "colors": {
                "foreground": "red",
                "background": "#000000",
                "accent": "#123456",
                "success": "#00ff00",
                "warning": "#ffcc00",
                "error": "#ff0000",
                "muted": "#888888"
            },
            "syntax": {
                "keyword": "#111111",
                "string": "#222222",
                "number": "#333333",
                "comment": "#444444",
                "function": "#555555"
            },
            "ui": {
                "border": "#666666",
                "selection": "#777777",
                "cursor": "#888888"
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let err = Theme::load(&path).unwrap_err();
        assert!(
            matches!(&err, Error::Validation(_)),
            "expected validation error, got {err:?}"
        );
    }

    #[test]
    fn discover_themes_from_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let global = dir.path().join("global");
        let project = dir.path().join("project");
        let global_theme_dir = global.join("themes");
        let project_theme_dir = project.join("themes");
        fs::create_dir_all(&global_theme_dir).unwrap();
        fs::create_dir_all(&project_theme_dir).unwrap();
        fs::write(global_theme_dir.join("g.json"), "{}").unwrap();
        fs::write(project_theme_dir.join("p.json"), "{}").unwrap();

        let roots = ThemeRoots {
            global_dir: global,
            project_dir: project,
        };
        let themes = Theme::discover_themes_with_roots(&roots);
        assert_eq!(themes.len(), 2);
    }

    #[test]
    fn default_themes_validate() {
        Theme::dark().validate().expect("dark theme valid");
        Theme::light().validate().expect("light theme valid");
        Theme::solarized()
            .validate()
            .expect("solarized theme valid");
    }

    #[test]
    fn resolve_spec_supports_builtins() {
        let cwd = Path::new(".");
        assert_eq!(Theme::resolve_spec("dark", cwd).unwrap().name, "dark");
        assert_eq!(Theme::resolve_spec("light", cwd).unwrap().name, "light");
        assert_eq!(
            Theme::resolve_spec("solarized", cwd).unwrap().name,
            "solarized"
        );
    }

    #[test]
    fn resolve_spec_loads_from_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("custom.json");
        let json = serde_json::json!({
            "name": "custom",
            "version": "1.0",
            "colors": {
                "foreground": "#ffffff",
                "background": "#000000",
                "accent": "#123456",
                "success": "#00ff00",
                "warning": "#ffcc00",
                "error": "#ff0000",
                "muted": "#888888"
            },
            "syntax": {
                "keyword": "#111111",
                "string": "#222222",
                "number": "#333333",
                "comment": "#444444",
                "function": "#555555"
            },
            "ui": {
                "border": "#666666",
                "selection": "#777777",
                "cursor": "#888888"
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let theme = Theme::resolve_spec(path.to_str().unwrap(), dir.path()).expect("resolve spec");
        assert_eq!(theme.name, "custom");
    }

    #[test]
    fn resolve_spec_errors_on_missing_path() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let err = Theme::resolve_spec("does-not-exist.json", cwd.path()).unwrap_err();
        assert!(
            matches!(err, Error::Config(_)),
            "expected config error, got {err:?}"
        );
    }

    #[test]
    fn looks_like_theme_path_detects_names_and_paths() {
        assert!(!looks_like_theme_path("dark"));
        assert!(!looks_like_theme_path("custom-theme"));
        assert!(looks_like_theme_path("dark.json"));
        assert!(looks_like_theme_path("themes/dark"));
        assert!(looks_like_theme_path(r"themes\dark"));
        assert!(looks_like_theme_path("~/themes/dark.json"));
    }

    #[test]
    fn resolve_theme_path_handles_home_relative_and_absolute() {
        let cwd = Path::new("/work/cwd");
        let home = dirs::home_dir().unwrap_or_else(|| cwd.to_path_buf());

        assert_eq!(
            resolve_theme_path("themes/dark.json", cwd),
            cwd.join("themes/dark.json")
        );
        assert_eq!(
            resolve_theme_path("/tmp/theme.json", cwd),
            PathBuf::from("/tmp/theme.json")
        );
        assert_eq!(resolve_theme_path("~", cwd), home);
        assert_eq!(
            resolve_theme_path("~/themes/dark.json", cwd),
            home.join("themes/dark.json")
        );
        assert_eq!(resolve_theme_path("~custom", cwd), home.join("custom"));
    }

    #[test]
    fn parse_hex_color_trims_and_rejects_invalid_inputs() {
        assert_eq!(parse_hex_color("  #A0b1C2 "), Some((160, 177, 194)));
        assert_eq!(parse_hex_color("A0b1C2"), None);
        assert_eq!(parse_hex_color("#123"), None);
        assert_eq!(parse_hex_color("#12345G"), None);
    }

    #[test]
    fn is_light_uses_background_luminance_threshold() {
        let mut theme = Theme::dark();
        theme.colors.background = "#808080".to_string();
        assert!(theme.is_light(), "mid-gray should be treated as light");

        theme.colors.background = "#7f7f7f".to_string();
        assert!(!theme.is_light(), "just below threshold should be dark");

        theme.colors.background = "not-a-color".to_string();
        assert!(!theme.is_light(), "invalid colors should default to dark");
    }

    #[test]
    fn resolve_falls_back_to_dark_for_invalid_spec() {
        let cfg = Config {
            theme: Some("does-not-exist".to_string()),
            ..Default::default()
        };
        let cwd = tempfile::tempdir().expect("tempdir");
        let resolved = Theme::resolve(&cfg, cwd.path());
        assert_eq!(resolved.name, "dark");
    }

    // ── resolve with empty/None config ───────────────────────────────
    //
    // With no theme configured, `resolve` auto-detects from COLORFGBG
    // (matching original Pi). In environments without COLORFGBG this is
    // dark; assert against `Theme::detected()` so the tests hold either way.

    #[test]
    fn resolve_auto_detects_when_no_theme_set() {
        let cfg = Config {
            theme: None,
            ..Default::default()
        };
        let cwd = tempfile::tempdir().expect("tempdir");
        let resolved = Theme::resolve(&cfg, cwd.path());
        assert_eq!(resolved.name, Theme::detected().name);
    }

    #[test]
    fn resolve_auto_detects_when_theme_is_empty() {
        let cfg = Config {
            theme: Some(String::new()),
            ..Default::default()
        };
        let cwd = tempfile::tempdir().expect("tempdir");
        let resolved = Theme::resolve(&cfg, cwd.path());
        assert_eq!(resolved.name, Theme::detected().name);
    }

    #[test]
    fn resolve_auto_detects_when_theme_is_whitespace() {
        let cfg = Config {
            theme: Some("   ".to_string()),
            ..Default::default()
        };
        let cwd = tempfile::tempdir().expect("tempdir");
        let resolved = Theme::resolve(&cfg, cwd.path());
        assert_eq!(resolved.name, Theme::detected().name);
    }

    // ── terminal background detection (COLORFGBG) ────────────────────

    #[test]
    fn classify_colorfgbg_missing_defaults_to_dark() {
        assert_eq!(classify_colorfgbg(None), TerminalBackground::Dark);
        assert_eq!(classify_colorfgbg(Some("")), TerminalBackground::Dark);
        assert_eq!(classify_colorfgbg(Some("   ")), TerminalBackground::Dark);
    }

    #[test]
    fn classify_colorfgbg_two_part_form() {
        assert_eq!(classify_colorfgbg(Some("0;15")), TerminalBackground::Light);
        assert_eq!(classify_colorfgbg(Some("15;0")), TerminalBackground::Dark);
        assert_eq!(classify_colorfgbg(Some("15;7")), TerminalBackground::Dark);
        assert_eq!(classify_colorfgbg(Some("0;8")), TerminalBackground::Light);
    }

    #[test]
    fn classify_colorfgbg_three_part_form_uses_last_segment() {
        assert_eq!(
            classify_colorfgbg(Some("0;default;15")),
            TerminalBackground::Light
        );
        assert_eq!(
            classify_colorfgbg(Some("15;default;0")),
            TerminalBackground::Dark
        );
    }

    #[test]
    fn classify_colorfgbg_unparseable_defaults_to_dark() {
        assert_eq!(
            classify_colorfgbg(Some("default;default")),
            TerminalBackground::Dark
        );
        assert_eq!(
            classify_colorfgbg(Some("garbage")),
            TerminalBackground::Dark
        );
        assert_eq!(classify_colorfgbg(Some("0;abc")), TerminalBackground::Dark);
        assert_eq!(classify_colorfgbg(Some(";;")), TerminalBackground::Dark);
        assert_eq!(classify_colorfgbg(Some("0;-1")), TerminalBackground::Dark);
    }

    #[test]
    fn from_background_maps_to_builtin_themes() {
        assert_eq!(
            Theme::from_background(TerminalBackground::Dark).name,
            "dark"
        );
        assert_eq!(
            Theme::from_background(TerminalBackground::Light).name,
            "light"
        );
    }

    #[test]
    fn resolve_spec_auto_detection_specs() {
        let cwd = Path::new(".");
        let expected = Theme::detected().name;
        for spec in [
            "light/dark",
            "LIGHT/DARK",
            "auto",
            "Auto",
            "system",
            "SYSTEM",
        ] {
            let resolved = Theme::resolve_spec(spec, cwd).expect("resolve auto spec");
            assert_eq!(resolved.name, expected, "spec {spec:?}");
            assert!(
                resolved.name == "dark" || resolved.name == "light",
                "auto specs resolve to a built-in, got {}",
                resolved.name
            );
        }
    }

    // ── resolve_spec case insensitivity ──────────────────────────────

    #[test]
    fn resolve_spec_case_insensitive() {
        let cwd = Path::new(".");
        assert_eq!(Theme::resolve_spec("DARK", cwd).unwrap().name, "dark");
        assert_eq!(Theme::resolve_spec("Light", cwd).unwrap().name, "light");
        assert_eq!(
            Theme::resolve_spec("SOLARIZED", cwd).unwrap().name,
            "solarized"
        );
    }

    #[test]
    fn resolve_spec_empty_returns_error() {
        let err = Theme::resolve_spec("", Path::new(".")).unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }

    // ── validate_color edge cases ────────────────────────────────────

    #[test]
    fn validate_color_valid() {
        assert!(Theme::validate_color("test", "#000000").is_ok());
        assert!(Theme::validate_color("test", "#ffffff").is_ok());
        assert!(Theme::validate_color("test", "#AbCdEf").is_ok());
    }

    #[test]
    fn validate_color_invalid_no_hash() {
        assert!(Theme::validate_color("test", "000000").is_err());
    }

    #[test]
    fn validate_color_invalid_too_short() {
        assert!(Theme::validate_color("test", "#123").is_err());
    }

    #[test]
    fn validate_color_invalid_chars() {
        assert!(Theme::validate_color("test", "#ZZZZZZ").is_err());
    }

    // ── validate ─────────────────────────────────────────────────────

    #[test]
    fn validate_rejects_empty_name() {
        let mut theme = Theme::dark();
        theme.name = String::new();
        assert!(theme.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_version() {
        let mut theme = Theme::dark();
        theme.version = "  ".to_string();
        assert!(theme.validate().is_err());
    }

    // ── is_light ─────────────────────────────────────────────────────

    #[test]
    fn dark_theme_is_not_light() {
        assert!(!Theme::dark().is_light());
    }

    #[test]
    fn light_theme_is_light() {
        assert!(Theme::light().is_light());
    }

    // ── parse_hex_color ──────────────────────────────────────────────

    #[test]
    fn parse_hex_color_black_and_white() {
        assert_eq!(parse_hex_color("#000000"), Some((0, 0, 0)));
        assert_eq!(parse_hex_color("#ffffff"), Some((255, 255, 255)));
    }

    #[test]
    fn parse_hex_color_empty_returns_none() {
        assert_eq!(parse_hex_color(""), None);
    }

    // ── glob_json ────────────────────────────────────────────────────

    #[test]
    fn glob_json_nonexistent_dir() {
        let result = glob_json(Path::new("/nonexistent/dir"));
        assert!(result.is_empty());
    }

    #[test]
    fn glob_json_dir_with_non_json_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("readme.txt"), "hi").unwrap();
        fs::write(dir.path().join("theme.json"), "{}").unwrap();
        fs::write(dir.path().join("other.toml"), "").unwrap();

        let result = glob_json(dir.path());
        assert_eq!(result.len(), 1);
        assert!(result[0].to_string_lossy().ends_with("theme.json"));
    }

    // ── discover_themes_with_roots ──────────────────────────────────

    #[test]
    fn discover_themes_empty_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let roots = ThemeRoots {
            global_dir: dir.path().join("global"),
            project_dir: dir.path().join("project"),
        };
        let themes = Theme::discover_themes_with_roots(&roots);
        assert!(themes.is_empty());
    }

    // ── Theme serialization roundtrip ────────────────────────────────

    #[test]
    fn theme_serde_roundtrip() {
        let theme = Theme::dark();
        let json = serde_json::to_string(&theme).unwrap();
        let theme2: Theme = serde_json::from_str(&json).unwrap();
        assert_eq!(theme.name, theme2.name);
        assert_eq!(theme.colors.foreground, theme2.colors.foreground);
    }

    // ── load_by_name_with_roots ─────────────────────────────────────

    #[test]
    fn load_by_name_empty_name_returns_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let roots = ThemeRoots {
            global_dir: dir.path().join("global"),
            project_dir: dir.path().join("project"),
        };
        let err = Theme::load_by_name_with_roots("", &roots).unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }

    #[test]
    fn load_by_name_not_found_returns_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let roots = ThemeRoots {
            global_dir: dir.path().join("global"),
            project_dir: dir.path().join("project"),
        };
        let err = Theme::load_by_name_with_roots("nonexistent", &roots).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn load_by_name_finds_theme_in_global_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let global_themes = dir.path().join("global/themes");
        fs::create_dir_all(&global_themes).unwrap();

        let theme = Theme::dark();
        let mut custom = theme;
        custom.name = "mycustom".to_string();
        let json = serde_json::to_string_pretty(&custom).unwrap();
        fs::write(global_themes.join("mycustom.json"), json).unwrap();

        let roots = ThemeRoots {
            global_dir: dir.path().join("global"),
            project_dir: dir.path().join("project"),
        };
        let loaded = Theme::load_by_name_with_roots("mycustom", &roots).unwrap();
        assert_eq!(loaded.name, "mycustom");
    }

    #[test]
    fn load_by_name_project_overrides_global() {
        let dir = tempfile::tempdir().expect("tempdir");
        let global_themes = dir.path().join("global/themes");
        let project_themes = dir.path().join("project/themes");
        fs::create_dir_all(&global_themes).unwrap();
        fs::create_dir_all(&project_themes).unwrap();

        // Global theme with accent "#111111"
        let mut global_theme = Theme::dark();
        global_theme.name = "shared".to_string();
        global_theme.colors.accent = "#111111".to_string();
        let json = serde_json::to_string_pretty(&global_theme).unwrap();
        fs::write(global_themes.join("shared.json"), json).unwrap();

        // Project theme with same name but accent "#222222"
        let mut project_theme = Theme::dark();
        project_theme.name = "shared".to_string();
        project_theme.colors.accent = "#222222".to_string();
        let json = serde_json::to_string_pretty(&project_theme).unwrap();
        fs::write(project_themes.join("shared.json"), json).unwrap();

        let roots = ThemeRoots {
            global_dir: dir.path().join("global"),
            project_dir: dir.path().join("project"),
        };
        let loaded = Theme::load_by_name_with_roots("shared", &roots).unwrap();
        assert_eq!(
            loaded.colors.accent, "#222222",
            "project theme should override global theme with the same name"
        );
    }

    #[test]
    fn load_by_name_invalid_project_override_does_not_fall_back_to_global() {
        let dir = tempfile::tempdir().expect("tempdir");
        let global_themes = dir.path().join("global/themes");
        let project_themes = dir.path().join("project/themes");
        fs::create_dir_all(&global_themes).unwrap();
        fs::create_dir_all(&project_themes).unwrap();

        let mut global_theme = Theme::dark();
        global_theme.name = "shared".to_string();
        let json = serde_json::to_string_pretty(&global_theme).unwrap();
        fs::write(global_themes.join("shared.json"), json).unwrap();
        fs::write(project_themes.join("shared.json"), "{ not valid json").unwrap();

        let roots = ThemeRoots {
            global_dir: dir.path().join("global"),
            project_dir: dir.path().join("project"),
        };
        let err = Theme::load_by_name_with_roots("shared", &roots).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Failed to load theme 'shared'"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("project/themes/shared.json"),
            "unexpected error: {message}"
        );
    }

    // ── tui_styles and glamour_style_config smoke tests ─────────────

    #[cfg(feature = "tui")]
    #[test]
    fn tui_styles_returns_valid_struct() {
        let styles = Theme::dark().tui_styles();
        // Just verify all fields are accessible without panic
        let _ = format!("{:?}", styles.title);
        let _ = format!("{:?}", styles.muted);
        let _ = format!("{:?}", styles.accent);
        let _ = format!("{:?}", styles.error_bold);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn glamour_style_config_smoke() {
        let dark_config = Theme::dark().glamour_style_config();
        let light_config = Theme::light().glamour_style_config();
        // Verify the configs are created without panic
        assert!(dark_config.document.style.color.is_some());
        assert!(light_config.document.style.color.is_some());
    }

    mod proptest_theme {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `parse_hex_color` never panics.
            #[test]
            fn parse_hex_never_panics(s in ".{0,20}") {
                let _ = parse_hex_color(&s);
            }

            /// Valid 6-digit hex colors parse successfully.
            #[test]
            fn parse_hex_valid(r in 0u8..=255, g in 0u8..=255, b in 0u8..=255) {
                let hex = format!("#{r:02x}{g:02x}{b:02x}");
                let parsed = parse_hex_color(&hex);
                assert_eq!(parsed, Some((r, g, b)));
            }

            /// Uppercase hex also parses.
            #[test]
            fn parse_hex_case_insensitive(r in 0u8..=255, g in 0u8..=255, b in 0u8..=255) {
                let upper = format!("#{r:02X}{g:02X}{b:02X}");
                let lower = format!("#{r:02x}{g:02x}{b:02x}");
                assert_eq!(parse_hex_color(&upper), parse_hex_color(&lower));
            }

            /// Missing `#` prefix returns None.
            #[test]
            fn parse_hex_missing_hash(hex in "[0-9a-f]{6}") {
                assert!(parse_hex_color(&hex).is_none());
            }

            /// Wrong-length hex (not 6 digits) returns None.
            #[test]
            fn parse_hex_wrong_length(n in 1..10usize) {
                if n == 6 { return Ok(()); }
                let hex = format!("#{}", "a".repeat(n));
                assert!(parse_hex_color(&hex).is_none());
            }

            /// Whitespace-padded hex parses correctly.
            #[test]
            fn parse_hex_trims(r in 0u8..=255, g in 0u8..=255, b in 0u8..=255, ws in "[ \\t]{0,3}") {
                let hex = format!("{ws}#{r:02x}{g:02x}{b:02x}{ws}");
                assert_eq!(parse_hex_color(&hex), Some((r, g, b)));
            }

            /// `looks_like_theme_path` returns true for tilde paths.
            #[test]
            fn theme_path_tilde(suffix in "[a-z/]{0,20}") {
                assert!(looks_like_theme_path(&format!("~{suffix}")));
            }

            /// `looks_like_theme_path` returns true for .json extension.
            #[test]
            fn theme_path_json_ext(name in "[a-z]{1,10}") {
                assert!(looks_like_theme_path(&format!("{name}.json")));
            }

            /// `looks_like_theme_path` returns true for paths with slashes.
            #[test]
            fn theme_path_with_slash(a in "[a-z]{1,10}", b in "[a-z]{1,10}") {
                assert!(looks_like_theme_path(&format!("{a}/{b}")));
            }

            /// `looks_like_theme_path` returns false for plain names.
            #[test]
            fn theme_path_plain_name(name in "[a-z]{1,10}") {
                assert!(!looks_like_theme_path(&name));
            }

            /// `is_light` — black is dark, white is light.
            #[test]
            fn is_light_boundary(_dummy in 0..1u8) {
                let mut dark = Theme::dark();
                dark.colors.background = "#000000".to_string();
                assert!(!dark.is_light());

                dark.colors.background = "#ffffff".to_string();
                assert!(dark.is_light());
            }

            /// `is_light` — luminance threshold at ~128.
            #[test]
            fn is_light_luminance(r in 0u8..=255, g in 0u8..=255, b in 0u8..=255) {
                let mut theme = Theme::dark();
                theme.colors.background = format!("#{r:02x}{g:02x}{b:02x}");
                let luma =
                    0.0722_f64.mul_add(f64::from(b), 0.2126_f64.mul_add(f64::from(r), 0.7152 * f64::from(g)));
                assert_eq!(theme.is_light(), luma >= 128.0);
            }

            /// `is_light` returns false for invalid background color.
            #[test]
            fn is_light_invalid_color(s in "[a-z]{3,10}") {
                let mut theme = Theme::dark();
                theme.colors.background = s;
                assert!(!theme.is_light());
            }

            /// `Theme::dark()` serde roundtrip.
            #[test]
            fn theme_dark_serde_roundtrip(_dummy in 0..1u8) {
                let theme = Theme::dark();
                let json = serde_json::to_string(&theme).unwrap();
                let back: Theme = serde_json::from_str(&json).unwrap();
                assert_eq!(back.name, theme.name);
                assert_eq!(back.colors.background, theme.colors.background);
            }

            /// `Theme::light()` serde roundtrip.
            #[test]
            fn theme_light_serde_roundtrip(_dummy in 0..1u8) {
                let theme = Theme::light();
                let json = serde_json::to_string(&theme).unwrap();
                let back: Theme = serde_json::from_str(&json).unwrap();
                assert_eq!(back.name, theme.name);
                assert_eq!(back.colors.background, theme.colors.background);
            }

            /// `resolve_theme_path` — absolute paths are returned as-is.
            #[test]
            fn resolve_absolute_path(suffix in "[a-z]{1,20}") {
                let abs = format!("/tmp/{suffix}.json");
                let resolved = resolve_theme_path(&abs, Path::new("/cwd"));
                assert_eq!(resolved, PathBuf::from(&abs));
            }

            /// `resolve_theme_path` — relative paths are joined with cwd.
            #[test]
            fn resolve_relative_path(name in "[a-z]{1,10}") {
                let cwd = Path::new("/some/dir");
                let resolved = resolve_theme_path(&name, cwd);
                assert_eq!(resolved, cwd.join(&name));
            }

            /// `Theme::validate` accepts arbitrary valid 6-digit hex palettes.
            #[test]
            fn theme_validate_accepts_generated_valid_palette(
                name in "[a-z][a-z0-9_-]{0,15}",
                version in "[0-9]{1,2}\\.[0-9]{1,2}",
                palette in proptest::collection::vec((0u8..=255, 0u8..=255, 0u8..=255), 15)
            ) {
                let mut colors = palette.into_iter();
                let next_hex = |colors: &mut std::vec::IntoIter<(u8, u8, u8)>| -> String {
                    let (r, g, b) = colors.next().expect("palette length is fixed to 15");
                    format!("#{r:02x}{g:02x}{b:02x}")
                };

                let mut theme = Theme::dark();
                theme.name = name;
                theme.version = version;

                theme.colors.foreground = next_hex(&mut colors);
                theme.colors.background = next_hex(&mut colors);
                theme.colors.accent = next_hex(&mut colors);
                theme.colors.success = next_hex(&mut colors);
                theme.colors.warning = next_hex(&mut colors);
                theme.colors.error = next_hex(&mut colors);
                theme.colors.muted = next_hex(&mut colors);

                theme.syntax.keyword = next_hex(&mut colors);
                theme.syntax.string = next_hex(&mut colors);
                theme.syntax.number = next_hex(&mut colors);
                theme.syntax.comment = next_hex(&mut colors);
                theme.syntax.function = next_hex(&mut colors);

                theme.ui.border = next_hex(&mut colors);
                theme.ui.selection = next_hex(&mut colors);
                theme.ui.cursor = next_hex(&mut colors);

                assert!(theme.validate().is_ok());
            }

            /// `Theme::validate` fails closed when any color field is invalid.
            #[test]
            fn theme_validate_rejects_invalid_color_fields(field_idx in 0usize..15usize) {
                let mut theme = Theme::dark();
                let invalid = "not-a-color".to_string();

                match field_idx {
                    0 => theme.colors.foreground = invalid,
                    1 => theme.colors.background = invalid,
                    2 => theme.colors.accent = invalid,
                    3 => theme.colors.success = invalid,
                    4 => theme.colors.warning = invalid,
                    5 => theme.colors.error = invalid,
                    6 => theme.colors.muted = invalid,
                    7 => theme.syntax.keyword = invalid,
                    8 => theme.syntax.string = invalid,
                    9 => theme.syntax.number = invalid,
                    10 => theme.syntax.comment = invalid,
                    11 => theme.syntax.function = invalid,
                    12 => theme.ui.border = invalid,
                    13 => theme.ui.selection = invalid,
                    14 => theme.ui.cursor = invalid,
                    _ => unreachable!("field_idx range is 0..15"),
                }

                assert!(theme.validate().is_err());
            }
        }
    }
}
