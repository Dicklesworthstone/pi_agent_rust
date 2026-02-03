# Existing Pi Structure and Architecture

> **After reading this document, you should NOT need to consult the legacy TypeScript code.**

This document is the authoritative specification for the Rust port of pi-mono.

---

## Table of Contents

1. [Project Overview](#1-project-overview)
2. [Message Types and Content Blocks](#2-message-types-and-content-blocks)
3. [Streaming Events](#3-streaming-events)
4. [Provider Interface](#4-provider-interface)
5. [Tool System](#5-tool-system)
6. [Session File Format](#6-session-file-format)
7. [Configuration](#7-configuration)
8. [Authentication Storage](#8-authentication-storage)
9. [CLI Commands and Flags](#9-cli-commands-and-flags)
10. [Execution Flow](#10-execution-flow)

---

## 1. Project Overview

Pi is an AI coding agent platform with these core components:

| Component | TypeScript Package | Rust Equivalent |
|-----------|-------------------|-----------------|
| LLM Provider Abstraction | `@mariozechner/pi-ai` | `pi::provider` module |
| Agent Runtime | `@mariozechner/pi-agent` | `pi::agent` module |
| CLI Application | `@mariozechner/pi-coding-agent` | `pi` binary |
| Terminal UI | `@mariozechner/pi-tui` | `pi::tui` module |

### Key Statistics (TypeScript)
- **7 built-in tools**: read, bash, edit, write, grep, find, ls
- **20+ LLM providers**: Anthropic, OpenAI, Google, Bedrock, etc.
- **Session format version**: 3
- **Default tools enabled**: read, bash, edit, write

---

## 2. Message Types and Content Blocks

### 2.1 Message Union Type

```rust
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}
```

### 2.2 User Message

```rust
pub struct UserMessage {
    pub content: UserContent,  // String or Vec<ContentBlock>
    pub timestamp: i64,        // Unix milliseconds
}

pub enum UserContent {
    Text(String),
    Blocks(Vec<ContentBlock>),  // TextContent | ImageContent only
}
```

### 2.3 Assistant Message

```rust
pub struct AssistantMessage {
    pub content: Vec<AssistantContentBlock>,  // Text | Thinking | ToolCall
    pub api: String,                          // e.g., "anthropic-messages"
    pub provider: String,                     // e.g., "anthropic"
    pub model: String,                        // Model ID
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    pub timestamp: i64,
}
```

### 2.4 Tool Result Message

```rust
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,  // TextContent | ImageContent
    pub details: Option<serde_json::Value>,
    pub is_error: bool,
    pub timestamp: i64,
}
```

### 2.5 Stop Reason

```rust
pub enum StopReason {
    Stop,      // Normal completion
    Length,    // Max tokens reached
    ToolUse,   // Tool call pending
    Error,     // Error occurred
    Aborted,   // User cancelled
}
```

### 2.6 Content Blocks

```rust
pub enum ContentBlock {
    Text(TextContent),
    Thinking(ThinkingContent),
    Image(ImageContent),
    ToolCall(ToolCall),
}

pub struct TextContent {
    pub text: String,
    pub text_signature: Option<String>,  // Provider-specific
}

pub struct ThinkingContent {
    pub thinking: String,
    pub thinking_signature: Option<String>,  // For replay
}

pub struct ImageContent {
    pub data: String,       // Base64 encoded
    pub mime_type: String,  // "image/jpeg", "image/png", etc.
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub thought_signature: Option<String>,  // Google-specific
}
```

### 2.7 Usage Tracking

```rust
pub struct Usage {
    pub input: u64,        // Input tokens (excluding cache read)
    pub output: u64,       // Output tokens
    pub cache_read: u64,   // Tokens read from cache
    pub cache_write: u64,  // Tokens written to cache
    pub total_tokens: u64,
    pub cost: Cost,
}

pub struct Cost {
    pub input: f64,       // Dollars
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}
```

---

## 3. Streaming Events

### 3.1 Event Types

```rust
pub enum StreamEvent {
    Start { partial: AssistantMessage },

    TextStart { content_index: usize, partial: AssistantMessage },
    TextDelta { content_index: usize, delta: String, partial: AssistantMessage },
    TextEnd { content_index: usize, content: String, partial: AssistantMessage },

    ThinkingStart { content_index: usize, partial: AssistantMessage },
    ThinkingDelta { content_index: usize, delta: String, partial: AssistantMessage },
    ThinkingEnd { content_index: usize, content: String, partial: AssistantMessage },

    ToolCallStart { content_index: usize, partial: AssistantMessage },
    ToolCallDelta { content_index: usize, delta: String, partial: AssistantMessage },
    ToolCallEnd { content_index: usize, tool_call: ToolCall, partial: AssistantMessage },

    Done { reason: StopReason, message: AssistantMessage },
    Error { reason: StopReason, error: AssistantMessage },
}
```

### 3.2 Event Sequences

**Text response:**
```
Start → TextStart → TextDelta* → TextEnd → Done(Stop)
```

**Tool call:**
```
Start → ToolCallStart → ToolCallDelta* → ToolCallEnd → Done(ToolUse)
```

**With thinking:**
```
Start → ThinkingStart → ThinkingDelta* → ThinkingEnd → TextStart → ... → Done
```

---

## 4. Provider Interface

### 4.1 Provider Trait

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn api(&self) -> &str;

    async fn stream(
        &self,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>>;
}
```

### 4.2 Context

```rust
pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
}
```

### 4.3 Stream Options

```rust
pub struct StreamOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub api_key: Option<String>,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    pub headers: HashMap<String, String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

pub enum CacheRetention {
    None,
    Short,
    Long,  // 1 hour TTL on Anthropic
}

pub enum ThinkingLevel {
    Off,
    Minimal,  // 1024 tokens
    Low,      // 2048 tokens
    Medium,   // 8192 tokens
    High,     // 16384 tokens
    XHigh,    // Model max
}

pub struct ThinkingBudgets {
    pub minimal: u32,  // Default: 1024
    pub low: u32,      // Default: 2048
    pub medium: u32,   // Default: 8192
    pub high: u32,     // Default: 16384
}
```

### 4.4 Model Definition

```rust
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,              // "anthropic-messages", "openai-completions", etc.
    pub provider: String,         // "anthropic", "openai", etc.
    pub base_url: String,
    pub reasoning: bool,          // Supports thinking/reasoning
    pub input: Vec<InputType>,    // ["text", "image"]
    pub cost: ModelCost,
    pub context_window: u32,
    pub max_tokens: u32,
    pub headers: HashMap<String, String>,
}

pub struct ModelCost {
    pub input: f64,       // $/million tokens
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

pub enum InputType {
    Text,
    Image,
}
```

### 4.5 Known APIs

```rust
pub enum Api {
    AnthropicMessages,
    OpenAICompletions,
    OpenAIResponses,
    AzureOpenAIResponses,
    BedrockConverseStream,
    GoogleGenerativeAI,
    GoogleGeminiCli,
    GoogleVertex,
    Custom(String),
}
```

### 4.6 Known Providers

```rust
pub enum KnownProvider {
    Anthropic,
    OpenAI,
    Google,
    GoogleVertex,
    AmazonBedrock,
    AzureOpenAI,
    GithubCopilot,
    XAI,
    Groq,
    Cerebras,
    OpenRouter,
    Mistral,
    // ... more
}
```

---

## 5. Tool System

### 5.1 Tool Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;  // JSON Schema

    async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send>>,
    ) -> Result<ToolOutput>;
}

pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub details: Option<serde_json::Value>,
}

pub struct ToolUpdate {
    pub content: Vec<ContentBlock>,
    pub details: Option<serde_json::Value>,
}
```

### 5.2 Tool Definition for API

```rust
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,  // JSON Schema
}
```

### 5.3 Built-in Tools

#### READ Tool

**Purpose:** Read file contents (text or images)

**Parameters:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "File path (relative or absolute)" },
    "offset": { "type": "integer", "description": "1-indexed line to start from" },
    "limit": { "type": "integer", "description": "Max lines to read" }
  },
  "required": ["path"]
}
```

**Behavior:**
- Truncation: 2000 lines OR 50KB (whichever first)
- Truncation method: `truncate_head` (keeps beginning)
- Image support: jpg, png, gif, webp → base64 with optional resize to 2000x2000
- Supports `~` expansion in paths

**Error conditions:**
- Path not found
- Permission denied
- Offset beyond EOF

---

#### BASH Tool

**Purpose:** Execute shell commands

**Parameters:**
```json
{
  "type": "object",
  "properties": {
    "command": { "type": "string", "description": "Bash command to execute" },
    "timeout": { "type": "integer", "description": "Timeout in seconds" }
  },
  "required": ["command"]
}
```

**Behavior:**
- Streams output via `on_update` callback (last 100KB rolling buffer)
- Truncation: 2000 lines OR 50KB (whichever first)
- Truncation method: `truncate_tail` (keeps end, shows errors)
- Creates temp file if output > 50KB (path in `details.full_output_path`)
- Inherits shell environment
- Optional command prefix (e.g., "shopt -s expand_aliases")

**Error conditions:**
- Non-zero exit code → Error with output + "Command exited with code X"
- Timeout → Error with output + "Command timed out after X seconds"
- Abort → Error "Command aborted"
- Kills entire process tree on timeout/abort

---

#### WRITE Tool

**Purpose:** Create or overwrite files

**Parameters:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "File path" },
    "content": { "type": "string", "description": "Content to write" }
  },
  "required": ["path", "content"]
}
```

**Behavior:**
- Creates parent directories recursively
- UTF-8 encoding
- Output: "Successfully wrote X bytes to path"

---

#### EDIT Tool

**Purpose:** Replace exact text in files

**Parameters:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "File path" },
    "oldText": { "type": "string", "description": "Exact text to find" },
    "newText": { "type": "string", "description": "Replacement text" }
  },
  "required": ["path", "oldText", "newText"]
}
```

**Matching algorithm:**
1. Try exact match
2. Fall back to fuzzy match:
   - Strip trailing whitespace per line
   - Normalize smart quotes (U+2018-U+201F → ' or ")
   - Normalize dashes (U+2010-U+2015, U+2212 → -)
   - Normalize special spaces (U+00A0, U+2002-U+200A, etc. → space)

**Validation:**
- Must find `oldText` exactly once
- Must actually change content (old != new)

**Line ending handling:**
- Detects original line ending (CRLF vs LF)
- Normalizes to LF internally
- Restores original ending in output

**BOM handling:**
- Strips UTF-8 BOM before matching
- Restores BOM in output

**Output includes:**
- Success message
- Unified diff with line numbers
- First changed line number

---

#### GREP Tool

**Purpose:** Search file contents (via ripgrep)

**Parameters:**
```json
{
  "type": "object",
  "properties": {
    "pattern": { "type": "string", "description": "Regex or literal pattern" },
    "path": { "type": "string", "description": "Directory or file", "default": "." },
    "glob": { "type": "string", "description": "Glob filter (e.g., *.ts)" },
    "ignoreCase": { "type": "boolean", "default": false },
    "literal": { "type": "boolean", "default": false },
    "context": { "type": "integer", "default": 0 },
    "limit": { "type": "integer", "default": 100 }
  },
  "required": ["pattern"]
}
```

**Behavior:**
- Uses ripgrep (`rg --json`)
- Respects `.gitignore`
- Searches dotfiles (`--hidden`)
- Individual lines truncated to 500 chars

**Limits:**
- Match limit: 100 (default)
- Byte limit: 50KB
- Line length: 500 chars

**Output format:**
```
path/file.ts:42: matching line content
path/file.ts-41- context line
```

---

#### FIND Tool

**Purpose:** Find files by glob pattern (via fd)

**Parameters:**
```json
{
  "type": "object",
  "properties": {
    "pattern": { "type": "string", "description": "Glob pattern (e.g., *.ts)" },
    "path": { "type": "string", "description": "Directory", "default": "." },
    "limit": { "type": "integer", "default": 1000 }
  },
  "required": ["pattern"]
}
```

**Behavior:**
- Uses fd (`fd --glob`)
- Respects `.gitignore`
- Searches dotfiles (`--hidden`)
- Returns relative paths
- Directories marked with trailing `/`

**Limits:**
- Result limit: 1000 (default)
- Byte limit: 50KB

---

#### LS Tool

**Purpose:** List directory contents

**Parameters:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Directory", "default": "." },
    "limit": { "type": "integer", "default": 500 }
  }
}
```

**Behavior:**
- Sorted alphabetically (case-insensitive)
- Directories marked with trailing `/`
- Includes dotfiles
- Skips entries that fail stat

**Limits:**
- Entry limit: 500 (default)
- Byte limit: 50KB

---

### 5.4 Truncation Constants

```rust
pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;  // 50KB
pub const GREP_MAX_LINE_LENGTH: usize = 500;
```

---

## 6. Session File Format

### 6.1 File Organization

```
~/.pi/agent/sessions/
└── --{encoded-cwd}--/
    └── {timestamp}_{session-id}.jsonl
```

**CWD encoding:** `--${cwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`
- Example: `/home/user/project` → `--home-user-project--`

**Timestamp format:** `YYYY-MM-DDTHH-mm-ss.sssZ` (colons replaced with hyphens)

### 6.2 Session Version

Current version: **3**

### 6.3 Header Structure

```rust
pub struct SessionHeader {
    pub r#type: String,              // "session"
    pub version: Option<u8>,         // Usually 3
    pub id: String,                  // UUID
    pub timestamp: String,           // ISO-8601
    pub cwd: String,                 // Absolute path
    pub provider: Option<String>,    // Provider name (optional)
    pub model_id: Option<String>,    // Model ID (optional)
    pub thinking_level: Option<String>,  // "off"|"minimal"|... (optional)
    pub parent_session: Option<String>,  // Parent session path (serialized as "branchedFrom"; accepts legacy "parentSession")
}
```

**Serialization:**
```json
{
  "type": "session",
  "version": 3,
  "id": "uuid-string",
  "timestamp": "2024-01-15T10:30:45.123Z",
  "cwd": "/absolute/path/to/dir",
  "provider": "anthropic",
  "modelId": "claude-sonnet-4-20250514",
  "thinkingLevel": "medium",
  "branchedFrom": "/path/to/parent.jsonl"
}
```

### 6.4 Entry Types

All entries have base fields:

```rust
pub struct EntryBase {
    pub id: Option<String>,   // 8-char hex or UUID (may be missing on disk)
    pub parent_id: Option<String>,
    pub timestamp: String,    // ISO-8601
}
```

#### Message Entry

```rust
pub struct MessageEntry {
    #[serde(flatten)]
    pub base: EntryBase,      // type: "message"
    pub message: SessionMessage,
}

pub enum SessionMessage {
    User { content: UserContent, timestamp: Option<i64> },
    Assistant { /* full AssistantMessage fields */ },
    ToolResult { tool_use_id: String, content: Vec<ContentBlock>, timestamp: Option<i64> },
    Custom { custom_type: String, content: String, display: bool, details: Option<Value> },
    BashExecution { command: String, output: String, exit_code: i32, /* ... */ },
    BranchSummary { summary: String, from_id: String },
    CompactionSummary { summary: String, tokens_before: u64 },
}
```

#### Model Change Entry

```rust
pub struct ModelChangeEntry {
    #[serde(flatten)]
    pub base: EntryBase,      // type: "model_change"
    pub provider: String,
    pub model_id: String,
}
```

#### Thinking Level Change Entry

```rust
pub struct ThinkingLevelChangeEntry {
    #[serde(flatten)]
    pub base: EntryBase,      // type: "thinking_level_change"
    pub thinking_level: String,  // "off"|"minimal"|"low"|"medium"|"high"|"xhigh"
}
```

#### Compaction Entry

```rust
pub struct CompactionEntry {
    #[serde(flatten)]
    pub base: EntryBase,      // type: "compaction"
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub details: Option<Value>,
    pub from_hook: Option<bool>,
}
```

#### Branch Summary Entry

```rust
pub struct BranchSummaryEntry {
    #[serde(flatten)]
    pub base: EntryBase,      // type: "branch_summary"
    pub from_id: String,      // "root" or entry ID
    pub summary: String,
    pub details: Option<Value>,
    pub from_hook: Option<bool>,
}
```

#### Label Entry

```rust
pub struct LabelEntry {
    #[serde(flatten)]
    pub base: EntryBase,      // type: "label"
    pub target_id: String,
    pub label: Option<String>,  // None = delete label
}
```

#### Session Info Entry

```rust
pub struct SessionInfoEntry {
    #[serde(flatten)]
    pub base: EntryBase,      // type: "session_info"
    pub name: Option<String>,
}
```

### 6.5 Tree Structure

- Each entry has `id` and `parent_id`
- Forms a linked tree enabling branching
- Leaf pointer tracks current position
- Moving leaf pointer enables branching without modifying history

### 6.6 ID Generation

- Format: 8-character hexadecimal (from UUID slice)
- Collision checking: 100 retries before full UUID
- Uniqueness scope: Per-session only

---

## 7. Configuration

### 7.1 File Locations

| Type | Path |
|------|------|
| Global settings | `~/.pi/agent/settings.json` |
| Project settings | `./.pi/settings.json` |
| Auth | `~/.pi/agent/auth.json` |
| Models | `~/.pi/agent/models.json` |
| Sessions | `~/.pi/agent/sessions/` |

### 7.2 Settings Structure

```rust
pub struct Settings {
    // Appearance
    pub theme: Option<String>,
    pub hide_thinking_block: Option<bool>,  // Default: false
    pub show_hardware_cursor: Option<bool>, // Default: false

    // Model Configuration
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_thinking_level: Option<String>,
    pub enabled_models: Option<Vec<String>>,  // Patterns for Ctrl+P cycling

    // Message Handling
    pub steering_mode: Option<String>,        // "all" | "one-at-a-time"
    pub follow_up_mode: Option<String>,       // "all" | "one-at-a-time"

    // Terminal Behavior
    pub quiet_startup: Option<bool>,          // Default: false
    pub collapse_changelog: Option<bool>,     // Default: false
    pub double_escape_action: Option<String>, // "fork" | "tree" | "none"
    pub editor_padding_x: Option<u32>,        // Default: 0
    pub autocomplete_max_visible: Option<u32>,// Default: 5

    // Compaction
    pub compaction: Option<CompactionSettings>,

    // Branch Summarization
    pub branch_summary: Option<BranchSummarySettings>,

    // Retry Configuration
    pub retry: Option<RetrySettings>,

    // Shell
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,

    // Images
    pub images: Option<ImageSettings>,

    // Terminal Display
    pub terminal: Option<TerminalSettings>,

    // Thinking Budgets
    pub thinking_budgets: Option<ThinkingBudgets>,

    // Extensions/Skills/etc.
    pub packages: Option<Vec<PackageSource>>,
    pub extensions: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
    pub themes: Option<Vec<String>>,
    pub enable_skill_commands: Option<bool>,  // Default: true
}

pub struct CompactionSettings {
    pub enabled: Option<bool>,         // Default: true
    pub reserve_tokens: Option<u32>,   // Default: 16384
    pub keep_recent_tokens: Option<u32>, // Default: 20000
}

pub struct RetrySettings {
    pub enabled: Option<bool>,         // Default: true
    pub max_retries: Option<u32>,      // Default: 3
    pub base_delay_ms: Option<u32>,    // Default: 2000
    pub max_delay_ms: Option<u32>,     // Default: 60000
}

pub struct ImageSettings {
    pub auto_resize: Option<bool>,     // Default: true (2000x2000 max)
    pub block_images: Option<bool>,    // Default: false
}

pub struct TerminalSettings {
    pub show_images: Option<bool>,     // Default: true (if supported)
    pub clear_on_shrink: Option<bool>, // Default: false
}
```

### 7.3 Settings Precedence

1. CLI flags (highest)
2. Environment variables
3. Project settings (`./.pi/settings.json`)
4. Global settings (`~/.pi/agent/settings.json`)
5. Built-in defaults (lowest)

### 7.4 Environment Variables

```rust
// Config paths
PI_CODING_AGENT_DIR     // Override ~/.pi/agent
PI_PACKAGE_DIR          // Override package assets

// API Keys (per provider)
ANTHROPIC_API_KEY
OPENAI_API_KEY
GOOGLE_API_KEY
GOOGLE_CLOUD_API_KEY
AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY
XAI_API_KEY
GROQ_API_KEY
CEREBRAS_API_KEY
OPENROUTER_API_KEY
MISTRAL_API_KEY
// ... etc.
```

---

## 8. Authentication Storage

### 8.1 Auth File

- **Path:** `~/.pi/agent/auth.json`
- **Permissions:** `0o600` (read-write owner only)
- **Locking:** File lock with 30-second stale timeout

### 8.2 Credential Types

```rust
pub enum AuthCredential {
    ApiKey { key: String },
    OAuth {
        access_token: String,
        refresh_token: String,
        expires: i64,  // Unix milliseconds
    },
}
```

### 8.3 Auth File Structure

```json
{
  "anthropic": { "type": "api_key", "key": "sk-ant-..." },
  "github-copilot": {
    "type": "oauth",
    "access_token": "...",
    "refresh_token": "...",
    "expires": 1234567890000
  }
}
```

### 8.4 API Key Resolution Priority

1. Runtime override (`--api-key` flag)
2. API key from `auth.json` (`type: "api_key"`)
3. OAuth token from `auth.json` (auto-refresh if expired)
4. Environment variable (provider-specific)
5. Fallback resolver (custom providers)

### 8.5 OAuth: Anthropic (Claude Pro/Max)

This port supports **Anthropic OAuth** as an alternative to API keys. Credentials are stored in `auth.json` under the provider key `"anthropic"` with `type: "oauth"`.

#### 8.5.1 PKCE

The login flow uses PKCE (RFC 7636):
- `verifier`: 32 random bytes, Base64URL (no padding)
- `challenge`: Base64URL(SHA256(verifier))

#### 8.5.2 Authorization URL

- `client_id`: `9d1c250a-e61b-44d9-88ed-5944d1962f5e`
- `authorize_url`: `https://claude.ai/oauth/authorize`
- `redirect_uri`: `https://console.anthropic.com/oauth/code/callback`
- `scopes`: `org:create_api_key user:profile user:inference`

Query params:
- `code=true`
- `client_id=<client_id>`
- `response_type=code`
- `redirect_uri=<redirect_uri>`
- `scope=<scopes>`
- `code_challenge=<challenge>`
- `code_challenge_method=S256`
- `state=<verifier>`

The user completes login in the browser and then pastes either:
- the full callback URL, or
- `code#state`, or
- just `code` (in which case `state` defaults to the original `verifier`)

#### 8.5.3 Token Exchange

POST `https://console.anthropic.com/v1/oauth/token` with JSON:
```json
{
  "grant_type": "authorization_code",
  "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
  "code": "<code>",
  "state": "<state>",
  "redirect_uri": "https://console.anthropic.com/oauth/code/callback",
  "code_verifier": "<verifier>"
}
```

Response JSON:
```json
{ "access_token": "...", "refresh_token": "...", "expires_in": 1234 }
```

Expiry is stored in milliseconds as:
```
expires = now_ms + (expires_in * 1000) - (5 * 60 * 1000)
```

#### 8.5.4 Refresh

If an OAuth credential is expired, it must be refreshed automatically.

POST `https://console.anthropic.com/v1/oauth/token` with JSON:
```json
{
  "grant_type": "refresh_token",
  "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
  "refresh_token": "<refresh_token>"
}
```

The refreshed credentials overwrite the stored entry in `auth.json` and are persisted immediately.

---

## 9. CLI Commands and Flags

### 9.1 Package Commands

| Command | Syntax | Description |
|---------|--------|-------------|
| `install` | `pi install <source> [-l\|--local]` | Install extension/skill/prompt/theme |
| `remove` | `pi remove <source> [-l\|--local]` | Remove from settings |
| `update` | `pi update [source]` | Update all or specific source |
| `list` | `pi list` | List global + project packages |
| `config` | `pi config` | Open TUI config selector |

### 9.2 Flags (Complete List)

#### Help & Version
| Flag | Aliases | Type | Default |
|------|---------|------|---------|
| `--help` | `-h` | bool | false |
| `--version` | `-v` | bool | false |

#### Model Configuration
| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--provider` | string | "google" | Provider name |
| `--model` | string | "gemini-2.5-flash" | Model ID |
| `--api-key` | string | None | API key override |
| `--models` | string | None | Comma-separated model patterns for cycling |

#### Thinking/Reasoning
| Flag | Type | Values | Default |
|------|------|--------|---------|
| `--thinking` | enum | off, minimal, low, medium, high, xhigh | None |

#### System Prompt
| Flag | Type | Description |
|------|------|-------------|
| `--system-prompt` | string | Override system prompt |
| `--append-system-prompt` | string | Append to system prompt |

#### Session Management
| Flag | Aliases | Type | Description |
|------|---------|------|-------------|
| `--continue` | `-c` | bool | Continue previous session |
| `--resume` | `-r` | bool | Select session from picker |
| `--session` | | string | Specific session file path |
| `--session-dir` | | string | Session storage directory |
| `--no-session` | | bool | Ephemeral session |

#### Mode & Output
| Flag | Aliases | Type | Values |
|------|---------|------|--------|
| `--mode` | | enum | text, json, rpc |
| `--print` | `-p` | bool | Non-interactive mode |
| `--verbose` | | bool | Verbose startup |

#### Tools
| Flag | Type | Default |
|------|------|---------|
| `--no-tools` | bool | Disable all tools |
| `--tools` | string | "read,bash,edit,write" |

#### Extensions
| Flag | Aliases | Type |
|------|---------|------|
| `--extension` | `-e` | string (repeatable) |
| `--no-extensions` | | bool |

#### Skills
| Flag | Type |
|------|------|
| `--skill` | string (repeatable) |
| `--no-skills` | bool |

#### Prompt Templates
| Flag | Type |
|------|------|
| `--prompt-template` | string (repeatable) |
| `--no-prompt-templates` | bool |

#### Themes
| Flag | Type |
|------|------|
| `--theme` | string (repeatable) |
| `--no-themes` | bool |

#### Export & Listing
| Flag | Type | Description |
|------|------|-------------|
| `--export` | string | Export session to HTML |
| `--list-models` | bool/string | List available models |

### 9.3 Positional Arguments

- **File arguments:** Prefixed with `@` (e.g., `@file.md`)
- **Message arguments:** Any non-flag positional argument

### 9.4 Usage

```
pi [options] [@files...] [messages...]
```

---

## 10. Execution Flow

```
1. Check for package commands (install/remove/update/list/config)
   └─ If matched: execute & exit(0)

2. Run migrations

3. First pass: parse extension/skill/prompt/theme flags
   └─ Load resources

4. Second pass: parse with extension-registered flags

5. Early exits:
   ├─ --version → print version & exit(0)
   ├─ --help → print help & exit(0)
   ├─ --list-models → list models & exit(0)
   └─ --export → export & exit(0/1)

6. Handle stdin (if not TTY and not RPC mode)
   └─ Prepend stdin to messages, force print=true

7. Prepare initial message from @files and message args

8. Determine mode:
   └─ isInteractive = !print && mode == undefined

9. Create session manager:
   ├─ --no-session → in-memory
   ├─ --session → open/fork specific file
   ├─ --continue → continue most recent
   ├─ --resume → show picker
   └─ default → create new

10. Resolve model scope from --models or settings

11. Build session options

12. Apply --api-key override

13. Create agent session
    └─ exit(1) if no model and not interactive

14. Clamp thinking level to model capabilities

15. Run mode:
    ├─ RPC → runRpcMode() [continues indefinitely]
    ├─ Interactive → InteractiveMode.run() [continues indefinitely]
    └─ Print → runPrintMode() → exit(0)
```

### 10.1 Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Error (invalid args, file not found, API error, etc.) |

---

## 11. RPC Mode Protocol (JSON over stdio)

### 11.1 Start RPC Mode

```bash
pi --mode rpc [options]
```

Common options:
- `--provider <name>`
- `--model <id>`
- `--no-session`
- `--session-dir <path>`

### 11.2 Protocol Overview

- **Commands**: JSON objects sent to stdin, one per line.
- **Responses**: JSON objects with `type: "response"` indicating success/failure.
- **Events**: JSON objects streamed to stdout as JSON lines.
- All commands support optional `id` for correlation; responses echo `id`.

### 11.3 Commands

#### Prompting

**prompt**
- Request: `{"id":"req-1","type":"prompt","message":"Hello"}`
- Optional `images`: array of `ImageContent` objects (`type:"image"`, `source` = `{type:"base64", mediaType, data}`).
- Optional `streamingBehavior`: `"steer"` or `"followUp"`.
  - If agent is streaming and `streamingBehavior` is absent → error.
  - `"steer"`: interrupt after current tool execution; remaining tool calls skipped.
  - `"followUp"`: queue after agent finishes.
- Extension commands (`/command`) execute immediately even during streaming.
- Skill commands (`/skill:name`) and prompt templates (`/template`) expand before queueing.
- Response: `{"type":"response","command":"prompt","success":true}`

**steer**
- Request: `{"type":"steer","message":"Stop and do this instead"}`
- Same expansion rules; extension commands not allowed (use `prompt`).
- Response: `{"type":"response","command":"steer","success":true}`

**follow_up**
- Request: `{"type":"follow_up","message":"After you're done, also do this"}`
- Same expansion rules; extension commands not allowed.
- Response: `{"type":"response","command":"follow_up","success":true}`

**abort**
- Request: `{"type":"abort"}`
- Response: `{"type":"response","command":"abort","success":true}`

**new_session**
- Request: `{"type":"new_session"}` or `{"type":"new_session","parentSession":"/path/to/parent.jsonl"}`
- Can be cancelled by `session_before_switch` extension handler.
- Response: `{"type":"response","command":"new_session","success":true,"data":{"cancelled":false}}`

#### State

**get_state**
- Request: `{"type":"get_state"}`
- Response data:
  - `model`: full `Model` or `null`
  - `thinkingLevel`: `"off"|"minimal"|"low"|"medium"|"high"|"xhigh"`
  - `isStreaming`: bool
  - `isCompacting`: bool
  - `steeringMode`: `"all"|"one-at-a-time"`
  - `followUpMode`: `"all"|"one-at-a-time"`
  - `sessionFile`: string | null
  - `sessionId`: string
  - `sessionName`: string | null
  - `autoCompactionEnabled`: bool
  - `messageCount`: number
  - `pendingMessageCount`: number

**get_messages**
- Request: `{"type":"get_messages"}`
- Response: `{"type":"response","command":"get_messages","success":true,"data":{"messages":[...]}}`
- Messages are `AgentMessage` objects (User/Assistant/ToolResult/BashExecution).

#### Model

**set_model**
- Request: `{"type":"set_model","provider":"anthropic","modelId":"claude-sonnet-4-20250514"}`
- Response: `{"type":"response","command":"set_model","success":true,"data":<Model>}`

**cycle_model**
- Request: `{"type":"cycle_model"}`
- Response: `{"type":"response","command":"cycle_model","success":true,"data":{"model":<Model>,"thinkingLevel":"medium","isScoped":false}}`
- Returns `data: null` if only one model available.

**get_available_models**
- Request: `{"type":"get_available_models"}`
- Response: `{"type":"response","command":"get_available_models","success":true,"data":{"models":[<Model>,...]}}`

#### Thinking

**set_thinking_level**
- Request: `{"type":"set_thinking_level","level":"high"}`
- Levels: `"off"|"minimal"|"low"|"medium"|"high"|"xhigh"`
- Response: success true

**cycle_thinking_level**
- Request: `{"type":"cycle_thinking_level"}`
- Response: `{"type":"response","command":"cycle_thinking_level","success":true,"data":{"level":"high"}}`
- Returns `data: null` if model does not support thinking.

#### Queue Modes

**set_steering_mode**
- Request: `{"type":"set_steering_mode","mode":"one-at-a-time"}`
- Modes: `"all"` or `"one-at-a-time"` (default)
- Response: success true

**set_follow_up_mode**
- Request: `{"type":"set_follow_up_mode","mode":"one-at-a-time"}`
- Modes: `"all"` or `"one-at-a-time"` (default)
- Response: success true

#### Compaction

**compact**
- Request: `{"type":"compact"}` or `{"type":"compact","customInstructions":"Focus on code changes"}`
- Response data: `{summary, firstKeptEntryId, tokensBefore, details}`

**set_auto_compaction**
- Request: `{"type":"set_auto_compaction","enabled":true}`
- Response: success true

#### Retry

**set_auto_retry**
- Request: `{"type":"set_auto_retry","enabled":true}`
- Response: success true

**abort_retry**
- Request: `{"type":"abort_retry"}`
- Response: success true

#### Bash

**bash**
- Request: `{"type":"bash","command":"ls -la"}`
- Response data:
  - `output`: string
  - `exitCode`: number
  - `cancelled`: bool
  - `truncated`: bool
  - `fullOutputPath`: string | null (only when truncated)

**abort_bash**
- Request: `{"type":"abort_bash"}`
- Response: success true

#### Session

**get_session_stats**
- Request: `{"type":"get_session_stats"}`
- Response data:
  - `sessionFile`, `sessionId`
  - `userMessages`, `assistantMessages`, `toolCalls`, `toolResults`, `totalMessages`
  - `tokens`: `{input, output, cacheRead, cacheWrite, total}`
  - `cost`: number (total $)

**export_html**
- Request: `{"type":"export_html"}` or `{"type":"export_html","outputPath":"/tmp/session.html"}`
- Response data: `{path: "<output path>"}`

**switch_session**
- Request: `{"type":"switch_session","sessionPath":"/path/to/session.jsonl"}`
- Can be cancelled by `session_before_switch` extension handler.
- Response data: `{cancelled: false}`

**fork**
- Request: `{"type":"fork","entryId":"abc123"}`
- `entryId` must be a user message entry.
- Creates a new session (branched from parent of selected entry).
- Response data: `{text:"<user message text>", cancelled:false}`
- Can be cancelled by `session_before_fork` extension handler.

**get_fork_messages**
- Request: `{"type":"get_fork_messages"}`
- Response data: `{messages:[{entryId, text}, ...]}`

**get_last_assistant_text**
- Request: `{"type":"get_last_assistant_text"}`
- Response data: `{text: "<assistant text>"}` or `{text: null}`

**set_session_name**
- Request: `{"type":"set_session_name","name":"my-feature-work"}`
- Response: success true

#### Commands

**get_commands**
- Request: `{"type":"get_commands"}`
- Response data: `{"commands":[{name, description?, source, location?, path?}, ...]}`
  - `source`: `"extension"|"template"|"skill"`
  - `location`: `"user"|"project"|"path"` (not present for extensions)

### 11.4 Events

| Event | Fields |
|-------|--------|
| `agent_start` | `{type:"agent_start"}` |
| `agent_end` | `{type:"agent_end", messages:[AgentMessage], error?}` |
| `turn_start` | `{type:"turn_start"}` |
| `turn_end` | `{type:"turn_end", message:AgentMessage, toolResults:[AgentMessage]}` |
| `message_start` | `{type:"message_start", message:AgentMessage}` |
| `message_update` | `{type:"message_update", message:AgentMessage, assistantMessageEvent:<delta>}` |
| `message_end` | `{type:"message_end", message:AgentMessage}` |
| `tool_execution_start` | `{type:"tool_execution_start", toolCallId, toolName, args}` |
| `tool_execution_update` | `{type:"tool_execution_update", toolCallId, toolName, args, partialResult}` |
| `tool_execution_end` | `{type:"tool_execution_end", toolCallId, toolName, result, isError}` |
| `auto_compaction_start` | `{type:"auto_compaction_start", reason:"threshold"|"overflow"}` |
| `auto_compaction_end` | `{type:"auto_compaction_end", result, aborted, willRetry, errorMessage?}` |
| `auto_retry_start` | `{type:"auto_retry_start", attempt, maxAttempts, delayMs, errorMessage}` |
| `auto_retry_end` | `{type:"auto_retry_end", success, attempt, finalError?}` |
| `extension_error` | `{type:"extension_error", extensionPath, event, error}` |

**assistantMessageEvent delta types** (streaming):
- `start`
- `text_start`, `text_delta`, `text_end`
- `thinking_start`, `thinking_delta`, `thinking_end`
- `toolcall_start`, `toolcall_delta`, `toolcall_end` (includes full `toolCall`)
- `done` (`reason`: `"stop"|"length"|"toolUse"`)
- `error` (`reason`: `"aborted"|"error"`)

### 11.5 Extension UI Protocol (RPC)

**extension_ui_request** (stdout):
- Base: `{type:"extension_ui_request", id, method, ...}`
- Dialog methods block until response or timeout:
  - `select`: `{title, options:[{label,value}], placeholder?, default?, timeout?}`
  - `confirm`: `{title, message, default?, timeout?}`
  - `input`: `{title, placeholder?, default?, password?, timeout?}`
  - `editor`: `{title, language?, default?, readOnly?, timeout?}`
- Fire-and-forget methods (no response expected):
  - `notify`: `{title, message, level?}`
  - `setStatus`: `{text}`
  - `setWidget`: `{content}`
  - `setTitle`: `{title}`
  - `set_editor_text`: `{text}`
- RPC limitations: `custom()` returns `undefined`; `setWorkingMessage`, `setFooter`, `setHeader`,
  `setEditorComponent` are no-ops; `getEditorText()` returns `""`.

**extension_ui_response** (stdin):
- Base: `{type:"extension_ui_response", id, value?, cancelled?}`
- Dialog responses:
  - select/input/editor: `{value: <selected/entered>}`
  - confirm: `{value: true|false}`
  - cancellation: `{cancelled: true}`

### 11.6 RPC Types

**Model**
- `id`, `name`, `api`, `provider`, `baseUrl`
- `reasoning` (bool)
- `input`: `["text","image"]`
- `contextWindow`, `maxTokens`
- `cost`: `{input, output, cacheRead, cacheWrite}`

**UserMessage**
- `{role:"user", content, timestamp, attachments:[]}`
- `content` can be string or array of `TextContent`/`ImageContent`.

**AssistantMessage**
- `{role:"assistant", content:[...], api, provider, model, usage, stopReason, timestamp}`
- `usage`: `{input, output, cacheRead, cacheWrite, cost:{input, output, cacheRead, cacheWrite, total}}`
- `stopReason`: `"stop"|"length"|"toolUse"|"error"|"aborted"`

**ToolResultMessage**
- `{role:"toolResult", toolCallId, toolName, content, isError, timestamp}`

**BashExecutionMessage**
- `{role:"bashExecution", command, output, exitCode, cancelled, truncated, fullOutputPath, timestamp}`

**Attachment**
- `{id, type:"image", fileName, mimeType, size, content, extractedText, preview}`

---

## Summary

This specification covers:
- **Message types:** User, Assistant, ToolResult with all content block variants
- **Streaming:** Full event type enumeration with sequences
- **Providers:** Trait definition and model registry structure
- **Tools:** All 7 built-in tools with exact parameters and behaviors
- **Sessions:** JSONL format with tree structure
- **Config:** Settings structure with precedence rules
- **Auth:** Credential storage with OAuth refresh
- **CLI:** Complete flag list with execution flow
- **RPC:** JSON command protocol with events, types, and extension UI

**After reading this document, you should NOT need to consult the legacy TypeScript code.**
