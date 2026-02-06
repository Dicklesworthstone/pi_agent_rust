//! Keybindings and action catalog for interactive mode.
//!
//! This module defines all available actions and their default key bindings,
//! matching the legacy Pi Agent behavior from keybindings.md.
//!
//! ## Usage
//!
//! ```ignore
//! use pi::keybindings::{AppAction, KeyBindings};
//!
//! let bindings = KeyBindings::default();
//! let action = bindings.lookup(&key_event);
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

// ============================================================================
// Load Result (for user config loading with diagnostics)
// ============================================================================

/// Result of loading keybindings with diagnostics.
#[derive(Debug)]
pub struct KeyBindingsLoadResult {
    /// The loaded keybindings (defaults if loading failed).
    pub bindings: KeyBindings,
    /// Path that was attempted to load.
    pub path: PathBuf,
    /// Warnings encountered during loading.
    pub warnings: Vec<KeyBindingsWarning>,
}

impl KeyBindingsLoadResult {
    /// Check if there were any warnings.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Format warnings for display.
    #[must_use]
    pub fn format_warnings(&self) -> String {
        self.warnings
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Warning types for keybindings loading.
#[derive(Debug, Clone)]
pub enum KeyBindingsWarning {
    /// Could not read the config file.
    ReadError { path: PathBuf, error: String },
    /// Could not parse the config file as JSON.
    ParseError { path: PathBuf, error: String },
    /// Unknown action ID in config.
    UnknownAction { action: String, path: PathBuf },
    /// Invalid key string in config.
    InvalidKey {
        action: String,
        key: String,
        error: String,
        path: PathBuf,
    },
    /// Invalid value type for key (not a string).
    InvalidKeyValue {
        action: String,
        index: usize,
        path: PathBuf,
    },
}

impl fmt::Display for KeyBindingsWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadError { path, error } => {
                write!(f, "Cannot read {}: {}", path.display(), error)
            }
            Self::ParseError { path, error } => {
                write!(f, "Invalid JSON in {}: {}", path.display(), error)
            }
            Self::UnknownAction { action, path } => {
                write!(
                    f,
                    "Unknown action '{}' in {} (ignored)",
                    action,
                    path.display()
                )
            }
            Self::InvalidKey {
                action,
                key,
                error,
                path,
            } => {
                write!(
                    f,
                    "Invalid key '{}' for action '{}' in {}: {}",
                    key,
                    action,
                    path.display(),
                    error
                )
            }
            Self::InvalidKeyValue {
                action,
                index,
                path,
            } => {
                write!(
                    f,
                    "Invalid value type at index {} for action '{}' in {} (expected string)",
                    index,
                    action,
                    path.display()
                )
            }
        }
    }
}

// ============================================================================
// Action Categories (for /hotkeys display grouping)
// ============================================================================

/// Categories for organizing actions in /hotkeys display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionCategory {
    CursorMovement,
    Deletion,
    TextInput,
    KillRing,
    Clipboard,
    Application,
    Session,
    ModelsThinking,
    Display,
    MessageQueue,
    Selection,
    SessionPicker,
}

impl ActionCategory {
    /// Human-readable display name for the category.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::CursorMovement => "Cursor Movement",
            Self::Deletion => "Deletion",
            Self::TextInput => "Text Input",
            Self::KillRing => "Kill Ring",
            Self::Clipboard => "Clipboard",
            Self::Application => "Application",
            Self::Session => "Session",
            Self::ModelsThinking => "Models & Thinking",
            Self::Display => "Display",
            Self::MessageQueue => "Message Queue",
            Self::Selection => "Selection (Lists, Pickers)",
            Self::SessionPicker => "Session Picker",
        }
    }

    /// Get all categories in display order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::CursorMovement,
            Self::Deletion,
            Self::TextInput,
            Self::KillRing,
            Self::Clipboard,
            Self::Application,
            Self::Session,
            Self::ModelsThinking,
            Self::Display,
            Self::MessageQueue,
            Self::Selection,
            Self::SessionPicker,
        ]
    }
}

// ============================================================================
// App Actions
// ============================================================================

/// All available actions that can be bound to keys.
///
/// Action IDs are stable (snake_case) for JSON serialization/deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AppAction {
    // Cursor Movement
    CursorUp,
    CursorDown,
    CursorLeft,
    CursorRight,
    CursorWordLeft,
    CursorWordRight,
    CursorLineStart,
    CursorLineEnd,
    JumpForward,
    JumpBackward,
    PageUp,
    PageDown,

    // Deletion
    DeleteCharBackward,
    DeleteCharForward,
    DeleteWordBackward,
    DeleteWordForward,
    DeleteToLineStart,
    DeleteToLineEnd,

    // Text Input
    NewLine,
    Submit,
    Tab,

    // Kill Ring
    Yank,
    YankPop,
    Undo,

    // Clipboard
    Copy,
    PasteImage,

    // Application
    Interrupt,
    Clear,
    Exit,
    Suspend,
    ExternalEditor,

    // Session
    NewSession,
    Tree,
    Fork,
    BranchPicker,
    BranchNextSibling,
    BranchPrevSibling,

    // Models & Thinking
    SelectModel,
    CycleModelForward,
    CycleModelBackward,
    CycleThinkingLevel,

    // Display
    ExpandTools,
    ToggleThinking,

    // Message Queue
    FollowUp,
    Dequeue,

    // Selection (Lists, Pickers)
    SelectUp,
    SelectDown,
    SelectPageUp,
    SelectPageDown,
    SelectConfirm,
    SelectCancel,

    // Session Picker
    ToggleSessionPath,
    ToggleSessionSort,
    ToggleSessionNamedFilter,
    RenameSession,
    DeleteSession,
    DeleteSessionNoninvasive,
}

impl AppAction {
    /// Human-readable display name for the action.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            // Cursor Movement
            Self::CursorUp => "Move cursor up",
            Self::CursorDown => "Move cursor down",
            Self::CursorLeft => "Move cursor left",
            Self::CursorRight => "Move cursor right",
            Self::CursorWordLeft => "Move cursor word left",
            Self::CursorWordRight => "Move cursor word right",
            Self::CursorLineStart => "Move to line start",
            Self::CursorLineEnd => "Move to line end",
            Self::JumpForward => "Jump forward to character",
            Self::JumpBackward => "Jump backward to character",
            Self::PageUp => "Scroll up by page",
            Self::PageDown => "Scroll down by page",

            // Deletion
            Self::DeleteCharBackward => "Delete character backward",
            Self::DeleteCharForward => "Delete character forward",
            Self::DeleteWordBackward => "Delete word backward",
            Self::DeleteWordForward => "Delete word forward",
            Self::DeleteToLineStart => "Delete to line start",
            Self::DeleteToLineEnd => "Delete to line end",

            // Text Input
            Self::NewLine => "Insert new line",
            Self::Submit => "Submit input",
            Self::Tab => "Tab / autocomplete",

            // Kill Ring
            Self::Yank => "Paste most recently deleted text",
            Self::YankPop => "Cycle through deleted text after yank",
            Self::Undo => "Undo last edit",

            // Clipboard
            Self::Copy => "Copy selection",
            Self::PasteImage => "Paste image from clipboard",

            // Application
            Self::Interrupt => "Cancel / abort",
            Self::Clear => "Clear editor",
            Self::Exit => "Exit (when editor empty)",
            Self::Suspend => "Suspend to background",
            Self::ExternalEditor => "Open in external editor",

            // Session
            Self::NewSession => "Start a new session",
            Self::Tree => "Open session tree navigator",
            Self::Fork => "Fork current session",
            Self::BranchPicker => "Open branch picker",
            Self::BranchNextSibling => "Switch to next sibling branch",
            Self::BranchPrevSibling => "Switch to previous sibling branch",

            // Models & Thinking
            Self::SelectModel => "Open model selector",
            Self::CycleModelForward => "Cycle to next model",
            Self::CycleModelBackward => "Cycle to previous model",
            Self::CycleThinkingLevel => "Cycle thinking level",

            // Display
            Self::ExpandTools => "Collapse/expand tool output",
            Self::ToggleThinking => "Collapse/expand thinking blocks",

            // Message Queue
            Self::FollowUp => "Queue follow-up message",
            Self::Dequeue => "Restore queued messages to editor",

            // Selection
            Self::SelectUp => "Move selection up",
            Self::SelectDown => "Move selection down",
            Self::SelectPageUp => "Page up in list",
            Self::SelectPageDown => "Page down in list",
            Self::SelectConfirm => "Confirm selection",
            Self::SelectCancel => "Cancel selection",

