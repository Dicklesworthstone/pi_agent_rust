#![forbid(unsafe_code)]

//! Powerline status line, footer, and sticky HUDs (OMP-ADOPT / bd-cv653.9.4).
//!
//! Provides customizable status-line rendering with powerline glyphs, segment
//! priority-based responsive dropping, and per-session accent hue calculation.

use serde::{Deserialize, Serialize};

fn display_width(text: &str) -> usize {
    #[cfg(feature = "tui")]
    {
        unicode_width::UnicodeWidthStr::width(text)
    }
    #[cfg(not(feature = "tui"))]
    {
        text.chars().count()
    }
}

fn truncate_display_width(text: &str, maximum_width: usize) -> String {
    #[cfg(feature = "tui")]
    {
        let mut end = 0;
        for (index, character) in text.char_indices() {
            let candidate_end = index + character.len_utf8();
            if display_width(&text[..candidate_end]) > maximum_width {
                break;
            }
            end = candidate_end;
        }
        text[..end].to_string()
    }
    #[cfg(not(feature = "tui"))]
    {
        text.chars().take(maximum_width).collect()
    }
}

/// Predefined status line presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusLinePreset {
    #[default]
    Default,
    Minimal,
    Compact,
    Full,
    Nerd,
    Ascii,
}

/// Powerline separator style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeparatorStyle {
    #[default]
    Powerline, //  / 
    Thin,  //  / 
    Slash, // /
    Dot,   // •
    Pipe,  // |
}

impl SeparatorStyle {
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Powerline => "",
            Self::Thin => "",
            Self::Slash => "/",
            Self::Dot => "•",
            Self::Pipe => "|",
        }
    }
}

/// Status segment identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentId {
    Model,
    Thinking,
    Mode,
    Path,
    Git,
    ContextPct,
    Cost,
    Tokens,
    Subagents,
    SessionName,
    Time,
}

/// Status segment rendering context.
#[derive(Debug, Clone, Default)]
pub struct StatusContext<'a> {
    pub model: &'a str,
    pub thinking_level: Option<&'a str>,
    pub mode: &'a str,
    pub cwd: &'a str,
    pub git_branch: Option<&'a str>,
    pub git_dirty: bool,
    pub context_pct: u8,
    pub cost_usd: f64,
    pub tokens_used: u64,
    pub subagent_count: usize,
    pub session_name: &'a str,
    pub timestamp_str: &'a str,
}

/// Individual segment definition with priority and rendering logic.
#[derive(Debug, Clone)]
pub struct StatusSegment {
    pub id: SegmentId,
    pub priority: u8, // 1 = highest, 10 = lowest (dropped first on narrow terminals)
    pub min_width: usize,
}

impl StatusSegment {
    #[must_use]
    pub const fn new(id: SegmentId, priority: u8, min_width: usize) -> Self {
        Self {
            id,
            priority,
            min_width,
        }
    }

    #[must_use]
    pub fn render(&self, ctx: &StatusContext) -> Option<String> {
        self.render_with_icons(ctx, true)
    }

    fn render_with_icons(&self, ctx: &StatusContext, use_icons: bool) -> Option<String> {
        fn text(value: &str) -> Option<String> {
            let sanitized: String = value
                .chars()
                .filter(|character| !character.is_control())
                .collect();
            let sanitized = sanitized.trim();
            (!sanitized.is_empty()).then(|| sanitized.to_string())
        }

        match self.id {
            SegmentId::Model => {
                let model = text(ctx.model)?;
                if use_icons {
                    Some(format!("󰚩 {model}"))
                } else {
                    Some(model)
                }
            }
            SegmentId::Thinking => {
                let level = text(ctx.thinking_level?)?;
                Some(if use_icons {
                    format!("󱜙 {level}")
                } else {
                    format!("think:{level}")
                })
            }
            SegmentId::Mode => {
                let mode = text(ctx.mode)?;
                Some(mode.to_uppercase())
            }
            SegmentId::Path => {
                let cwd = text(ctx.cwd)?;
                if use_icons {
                    Some(format!(" {cwd}"))
                } else {
                    Some(cwd)
                }
            }
            SegmentId::Git => {
                let branch = text(ctx.git_branch?)?;
                let status_icon = if ctx.git_dirty { "*" } else { "" };
                Some(if use_icons {
                    format!(" {branch}{status_icon}")
                } else {
                    format!("git:{branch}{status_icon}")
                })
            }
            SegmentId::ContextPct => Some(format!("ctx: {}%", ctx.context_pct)),
            SegmentId::Cost => {
                if ctx.cost_usd > 0.0 {
                    Some(format!("${:.3}", ctx.cost_usd))
                } else {
                    None
                }
            }
            SegmentId::Tokens => {
                if ctx.tokens_used > 0 {
                    Some(format!("{} tok", ctx.tokens_used))
                } else {
                    None
                }
            }
            SegmentId::Subagents => {
                if ctx.subagent_count > 0 {
                    Some(if use_icons {
                        format!("󰭻 {}", ctx.subagent_count)
                    } else {
                        format!("agents:{}", ctx.subagent_count)
                    })
                } else {
                    None
                }
            }
            SegmentId::SessionName => {
                let session_name = text(ctx.session_name)?;
                if use_icons {
                    Some(format!("🏷 {session_name}"))
                } else {
                    Some(format!("session:{session_name}"))
                }
            }
            SegmentId::Time => text(ctx.timestamp_str),
        }
    }
}

