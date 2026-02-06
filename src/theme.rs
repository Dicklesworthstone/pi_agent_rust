//! JSON theme file format and loader.
//!
//! This module defines a Pi-specific theme schema and discovery rules:
//! - Global themes: `~/.pi/agent/themes/*.json`
//! - Project themes: `<cwd>/.pi/themes/*.json`

use crate::config::Config;
use crate::error::{Error, Result};
use glamour::{Style as GlamourStyle, StyleConfig as GlamourStyleConfig};
use lipgloss::Style as LipglossStyle;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

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
    /// - If `config.theme` is unset/empty, defaults to [`Theme::dark`].
    /// - If set to `dark`, `light`, or `solarized`, uses built-in defaults.
    /// - Otherwise, attempts to resolve a theme spec:
    ///   - discovered theme name (from user/project theme dirs)
    ///   - theme JSON file path (absolute or cwd-relative, supports `~/...`)
    ///
    /// Falls back to dark on error.
    #[must_use]
    pub fn resolve(config: &Config, cwd: &Path) -> Self {
        let Some(spec) = config.theme.as_deref() else {
            return Self::dark();
        };
        let spec = spec.trim();
        if spec.is_empty() {
            return Self::dark();
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
    pub fn load_by_name_with_roots(name: &str, roots: &ThemeRoots) -> Result<Self> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::validation("Theme name is empty"));
        }

        for path in Self::discover_themes_with_roots(roots) {
            if let Ok(theme) = Self::load(&path) {
                if theme.name.eq_ignore_ascii_case(name) {
                    return Ok(theme);
                }
            }
        }

        Err(Error::config(format!("Theme not found: {name}")))
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
    if trimmed.starts_with('~') {
        return dirs::home_dir()
            .unwrap_or_else(|| cwd.to_path_buf())
            .join(trimmed.trim_start_matches('~'));
    }

    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn parse_hex_color(value: &str) -> Option<(u8, u8, u8)> {
    let value = value.trim();
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
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

    #[test]
    fn resolve_defaults_to_dark_when_no_theme_set() {
        let cfg = Config {
            theme: None,
            ..Default::default()
        };
        let cwd = tempfile::tempdir().expect("tempdir");
        let resolved = Theme::resolve(&cfg, cwd.path());
        assert_eq!(resolved.name, "dark");
    }

    #[test]
    fn resolve_defaults_to_dark_when_theme_is_empty() {
        let cfg = Config {
            theme: Some("".to_string()),
            ..Default::default()
        };
        let cwd = tempfile::tempdir().expect("tempdir");
        let resolved = Theme::resolve(&cfg, cwd.path());
        assert_eq!(resolved.name, "dark");
    }

    #[test]
    fn resolve_defaults_to_dark_when_theme_is_whitespace() {
        let cfg = Config {
            theme: Some("   ".to_string()),
            ..Default::default()
        };
        let cwd = tempfile::tempdir().expect("tempdir");
        let resolved = Theme::resolve(&cfg, cwd.path());
        assert_eq!(resolved.name, "dark");
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
        theme.name = "".to_string();
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

    // ── tui_styles and glamour_style_config smoke tests ─────────────

    #[test]
    fn tui_styles_returns_valid_struct() {
        let styles = Theme::dark().tui_styles();
        // Just verify all fields are accessible without panic
        let _ = format!("{:?}", styles.title);
        let _ = format!("{:?}", styles.muted);
        let _ = format!("{:?}", styles.accent);
        let _ = format!("{:?}", styles.error_bold);
    }

    #[test]
    fn glamour_style_config_smoke() {
        let dark_config = Theme::dark().glamour_style_config();
        let light_config = Theme::light().glamour_style_config();
        // Verify the configs are created without panic
        assert!(dark_config.document.style.color.is_some());
        assert!(light_config.document.style.color.is_some());
    }
}