            // Session Picker
            Self::ToggleSessionPath => "Toggle path display",
            Self::ToggleSessionSort => "Toggle sort mode",
            Self::ToggleSessionNamedFilter => "Toggle named-only filter",
            Self::RenameSession => "Rename session",
            Self::DeleteSession => "Delete session",
            Self::DeleteSessionNoninvasive => "Delete session (when query empty)",
        }
    }

    /// Get the category this action belongs to.
    #[must_use]
    pub const fn category(&self) -> ActionCategory {
        match self {
            Self::CursorUp
            | Self::CursorDown
            | Self::CursorLeft
            | Self::CursorRight
            | Self::CursorWordLeft
            | Self::CursorWordRight
            | Self::CursorLineStart
            | Self::CursorLineEnd
            | Self::JumpForward
            | Self::JumpBackward
            | Self::PageUp
            | Self::PageDown => ActionCategory::CursorMovement,

            Self::DeleteCharBackward
            | Self::DeleteCharForward
            | Self::DeleteWordBackward
            | Self::DeleteWordForward
            | Self::DeleteToLineStart
            | Self::DeleteToLineEnd => ActionCategory::Deletion,

            Self::NewLine | Self::Submit | Self::Tab => ActionCategory::TextInput,

            Self::Yank | Self::YankPop | Self::Undo => ActionCategory::KillRing,

            Self::Copy | Self::PasteImage => ActionCategory::Clipboard,

            Self::Interrupt | Self::Clear | Self::Exit | Self::Suspend | Self::ExternalEditor => {
                ActionCategory::Application
            }

            Self::NewSession
            | Self::Tree
            | Self::Fork
            | Self::BranchPicker
            | Self::BranchNextSibling
            | Self::BranchPrevSibling => ActionCategory::Session,

            Self::SelectModel
            | Self::CycleModelForward
            | Self::CycleModelBackward
            | Self::CycleThinkingLevel => ActionCategory::ModelsThinking,

            Self::ExpandTools | Self::ToggleThinking => ActionCategory::Display,

            Self::FollowUp | Self::Dequeue => ActionCategory::MessageQueue,

            Self::SelectUp
            | Self::SelectDown
            | Self::SelectPageUp
            | Self::SelectPageDown
            | Self::SelectConfirm
            | Self::SelectCancel => ActionCategory::Selection,

            Self::ToggleSessionPath
            | Self::ToggleSessionSort
            | Self::ToggleSessionNamedFilter
            | Self::RenameSession
            | Self::DeleteSession
            | Self::DeleteSessionNoninvasive => ActionCategory::SessionPicker,
        }
    }

    /// Get all actions in a category.
    #[must_use]
    pub fn in_category(category: ActionCategory) -> Vec<Self> {
        Self::all()
            .iter()
            .copied()
            .filter(|a| a.category() == category)
            .collect()
    }

    /// Get all actions.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            // Cursor Movement
            Self::CursorUp,
            Self::CursorDown,
            Self::CursorLeft,
            Self::CursorRight,
            Self::CursorWordLeft,
            Self::CursorWordRight,
            Self::CursorLineStart,
            Self::CursorLineEnd,
            Self::JumpForward,
            Self::JumpBackward,
            Self::PageUp,
            Self::PageDown,
            // Deletion
            Self::DeleteCharBackward,
            Self::DeleteCharForward,
            Self::DeleteWordBackward,
            Self::DeleteWordForward,
            Self::DeleteToLineStart,
            Self::DeleteToLineEnd,
            // Text Input
            Self::NewLine,
            Self::Submit,
            Self::Tab,
            // Kill Ring
            Self::Yank,
            Self::YankPop,
            Self::Undo,
            // Clipboard
            Self::Copy,
            Self::PasteImage,
            // Application
            Self::Interrupt,
            Self::Clear,
            Self::Exit,
            Self::Suspend,
            Self::ExternalEditor,
            // Session
            Self::NewSession,
            Self::Tree,
            Self::Fork,
            Self::BranchPicker,
            Self::BranchNextSibling,
            Self::BranchPrevSibling,
            // Models & Thinking
            Self::SelectModel,
            Self::CycleModelForward,
            Self::CycleModelBackward,
            Self::CycleThinkingLevel,
            // Display
            Self::ExpandTools,
            Self::ToggleThinking,
            // Message Queue
            Self::FollowUp,
            Self::Dequeue,
            // Selection
            Self::SelectUp,
            Self::SelectDown,
            Self::SelectPageUp,
            Self::SelectPageDown,
            Self::SelectConfirm,
            Self::SelectCancel,
            // Session Picker
            Self::ToggleSessionPath,
            Self::ToggleSessionSort,
            Self::ToggleSessionNamedFilter,
            Self::RenameSession,
            Self::DeleteSession,
            Self::DeleteSessionNoninvasive,
        ]
    }
}

impl fmt::Display for AppAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Use serde's camelCase serialization for display
        write!(
            f,
            "{}",
            serde_json::to_string(self)
                .unwrap_or_default()
                .trim_matches('"')
        )
    }
}

// ============================================================================
// Key Modifiers
// ============================================================================

/// Key modifiers (ctrl, shift, alt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl KeyModifiers {
    /// No modifiers.
    pub const NONE: Self = Self {
        ctrl: false,
        shift: false,
        alt: false,
    };

    /// Ctrl modifier only.
    pub const CTRL: Self = Self {
        ctrl: true,
        shift: false,
        alt: false,
    };

    /// Shift modifier only.
    pub const SHIFT: Self = Self {
        ctrl: false,
        shift: true,
        alt: false,
    };

    /// Alt modifier only.
    pub const ALT: Self = Self {
        ctrl: false,
        shift: false,
        alt: true,
    };

    /// Ctrl+Shift modifiers.
    pub const CTRL_SHIFT: Self = Self {
        ctrl: true,
        shift: true,
        alt: false,
    };

    /// Ctrl+Alt modifiers.
    pub const CTRL_ALT: Self = Self {
        ctrl: true,
        shift: false,
        alt: true,
    };

    /// Alt+Shift modifiers (alias for consistency).
    pub const ALT_SHIFT: Self = Self {
        ctrl: false,
        shift: true,
        alt: true,
    };
}

// ============================================================================
// Key Binding
// ============================================================================

/// A key binding (key + modifiers).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyBinding {
    pub key: String,
    pub modifiers: KeyModifiers,
}

impl KeyBinding {
    /// Create a new key binding.
    #[must_use]
    pub fn new(key: impl Into<String>, modifiers: KeyModifiers) -> Self {
        Self {
            key: key.into(),
            modifiers,
        }
    }

    /// Create a key binding with no modifiers.
    #[must_use]
    pub fn plain(key: impl Into<String>) -> Self {
        Self::new(key, KeyModifiers::NONE)
    }

    /// Create a key binding with ctrl modifier.
    #[must_use]
    pub fn ctrl(key: impl Into<String>) -> Self {
        Self::new(key, KeyModifiers::CTRL)
    }

    /// Create a key binding with alt modifier.
    #[must_use]
    pub fn alt(key: impl Into<String>) -> Self {
        Self::new(key, KeyModifiers::ALT)
    }

    /// Create a key binding with shift modifier.
    #[must_use]
    pub fn shift(key: impl Into<String>) -> Self {
        Self::new(key, KeyModifiers::SHIFT)
    }

    /// Create a key binding with ctrl+shift modifiers.
    #[must_use]
    pub fn ctrl_shift(key: impl Into<String>) -> Self {
        Self::new(key, KeyModifiers::CTRL_SHIFT)
    }

    /// Create a key binding with ctrl+alt modifiers.
    #[must_use]
    pub fn ctrl_alt(key: impl Into<String>) -> Self {
        Self::new(key, KeyModifiers::CTRL_ALT)
    }