/// Powerline status line renderer.
#[derive(Debug, Clone)]
pub struct PowerlineStatusLine {
    pub preset: StatusLinePreset,
    pub separator: SeparatorStyle,
    pub segments: Vec<StatusSegment>,
}

impl Default for PowerlineStatusLine {
    fn default() -> Self {
        Self::with_preset(StatusLinePreset::Default)
    }
}

impl PowerlineStatusLine {
    #[must_use]
    pub fn with_preset(preset: StatusLinePreset) -> Self {
        let segments = match preset {
            StatusLinePreset::Minimal => vec![
                StatusSegment::new(SegmentId::Model, 1, 10),
                StatusSegment::new(SegmentId::Mode, 2, 6),
            ],
            StatusLinePreset::Compact => vec![
                StatusSegment::new(SegmentId::Model, 1, 10),
                StatusSegment::new(SegmentId::Thinking, 3, 8),
                StatusSegment::new(SegmentId::Mode, 2, 6),
                StatusSegment::new(SegmentId::Git, 4, 12),
                StatusSegment::new(SegmentId::ContextPct, 5, 8),
            ],
            StatusLinePreset::Default | StatusLinePreset::Nerd => vec![
                StatusSegment::new(SegmentId::Model, 1, 10),
                StatusSegment::new(SegmentId::Thinking, 3, 8),
                StatusSegment::new(SegmentId::Mode, 2, 6),
                StatusSegment::new(SegmentId::Path, 4, 14),
                StatusSegment::new(SegmentId::Git, 5, 12),
                StatusSegment::new(SegmentId::ContextPct, 6, 8),
                StatusSegment::new(SegmentId::Cost, 7, 8),
                StatusSegment::new(SegmentId::Subagents, 8, 6),
            ],
            StatusLinePreset::Full => vec![
                StatusSegment::new(SegmentId::Model, 1, 10),
                StatusSegment::new(SegmentId::Thinking, 3, 8),
                StatusSegment::new(SegmentId::Mode, 2, 6),
                StatusSegment::new(SegmentId::Path, 4, 14),
                StatusSegment::new(SegmentId::Git, 5, 12),
                StatusSegment::new(SegmentId::ContextPct, 6, 8),
                StatusSegment::new(SegmentId::Tokens, 7, 10),
                StatusSegment::new(SegmentId::Cost, 8, 8),
                StatusSegment::new(SegmentId::Subagents, 9, 6),
                StatusSegment::new(SegmentId::SessionName, 10, 12),
                StatusSegment::new(SegmentId::Time, 11, 8),
            ],
            StatusLinePreset::Ascii => vec![
                StatusSegment::new(SegmentId::Model, 1, 10),
                StatusSegment::new(SegmentId::Mode, 2, 6),
                StatusSegment::new(SegmentId::Git, 3, 10),
                StatusSegment::new(SegmentId::ContextPct, 4, 8),
            ],
        };

        let separator = match preset {
            StatusLinePreset::Ascii => SeparatorStyle::Pipe,
            StatusLinePreset::Minimal | StatusLinePreset::Compact => SeparatorStyle::Slash,
            _ => SeparatorStyle::Powerline,
        };

        Self {
            preset,
            separator,
            segments,
        }
    }

    /// Render status line fitted into `available_width`.
    #[must_use]
    pub fn render(&self, ctx: &StatusContext, available_width: usize) -> String {
        let mut rendered_segments = Vec::new();
        for seg in &self.segments {
            if let Some(text) = seg.render_with_icons(ctx, self.preset != StatusLinePreset::Ascii) {
                rendered_segments.push((seg.priority, text));
            }
        }

        // Sort by priority descending to identify segments to drop first
        let sep_width = display_width(self.separator.glyph()) + 2;
        let sep_str = format!(" {} ", self.separator.glyph());
        while !rendered_segments.is_empty() {
            let total_len: usize = rendered_segments
                .iter()
                .map(|(_, text)| display_width(text))
                .sum::<usize>()
                + if rendered_segments.len() > 1 {
                    (rendered_segments.len() - 1) * sep_width
                } else {
                    0
                };

            if total_len <= available_width || rendered_segments.len() <= 1 {
                break;
            }

            // Drop lowest priority segment (highest priority number)
            if let Some((max_idx, _)) = rendered_segments
                .iter()
                .enumerate()
                .max_by_key(|(_, (p, _))| *p)
            {
                rendered_segments.remove(max_idx);
            }
        }

        let rendered = rendered_segments
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join(&sep_str);
        truncate_display_width(&rendered, available_width)
    }
}

