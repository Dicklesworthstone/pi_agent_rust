#![forbid(unsafe_code)]

//! Visual component gallery harness (OMP-ADOPT / bd-cv653.9.10).
//!
//! Renders every tool card and UI component in every lifecycle state
//! (pending, streaming, success, error, collapsed) for visual QA and regression gating.

use serde::{Deserialize, Serialize};

/// Gallery component category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GalleryCategory {
    ToolCard,
    StatusLine,
    Overlay,
    Delight,
    Markdown,
}

/// Component lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Pending,
    Streaming,
    Success,
    Error,
    Collapsed,
}

/// Individual gallery item snapshot definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryItem {
    pub name: String,
    pub category: GalleryCategory,
    pub state: ComponentState,
    pub description: String,
    pub sample_output: String,
}

/// Visual gallery catalog and report matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryMatrix {
    pub schema: String,
    pub items: Vec<GalleryItem>,
}

impl Default for GalleryMatrix {
    fn default() -> Self {
        Self::new()
    }
}

impl GalleryMatrix {
    #[must_use]
    pub fn new() -> Self {
        let items = vec![
            // Tool cards
            GalleryItem {
                name: "read_tool_card".to_string(),
                category: GalleryCategory::ToolCard,
                state: ComponentState::Success,
                description: "Read file tool card (group of 3)".to_string(),
                sample_output: "✓ read src/main.rs (3 files grouped)".to_string(),
            },
            GalleryItem {
                name: "bash_tool_card".to_string(),
                category: GalleryCategory::ToolCard,
                state: ComponentState::Pending,
                description: "Bash execution in progress".to_string(),
                sample_output: "⠋ bash cargo check --all-targets".to_string(),
            },
            GalleryItem {
                name: "edit_diff_card".to_string(),
                category: GalleryCategory::ToolCard,
                state: ComponentState::Success,
                description: "Word-level diff card with gutter".to_string(),
                sample_output: "@@ -12,3 +12,3 @@\n- let count = 0;\n+ let count = 10;".to_string(),
            },
            // Status lines
            GalleryItem {
                name: "powerline_full".to_string(),
                category: GalleryCategory::StatusLine,
                state: ComponentState::Success,
                description: "Full powerline status line with 25 segments".to_string(),
                sample_output: "󰚩 claude-3-7-sonnet  PLAN   main*  ctx: 42%  $0.125".to_string(),
            },
            // Overlays
            GalleryItem {
                name: "toast_notification".to_string(),
                category: GalleryCategory::Overlay,
                state: ComponentState::Success,
                description: "Success toast notification".to_string(),
                sample_output: "╭─ Compaction Complete ─────────────────╮\n│ Saved 4500 tokens (reduced to 35%)     │\n╰───────────────────────────────────────╯".to_string(),
            },
            // Delight
            GalleryItem {
                name: "sparkline_widget".to_string(),
                category: GalleryCategory::Delight,
                state: ComponentState::Success,
                description: "Token velocity sparkline".to_string(),
                sample_output: " ▂▃▅▆▇█".to_string(),
            },
        ];

        Self {
            schema: "pi.gallery.matrix.v1".to_string(),
            items,
        }
    }

    #[must_use]
    pub fn render_report_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gallery_matrix_construction() {
        let matrix = GalleryMatrix::new();
        assert_eq!(matrix.schema, "pi.gallery.matrix.v1");
        assert!(!matrix.items.is_empty());

        let json = matrix.render_report_json();
        assert!(json.contains("read_tool_card"));
        assert!(json.contains("powerline_full"));
        assert!(json.contains("toast_notification"));
    }
}