    /// Convert a bubbletea KeyMsg to a KeyBinding for lookup.
    ///
    /// Returns `None` for paste events or multi-character input that
    /// cannot map to a single key binding.
    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub fn from_bubbletea_key(key: &bubbletea::KeyMsg) -> Option<Self> {
        use bubbletea::KeyType;

        // Skip paste events - they're not keybindings
        if key.paste {
            return None;
        }

        let (key_name, mut modifiers) = match key.key_type {
            // Control keys map to ctrl+letter
            KeyType::Null => ("@", KeyModifiers::CTRL),
            KeyType::CtrlA => ("a", KeyModifiers::CTRL),
            KeyType::CtrlB => ("b", KeyModifiers::CTRL),
            KeyType::CtrlC => ("c", KeyModifiers::CTRL),
            KeyType::CtrlD => ("d", KeyModifiers::CTRL),
            KeyType::CtrlE => ("e", KeyModifiers::CTRL),
            KeyType::CtrlF => ("f", KeyModifiers::CTRL),
            KeyType::CtrlG => ("g", KeyModifiers::CTRL),
            KeyType::CtrlH => ("h", KeyModifiers::CTRL),
            KeyType::Tab => ("tab", KeyModifiers::NONE),
            KeyType::CtrlJ => ("j", KeyModifiers::CTRL),
            KeyType::CtrlK => ("k", KeyModifiers::CTRL),
            KeyType::CtrlL => ("l", KeyModifiers::CTRL),
            KeyType::Enter => ("enter", KeyModifiers::NONE),
            KeyType::ShiftEnter => ("enter", KeyModifiers::SHIFT),
            KeyType::CtrlEnter => ("enter", KeyModifiers::CTRL),
            KeyType::CtrlShiftEnter => ("enter", KeyModifiers::CTRL_SHIFT),
            KeyType::CtrlN => ("n", KeyModifiers::CTRL),
            KeyType::CtrlO => ("o", KeyModifiers::CTRL),
            KeyType::CtrlP => ("p", KeyModifiers::CTRL),
            KeyType::CtrlQ => ("q", KeyModifiers::CTRL),
            KeyType::CtrlR => ("r", KeyModifiers::CTRL),
            KeyType::CtrlS => ("s", KeyModifiers::CTRL),
            KeyType::CtrlT => ("t", KeyModifiers::CTRL),
            KeyType::CtrlU => ("u", KeyModifiers::CTRL),
            KeyType::CtrlV => ("v", KeyModifiers::CTRL),
            KeyType::CtrlW => ("w", KeyModifiers::CTRL),
            KeyType::CtrlX => ("x", KeyModifiers::CTRL),
            KeyType::CtrlY => ("y", KeyModifiers::CTRL),
            KeyType::CtrlZ => ("z", KeyModifiers::CTRL),
            KeyType::Esc => ("escape", KeyModifiers::NONE),
            KeyType::CtrlBackslash => ("\\", KeyModifiers::CTRL),
            KeyType::CtrlCloseBracket => ("]", KeyModifiers::CTRL),
            KeyType::CtrlCaret => ("^", KeyModifiers::CTRL),
            KeyType::CtrlUnderscore => ("_", KeyModifiers::CTRL),
            KeyType::Backspace => ("backspace", KeyModifiers::NONE),

            // Arrow keys
            KeyType::Up => ("up", KeyModifiers::NONE),
            KeyType::Down => ("down", KeyModifiers::NONE),
            KeyType::Left => ("left", KeyModifiers::NONE),
            KeyType::Right => ("right", KeyModifiers::NONE),

            // Shift variants
            KeyType::ShiftTab => ("tab", KeyModifiers::SHIFT),
            KeyType::ShiftUp => ("up", KeyModifiers::SHIFT),
            KeyType::ShiftDown => ("down", KeyModifiers::SHIFT),
            KeyType::ShiftLeft => ("left", KeyModifiers::SHIFT),
            KeyType::ShiftRight => ("right", KeyModifiers::SHIFT),
            KeyType::ShiftHome => ("home", KeyModifiers::SHIFT),
            KeyType::ShiftEnd => ("end", KeyModifiers::SHIFT),

            // Ctrl variants
            KeyType::CtrlUp => ("up", KeyModifiers::CTRL),
            KeyType::CtrlDown => ("down", KeyModifiers::CTRL),
            KeyType::CtrlLeft => ("left", KeyModifiers::CTRL),
            KeyType::CtrlRight => ("right", KeyModifiers::CTRL),
            KeyType::CtrlHome => ("home", KeyModifiers::CTRL),
            KeyType::CtrlEnd => ("end", KeyModifiers::CTRL),
            KeyType::CtrlPgUp => ("pageup", KeyModifiers::CTRL),
            KeyType::CtrlPgDown => ("pagedown", KeyModifiers::CTRL),

            // Ctrl+Shift variants
            KeyType::CtrlShiftUp => ("up", KeyModifiers::CTRL_SHIFT),
            KeyType::CtrlShiftDown => ("down", KeyModifiers::CTRL_SHIFT),
            KeyType::CtrlShiftLeft => ("left", KeyModifiers::CTRL_SHIFT),
            KeyType::CtrlShiftRight => ("right", KeyModifiers::CTRL_SHIFT),
            KeyType::CtrlShiftHome => ("home", KeyModifiers::CTRL_SHIFT),
            KeyType::CtrlShiftEnd => ("end", KeyModifiers::CTRL_SHIFT),

            // Navigation
            KeyType::Home => ("home", KeyModifiers::NONE),
            KeyType::End => ("end", KeyModifiers::NONE),
            KeyType::PgUp => ("pageup", KeyModifiers::NONE),
            KeyType::PgDown => ("pagedown", KeyModifiers::NONE),
            KeyType::Delete => ("delete", KeyModifiers::NONE),
            KeyType::Insert => ("insert", KeyModifiers::NONE),
            KeyType::Space => ("space", KeyModifiers::NONE),

            // Function keys
            KeyType::F1 => ("f1", KeyModifiers::NONE),
            KeyType::F2 => ("f2", KeyModifiers::NONE),
            KeyType::F3 => ("f3", KeyModifiers::NONE),
            KeyType::F4 => ("f4", KeyModifiers::NONE),
            KeyType::F5 => ("f5", KeyModifiers::NONE),
            KeyType::F6 => ("f6", KeyModifiers::NONE),
            KeyType::F7 => ("f7", KeyModifiers::NONE),
            KeyType::F8 => ("f8", KeyModifiers::NONE),
            KeyType::F9 => ("f9", KeyModifiers::NONE),
            KeyType::F10 => ("f10", KeyModifiers::NONE),
            KeyType::F11 => ("f11", KeyModifiers::NONE),
            KeyType::F12 => ("f12", KeyModifiers::NONE),
            KeyType::F13 => ("f13", KeyModifiers::NONE),
            KeyType::F14 => ("f14", KeyModifiers::NONE),
            KeyType::F15 => ("f15", KeyModifiers::NONE),
            KeyType::F16 => ("f16", KeyModifiers::NONE),
            KeyType::F17 => ("f17", KeyModifiers::NONE),
            KeyType::F18 => ("f18", KeyModifiers::NONE),
            KeyType::F19 => ("f19", KeyModifiers::NONE),
            KeyType::F20 => ("f20", KeyModifiers::NONE),

            // Character input
            KeyType::Runes => {
                // Only handle single-character input
                if key.runes.len() != 1 {
                    return None;
                }
                let c = key.runes[0];
                // Return a binding for the character
                // Alt modifier is handled below
                return Some(Self {
                    key: c.to_lowercase().to_string(),
                    modifiers: if key.alt {
                        KeyModifiers::ALT
                    } else {
                        KeyModifiers::NONE
                    },
                });
            }
        };

        // Apply alt modifier if set (for non-Runes keys)
        if key.alt {
            modifiers.alt = true;
        }

        Some(Self {
            key: key_name.to_string(),
            modifiers,
        })
    }
}

impl fmt::Display for KeyBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.modifiers.ctrl {
            parts.push("ctrl");
        }
        if self.modifiers.alt {
            parts.push("alt");
        }
        if self.modifiers.shift {
            parts.push("shift");
        }
        parts.push(&self.key);
        write!(f, "{}", parts.join("+"))
    }
}

impl FromStr for KeyBinding {
    type Err = KeyBindingParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_key_binding(s)
    }
}

/// Error type for key binding parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyBindingParseError {
    /// The input string was empty.
    Empty,
    /// No key found in the binding (only modifiers).
    NoKey,
    /// Multiple keys found (e.g., "a+b").
    MultipleKeys { binding: String },
    /// Duplicate modifier (e.g., "ctrl+ctrl+x").
    DuplicateModifier { modifier: String, binding: String },
    /// Unknown modifier (e.g., "meta+enter").
    UnknownModifier { modifier: String, binding: String },
    /// Unknown key name.
    UnknownKey { key: String, binding: String },
}

impl fmt::Display for KeyBindingParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "Empty key binding"),
            Self::NoKey => write!(f, "No key in binding (only modifiers)"),
            Self::MultipleKeys { binding } => write!(f, "Multiple keys in binding: {binding}"),
            Self::DuplicateModifier { modifier, binding } => {
                write!(f, "Duplicate modifier '{modifier}' in binding: {binding}")
            }
            Self::UnknownModifier { modifier, binding } => {
                write!(f, "Unknown modifier '{modifier}' in binding: {binding}")
            }
            Self::UnknownKey { key, binding } => {
                write!(f, "Unknown key '{key}' in binding: {binding}")
            }
        }
    }
}

impl std::error::Error for KeyBindingParseError {}

/// Normalize a key name to its canonical form.
///
/// Handles synonyms (esc→escape, return→enter) and case normalization.
fn normalize_key_name(key: &str) -> Option<String> {
    let lower = key.to_lowercase();

    // Check synonyms first
    let canonical = match lower.as_str() {
        // Synonyms
        "esc" => "escape",
        "return" => "enter",

        // Valid special keys and function keys (f1-f20 to match bubbletea KeyType coverage)
        "escape" | "enter" | "tab" | "space" | "backspace" | "delete" | "insert" | "clear"
        | "home" | "end" | "pageup" | "pagedown" | "up" | "down" | "left" | "right" | "f1"
        | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" | "f13"
        | "f14" | "f15" | "f16" | "f17" | "f18" | "f19" | "f20" => &lower,

        // Single letters (a-z)
        s if s.len() == 1 && s.chars().next().is_some_and(|c| c.is_ascii_lowercase()) => &lower,

        // Symbols (single characters that are valid keys)
        "`" | "-" | "=" | "[" | "]" | "\\" | ";" | "'" | "," | "." | "/" | "!" | "@" | "#"
        | "$" | "%" | "^" | "&" | "*" | "(" | ")" | "_" | "+" | "|" | "~" | "{" | "}" | ":"
        | "<" | ">" | "?" | "\"" => &lower,

        // Invalid key
        _ => return None,
    };

    Some(canonical.to_string())
}