/// Compute a stable per-session accent hue (0..360) based on djb2 hash.
#[must_use]
pub fn compute_session_accent_hue(session_name: &str) -> u16 {
    if session_name.is_empty() {
        return 210; // Default cool blue
    }

    let mut hash: u64 = 5381;
    for byte in session_name.bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(u64::from(byte));
    }

    (hash % 360) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_powerline_status_line_presets() {
        let ctx = StatusContext {
            model: "claude-3-7-sonnet",
            thinking_level: Some("high"),
            mode: "plan",
            cwd: "pi_agent_rust",
            git_branch: Some("main"),
            git_dirty: true,
            context_pct: 42,
            cost_usd: 0.125,
            tokens_used: 12500,
            subagent_count: 2,
            session_name: "alpha-session",
            timestamp_str: "14:02:00",
        };

        let minimal = PowerlineStatusLine::with_preset(StatusLinePreset::Minimal);
        let min_rendered = minimal.render(&ctx, 120);
        assert!(min_rendered.contains("claude-3-7-sonnet"));
        assert!(min_rendered.contains("PLAN"));

        let full = PowerlineStatusLine::with_preset(StatusLinePreset::Full);
        let full_rendered = full.render(&ctx, 200);
        assert!(full_rendered.contains("claude-3-7-sonnet"));
        assert!(full_rendered.contains("main*"));
        assert!(full_rendered.contains("42%"));
        assert!(full_rendered.contains("$0.125"));
    }

    #[test]
    fn test_status_line_responsive_dropping() {
        let ctx = StatusContext {
            model: "gemini-2.5-pro",
            thinking_level: Some("low"),
            mode: "agent",
            cwd: "/very/long/path/to/project/src",
            git_branch: Some("feature/super-long-branch-name"),
            git_dirty: false,
            context_pct: 88,
            cost_usd: 1.450,
            tokens_used: 98000,
            subagent_count: 5,
            session_name: "long-session-descriptor",
            timestamp_str: "18:45:12",
        };

        let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Full);
        // Wide terminal: all segments present
        let wide = status_line.render(&ctx, 200);
        assert!(wide.contains("gemini-2.5-pro"));
        assert!(wide.contains("feature/super-long-branch-name"));

        // Narrow terminal: lower priority segments dropped
        let narrow = status_line.render(&ctx, 35);
        assert!(display_width(&narrow) <= 35);
    }

    #[test]
    fn test_status_line_clamps_a_single_long_segment() {
        let ctx = StatusContext {
            model: "model-name-that-is-much-too-long",
            ..StatusContext::default()
        };
        let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Minimal);
        let rendered = status_line.render(&ctx, 8);
        assert_eq!(display_width(&rendered), 8);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn test_status_line_clamps_wide_unicode_to_terminal_cells() {
        let ctx = StatusContext {
            model: "模型🙂模型🙂",
            ..StatusContext::default()
        };
        let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Minimal);
        let rendered = status_line.render(&ctx, 7);

        assert!(display_width(&rendered) <= 7, "rendered {rendered:?}");
        assert!(rendered.ends_with("模型"), "rendered {rendered:?}");
        assert!(!rendered.contains('🙂'), "rendered {rendered:?}");
    }

    #[test]
    fn test_ascii_preset_uses_only_ascii_chrome() {
        let ctx = StatusContext {
            model: "gpt-4o",
            mode: "act",
            git_branch: Some("main"),
            git_dirty: true,
            context_pct: 42,
            ..StatusContext::default()
        };
        let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Ascii);
        let rendered = status_line.render(&ctx, 120);
        assert!(rendered.is_ascii(), "ASCII preset rendered {rendered:?}");
        assert!(rendered.contains("git:main*"));
    }

    #[test]
    fn test_status_line_strips_controls_from_context_fields() {
        let ctx = StatusContext {
            model: "safe\x1b]2;bad\nmodel",
            mode: "act",
            ..StatusContext::default()
        };
        let status_line = PowerlineStatusLine::with_preset(StatusLinePreset::Minimal);
        let rendered = status_line.render(&ctx, 120);
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\n'));
        assert!(rendered.contains("safe]2;badmodel"));
    }

    #[test]
    fn test_session_accent_hue_distribution() {
        let hue1 = compute_session_accent_hue("session-alpha");
        let hue2 = compute_session_accent_hue("session-beta");
        let hue3 = compute_session_accent_hue("session-gamma");

        assert!(hue1 < 360);
        assert!(hue2 < 360);
        assert!(hue3 < 360);
        assert_ne!(hue1, hue2);
        assert_ne!(hue2, hue3);
    }
}
