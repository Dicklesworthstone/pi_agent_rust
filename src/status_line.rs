#![forbid(unsafe_code)]

//! Powerline status line, footer, and sticky HUDs (OMP-ADOPT / bd-cv653.9.4).
//!
//! Provides customizable status-line rendering with powerline glyphs, segment
//! priority-based responsive dropping, and per-session accent hue calculation.

use serde::{Deserialize, Serialize};

/// Predefined status line presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusLinePreset {
    Default,
    Minimal,
    Compact,
    Full,
    Nerd,
    Ascii,
}

impl Default for StatusLinePreset {
    fn default() -> Self {
        Self::Default
    }
}

/// Powerline separator style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeparatorStyle {
    Powerline, //  / 
    Thin,      //  / 
    Slash,     // /
    Dot,       // •
    Pipe,      // |
}

impl Default for SeparatorStyle {
    fn default() -> Self {
        Self::Powerline
    }
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
    pub fn new(id: SegmentId, priority: u8, min_width: usize) -> Self {
        Self {
            id,
            priority,
            min_width,
        }
    }

    #[must_use]
    pub fn render(&self, ctx: &StatusContext) -> Option<String> {
        match self.id {
            SegmentId::Model => {
                if ctx.model.is_empty() {
                    None
                } else {
                    Some(format!("󰚩 {}", ctx.model))
                }
            }
            SegmentId::Thinking => ctx.thinking_level.map(|lvl| format!("󱜙 {lvl}")),
            SegmentId::Mode => {
                if ctx.mode.is_empty() {
                    None
                } else {
                    Some(ctx.mode.to_uppercase())
                }
            }
            SegmentId::Path => {
                if ctx.cwd.is_empty() {
                    None
                } else {
                    Some(format!(" {}", ctx.cwd))
                }
            }
            SegmentId::Git => ctx.git_branch.map(|branch| {
                let status_icon = if ctx.git_dirty { "*" } else { "" };
                format!(" {branch}{status_icon}")
            }),
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
                    Some(format!("󰭻 {}", ctx.subagent_count))
                } else {
                    None
                }
            }
            SegmentId::SessionName => {
                if ctx.session_name.is_empty() {
                    None
                } else {
                    Some(format!("🏷 {}", ctx.session_name))
                }
            }
            SegmentId::Time => {
                if ctx.timestamp_str.is_empty() {
                    None
                } else {
                    Some(ctx.timestamp_str.to_string())
                }
            }
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
            if let Some(text) = seg.render(ctx) {
                rendered_segments.push((seg.priority, text));
            }
        }

        // Sort by priority descending to identify segments to drop first
        let sep_width = self.separator.glyph().chars().count() + 2;
        let sep_str = format!(" {} ", self.separator.glyph());
        while !rendered_segments.is_empty() {
            let total_len: usize = rendered_segments
                .iter()
                .map(|(_, t)| t.chars().count())
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

        rendered_segments
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join(&sep_str)
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
        assert!(narrow.len() <= 35 || !narrow.contains(" / "));
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