/// Parse a key binding string into a KeyBinding.
///
/// Supports formats like:
/// - "a" (single key)
/// - "ctrl+a" (modifier + key)
/// - "ctrl+shift+p" (multiple modifiers + key)
/// - "pageUp" (special key, case insensitive)
///
/// # Errors
///
/// Returns an error for:
/// - Empty strings
/// - No key (only modifiers)
/// - Multiple keys
/// - Duplicate modifiers
/// - Unknown keys
fn parse_key_binding(s: &str) -> Result<KeyBinding, KeyBindingParseError> {
    let binding = s.trim();
    if binding.is_empty() {
        return Err(KeyBindingParseError::Empty);
    }

    // Be forgiving about whitespace: "ctrl + a" is treated as "ctrl+a".
    let compacted = binding
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    let normalized = compacted.to_lowercase();
    let mut rest = normalized.as_str();

    let mut ctrl_seen = false;
    let mut alt_seen = false;
    let mut shift_seen = false;

    // Parse modifiers as a prefix chain so we can represent the '+' key itself (e.g. "ctrl++").
    loop {
        if let Some(after) = rest.strip_prefix("ctrl+") {
            if ctrl_seen {
                return Err(KeyBindingParseError::DuplicateModifier {
                    modifier: "ctrl".to_string(),
                    binding: binding.to_string(),
                });
            }
            ctrl_seen = true;
            rest = after;
            continue;
        }
        if let Some(after) = rest.strip_prefix("control+") {
            if ctrl_seen {
                return Err(KeyBindingParseError::DuplicateModifier {
                    modifier: "ctrl".to_string(),
                    binding: binding.to_string(),
                });
            }
            ctrl_seen = true;
            rest = after;
            continue;
        }
        if let Some(after) = rest.strip_prefix("alt+") {
            if alt_seen {
                return Err(KeyBindingParseError::DuplicateModifier {
                    modifier: "alt".to_string(),
                    binding: binding.to_string(),
                });
            }
            alt_seen = true;
            rest = after;
            continue;
        }
        if let Some(after) = rest.strip_prefix("shift+") {
            if shift_seen {
                return Err(KeyBindingParseError::DuplicateModifier {
                    modifier: "shift".to_string(),
                    binding: binding.to_string(),
                });
            }
            shift_seen = true;
            rest = after;
            continue;
        }
        break;
    }

    if rest.is_empty() {
        return Err(KeyBindingParseError::NoKey);
    }

    // Allow "ctrl" / "ctrl+shift" to be treated as "only modifiers".
    if matches!(rest, "ctrl" | "control" | "alt" | "shift") {
        return Err(KeyBindingParseError::NoKey);
    }

    // After consuming known modifiers, any remaining '+' means either:
    // - the '+' key itself (rest == "+")
    // - multiple keys (e.g. "a+b") or an unknown modifier (e.g. "meta+enter")
    if rest.contains('+') && rest != "+" {
        let first = rest.split('+').next().unwrap_or("");
        if first.is_empty() || normalize_key_name(first).is_some() {
            return Err(KeyBindingParseError::MultipleKeys {
                binding: binding.to_string(),
            });
        }
        return Err(KeyBindingParseError::UnknownModifier {
            modifier: first.to_string(),
            binding: binding.to_string(),
        });
    }

    let key = normalize_key_name(rest).ok_or_else(|| KeyBindingParseError::UnknownKey {
        key: rest.to_string(),
        binding: binding.to_string(),
    })?;

    Ok(KeyBinding {
        key,
        modifiers: KeyModifiers {
            ctrl: ctrl_seen,
            shift: shift_seen,
            alt: alt_seen,
        },
    })
}

/// Check if a key string is valid (for validation without full parsing).
#[must_use]
pub fn is_valid_key(s: &str) -> bool {
    parse_key_binding(s).is_ok()
}

impl Serialize for KeyBinding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for KeyBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ============================================================================
// Key Bindings Map
// ============================================================================

/// Complete keybindings configuration.
#[derive(Debug, Clone)]
pub struct KeyBindings {
    /// Map from action to list of key bindings.
    bindings: HashMap<AppAction, Vec<KeyBinding>>,
    /// Reverse map for fast lookup.
    reverse: HashMap<KeyBinding, AppAction>,
}

impl KeyBindings {
    /// Create keybindings with default bindings.
    #[must_use]
    pub fn new() -> Self {
        let bindings = Self::default_bindings();
        let reverse = Self::build_reverse_map(&bindings);
        Self { bindings, reverse }
    }

    /// Load keybindings from a JSON file, merging with defaults.
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let overrides: HashMap<AppAction, Vec<KeyBinding>> = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        let mut bindings = Self::default_bindings();
        for (action, keys) in overrides {
            bindings.insert(action, keys);
        }

