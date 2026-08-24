#![forbid(unsafe_code)]

//! Unified overlay and set-piece surfaces (OMP-ADOPT / bd-cv653.9.8).
//!
//! Provides overlay stack management (Esc-stack, focus-trap), toast notifications,
//! welcome screen, help overlay, and picker modals.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Overlay surface types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayKind {
    Picker,
    Help,
    Welcome,
    Toast,
    AskDialog,
    PlanReview,
    SetupWizard,
    SettingsEditor,
}

/// Toast notification severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// A transient toast notification item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToastNotification {
    pub id: String,
    pub title: String,
    pub message: String,
    pub level: ToastLevel,
    pub duration_ms: u64,
}

impl ToastNotification {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        message: impl Into<String>,
        level: ToastLevel,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            message: message.into(),
            level,
            duration_ms: 4000,
        }
    }
}

/// Toast notification queue.
#[derive(Debug, Clone, Default)]
pub struct ToastQueue {
    toasts: VecDeque<ToastNotification>,
}

impl ToastQueue {
    pub fn push(&mut self, toast: ToastNotification) {
        if self.toasts.len() >= 5 {
            self.toasts.pop_front();
        }
        self.toasts.push_back(toast);
    }

    pub fn pop(&mut self) -> Option<ToastNotification> {
        self.toasts.pop_front()
    }

    #[must_use]
    pub fn active_toasts(&self) -> &VecDeque<ToastNotification> {
        &self.toasts
    }

    pub fn clear(&mut self) {
        self.toasts.clear();
    }
}

/// Welcome screen model with curated tips.
#[derive(Debug, Clone)]
pub struct WelcomeScreen {
    pub greeting: String,
    pub recent_sessions: Vec<String>,
    pub tips: Vec<String>,
    pub active_tip_index: usize,
}

impl Default for WelcomeScreen {
    fn default() -> Self {
        Self {
            greeting: "Welcome to Pi Agent".to_string(),
            recent_sessions: Vec::new(),
            tips: vec![
                "Tip: Type /help to see all available slash commands and shortcuts.".to_string(),
                "Tip: Use @<filename> to autocomplete and reference project files.".to_string(),
                "Tip: Use !<command> to execute bash commands directly in the conversation.".to_string(),
                "Tip: Type /model to interactively switch AI models and providers.".to_string(),
                "Tip: Press Ctrl+P to quickly cycle between model roles.".to_string(),
            ],
            active_tip_index: 0,
        }
    }
}

impl WelcomeScreen {
    #[must_use]
    pub fn current_tip(&self) -> &str {
        if self.tips.is_empty() {
            ""
        } else {
            self.tips
                .get(self.active_tip_index % self.tips.len())
                .map_or("", String::as_str)
        }
    }

    pub fn next_tip(&mut self) {
        if !self.tips.is_empty() {
            self.active_tip_index = (self.active_tip_index + 1) % self.tips.len();
        }
    }
}

/// General overlay modal item on the Esc-stack.
#[derive(Debug, Clone)]
pub struct OverlayEntry {
    pub kind: OverlayKind,
    pub title: String,
    pub items: Vec<String>,
    pub selected_index: usize,
    pub is_dismissible: bool,
}

/// Overlay stack managing layered modals and Esc-dismissal.
#[derive(Debug, Clone, Default)]
pub struct OverlayStack {
    stack: Vec<OverlayEntry>,
}

impl OverlayStack {
    pub fn push(&mut self, entry: OverlayEntry) {
        self.stack.push(entry);
    }

    pub fn pop(&mut self) -> Option<OverlayEntry> {
        self.stack.pop()
    }

    pub fn dismiss_top(&mut self) -> bool {
        if let Some(top) = self.stack.last() {
            if top.is_dismissible {
                self.stack.pop();
                return true;
            }
        }
        false
    }

    #[must_use]
    pub fn top(&self) -> Option<&OverlayEntry> {
        self.stack.last()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.stack.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toast_queue_capacity_and_fifo() {
        let mut queue = ToastQueue::default();
        for i in 1..=6 {
            queue.push(ToastNotification::new(
                format!("toast-{i}"),
                format!("Title {i}"),
                "message",
                ToastLevel::Info,
            ));
        }

        // Capacity capped at 5
        assert_eq!(queue.active_toasts().len(), 5);
        // First item (toast-1) was dropped
        assert_eq!(queue.pop().unwrap().id, "toast-2");
    }

    #[test]
    fn test_welcome_screen_tips_rotation() {
        let mut welcome = WelcomeScreen::default();
        let tip1 = welcome.current_tip().to_string();
        welcome.next_tip();
        let tip2 = welcome.current_tip().to_string();
        assert_ne!(tip1, tip2);
    }

    #[test]
    fn test_overlay_stack_dismissal() {
        let mut stack = OverlayStack::default();
        stack.push(OverlayEntry {
            kind: OverlayKind::Picker,
            title: "Model Picker".to_string(),
            items: vec!["gpt-4o".to_string(), "claude-3-7-sonnet".to_string()],
            selected_index: 0,
            is_dismissible: true,
        });

        assert_eq!(stack.len(), 1);
        assert!(!stack.is_empty());
        assert_eq!(stack.top().unwrap().title, "Model Picker");

        assert!(stack.dismiss_top());
        assert!(stack.is_empty());
    }
}