        let reverse = Self::build_reverse_map(&bindings);
        Ok(Self { bindings, reverse })
    }

    /// Get the default user keybindings path: `~/.pi/agent/keybindings.json`
    #[must_use]
    pub fn user_config_path() -> std::path::PathBuf {
        crate::config::Config::global_dir().join("keybindings.json")
    }

    /// Load keybindings from user config, returning defaults with diagnostics if loading fails.
    ///
    /// This method never fails - it always returns valid keybindings (defaults at minimum).
    /// Warnings are collected in `KeyBindingsLoadResult` for display to the user.
    ///
    /// # User Config Format
    ///
    /// The config file is a JSON object mapping action IDs (camelCase) to key bindings:
    ///
    /// ```json
    /// {
    ///   "cursorUp": ["up", "ctrl+p"],
    ///   "cursorDown": ["down", "ctrl+n"],
    ///   "deleteWordBackward": ["ctrl+w", "alt+backspace"]
    /// }
    /// ```
    #[must_use]
    pub fn load_from_user_config() -> KeyBindingsLoadResult {
        let path = Self::user_config_path();
        Self::load_from_path_with_diagnostics(&path)
    }

    /// Load keybindings from a specific path with full diagnostics.
    ///
    /// Returns defaults with warnings if:
    /// - File doesn't exist (no warning - this is normal)
    /// - File is not valid JSON
    /// - File contains unknown action IDs
    /// - File contains invalid key strings
    #[must_use]
    pub fn load_from_path_with_diagnostics(path: &Path) -> KeyBindingsLoadResult {
        let mut warnings = Vec::new();

        // Check if file exists
        if !path.exists() {
            return KeyBindingsLoadResult {
                bindings: Self::new(),
                path: path.to_path_buf(),
                warnings,
            };
        }

        // Read file
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                warnings.push(KeyBindingsWarning::ReadError {
                    path: path.to_path_buf(),
                    error: e.to_string(),
                });
                return KeyBindingsLoadResult {
                    bindings: Self::new(),
                    path: path.to_path_buf(),
                    warnings,
                };
            }
        };

        // Parse as loose JSON (object with string keys and string/array values)
        let raw: HashMap<String, serde_json::Value> = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(KeyBindingsWarning::ParseError {
                    path: path.to_path_buf(),
                    error: e.to_string(),
                });
                return KeyBindingsLoadResult {
                    bindings: Self::new(),
                    path: path.to_path_buf(),
                    warnings,
                };
            }
        };

        // Start with defaults
        let mut bindings = Self::default_bindings();

        // Process each entry
        for (action_str, value) in raw {
            // Try to parse action ID
            let action: AppAction =
                if let Ok(a) = serde_json::from_value(serde_json::json!(action_str)) {
                    a
                } else {
                    warnings.push(KeyBindingsWarning::UnknownAction {
                        action: action_str,
                        path: path.to_path_buf(),
                    });
                    continue;
                };

            // Parse key bindings (can be string or array of strings)
            let key_strings: Vec<String> = match value {
                serde_json::Value::String(s) => vec![s],
                serde_json::Value::Array(arr) => {
                    let mut keys = Vec::new();
                    for (idx, v) in arr.into_iter().enumerate() {
                        match v {
                            serde_json::Value::String(s) => keys.push(s),
                            _ => {
                                warnings.push(KeyBindingsWarning::InvalidKeyValue {
                                    action: action.to_string(),
                                    index: idx,
                                    path: path.to_path_buf(),
                                });
                            }
                        }
                    }
                    keys
                }
                _ => {
                    warnings.push(KeyBindingsWarning::InvalidKeyValue {
                        action: action.to_string(),
                        index: 0,
                        path: path.to_path_buf(),
                    });
                    continue;
                }
            };

            // Parse each key string
            let mut parsed_keys = Vec::new();
            for key_str in key_strings {
                match key_str.parse::<KeyBinding>() {
                    Ok(binding) => parsed_keys.push(binding),
                    Err(e) => {
                        warnings.push(KeyBindingsWarning::InvalidKey {
                            action: action.to_string(),
                            key: key_str,
                            error: e.to_string(),
                            path: path.to_path_buf(),
                        });
                    }
                }
            }

            // Only override if we got at least one valid key
            if !parsed_keys.is_empty() {
                bindings.insert(action, parsed_keys);
            }
        }

        let reverse = Self::build_reverse_map(&bindings);
        KeyBindingsLoadResult {
            bindings: Self { bindings, reverse },
            path: path.to_path_buf(),
            warnings,
        }
    }

    /// Look up the action for a key binding.
    #[must_use]
    pub fn lookup(&self, binding: &KeyBinding) -> Option<AppAction> {
        self.reverse.get(binding).copied()
    }

    /// Return all actions bound to a key binding.
    ///
    /// Many bindings are context-dependent (e.g. `ctrl+d` can mean "delete forward" in the editor
    /// but "exit" when the editor is empty). Callers should resolve collisions based on UI state.
    #[must_use]
    pub fn matching_actions(&self, binding: &KeyBinding) -> Vec<AppAction> {
        AppAction::all()
            .iter()
            .copied()
            .filter(|&action| self.get_bindings(action).contains(binding))
            .collect()
    }

    /// Get all key bindings for an action.
    #[must_use]
    pub fn get_bindings(&self, action: AppAction) -> &[KeyBinding] {
        self.bindings.get(&action).map_or(&[], Vec::as_slice)
    }

    /// Iterate all actions with their bindings (for /hotkeys display).
    pub fn iter(&self) -> impl Iterator<Item = (AppAction, &[KeyBinding])> {
        AppAction::all()
            .iter()
            .map(|&action| (action, self.get_bindings(action)))
    }

    /// Iterate actions in a category with their bindings.
    pub fn iter_category(
        &self,
        category: ActionCategory,
    ) -> impl Iterator<Item = (AppAction, &[KeyBinding])> {
        AppAction::in_category(category)
            .into_iter()
            .map(|action| (action, self.get_bindings(action)))
    }

    fn build_reverse_map(
        bindings: &HashMap<AppAction, Vec<KeyBinding>>,
    ) -> HashMap<KeyBinding, AppAction> {
        let mut reverse = HashMap::new();
        // Deterministic reverse map:
        // - iterate actions in stable order (AppAction::all)
        // - keep the first mapping for a given key (collisions are context-dependent)
        for &action in AppAction::all() {
            let Some(keys) = bindings.get(&action) else {
                continue;
            };
            for key in keys {
                reverse.entry(key.clone()).or_insert(action);
            }
        }
        reverse
    }

    /// Default key bindings matching legacy Pi Agent.
    #[allow(clippy::too_many_lines)]
    fn default_bindings() -> HashMap<AppAction, Vec<KeyBinding>> {
        let mut m = HashMap::new();

        // Cursor Movement
        m.insert(AppAction::CursorUp, vec![KeyBinding::plain("up")]);
        m.insert(AppAction::CursorDown, vec![KeyBinding::plain("down")]);
        m.insert(
            AppAction::CursorLeft,
            vec![KeyBinding::plain("left"), KeyBinding::ctrl("b")],
        );
        m.insert(
            AppAction::CursorRight,
            vec![KeyBinding::plain("right"), KeyBinding::ctrl("f")],
        );
        m.insert(
            AppAction::CursorWordLeft,
            vec![
                KeyBinding::alt("left"),
                KeyBinding::ctrl("left"),
                KeyBinding::alt("b"),
            ],
        );
        m.insert(
            AppAction::CursorWordRight,
            vec![
                KeyBinding::alt("right"),
                KeyBinding::ctrl("right"),
                KeyBinding::alt("f"),
            ],
        );
        m.insert(
            AppAction::CursorLineStart,
            vec![KeyBinding::plain("home"), KeyBinding::ctrl("a")],
        );
        m.insert(
            AppAction::CursorLineEnd,
            vec![KeyBinding::plain("end"), KeyBinding::ctrl("e")],
        );
        m.insert(AppAction::JumpForward, vec![KeyBinding::ctrl("]")]);
        m.insert(AppAction::JumpBackward, vec![KeyBinding::ctrl_alt("]")]);
        m.insert(AppAction::PageUp, vec![KeyBinding::plain("pageup")]);
        m.insert(AppAction::PageDown, vec![KeyBinding::plain("pagedown")]);

        // Deletion
        m.insert(
            AppAction::DeleteCharBackward,
            vec![KeyBinding::plain("backspace")],
        );
        m.insert(
            AppAction::DeleteCharForward,
            vec![KeyBinding::plain("delete"), KeyBinding::ctrl("d")],
        );
        m.insert(
            AppAction::DeleteWordBackward,
            vec![KeyBinding::ctrl("w"), KeyBinding::alt("backspace")],
        );
        m.insert(
            AppAction::DeleteWordForward,
            vec![KeyBinding::alt("d"), KeyBinding::alt("delete")],
        );
        m.insert(AppAction::DeleteToLineStart, vec![KeyBinding::ctrl("u")]);
        m.insert(AppAction::DeleteToLineEnd, vec![KeyBinding::ctrl("k")]);

        // Text Input
        m.insert(
            AppAction::NewLine,
            vec![KeyBinding::shift("enter"), KeyBinding::ctrl("enter")],
        );
        m.insert(AppAction::Submit, vec![KeyBinding::plain("enter")]);
        m.insert(AppAction::Tab, vec![KeyBinding::plain("tab")]);

        // Kill Ring
        m.insert(AppAction::Yank, vec![KeyBinding::ctrl("y")]);
        m.insert(AppAction::YankPop, vec![KeyBinding::alt("y")]);
        m.insert(AppAction::Undo, vec![KeyBinding::ctrl("-")]);

        // Clipboard
        m.insert(AppAction::Copy, vec![KeyBinding::ctrl("c")]);
        m.insert(AppAction::PasteImage, vec![KeyBinding::ctrl("v")]);

        // Application
        m.insert(AppAction::Interrupt, vec![KeyBinding::plain("escape")]);
        m.insert(AppAction::Clear, vec![KeyBinding::ctrl("c")]);
        m.insert(AppAction::Exit, vec![KeyBinding::ctrl("d")]);
        m.insert(AppAction::Suspend, vec![KeyBinding::ctrl("z")]);
        m.insert(AppAction::ExternalEditor, vec![KeyBinding::ctrl("g")]);

        // Session (no default bindings)
        m.insert(AppAction::NewSession, vec![]);
        m.insert(AppAction::Tree, vec![]);
        m.insert(AppAction::Fork, vec![]);
        m.insert(AppAction::BranchPicker, vec![KeyBinding::ctrl("b")]);
        m.insert(
            AppAction::BranchNextSibling,
            vec![KeyBinding::ctrl("right")],
        );
        m.insert(AppAction::BranchPrevSibling, vec![KeyBinding::ctrl("left")]);

        // Models & Thinking
        m.insert(AppAction::SelectModel, vec![KeyBinding::ctrl("l")]);
        m.insert(AppAction::CycleModelForward, vec![KeyBinding::ctrl("p")]);
        m.insert(
            AppAction::CycleModelBackward,
            vec![KeyBinding::ctrl_shift("p")],
        );
        m.insert(
            AppAction::CycleThinkingLevel,
            vec![KeyBinding::shift("tab")],
        );

        // Display
        m.insert(AppAction::ExpandTools, vec![KeyBinding::ctrl("o")]);
        m.insert(AppAction::ToggleThinking, vec![KeyBinding::ctrl("t")]);

        // Message Queue
        m.insert(AppAction::FollowUp, vec![KeyBinding::alt("enter")]);
        m.insert(AppAction::Dequeue, vec![KeyBinding::alt("up")]);

        // Selection (Lists, Pickers)
        m.insert(AppAction::SelectUp, vec![KeyBinding::plain("up")]);
        m.insert(AppAction::SelectDown, vec![KeyBinding::plain("down")]);
        m.insert(AppAction::SelectPageUp, vec![KeyBinding::plain("pageup")]);
        m.insert(
            AppAction::SelectPageDown,
            vec![KeyBinding::plain("pagedown")],
        );
        m.insert(AppAction::SelectConfirm, vec![KeyBinding::plain("enter")]);
        m.insert(
            AppAction::SelectCancel,
            vec![KeyBinding::plain("escape"), KeyBinding::ctrl("c")],
        );

        // Session Picker
        m.insert(AppAction::ToggleSessionPath, vec![KeyBinding::ctrl("p")]);
        m.insert(AppAction::ToggleSessionSort, vec![KeyBinding::ctrl("s")]);
        m.insert(
            AppAction::ToggleSessionNamedFilter,
            vec![KeyBinding::ctrl("n")],
        );
        m.insert(AppAction::RenameSession, vec![KeyBinding::ctrl("r")]);
        m.insert(AppAction::DeleteSession, vec![KeyBinding::ctrl("d")]);
        m.insert(
            AppAction::DeleteSessionNoninvasive,
            vec![KeyBinding::ctrl("backspace")],
        );

        m
    }
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_binding_parse() {
        let binding: KeyBinding = "ctrl+a".parse().unwrap();
        assert_eq!(binding.key, "a");
        assert!(binding.modifiers.ctrl);
        assert!(!binding.modifiers.alt);
        assert!(!binding.modifiers.shift);

        let binding: KeyBinding = "alt+shift+f".parse().unwrap();
        assert_eq!(binding.key, "f");
        assert!(!binding.modifiers.ctrl);
        assert!(binding.modifiers.alt);
        assert!(binding.modifiers.shift);

        let binding: KeyBinding = "enter".parse().unwrap();
        assert_eq!(binding.key, "enter");
        assert!(!binding.modifiers.ctrl);
        assert!(!binding.modifiers.alt);
        assert!(!binding.modifiers.shift);
    }

    #[test]
    fn test_key_binding_display() {
        let binding = KeyBinding::ctrl("a");
        assert_eq!(binding.to_string(), "ctrl+a");

        let binding = KeyBinding::new("f", KeyModifiers::ALT_SHIFT);
        assert_eq!(binding.to_string(), "alt+shift+f");

        let binding = KeyBinding::plain("enter");
        assert_eq!(binding.to_string(), "enter");
    }

    #[test]
    fn test_default_bindings() {
        let bindings = KeyBindings::new();

        // Check cursor movement
        let cursor_left = bindings.get_bindings(AppAction::CursorLeft);
        assert!(cursor_left.contains(&KeyBinding::plain("left")));
        assert!(cursor_left.contains(&KeyBinding::ctrl("b")));

        // Check ctrl+c maps to multiple actions (context-dependent)
        let ctrl_c = KeyBinding::ctrl("c");
        // Note: ctrl+c is bound to both Copy and Clear in legacy
        // The reverse lookup returns one of them
        let action = bindings.lookup(&ctrl_c);
        assert!(action == Some(AppAction::Copy) || action == Some(AppAction::Clear));
    }

    #[test]
    fn test_action_categories() {
        assert_eq!(
            AppAction::CursorUp.category(),
            ActionCategory::CursorMovement
        );
        assert_eq!(
            AppAction::DeleteWordBackward.category(),
            ActionCategory::Deletion
        );
        assert_eq!(AppAction::Submit.category(), ActionCategory::TextInput);
        assert_eq!(AppAction::Yank.category(), ActionCategory::KillRing);
    }

    #[test]
    fn test_action_iteration() {
        let bindings = KeyBindings::new();

        // All actions should be iterable
        assert!(bindings.iter().next().is_some());

        // Category iteration
        let cursor_actions: Vec<_> = bindings
            .iter_category(ActionCategory::CursorMovement)
            .collect();
        assert!(
            cursor_actions
                .iter()
                .any(|(a, _)| *a == AppAction::CursorUp)
        );
    }

    #[test]
    fn test_action_display_names() {
        assert_eq!(AppAction::CursorUp.display_name(), "Move cursor up");
        assert_eq!(AppAction::Submit.display_name(), "Submit input");
        assert_eq!(
            AppAction::ExternalEditor.display_name(),
            "Open in external editor"
        );
    }

    #[test]
    fn test_all_actions_have_categories() {
        for action in AppAction::all() {
            // Should not panic
            let _ = action.category();
        }
    }

    #[test]
    fn test_json_serialization() {
        let action = AppAction::CursorWordLeft;
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, "\"cursorWordLeft\"");

        let parsed: AppAction = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, action);
    }

    #[test]
    fn test_key_binding_json_roundtrip() {
        let binding = KeyBinding::ctrl_shift("p");
        let json = serde_json::to_string(&binding).unwrap();
        let parsed: KeyBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, binding);
    }

    // ============================================================================
    // Key Parsing: Synonyms
    // ============================================================================

    #[test]
    fn test_parse_synonym_esc() {
        let binding: KeyBinding = "esc".parse().unwrap();
        assert_eq!(binding.key, "escape");

        let binding: KeyBinding = "ESC".parse().unwrap();
        assert_eq!(binding.key, "escape");
    }

    #[test]
    fn test_parse_synonym_return() {
        let binding: KeyBinding = "return".parse().unwrap();
        assert_eq!(binding.key, "enter");

        let binding: KeyBinding = "RETURN".parse().unwrap();
        assert_eq!(binding.key, "enter");
    }

    // ============================================================================
    // Key Parsing: Case Insensitivity
    // ============================================================================

    #[test]
    fn test_parse_case_insensitive_modifiers() {
        let binding: KeyBinding = "CTRL+a".parse().unwrap();
        assert!(binding.modifiers.ctrl);
        assert_eq!(binding.key, "a");

        let binding: KeyBinding = "Ctrl+Shift+A".parse().unwrap();
        assert!(binding.modifiers.ctrl);
        assert!(binding.modifiers.shift);
        assert_eq!(binding.key, "a");

        let binding: KeyBinding = "ALT+F".parse().unwrap();
        assert!(binding.modifiers.alt);
        assert_eq!(binding.key, "f");
    }

    #[test]
    fn test_parse_case_insensitive_special_keys() {
        let binding: KeyBinding = "PageUp".parse().unwrap();
        assert_eq!(binding.key, "pageup");

        let binding: KeyBinding = "PAGEDOWN".parse().unwrap();
        assert_eq!(binding.key, "pagedown");

        let binding: KeyBinding = "ESCAPE".parse().unwrap();
        assert_eq!(binding.key, "escape");

        let binding: KeyBinding = "Tab".parse().unwrap();
        assert_eq!(binding.key, "tab");
    }

    // ============================================================================
    // Key Parsing: Special Keys
    // ============================================================================

    #[test]
    fn test_parse_all_special_keys() {
        // All special keys from the spec should parse
        let special_keys = [
            "escape",
            "enter",
            "tab",
            "space",
            "backspace",
            "delete",
            "insert",
            "clear",
            "home",
            "end",
            "pageup",
            "pagedown",
            "up",
            "down",
            "left",
            "right",
        ];

        for key in special_keys {
            let binding: KeyBinding = key.parse().unwrap();
            assert_eq!(binding.key, key, "Failed to parse special key: {key}");
        }
    }

    #[test]
    fn test_parse_function_keys() {
        // Test f1-f20 (matching bubbletea KeyType coverage)
        for i in 1..=20 {
            let key = format!("f{i}");
            let binding: KeyBinding = key.parse().unwrap();
            assert_eq!(binding.key, key, "Failed to parse function key: {key}");
        }
    }

    #[test]
    fn test_parse_letters() {
        for c in 'a'..='z' {
            let key = c.to_string();
            let binding: KeyBinding = key.parse().unwrap();
            assert_eq!(binding.key, key);
        }
    }

    #[test]
    fn test_parse_symbols() {
        let symbols = [
            "`", "-", "=", "[", "]", "\\", ";", "'", ",", ".", "/", "!", "@", "#", "$", "%", "^",
            "&", "*", "(", ")", "_", "+", "|", "~", "{", "}", ":", "<", ">", "?",
        ];

        for sym in symbols {
            let binding: KeyBinding = sym.parse().unwrap();
            assert_eq!(binding.key, sym, "Failed to parse symbol: {sym}");
        }
    }

    #[test]
    fn test_parse_plus_key_with_modifiers() {
        let binding: KeyBinding = "ctrl++".parse().unwrap();
        assert!(binding.modifiers.ctrl);
        assert_eq!(binding.key, "+");
        assert_eq!(binding.to_string(), "ctrl++");

        let binding: KeyBinding = "ctrl + +".parse().unwrap();
        assert!(binding.modifiers.ctrl);
        assert_eq!(binding.key, "+");
        assert_eq!(binding.to_string(), "ctrl++");
    }

    // ============================================================================
    // Key Parsing: Modifiers
    // ============================================================================

    #[test]
    fn test_parse_all_modifier_combinations() {
        // ctrl only
        let binding: KeyBinding = "ctrl+x".parse().unwrap();
        assert!(binding.modifiers.ctrl);
        assert!(!binding.modifiers.alt);
        assert!(!binding.modifiers.shift);

        // alt only
        let binding: KeyBinding = "alt+x".parse().unwrap();
        assert!(!binding.modifiers.ctrl);
        assert!(binding.modifiers.alt);
        assert!(!binding.modifiers.shift);

        // shift only
        let binding: KeyBinding = "shift+x".parse().unwrap();
        assert!(!binding.modifiers.ctrl);
        assert!(!binding.modifiers.alt);
        assert!(binding.modifiers.shift);

        // ctrl+alt
        let binding: KeyBinding = "ctrl+alt+x".parse().unwrap();
        assert!(binding.modifiers.ctrl);
        assert!(binding.modifiers.alt);
        assert!(!binding.modifiers.shift);

        // ctrl+shift
        let binding: KeyBinding = "ctrl+shift+x".parse().unwrap();
        assert!(binding.modifiers.ctrl);
        assert!(!binding.modifiers.alt);
        assert!(binding.modifiers.shift);

        // alt+shift
        let binding: KeyBinding = "alt+shift+x".parse().unwrap();
        assert!(!binding.modifiers.ctrl);
        assert!(binding.modifiers.alt);
        assert!(binding.modifiers.shift);

        // all three
        let binding: KeyBinding = "ctrl+shift+alt+x".parse().unwrap();
        assert!(binding.modifiers.ctrl);
        assert!(binding.modifiers.alt);
        assert!(binding.modifiers.shift);
    }

    #[test]
    fn test_parse_control_synonym() {
        let binding: KeyBinding = "control+a".parse().unwrap();
        assert!(binding.modifiers.ctrl);
        assert_eq!(binding.key, "a");
    }

    // ============================================================================
    // Key Parsing: Error Cases
    // ============================================================================

    #[test]
    fn test_parse_empty_string() {
        let result: Result<KeyBinding, _> = "".parse();
        assert!(matches!(result, Err(KeyBindingParseError::Empty)));
    }

    #[test]
    fn test_parse_whitespace_only() {
        let result: Result<KeyBinding, _> = "   ".parse();
        assert!(matches!(result, Err(KeyBindingParseError::Empty)));
    }

    #[test]
    fn test_parse_only_modifiers() {
        let result: Result<KeyBinding, _> = "ctrl".parse();
        assert!(matches!(result, Err(KeyBindingParseError::NoKey)));

        let result: Result<KeyBinding, _> = "ctrl+shift".parse();
        assert!(matches!(result, Err(KeyBindingParseError::NoKey)));
    }

    #[test]
    fn test_parse_multiple_keys() {
        let result: Result<KeyBinding, _> = "a+b".parse();
        assert!(matches!(
            result,
            Err(KeyBindingParseError::MultipleKeys { .. })
        ));

        let result: Result<KeyBinding, _> = "ctrl+a+b".parse();
        assert!(matches!(
            result,
            Err(KeyBindingParseError::MultipleKeys { .. })
        ));
    }

    #[test]
    fn test_parse_duplicate_modifiers() {
        let result: Result<KeyBinding, _> = "ctrl+ctrl+x".parse();
        assert!(matches!(
            result,
            Err(KeyBindingParseError::DuplicateModifier {
                modifier,
                ..
            }) if modifier == "ctrl"
        ));

        let result: Result<KeyBinding, _> = "alt+alt+x".parse();
        assert!(matches!(
            result,
            Err(KeyBindingParseError::DuplicateModifier {
                modifier,
                ..
            }) if modifier == "alt"
        ));

        let result: Result<KeyBinding, _> = "shift+shift+x".parse();
        assert!(matches!(
            result,
            Err(KeyBindingParseError::DuplicateModifier {
                modifier,
                ..
            }) if modifier == "shift"
        ));
    }

    #[test]
    fn test_parse_unknown_key() {
        let result: Result<KeyBinding, _> = "unknownkey".parse();
        assert!(matches!(
            result,
            Err(KeyBindingParseError::UnknownKey { .. })
        ));

        let result: Result<KeyBinding, _> = "ctrl+xyz".parse();
        assert!(matches!(
            result,
            Err(KeyBindingParseError::UnknownKey { .. })
        ));
    }

    #[test]
    fn test_parse_unknown_modifier() {
        let result: Result<KeyBinding, _> = "meta+enter".parse();
        assert!(matches!(
            result,
            Err(KeyBindingParseError::UnknownModifier { modifier, .. }) if modifier == "meta"
        ));

        let result: Result<KeyBinding, _> = "ctrl+meta+enter".parse();
        assert!(matches!(
            result,
            Err(KeyBindingParseError::UnknownModifier { modifier, .. }) if modifier == "meta"
        ));
    }

    // ============================================================================
    // Key Parsing: Normalization Stability
    // ============================================================================

    #[test]
    fn test_normalization_output_stable() {
        // Regardless of input casing, output should be stable
        let binding1: KeyBinding = "CTRL+SHIFT+P".parse().unwrap();
        let binding2: KeyBinding = "ctrl+shift+p".parse().unwrap();
        let binding3: KeyBinding = "Ctrl+Shift+P".parse().unwrap();

        assert_eq!(binding1.to_string(), binding2.to_string());
        assert_eq!(binding2.to_string(), binding3.to_string());
        assert_eq!(binding1.to_string(), "ctrl+shift+p");
    }

    #[test]
    fn test_synonym_normalization_stable() {
        let binding1: KeyBinding = "esc".parse().unwrap();
        let binding2: KeyBinding = "escape".parse().unwrap();
        let binding3: KeyBinding = "ESCAPE".parse().unwrap();

        assert_eq!(binding1.key, "escape");
        assert_eq!(binding2.key, "escape");
        assert_eq!(binding3.key, "escape");
    }

    // ============================================================================
    // Key Parsing: Legacy Keybindings from Docs
    // ============================================================================

    #[test]
    fn test_parse_all_legacy_default_bindings() {
        // All keys from the legacy keybindings.md should parse
        let legacy_bindings = [
            "up",
            "down",
            "left",
            "ctrl+b",
            "right",
            "ctrl+f",
            "alt+left",
            "ctrl+left",
            "alt+b",
            "alt+right",
            "ctrl+right",
            "alt+f",
            "home",
            "ctrl+a",
            "end",
            "ctrl+e",
            "ctrl+]",
            "ctrl+alt+]",
            "pageUp",
            "pageDown",
            "backspace",
            "delete",
            "ctrl+d",
            "ctrl+w",
            "alt+backspace",
            "alt+d",
            "alt+delete",
            "ctrl+u",
            "ctrl+k",
            "shift+enter",
            "enter",
            "tab",
            "ctrl+y",
            "alt+y",
            "ctrl+-",
            "ctrl+c",
            "ctrl+v",
            "escape",
            "ctrl+z",
            "ctrl+g",
            "ctrl+l",
            "ctrl+p",
            "shift+ctrl+p",
            "shift+tab",
            "ctrl+o",
            "ctrl+t",
            "alt+enter",
            "alt+up",
            "ctrl+s",
            "ctrl+n",
            "ctrl+r",
            "ctrl+backspace",
        ];

        for key in legacy_bindings {
            let result: Result<KeyBinding, _> = key.parse();
            assert!(result.is_ok(), "Failed to parse legacy binding: {key}");
        }
    }

    // ============================================================================
    // Utility Functions
    // ============================================================================

    #[test]
    fn test_is_valid_key() {
        assert!(is_valid_key("ctrl+a"));
        assert!(is_valid_key("enter"));
        assert!(is_valid_key("shift+tab"));

        assert!(!is_valid_key(""));
        assert!(!is_valid_key("ctrl+ctrl+x"));
        assert!(!is_valid_key("unknownkey"));
    }

    #[test]
    fn test_error_display() {
        let err = KeyBindingParseError::Empty;
        assert_eq!(err.to_string(), "Empty key binding");

        let err = KeyBindingParseError::DuplicateModifier {
            modifier: "ctrl".to_string(),
            binding: "ctrl+ctrl+x".to_string(),
        };
        assert!(err.to_string().contains("ctrl"));
        assert!(err.to_string().contains("ctrl+ctrl+x"));

        let err = KeyBindingParseError::UnknownKey {
            key: "xyz".to_string(),
            binding: "ctrl+xyz".to_string(),
        };
        assert!(err.to_string().contains("xyz"));

        let err = KeyBindingParseError::UnknownModifier {
            modifier: "meta".to_string(),
            binding: "meta+enter".to_string(),
        };
        assert!(err.to_string().contains("meta"));
        assert!(err.to_string().contains("meta+enter"));
    }

    // ============================================================================
    // User Config Loading (bd-3qm)
    // ============================================================================

    #[test]
    fn test_user_config_path_matches_global_dir() {
        let expected = crate::config::Config::global_dir().join("keybindings.json");
        assert_eq!(KeyBindings::user_config_path(), expected);
    }

    #[test]
    fn test_load_from_nonexistent_path_returns_defaults() {
        let path = std::path::Path::new("/nonexistent/keybindings.json");
        let result = KeyBindings::load_from_path_with_diagnostics(path);

        // Should return defaults with no warnings (missing file is normal)
        assert!(!result.has_warnings());
        assert!(result.bindings.lookup(&KeyBinding::ctrl("a")).is_some());
    }

    #[test]
    fn test_load_valid_override() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("keybindings.json");

        std::fs::write(
            &path,
            r#"{
                "cursorUp": ["up", "ctrl+p"],
                "cursorDown": "down"
            }"#,
        )
        .unwrap();

        let result = KeyBindings::load_from_path_with_diagnostics(&path);

        assert!(!result.has_warnings());

        // Check overrides
        let up_bindings = result.bindings.get_bindings(AppAction::CursorUp);
        assert!(up_bindings.contains(&KeyBinding::plain("up")));
        assert!(up_bindings.contains(&KeyBinding::ctrl("p")));

        // Check single string value works
        let down_bindings = result.bindings.get_bindings(AppAction::CursorDown);
        assert!(down_bindings.contains(&KeyBinding::plain("down")));
    }

    #[test]
    fn test_load_warns_on_unknown_action() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("keybindings.json");

        std::fs::write(
            &path,
            r#"{
                "cursorUp": ["up"],
                "unknownAction": ["ctrl+x"],
                "anotherBadAction": ["ctrl+y"]
            }"#,
        )
        .unwrap();

        let result = KeyBindings::load_from_path_with_diagnostics(&path);

        // Should have 2 warnings for unknown actions
        assert_eq!(result.warnings.len(), 2);
        assert!(result.format_warnings().contains("unknownAction"));
        assert!(result.format_warnings().contains("anotherBadAction"));

        // Valid action should still work
        let up_bindings = result.bindings.get_bindings(AppAction::CursorUp);
        assert!(up_bindings.contains(&KeyBinding::plain("up")));
    }

    #[test]
    fn test_load_warns_on_invalid_key() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("keybindings.json");

        std::fs::write(
            &path,
            r#"{
                "cursorUp": ["up", "invalidkey123", "ctrl+p"]
            }"#,
        )
        .unwrap();

        let result = KeyBindings::load_from_path_with_diagnostics(&path);

        // Should have 1 warning for invalid key
        assert_eq!(result.warnings.len(), 1);
        assert!(result.format_warnings().contains("invalidkey123"));

        // Valid keys should still be applied
        let up_bindings = result.bindings.get_bindings(AppAction::CursorUp);
        assert!(up_bindings.contains(&KeyBinding::plain("up")));
        assert!(up_bindings.contains(&KeyBinding::ctrl("p")));
        assert_eq!(up_bindings.len(), 2); // not 3
    }

    #[test]
    fn test_load_warns_on_invalid_json() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("keybindings.json");

        std::fs::write(&path, "{ not valid json }").unwrap();

        let result = KeyBindings::load_from_path_with_diagnostics(&path);

        // Should have 1 warning for parse error
        assert_eq!(result.warnings.len(), 1);
        assert!(matches!(
            result.warnings[0],
            KeyBindingsWarning::ParseError { .. }
        ));

        // Should return defaults
        assert!(result.bindings.lookup(&KeyBinding::ctrl("a")).is_some());
    }

    #[test]
    fn test_load_handles_invalid_value_type() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("keybindings.json");

        std::fs::write(
            &path,
            r#"{
                "cursorUp": 123,
                "cursorDown": ["down"]
            }"#,
        )
        .unwrap();

        let result = KeyBindings::load_from_path_with_diagnostics(&path);

        // Should have 1 warning for invalid value type
        assert_eq!(result.warnings.len(), 1);
        assert!(matches!(
            result.warnings[0],
            KeyBindingsWarning::InvalidKeyValue { .. }
        ));

        // Valid action should still work
        let down_bindings = result.bindings.get_bindings(AppAction::CursorDown);
        assert!(down_bindings.contains(&KeyBinding::plain("down")));
    }

    #[test]
    fn test_warning_display_format() {
        let warning = KeyBindingsWarning::UnknownAction {
            action: "badAction".to_string(),
            path: PathBuf::from("/test/keybindings.json"),
        };
        let msg = warning.to_string();
        assert!(msg.contains("badAction"));
        assert!(msg.contains("/test/keybindings.json"));
        assert!(msg.contains("ignored"));
    }

    // ============================================================================
    // KeyMsg → KeyBinding Conversion (bd-gze)
    // ============================================================================

    #[test]
    fn test_from_bubbletea_key_ctrl_keys() {
        use bubbletea::{KeyMsg, KeyType};

        // Test Ctrl+C
        let key = KeyMsg::from_type(KeyType::CtrlC);
        let binding = KeyBinding::from_bubbletea_key(&key).unwrap();
        assert_eq!(binding.key, "c");
        assert!(binding.modifiers.ctrl);
        assert!(!binding.modifiers.alt);

        // Test Ctrl+P
        let key = KeyMsg::from_type(KeyType::CtrlP);
        let binding = KeyBinding::from_bubbletea_key(&key).unwrap();
        assert_eq!(binding.key, "p");
        assert!(binding.modifiers.ctrl);
    }

    #[test]
    fn test_from_bubbletea_key_special_keys() {
        use bubbletea::{KeyMsg, KeyType};

        // Enter
        let binding = KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::Enter)).unwrap();
        assert_eq!(binding.key, "enter");
        assert_eq!(binding.modifiers, KeyModifiers::NONE);

        // Escape
        let binding = KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::Esc)).unwrap();
        assert_eq!(binding.key, "escape");

        // Tab
        let binding = KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::Tab)).unwrap();
        assert_eq!(binding.key, "tab");

        // Backspace
        let binding =
            KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::Backspace)).unwrap();
        assert_eq!(binding.key, "backspace");
    }

    #[test]
    fn test_from_bubbletea_key_arrow_keys() {
        use bubbletea::{KeyMsg, KeyType};

        // Plain arrows
        let binding = KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::Up)).unwrap();
        assert_eq!(binding.key, "up");
        assert_eq!(binding.modifiers, KeyModifiers::NONE);

        // Shift+arrows
        let binding = KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::ShiftUp)).unwrap();
        assert_eq!(binding.key, "up");
        assert!(binding.modifiers.shift);

        // Ctrl+arrows
        let binding =
            KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::CtrlLeft)).unwrap();
        assert_eq!(binding.key, "left");
        assert!(binding.modifiers.ctrl);

        // Ctrl+Shift+arrows
        let binding =
            KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::CtrlShiftDown)).unwrap();
        assert_eq!(binding.key, "down");
        assert!(binding.modifiers.ctrl);
        assert!(binding.modifiers.shift);
    }

    #[test]
    fn test_from_bubbletea_key_with_alt() {
        use bubbletea::{KeyMsg, KeyType};

        // Alt+arrow
        let key = KeyMsg::from_type(KeyType::Up).with_alt();
        let binding = KeyBinding::from_bubbletea_key(&key).unwrap();
        assert_eq!(binding.key, "up");
        assert!(binding.modifiers.alt);
        assert!(!binding.modifiers.ctrl);

        // Alt+letter (via Runes)
        let key = KeyMsg::from_char('f').with_alt();
        let binding = KeyBinding::from_bubbletea_key(&key).unwrap();
        assert_eq!(binding.key, "f");
        assert!(binding.modifiers.alt);
    }

    #[test]
    fn test_from_bubbletea_key_runes() {
        use bubbletea::KeyMsg;

        // Single character
        let key = KeyMsg::from_char('a');
        let binding = KeyBinding::from_bubbletea_key(&key).unwrap();
        assert_eq!(binding.key, "a");
        assert_eq!(binding.modifiers, KeyModifiers::NONE);

        // Uppercase becomes lowercase
        let key = KeyMsg::from_char('A');
        let binding = KeyBinding::from_bubbletea_key(&key).unwrap();
        assert_eq!(binding.key, "a");
    }

    #[test]
    fn test_from_bubbletea_key_multi_char_returns_none() {
        use bubbletea::KeyMsg;

        // Multi-character input (e.g., IME) cannot be a keybinding
        let key = KeyMsg::from_runes(vec!['a', 'b']);
        assert!(KeyBinding::from_bubbletea_key(&key).is_none());
    }

    #[test]
    fn test_from_bubbletea_key_paste_returns_none() {
        use bubbletea::KeyMsg;

        // Paste events should not be keybindings
        let key = KeyMsg::from_char('a').with_paste();
        assert!(KeyBinding::from_bubbletea_key(&key).is_none());
    }

    #[test]
    fn test_from_bubbletea_key_function_keys() {
        use bubbletea::{KeyMsg, KeyType};

        let binding = KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::F1)).unwrap();
        assert_eq!(binding.key, "f1");

        let binding = KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::F12)).unwrap();
        assert_eq!(binding.key, "f12");
    }

    #[test]
    fn test_from_bubbletea_key_navigation() {
        use bubbletea::{KeyMsg, KeyType};

        let binding = KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::Home)).unwrap();
        assert_eq!(binding.key, "home");

        let binding = KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::PgUp)).unwrap();
        assert_eq!(binding.key, "pageup");

        let binding = KeyBinding::from_bubbletea_key(&KeyMsg::from_type(KeyType::Delete)).unwrap();
        assert_eq!(binding.key, "delete");
    }

    #[test]
    fn test_keybinding_lookup_via_conversion() {
        use bubbletea::{KeyMsg, KeyType};

        let bindings = KeyBindings::new();

        // Ctrl+C should map to an action (Copy or Clear depending on context)
        let key = KeyMsg::from_type(KeyType::CtrlC);
        let binding = KeyBinding::from_bubbletea_key(&key).unwrap();
        assert!(bindings.lookup(&binding).is_some());

        // PageUp should map to PageUp action
        let key = KeyMsg::from_type(KeyType::PgUp);
        let binding = KeyBinding::from_bubbletea_key(&key).unwrap();
        let action = bindings.lookup(&binding);
        assert_eq!(action, Some(AppAction::PageUp));

        // Enter should map to Submit
        let key = KeyMsg::from_type(KeyType::Enter);
        let binding = KeyBinding::from_bubbletea_key(&key).unwrap();
        let action = bindings.lookup(&binding);
        assert_eq!(action, Some(AppAction::Submit));
    }
}
