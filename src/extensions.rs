//! Extension protocol, policy, and runtime scaffolding.
//!
//! This module defines the versioned extension protocol and provides
//! validation utilities plus a minimal WASM host scaffold.

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::connectors::Connector;
use crate::connectors::http::HttpConnector;
use crate::error::{Error, Result};
use crate::extension_events::{ToolCallEventResult, ToolResultEventResult};
use crate::extensions_js::{
    ExtensionRepairEvent, ExtensionToolDef, HostcallKind, HostcallRequest, PiJsRuntime,
    PiJsRuntimeConfig, js_to_json, json_to_js,
};
use crate::hostcall_amac::AmacBatchExecutor;
use crate::hostcall_rewrite::{
    HostcallRewriteEngine, HostcallRewritePlan, HostcallRewritePlanKind,
};
use crate::hostcall_superinstructions::{
    HostcallSuperinstructionCompiler, HostcallSuperinstructionPlan, execute_with_superinstruction,
};
use crate::hostcall_trace_jit::{GuardContext, TraceJitCompiler};
use crate::permissions::{PermissionStore, PersistedDecision};
use crate::resources::ExtensionResourcePaths;
use crate::scheduler::HostcallOutcome;
use crate::session::SessionMessage;
use crate::tools::ToolRegistry;
use ast_grep_core::{AstGrep, Pattern};
use ast_grep_language::SupportLang;
use asupersync::channel::{mpsc, oneshot};
use asupersync::runtime::RuntimeBuilder;
#[cfg(feature = "wasm-host")]
use asupersync::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use asupersync::time::{sleep, timeout, wall_now};
use asupersync::{Budget, Cx};
use async_trait::async_trait;
use regex::Regex;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering as StdOrdering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::thread;
use std::time::{Duration, Instant};
use url::Url;
use uuid::Uuid;

// Public filesystem contracts stay defined in this façade so both their
// import paths and `std::any::type_name` identities remain unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsOp {
    Read,
    Write,
    List,
    Stat,
    Mkdir,
    Delete,
}

#[derive(Debug, Clone)]
pub struct FsScopes {
    read_declared: bool,
    write_declared: bool,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct FsConnector {
    cwd: PathBuf,
    policy: ExtensionPolicy,
    scopes: FsScopes,
}

/// Operator trust marker for permission drift comparisons.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionPermissionTrust {
    /// No explicit operator trust decision accompanies this snapshot.
    #[default]
    Untrusted,
    /// The operator explicitly approved this permission change.
    ExplicitlyTrusted,
}

/// Stable input snapshot for extension permission drift detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionPermissionSnapshot {
    /// Extension identifier from manifest or catalog.
    pub extension_id: String,
    /// Flat manifest or runtime capability list.
    pub capabilities: Vec<String>,
    /// Structured v1/v2 capability manifest when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability_manifest: Option<CapabilityManifest>,
    /// Policy profile observed for this extension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_profile: Option<PolicyProfile>,
    /// Canonical checksum for the observed extension manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_checksum: Option<String>,
    /// Canonical checksum for the observed provenance snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance_snapshot_checksum: Option<String>,
    /// Catalog-declared capabilities expected for this extension.
    pub catalog_capabilities: Vec<String>,
    /// Catalog policy profile expected for this extension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_policy_profile: Option<PolicyProfile>,
    /// Catalog checksum expected for the manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_manifest_checksum: Option<String>,
    /// Catalog checksum expected for the provenance snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_provenance_checksum: Option<String>,
    /// Operator trust marker for this snapshot.
    pub trust: ExtensionPermissionTrust,
}

/// Primary class assigned to a permission drift report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionPermissionDriftClass {
    NoDrift,
    AddedCapabilities,
    AddedDangerousCapabilities,
    RemovedCapabilities,
    PolicyProfileMismatch,
    ProvenanceMismatch,
    MissingProvenance,
    StaleManifest,
    ExplicitlyTrustedChange,
}

/// Risk level for a permission drift report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionPermissionRiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

/// Provenance state observed during permission drift detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionPermissionProvenanceStatus {
    Verified,
    Missing,
    Mismatch,
    Stale,
    Trusted,
    NotRequired,
}

/// Launch verdict for a permission drift report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionPermissionDriftVerdict {
    Allow,
    AllowWithAudit,
    ReviewRequired,
    FailClosed,
}

/// Stable JSON-serializable evidence emitted by permission drift detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionPermissionDriftReport {
    pub extension_id: String,
    pub previous_capabilities: BTreeSet<String>,
    pub current_capabilities: BTreeSet<String>,
    pub added_capabilities: BTreeSet<String>,
    pub removed_capabilities: BTreeSet<String>,
    pub drift_class: ExtensionPermissionDriftClass,
    pub drift_classes: Vec<ExtensionPermissionDriftClass>,
    pub risk_level: ExtensionPermissionRiskLevel,
    pub provenance_status: ExtensionPermissionProvenanceStatus,
    pub recommended_action: String,
    pub verdict: ExtensionPermissionDriftVerdict,
}

/// Classification of dangerous command patterns for exec mediation.
///
/// Each variant represents a class of commands that pose a security risk when
/// executed by an extension. The classifier is deterministic: given the same
/// command string, the same classification is always returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DangerousCommandClass {
    /// Recursive deletion targeting root or broad paths (`rm -rf /`).
    RecursiveDelete,
    /// Device-level writes (`dd`, `mkfs`, `fdisk`).
    DeviceWrite,
    /// Fork bomb or process exhaustion patterns.
    ForkBomb,
    /// Pipe to shell execution (`curl | sh`, `wget | bash`).
    PipeToShell,
    /// System shutdown or reboot commands.
    SystemShutdown,
    /// Broad permission changes (`chmod 777`, `chmod -R 777`).
    PermissionEscalation,
    /// Killing critical system processes (`kill -9 1`, `pkill init`).
    ProcessTermination,
    /// Modifying /etc/passwd, /etc/shadow, or sudoers.
    CredentialFileModification,
    /// Disk wipe or overwrite patterns (`shred`, `wipefs`).
    DiskWipe,
    /// Network exfiltration via raw sockets or reverse shells.
    ReverseShell,
}

/// Risk tier for exec command classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecRiskTier {
    /// Low risk — normal commands.
    Low,
    /// Medium risk — commands that could be misused.
    Medium,
    /// High risk — commands with significant destructive potential.
    High,
    /// Critical risk — commands that could cause irreversible damage.
    Critical,
}

/// Policy configuration for exec mediation (SEC-4.3).
///
/// Controls which commands are allowed/denied based on pattern matching
/// and dangerous command classification. Evaluated after capability-level
/// policy and quota checks but before the actual command is spawned.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ExecMediationPolicy {
    /// When true, exec mediation is active and commands are classified.
    pub enabled: bool,
    /// Minimum risk tier that triggers a deny (default: Critical).
    /// Commands at or above this tier are blocked.
    pub deny_threshold: ExecRiskTier,
    /// Explicit command prefixes to deny (case-insensitive prefix match).
    /// These are checked before the built-in classifier.
    #[serde(default)]
    pub deny_patterns: Vec<String>,
    /// Explicit command prefixes to allow even if classified as dangerous.
    /// Use sparingly — allows overriding the classifier for specific tools.
    #[serde(default)]
    pub allow_patterns: Vec<String>,
    /// When true, commands classified as dangerous are logged even if allowed.
    pub audit_all_classified: bool,
}

/// Result of exec mediation evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecMediationResult {
    /// Command is allowed to proceed.
    Allow,
    /// Command is allowed but was classified as potentially dangerous.
    AllowWithAudit {
        class: DangerousCommandClass,
        reason: String,
    },
    /// Command is denied.
    Deny {
        class: Option<DangerousCommandClass>,
        reason: String,
    },
}

/// Patterns used to identify environment variables likely to contain secrets.
///
/// The broker uses suffix and prefix matching to catch common naming
/// conventions for API keys, tokens, passwords, and credentials.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SecretBrokerPolicy {
    /// When true, the secret broker is active.
    pub enabled: bool,
    /// Env var name suffixes that indicate a secret (case-insensitive).
    pub secret_suffixes: Vec<String>,
    /// Env var name prefixes that indicate a secret (case-insensitive).
    pub secret_prefixes: Vec<String>,
    /// Exact env var names that are always treated as secrets (case-insensitive).
    pub secret_exact: Vec<String>,
    /// Env vars on this list are never redacted, even if they match a pattern.
    pub disclosure_allowlist: Vec<String>,
    /// The replacement string used in place of secret values.
    pub redaction_placeholder: String,
}

/// Telemetry entry for exec mediation decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecMediationLedgerEntry {
    /// Unix epoch milliseconds.
    pub ts_ms: i64,
    /// Extension that requested exec.
    pub extension_id: Option<String>,
    /// Hash of the command (never log raw command).
    pub command_hash: String,
    /// Dangerous command class if classified.
    pub command_class: Option<String>,
    /// Risk tier of the classification.
    pub risk_tier: Option<String>,
    /// Mediation decision.
    pub decision: String,
    /// Human-readable reason.
    pub reason: String,
}

/// Telemetry entry for secret broker decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretBrokerLedgerEntry {
    /// Unix epoch milliseconds.
    pub ts_ms: i64,
    /// Extension requesting env access.
    pub extension_id: Option<String>,
    /// Hash of the env var name (never log raw name for denied vars).
    pub name_hash: String,
    /// Whether the value was redacted.
    pub redacted: bool,
    /// Reason for redaction or disclosure.
    pub reason: String,
}

/// Structured artifact for exec mediation decision history (SEC-4.3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecMediationArtifact {
    /// Schema identifier.
    pub schema: String,
    /// Generation timestamp (Unix epoch ms).
    pub generated_at_ms: i64,
    /// Number of entries.
    pub entry_count: usize,
    /// Decision entries.
    pub entries: Vec<ExecMediationLedgerEntry>,
}

/// Structured artifact for secret broker decision history (SEC-4.3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretBrokerArtifact {
    /// Schema identifier.
    pub schema: String,
    /// Generation timestamp (Unix epoch ms).
    pub generated_at_ms: i64,
    /// Number of entries.
    pub entry_count: usize,
    /// Decision entries.
    pub entries: Vec<SecretBrokerLedgerEntry>,
}

// Protocol DTOs and opcode contracts remain physically in the façade. Moving
// their impls is safe; moving these declarations would change type identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionMessage {
    pub id: String,
    pub version: String,
    #[serde(flatten)]
    pub body: ExtensionBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ExtensionBody {
    Register(RegisterPayload),
    ToolCall(ToolCallPayload),
    ToolResult(ToolResultPayload),
    SlashCommand(SlashCommandPayload),
    SlashResult(SlashResultPayload),
    EventHook(EventHookPayload),
    HostCall(HostCallPayload),
    HostResult(HostResultPayload),
    Log(Box<LogPayload>),
    Error(ErrorPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterPayload {
    pub name: String,
    pub version: String,
    pub api_version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_manifest: Option<CapabilityManifest>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub slash_commands: Vec<Value>,
    #[serde(default)]
    pub shortcuts: Vec<Value>,
    #[serde(default)]
    pub flags: Vec<Value>,
    #[serde(default)]
    pub event_hooks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityManifest {
    pub schema: String,
    pub capabilities: Vec<CapabilityRequirement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRequirement {
    pub capability: String,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intents: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connector_classes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hostcall_classes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<CapabilityScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<CapabilityProvenance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityProvenance {
    pub source: String,
    pub integrity: CapabilityIntegrityAttestation,
    pub publisher: CapabilityPublisherAttestation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityIntegrityAttestation {
    pub algorithm: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityPublisherAttestation {
    pub id: String,
    pub verification: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallPayload {
    pub call_id: String,
    pub name: String,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultPayload {
    pub call_id: String,
    pub output: Value,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCallPayload {
    pub call_id: String,
    pub capability: String,
    pub method: String,
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostCallErrorCode {
    Timeout,
    Denied,
    Io,
    InvalidRequest,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCallError {
    pub code: HostCallErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostStreamChunk {
    pub index: u64,
    pub is_last: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backpressure: Option<HostStreamBackpressure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostStreamBackpressure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostResultPayload {
    pub call_id: String,
    pub output: Value,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<HostCallError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<HostStreamChunk>,
}

pub const HOSTCALL_OPCODE_SCHEMA_VERSION: &str = "pi.ext.hostcall_opcode.v1";
pub const HOSTCALL_OPCODE_VERSION: u16 = 1;
pub const HOSTCALL_IO_URING_CONTEXT_SCHEMA_VERSION: &str = "pi.ext.io_uring_lane_input.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommonHostcallOpcode {
    ToolRead,
    ToolWrite,
    ToolEdit,
    ToolBash,
    SessionGetState,
    SessionGetMessages,
    SessionGetEntries,
    SessionGetBranch,
    SessionGetFile,
    SessionGetName,
    SessionSetName,
    SessionGetModel,
    SessionSetModel,
    SessionGetThinkingLevel,
    SessionSetThinkingLevel,
    SessionSetLabel,
    EventsGetActiveTools,
    EventsGetAllTools,
    EventsSetActiveTools,
    EventsEmit,
    EventsList,
    EventsGetModel,
    EventsSetModel,
    EventsGetThinkingLevel,
    EventsSetThinkingLevel,
    EventsGetFlag,
    EventsListFlags,
    EventsAppendEntry,
    EventsRegisterCommand,
}

/// Context for the shared hostcall dispatcher.
///
/// Carries the runtime resources needed to dispatch any hostcall, regardless of
/// whether it originated from a JS extension, WASM component, or protocol message.
pub struct HostCallContext<'a> {
    /// Runtime origin identifier (e.g. `"js"`, `"wasm"`, `"protocol"`).
    pub runtime_name: &'a str,
    /// Extension that initiated the call (for policy + logging).
    pub extension_id: Option<&'a str>,
    /// Built-in tool registry.
    pub tools: &'a ToolRegistry,
    /// HTTP connector for outbound requests.
    pub http: &'a HttpConnector,
    /// Extension manager for session/ui/events dispatch.
    pub manager: Option<ExtensionManager>,
    /// Policy governing capability access.
    pub policy: &'a ExtensionPolicy,
    /// Optional JS runtime for exec streaming.
    pub js_runtime: Option<&'a PiJsRuntime>,
    /// Test interceptor (if any).
    pub interceptor: Option<&'a dyn HostcallInterceptor>,
}

/// Configuration for the core-pinned hostcall reactor mesh.
#[derive(Debug, Clone)]
pub struct HostcallReactorConfig {
    /// Number of shard lanes. Each shard processes hostcalls independently.
    pub shard_count: usize,
    /// Maximum queued requests per shard lane before backpressure.
    pub lane_capacity: usize,
    /// Optional core affinity: `core_ids[shard_id]` = logical CPU for that shard.
    /// If `None` or shorter than `shard_count`, shards run unaffinied.
    pub core_ids: Option<Vec<usize>>,
}

/// A hostcall request enqueued into the reactor mesh for shard-local dispatch.
#[derive(Debug, Clone)]
pub struct HostcallReactorRequest {
    /// Unique call identifier.
    pub call_id: String,
    /// Typed fast-lane opcode for this request.
    // The dispatcher routes on this before enqueueing; drained requests keep it
    // for tests and external reactor diagnostics.
    #[allow(dead_code)]
    pub(crate) opcode: CommonHostcallOpcode,
    /// Params with the `"op"` key already stripped.
    pub params: Value,
    /// Destination shard (set by the mesh router).
    pub shard_id: usize,
    /// Monotone shard-local sequence.
    pub shard_seq: u64,
    /// Global monotone sequence for deterministic cross-shard ordering.
    pub global_seq: u64,
    /// Timestamp (nanoseconds since epoch) when enqueued.
    pub enqueued_at_ns: u64,
}

/// Completion of a reactor-dispatched hostcall.
#[derive(Debug, Clone)]
pub struct HostcallReactorCompletion {
    /// The call_id that was dispatched.
    pub call_id: String,
    /// Result of the dispatch.
    pub outcome: HostcallOutcome,
    /// Shard that processed this call.
    pub shard_id: usize,
    /// Dispatch latency in nanoseconds (from enqueue to completion).
    pub dispatch_latency_ns: u64,
}

/// Backpressure signal when a reactor shard lane is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostcallReactorBackpressure {
    pub shard_id: usize,
    pub depth: usize,
    pub capacity: usize,
}

/// Lightweight queueing telemetry for the reactor mesh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostcallReactorTelemetry {
    pub shard_count: usize,
    pub lane_capacity: usize,
    pub queue_depths: Vec<usize>,
    pub max_queue_depths: Vec<usize>,
    pub total_enqueued: Vec<u64>,
    pub rejected_enqueues: u64,
    pub total_dispatched: u64,
    pub lane_dispatch_latency_p95_ns: Vec<u64>,
    pub lane_dispatch_latency_p99_ns: Vec<u64>,
    pub overloaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overload_reason: Option<String>,
    /// Whether a NUMA slab pool is active.
    pub numa_pool_active: bool,
    /// Number of thread affinity advisories available.
    pub affinity_advisory_count: usize,
}

/// Deterministic SPSC reactor mesh for hostcall traffic.
///
/// Routes fast-lane hostcall requests to per-shard SPSC lanes using
/// stable hash routing (by call_id for affinity) or opcode-class routing.
///
/// **Routing policy:**
/// - Session opcodes: hash-routed by `call_id` (shard affinity preserves
///   per-call ordering for streaming scenarios).
/// - Events opcodes: round-robin across shards for load distribution.
/// - Tool opcodes: hash-routed by `call_id`.
///
/// **Drain policy:**
/// - `drain_shard(shard_id, budget)` for per-shard processing.
/// - `drain_global_order(budget)` for deterministic cross-shard ordering.
#[derive(Debug)]
pub struct HostcallReactorMesh {
    config: HostcallReactorConfig,
    lanes: Vec<HostcallSpscLane>,
    shard_seq: Vec<u64>,
    global_seq: u64,
    rr_cursor: usize,
    rejected_enqueues: u64,
    total_dispatched: u64,
    /// NUMA-aware slab pool for tracking per-shard resource utilization.
    numa_pool: Option<crate::scheduler::NumaSlabPool>,
    /// Thread affinity advice derived from the reactor's core mapping.
    affinity_advice: Vec<crate::scheduler::ThreadAffinityAdvice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandPayload {
    pub name: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashResultPayload {
    pub output: Value,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventHookPayload {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogPayload {
    pub schema: String,
    pub ts: String,
    pub level: LogLevel,
    pub event: String,
    pub message: String,
    pub correlation: LogCorrelation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LogSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogCorrelation {
    pub extension_id: String,
    pub scenario_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slash_command_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogSource {
    pub component: LogComponent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogComponent {
    Capture,
    Harness,
    Runtime,
    Extension,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// Extension UI request payload (host -> UI surface).
#[derive(Debug, Clone)]
pub struct ExtensionUiRequest {
    pub id: String,
    pub method: String,
    pub payload: Value,
    pub timeout_ms: Option<u64>,
    /// Extension that initiated this UI request (for provenance display).
    pub extension_id: Option<String>,
}

/// Extension UI response payload (UI surface -> host).
#[derive(Debug, Clone)]
pub struct ExtensionUiResponse {
    pub id: String,
    pub value: Option<Value>,
    pub cancelled: bool,
}

/// Minimal session access for extensions (hostcalls).
#[async_trait]
pub trait ExtensionSession: Send + Sync {
    async fn get_state(&self) -> Value;
    async fn get_messages(&self) -> Vec<SessionMessage>;
    async fn get_entries(&self) -> Vec<Value>;
    async fn get_branch(&self) -> Vec<Value>;
    async fn set_name(&self, name: String) -> Result<()>;
    async fn append_message(&self, message: SessionMessage) -> Result<()>;
    async fn append_custom_entry(&self, custom_type: String, data: Option<Value>) -> Result<()>;
    async fn set_model(&self, provider: String, model_id: String) -> Result<()>;
    async fn get_model(&self) -> (Option<String>, Option<String>);
    async fn set_thinking_level(&self, level: String) -> Result<()>;
    async fn get_thinking_level(&self) -> Option<String>;
    async fn set_label(&self, target_id: String, label: Option<String>) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionDeliverAs {
    Steer,
    FollowUp,
    NextTurn,
}

#[derive(Debug, Clone)]
pub struct ExtensionSendMessage {
    pub extension_id: Option<String>,
    pub custom_type: String,
    pub content: String,
    pub display: bool,
    pub details: Option<Value>,
    pub deliver_as: Option<ExtensionDeliverAs>,
    pub trigger_turn: bool,
}

#[derive(Debug, Clone)]
pub struct ExtensionSendUserMessage {
    pub extension_id: Option<String>,
    pub text: String,
    pub deliver_as: Option<ExtensionDeliverAs>,
}

#[derive(Debug, Clone)]
pub struct ExtensionAiCompletionRequest {
    pub model: Value,
    pub context: Value,
    pub options: Value,
    pub simple: bool,
}

#[async_trait]
pub trait ExtensionHostActions: Send + Sync {
    async fn send_message(&self, message: ExtensionSendMessage) -> Result<()>;
    async fn send_user_message(&self, message: ExtensionSendUserMessage) -> Result<()>;

    async fn complete_ai(&self, _request: ExtensionAiCompletionRequest) -> Result<Value> {
        Err(Error::extension(
            "@mariozechner/pi-ai completion host bridge is not configured".to_string(),
        ))
    }

    async fn list_ai_models(&self) -> Result<Value> {
        Err(Error::extension(
            "@mariozechner/pi-ai model registry host bridge is not configured".to_string(),
        ))
    }
}

mod compatibility;
mod event_coalescer_impl;
mod exec_mediation;
mod extension_manager_impl;
mod fs_connector;
mod permission_drift;
mod protocol;
use exec_mediation::{
    classify_credential_file_modification, classify_device_write, classify_disk_wipe,
    classify_fork_bomb, classify_permission_escalation, classify_pipe_to_shell,
    classify_process_termination, classify_recursive_delete, classify_reverse_shell,
    classify_system_shutdown, normalize_command_for_classification,
};
pub(crate) use protocol::validate_host_call;

/// Classify a command string into dangerous command classes.
///
/// Returns all matching classifications. A command may match multiple
/// classes (e.g., a reverse shell that also pipes to shell).
/// The classifier is deterministic and case-insensitive.
#[must_use]
pub fn classify_dangerous_command(cmd: &str, args: &[String]) -> Vec<DangerousCommandClass> {
    let mut classes = Vec::new();
    let full_cmd = if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{cmd} {}", args.join(" "))
    };
    let lower = normalize_command_for_classification(&full_cmd.to_ascii_lowercase());

    // --- Critical tier ---

    // Recursive delete targeting root or broad paths.
    if classify_recursive_delete(&lower) {
        classes.push(DangerousCommandClass::RecursiveDelete);
    }

    // Device-level writes.
    if classify_device_write(&lower) {
        classes.push(DangerousCommandClass::DeviceWrite);
    }

    // Fork bomb patterns.
    if classify_fork_bomb(&lower) {
        classes.push(DangerousCommandClass::ForkBomb);
    }

    // Disk wipe.
    if classify_disk_wipe(&lower) {
        classes.push(DangerousCommandClass::DiskWipe);
    }

    // Reverse shell.
    if classify_reverse_shell(&lower) {
        classes.push(DangerousCommandClass::ReverseShell);
    }

    // --- High tier ---

    // Pipe to shell.
    if classify_pipe_to_shell(&lower) {
        classes.push(DangerousCommandClass::PipeToShell);
    }

    // System shutdown.
    if classify_system_shutdown(&lower) {
        classes.push(DangerousCommandClass::SystemShutdown);
    }

    // Permission escalation.
    if classify_permission_escalation(&lower) {
        classes.push(DangerousCommandClass::PermissionEscalation);
    }

    // Process termination of critical processes.
    if classify_process_termination(&lower) {
        classes.push(DangerousCommandClass::ProcessTermination);
    }

    // Credential file modification.
    if classify_credential_file_modification(&lower) {
        classes.push(DangerousCommandClass::CredentialFileModification);
    }

    classes
}

/// Evaluate exec mediation policy for a command.
///
/// Called after capability-level policy allows exec, but before spawning.
/// Returns [`ExecMediationResult`] indicating whether the command should
/// proceed, be audited, or be denied.
#[must_use]
pub fn evaluate_exec_mediation(
    policy: &ExecMediationPolicy,
    cmd: &str,
    args: &[String],
) -> ExecMediationResult {
    if !policy.enabled {
        return ExecMediationResult::Allow;
    }

    let full_cmd = if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{cmd} {}", args.join(" "))
    };
    let lower = full_cmd.to_ascii_lowercase();

    // 1. Check explicit allow patterns (highest precedence override).
    for pattern in &policy.allow_patterns {
        if lower.starts_with(&pattern.to_ascii_lowercase()) {
            return ExecMediationResult::Allow;
        }
    }

    // 2. Check explicit deny patterns.
    for pattern in &policy.deny_patterns {
        if lower.starts_with(&pattern.to_ascii_lowercase()) {
            return ExecMediationResult::Deny {
                class: None,
                reason: format!("Command matches deny pattern: {pattern}"),
            };
        }
    }

    // 3. Classify via built-in rules.
    let classes = classify_dangerous_command(cmd, args);
    if classes.is_empty() {
        return ExecMediationResult::Allow;
    }

    // Find the highest-risk classification.
    let worst = classes
        .iter()
        .max_by_key(|c| c.risk_tier())
        .copied()
        .expect("classes is non-empty");

    if worst.risk_tier() >= policy.deny_threshold {
        ExecMediationResult::Deny {
            class: Some(worst),
            reason: format!(
                "Command classified as {} ({})",
                worst.label(),
                worst.risk_tier().label()
            ),
        }
    } else if policy.audit_all_classified {
        ExecMediationResult::AllowWithAudit {
            class: worst,
            reason: format!(
                "Command classified as {} ({}) — below deny threshold",
                worst.label(),
                worst.risk_tier().label()
            ),
        }
    } else {
        ExecMediationResult::Allow
    }
}

/// Redact secrets in a command string for safe logging.
///
/// Scans for patterns like `KEY=value` and replaces the value portion
/// for any key that matches the secret broker policy. Also redacts
/// inline `-p password` or `--password password` style arguments.
#[must_use]
pub fn redact_command_for_logging(policy: &SecretBrokerPolicy, cmd: &str) -> String {
    static PASSWORD_RE: OnceLock<Regex> = OnceLock::new();
    static ENV_RE: OnceLock<Regex> = OnceLock::new();

    if !policy.enabled {
        return cmd.to_string();
    }

    // 1. Redact -p/--password arguments
    // Handles: -p password, -p 'pass word', --password  =password
    let mut redacted = cmd.to_string();
    let password_regex = PASSWORD_RE.get_or_init(|| {
        Regex::new(r#"(?i)(--password|-p)(\s+|=)(?:'(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*"|[^\s]+)"#)
            .expect("regex")
    });
    redacted = password_regex
        .replace_all(&redacted, |caps: &regex::Captures| {
            let flag = &caps[1];
            let sep = &caps[2];
            format!("{flag}{sep}{}", policy.redaction_placeholder)
        })
        .to_string();

    // 2. Redact KEY=VALUE patterns
    // Handles: KEY=value, KEY='value with spaces', KEY="value with spaces"
    let env_regex = ENV_RE.get_or_init(|| {
        Regex::new(
            r#"([A-Za-z_][A-Za-z0-9_]*)=((?:'(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*"|[^\s'"]+)+)"#,
        )
        .expect("regex")
    });
    redacted = env_regex
        .replace_all(&redacted, |caps: &regex::Captures| {
            let key = &caps[1];
            let val = &caps[2];
            if policy.is_secret(key) {
                format!("{key}={}", policy.redaction_placeholder)
            } else {
                // Return original match unchanged
                format!("{key}={val}")
            }
        })
        .to_string();

    redacted
}

/// Compute SHA-256 hex digest of a string.
///
/// Used by SEC-4.3 ledger recording to hash command strings and env var names
/// without exposing raw values in telemetry.
#[must_use]
pub fn sha256_hex_standalone(input: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    format!("{digest:x}")
}
use permission_drift::{
    PermissionDriftFlags, capability_expansion_missing_provenance, capability_set_has_dangerous,
    checksum_mismatch, permission_drift_classes, permission_drift_provenance_status,
    permission_drift_recommended_action, permission_drift_risk_level, permission_drift_verdict,
    policy_profile_mismatch, primary_permission_drift_class, snapshot_capability_set,
    snapshot_catalog_capability_set, snapshot_provenance_map,
};

/// Detect permission drift between extension launch snapshots.
#[must_use]
pub fn detect_extension_permission_drift(
    previous: &ExtensionPermissionSnapshot,
    current: &ExtensionPermissionSnapshot,
) -> ExtensionPermissionDriftReport {
    let previous_capabilities = snapshot_capability_set(previous);
    let current_capabilities = snapshot_capability_set(current);
    let catalog_capabilities = snapshot_catalog_capability_set(current);
    let previous_provenance = snapshot_provenance_map(previous);
    let current_provenance = snapshot_provenance_map(current);

    let added_capabilities = current_capabilities
        .difference(&previous_capabilities)
        .cloned()
        .collect::<BTreeSet<_>>();
    let removed_capabilities = previous_capabilities
        .difference(&current_capabilities)
        .cloned()
        .collect::<BTreeSet<_>>();
    let catalog_missing_capabilities = catalog_capabilities
        .difference(&current_capabilities)
        .cloned()
        .collect::<BTreeSet<_>>();
    let manifest_has_catalog_gap =
        !catalog_capabilities.is_empty() && !catalog_missing_capabilities.is_empty();

    let has_added_dangerous = capability_set_has_dangerous(&added_capabilities);
    let has_added = !added_capabilities.is_empty();
    let has_removed = !removed_capabilities.is_empty();
    let missing_provenance = has_added
        && capability_expansion_missing_provenance(&added_capabilities, &current_provenance);
    let policy_mismatch = policy_profile_mismatch(previous, current, &added_capabilities);
    let manifest_checksum_mismatch = checksum_mismatch(
        current.manifest_checksum.as_ref(),
        current.catalog_manifest_checksum.as_ref(),
    );
    let provenance_checksum_mismatch = checksum_mismatch(
        current.provenance_snapshot_checksum.as_ref(),
        current.catalog_provenance_checksum.as_ref(),
    );
    let shared_provenance_mismatch = current_provenance.iter().any(|(capability, current)| {
        previous_provenance
            .get(capability)
            .is_some_and(|previous| previous != current)
    });
    let provenance_mismatch = provenance_checksum_mismatch || shared_provenance_mismatch;
    let trusted_change = current.trust == ExtensionPermissionTrust::ExplicitlyTrusted
        && (has_added
            || has_removed
            || policy_mismatch
            || manifest_checksum_mismatch
            || provenance_mismatch);

    let flags = PermissionDriftFlags {
        has_added_dangerous,
        has_added,
        has_removed,
        missing_provenance,
        policy_mismatch,
        manifest_stale: manifest_checksum_mismatch || manifest_has_catalog_gap,
        provenance_mismatch,
        provenance_empty: current_provenance.is_empty(),
        trusted_change,
    };

    let classes = permission_drift_classes(flags);
    let primary_class = primary_permission_drift_class(&classes);
    let provenance_status = permission_drift_provenance_status(flags);
    let verdict = permission_drift_verdict(flags);
    let risk_level = permission_drift_risk_level(verdict, flags);

    ExtensionPermissionDriftReport {
        extension_id: if current.extension_id.trim().is_empty() {
            previous.extension_id.clone()
        } else {
            current.extension_id.clone()
        },
        previous_capabilities,
        current_capabilities,
        added_capabilities,
        removed_capabilities,
        drift_class: primary_class,
        drift_classes: classes,
        risk_level,
        provenance_status,
        recommended_action: permission_drift_recommended_action(verdict, primary_class).to_string(),
        verdict,
    }
}

/// Detect permission drift and return the stable `JSON` evidence value.
pub fn detect_extension_permission_drift_json(
    previous: &ExtensionPermissionSnapshot,
    current: &ExtensionPermissionSnapshot,
) -> Result<Value> {
    serde_json::to_value(detect_extension_permission_drift(previous, current))
        .map_err(|err| Error::validation(format!("Serialize permission drift report: {err}")))
}
#[cfg(feature = "wasm-host")]
use protocol::validate_register;
#[cfg(test)]
use protocol::{
    HOSTCALL_MARSHALLING_FALLBACK_OPCODE_SHAPE_MISS, HOSTCALL_MARSHALLING_PATH_CANONICAL_FALLBACK,
    HOSTCALL_MARSHALLING_PATH_FAST_OPCODE, HOSTCALL_REACTOR_DEFAULT_LANE_CAPACITY,
    HOSTCALL_REWRITE_RULE_FAST_OPCODE_FUSION, parse_common_hostcall_opcode_code,
    reset_hostcall_superinstruction_state_for_tests, superinstruction_test_lock,
    validate_host_result,
};
use protocol::{
    HOSTCALL_MARSHALLING_PATH_CANONICAL_GENERIC, HostcallDispatchLane, HostcallLaneExecution,
    HostcallMarshallingArtifacts, HostcallMarshallingTelemetry, HostcallOpcodeResolution,
    HostcallPayloadArena, HostcallSpscLane, apply_hostcall_lane_kill_switch,
    host_call_error_code_str, hostcall_capability_class_from_capability,
    hostcall_io_uring_context_for_request, hostcall_opcode_context_for_params,
    merge_hostcall_context, params_without_key, parse_error_code, parse_session_opcode_atom,
    resolve_hostcall_opcode, select_hostcall_lane, validate_capability_manifest, validate_log,
    with_folded_ascii_alnum_token,
};

/// Convert a [`HostcallRequest`] (JS-origin) into the canonical [`HostCallPayload`].
///
/// The canonical params shapes are:
/// - `tool`:  `{ "name": <tool_name>, "input": <payload> }`
/// - `exec`:  `{ "cmd": <string>, ...payload_fields }`
/// - `http`:  payload passthrough
/// - `session/ui/events`:  `{ "op": <string>, ...payload_fields }`
pub fn hostcall_request_to_payload(request: &HostcallRequest) -> HostCallPayload {
    let method = request.method().to_string();
    let capability = request.required_capability().to_string();
    let params = request.params_for_hash();
    let timeout_ms = js_hostcall_timeout_ms(request);
    let context = merge_hostcall_context(
        hostcall_opcode_context_for_params(&method, &params),
        hostcall_io_uring_context_for_request(request),
    );

    HostCallPayload {
        call_id: request.call_id.clone(),
        capability,
        method,
        params,
        timeout_ms,
        cancel_token: None,
        context,
    }
}

/// Convert a [`HostResultPayload`] into the JS-facing [`HostcallOutcome`].
pub fn host_result_to_outcome(result: HostResultPayload) -> HostcallOutcome {
    if let Some(chunk_info) = result.chunk {
        return HostcallOutcome::StreamChunk {
            sequence: chunk_info.index,
            chunk: result.output,
            is_final: chunk_info.is_last,
        };
    }
    if result.is_error {
        let code = result
            .error
            .as_ref()
            .map_or("internal", |e| host_call_error_code_str(e.code));
        let message = result
            .error
            .as_ref()
            .map_or_else(|| "Unknown error".to_string(), |e| e.message.clone());
        HostcallOutcome::Error {
            code: code.to_string(),
            message,
        }
    } else {
        HostcallOutcome::Success(result.output)
    }
}

/// Convert a [`HostcallOutcome`] into a [`HostResultPayload`].
pub fn outcome_to_host_result(call_id: &str, outcome: &HostcallOutcome) -> HostResultPayload {
    match outcome {
        HostcallOutcome::Success(output) => HostResultPayload {
            call_id: call_id.to_string(),
            output: output.clone(),
            is_error: false,
            error: None,
            chunk: None,
        },
        HostcallOutcome::Error { code, message } => HostResultPayload {
            call_id: call_id.to_string(),
            output: json!({}),
            is_error: true,
            error: Some(HostCallError {
                code: parse_error_code(code),
                message: message.clone(),
                details: None,
                retryable: None,
            }),
            chunk: None,
        },
        HostcallOutcome::StreamChunk {
            sequence,
            chunk,
            is_final,
        } => HostResultPayload {
            call_id: call_id.to_string(),
            output: chunk.clone(),
            is_error: false,
            error: None,
            chunk: Some(HostStreamChunk {
                index: *sequence,
                is_last: *is_final,
                backpressure: None,
            }),
        },
    }
}

struct CancelGuard(Arc<std::sync::atomic::AtomicBool>);
impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn extension_wait_now() -> asupersync::types::Time {
    Cx::current()
        .and_then(|current| current.timer_driver())
        .map_or_else(wall_now, |driver| driver.now())
}

fn extension_wait_sleep(duration: Duration) -> asupersync::time::Sleep {
    sleep(extension_wait_now(), duration)
}

fn extension_wait_short_blocking_pause(duration: Duration) {
    std::thread::park_timeout(duration.min(Duration::from_millis(1)));
}

/// Canonicalize a path, stripping the `\\?\` verbatim prefix on Windows.
///
/// `std::fs::canonicalize` on Windows returns extended-length paths (`\\?\C:\...`)
/// which break QuickJS module resolution and JS string interpolation. This helper
/// strips that prefix so paths remain compatible with downstream consumers.
///
/// If `canonicalize` fails (e.g. path does not exist), this falls back to logical
/// normalization (`normalize_dot_segments`) of the absolute path to prevent
/// directory traversal exploits in security checks.
pub fn safe_canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).map_or_else(
        |_| {
            // Fallback for non-existent paths:
            // 1. Resolve to an absolute logical path.
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(path)
            };

            // 2. Try to anchor on the longest existing ancestor to respect symlinks.
            //    If we are in `/link/new_file` and `/link` -> `/target`, we want
            //    to resolve to `/target/new_file` to match the root resolution.
            for ancestor in absolute.ancestors().skip(1) {
                if let Ok(canonical_ancestor) = std::fs::canonicalize(ancestor)
                    && let Ok(suffix) = absolute.strip_prefix(ancestor)
                {
                    let combined = canonical_ancestor.join(suffix);
                    // Normalize handles any `..` in the suffix.
                    return strip_unc_prefix(normalize_dot_segments(&combined));
                }
            }

            // 3. Last resort: purely logical normalization.
            strip_unc_prefix(normalize_dot_segments(&absolute))
        },
        strip_unc_prefix,
    )
}

fn normalize_dot_segments(path: &Path) -> PathBuf {
    use std::ffi::{OsStr, OsString};
    use std::path::Component;

    let mut out = PathBuf::new();
    let mut normals: Vec<OsString> = Vec::new();
    let mut has_prefix = false;
    let mut has_root = false;

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                out.push(prefix.as_os_str());
                has_prefix = true;
            }
            Component::RootDir => {
                out.push(component.as_os_str());
                has_root = true;
            }
            Component::CurDir => {}
            Component::ParentDir => match normals.last() {
                Some(last) if last.as_os_str() != OsStr::new("..") => {
                    normals.pop();
                }
                _ => {
                    if !has_root && !has_prefix {
                        normals.push(OsString::from(".."));
                    }
                }
            },
            Component::Normal(part) => normals.push(part.to_os_string()),
        }
    }

    for part in normals {
        out.push(part);
    }

    out
}

/// Strip the `\\?\` or `//?/` verbatim prefix from a path on Windows. No-op on Unix.
#[allow(clippy::missing_const_for_fn)]
pub fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            if let Some(unc) = stripped.strip_prefix("UNC") {
                if unc.starts_with('\\') {
                    return PathBuf::from(format!(r"\{}", unc));
                }
            }
            return PathBuf::from(stripped);
        }
        // fd normalises separators to `/`, producing `//?/` instead of `\\?\`.
        if let Some(stripped) = s.strip_prefix("//?/") {
            if let Some(unc) = stripped.strip_prefix("UNC") {
                if unc.starts_with('/') {
                    return PathBuf::from(format!("/{}", unc));
                }
            }
            return PathBuf::from(stripped);
        }
    }
    path
}

/// Write JSON with sorted object keys directly into `out`, avoiding an
/// intermediate `serde_json::Value` tree.  Produces output identical to
/// `serde_json::to_string(&canonicalize_json(value))`.
// Retained as the string-emitting oracle for canonical-hash test/debug paths;
// the hot runtime path hashes directly through `hash_canonical_json_depth`.
#[allow(dead_code)]
fn write_canonical_json(value: &Value, out: &mut String) {
    use std::fmt::Write as _;
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let _ = write!(out, "{n}");
        }
        Value::String(s) => {
            write_json_escaped_str(s, out);
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            let mut first = true;
            for key in keys {
                if let Some(v) = map.get(key) {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    write_json_escaped_str(key, out);
                    out.push(':');
                    write_canonical_json(v, out);
                }
            }
            out.push('}');
        }
    }
}

/// Write a JSON-escaped string (with quotes) to `out`.  Uses a fast path for
/// ASCII strings that need no escaping (common for object keys and method
/// names), falling back to `serde_json::to_string` only when necessary.
#[allow(dead_code)]
fn write_json_escaped_str(s: &str, out: &mut String) {
    // Fast path: pure ASCII with no chars that require JSON escaping.
    if s.bytes().all(|b| b >= 0x20 && b != b'"' && b != b'\\') {
        out.reserve(s.len() + 2);
        out.push('"');
        out.push_str(s);
        out.push('"');
    } else {
        let escaped = serde_json::to_string(s).expect("string serialization");
        out.push_str(&escaped);
    }
}

/// Feed canonical JSON with sorted object keys directly into a SHA-256 hasher,
/// bypassing the intermediate `String` buffer entirely.
pub(crate) fn hash_canonical_json(value: &Value, hasher: &mut sha2::Sha256) {
    hash_canonical_json_depth(value, hasher, 0);
}

fn hash_canonical_json_depth(value: &Value, hasher: &mut sha2::Sha256, depth: usize) {
    if depth > 128 {
        hasher.update(b"too_deep");
        return;
    }

    match value {
        Value::Null => hasher.update(b"null"),
        Value::Bool(b) => hasher.update(if *b { &b"true"[..] } else { &b"false"[..] }),
        Value::Number(n) => {
            // Numbers are short — write to a small stack buffer.
            let mut buf = String::with_capacity(24);
            let _ = write!(buf, "{n}");
            hasher.update(buf.as_bytes());
        }
        Value::String(s) => {
            hash_json_escaped_str(s, hasher);
        }
        Value::Array(items) => {
            hasher.update(b"[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    hasher.update(b",");
                }
                hash_canonical_json_depth(item, hasher, depth + 1);
            }
            hasher.update(b"]");
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            hasher.update(b"{");
            let mut first = true;
            for key in keys {
                if let Some(v) = map.get(key) {
                    if !first {
                        hasher.update(b",");
                    }
                    first = false;
                    hash_json_escaped_str(key, hasher);
                    hasher.update(b":");
                    hash_canonical_json_depth(v, hasher, depth + 1);
                }
            }
            hasher.update(b"}");
        }
    }
}

/// Feed a JSON-escaped string (with quotes) directly into a SHA-256 hasher.
pub(crate) fn hash_json_escaped_str(s: &str, hasher: &mut sha2::Sha256) {
    use sha2::Digest as _;
    if s.bytes().all(|b| b >= 0x20 && b != b'"' && b != b'\\') {
        hasher.update(b"\"");
        hasher.update(s.as_bytes());
        hasher.update(b"\"");
    } else {
        let escaped = serde_json::to_string(s).expect("string serialization");
        hasher.update(escaped.as_bytes());
    }
}

/// Convert a SHA-256 digest to a lowercase hex string using a lookup table.
pub(crate) fn sha256_to_hex(digest: &[u8]) -> String {
    const HEX: [u8; 16] = *b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for &b in digest {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

pub(crate) fn hostcall_params_hash(method: &str, params: &Value) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hash_hostcall_envelope(method, br#","params":"#, &mut hasher, |h| {
        hash_canonical_json(params, h);
    });
    sha256_to_hex(hasher.finalize().as_slice())
}

/// Feed the *shape* of a JSON value into the hasher, replacing leaves with
/// type tags ("string", "number", etc.) without allocating an intermediate
/// `Value` tree.
fn hash_canonical_shape(value: &Value, hasher: &mut sha2::Sha256) {
    hash_canonical_shape_depth(value, hasher, 0);
}

fn hash_canonical_shape_depth(value: &Value, hasher: &mut sha2::Sha256, depth: usize) {
    if depth > 128 {
        hasher.update(b"too_deep");
        return;
    }

    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            hasher.update(b"{");
            let mut first = true;
            for key in keys {
                if let Some(v) = map.get(key) {
                    if !first {
                        hasher.update(b",");
                    }
                    first = false;
                    hash_json_escaped_str(key, hasher);
                    hasher.update(b":");
                    hash_canonical_shape_depth(v, hasher, depth + 1);
                }
            }
            hasher.update(b"}");
        }
        Value::Array(items) => {
            hasher.update(b"[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    hasher.update(b",");
                }
                hash_canonical_shape_depth(item, hasher, depth + 1);
            }
            hasher.update(b"]");
        }
        Value::String(_) => hasher.update(br#""string""#),
        Value::Number(_) => hasher.update(br#""number""#),
        Value::Bool(_) => hasher.update(br#""bool""#),
        Value::Null => hasher.update(br#""null""#),
    }
}

fn hostcall_params_shape_hash(method: &str, params: &Value) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hash_hostcall_envelope(method, br#","params_shape":"#, &mut hasher, |h| {
        hash_canonical_shape(params, h);
    });
    sha256_to_hex(hasher.finalize().as_slice())
}

/// Hash the canonical `{"method": ..., "<payload_key>": ...}` envelope using
/// the exact byte layout expected by historical hostcall hash artifacts.
fn hash_hostcall_envelope(
    method: &str,
    payload_key_prefix: &[u8],
    hasher: &mut sha2::Sha256,
    payload_writer: impl FnOnce(&mut sha2::Sha256),
) {
    use sha2::Digest as _;
    hasher.update(br#"{"method":"#);
    hash_json_escaped_str(method, hasher);
    hasher.update(payload_key_prefix);
    payload_writer(hasher);
    hasher.update(b"}");
}

pub const PROTOCOL_VERSION: &str = "1.0";
pub const LOG_SCHEMA_VERSION: &str = "pi.ext.log.v1";
pub const COMPAT_LEDGER_SCHEMA_VERSION: &str = "pi.ext.compat_ledger.v1";
pub const RUNTIME_RISK_LEDGER_SCHEMA_VERSION: &str = "pi.ext.runtime_risk_ledger.v1";
pub const RUNTIME_RISK_REPLAY_SCHEMA_VERSION: &str = "pi.ext.runtime_risk_replay.v1";
pub const RUNTIME_RISK_CALIBRATION_SCHEMA_VERSION: &str = "pi.ext.runtime_risk_calibration.v1";
pub const ADAPTIVE_HOSTCALL_POLICY_DIFF_SCHEMA_VERSION: &str =
    "pi.ext.adaptive_hostcall_policy_diff.v1";
pub const RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION: &str = "pi.ext.hostcall_telemetry.v1";
pub const RUNTIME_HOSTCALL_FEATURE_SCHEMA_VERSION: &str = "pi.ext.hostcall_feature_vector.v1";
pub const RUNTIME_HOSTCALL_FEATURE_BUDGET_US: u64 = 250;
pub const RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION: &str = "pi.ext.runtime_risk_explanation.v1";
pub const RUNTIME_RISK_EXPLANATION_TERM_BUDGET: usize = 12;
// Keep runtime fallback deterministic under normal CI/workstation variance.
pub const RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS: u64 = 25;
pub const RUNTIME_RISK_BASELINE_SCHEMA_VERSION: &str = "pi.ext.runtime_risk_baseline.v1";
pub const SECURITY_ALERT_SCHEMA_VERSION: &str = "pi.ext.security_alert.v1";
pub const INCIDENT_EVIDENCE_BUNDLE_SCHEMA_VERSION: &str = "pi.ext.incident_evidence_bundle.v1";
const RUNTIME_HOSTCALL_SEQUENCE_WINDOW: usize = 64;
const CAPABILITY_MANIFEST_SCHEMA_V1: &str = "pi.ext.cap.v1";
const CAPABILITY_MANIFEST_SCHEMA_V2: &str = "pi.ext.cap.v2";

fn runtime_risk_explanation_schema_default() -> String {
    RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION.to_string()
}

// ============================================================================
// Compatibility Scanner (bd-3bs)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatEvidence {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub snippet: String,
}

impl CompatEvidence {
    #[must_use]
    pub const fn new(file: String, line: usize, column: usize, snippet: String) -> Self {
        Self {
            file,
            line,
            column,
            snippet,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatCapabilityEvidence {
    pub capability: String,
    pub reason: String,
    pub evidence: Vec<CompatEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatRewriteEvidence {
    pub from: String,
    pub to: String,
    pub evidence: Vec<CompatEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatIssueEvidence {
    pub rule: String,
    pub message: String,
    pub evidence: Vec<CompatEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatLedger {
    pub schema: String,
    pub capabilities: Vec<CompatCapabilityEvidence>,
    pub rewrites: Vec<CompatRewriteEvidence>,
    pub forbidden: Vec<CompatIssueEvidence>,
    pub flagged: Vec<CompatIssueEvidence>,
}

impl CompatLedger {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema: COMPAT_LEDGER_SCHEMA_VERSION.to_string(),
            capabilities: Vec::new(),
            rewrites: Vec::new(),
            forbidden: Vec::new(),
            flagged: Vec::new(),
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
            && self.rewrites.is_empty()
            && self.forbidden.is_empty()
            && self.flagged.is_empty()
    }

    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

#[derive(Debug, Clone)]
pub struct CompatibilityScanner {
    root: PathBuf,
}

// ============================================================================
// Policy
// ============================================================================

// ---------------------------------------------------------------------------
// Capability taxonomy
// ---------------------------------------------------------------------------

/// Enumeration of all recognised extension capabilities.
///
/// Each variant maps 1-to-1 with a string token used in policy configuration
/// (e.g. `"read"`, `"exec"`). The canonical string is the
/// `#[serde(rename_all = "snake_case")]` form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Read files and directories.
    Read,
    /// Write / create / delete files and directories.
    Write,
    /// Outbound HTTP requests.
    Http,
    /// Subscribe to and emit lifecycle events.
    Events,
    /// Access session state (messages, model, labels, etc.).
    Session,
    /// UI operations (status, widgets, notifications).
    Ui,
    /// Execute shell commands (dangerous).
    Exec,
    /// Read environment variables (dangerous — may leak secrets).
    Env,
    /// Generic tool invocation.
    Tool,
    /// Logging (always allowed, included for completeness).
    Log,
}

/// All known capabilities in definition order.
pub const ALL_CAPABILITIES: &[Capability] = &[
    Capability::Read,
    Capability::Write,
    Capability::Http,
    Capability::Events,
    Capability::Session,
    Capability::Ui,
    Capability::Exec,
    Capability::Env,
    Capability::Tool,
    Capability::Log,
];

impl Capability {
    /// Parse a string token into a [`Capability`], case-insensitive.
    /// Returns `None` for unrecognised tokens.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "http" => Some(Self::Http),
            "events" => Some(Self::Events),
            "session" => Some(Self::Session),
            "ui" => Some(Self::Ui),
            "exec" => Some(Self::Exec),
            "env" => Some(Self::Env),
            "tool" => Some(Self::Tool),
            "log" => Some(Self::Log),
            _ => None,
        }
    }

    /// Canonical string token (matches serde rename).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Http => "http",
            Self::Events => "events",
            Self::Session => "session",
            Self::Ui => "ui",
            Self::Exec => "exec",
            Self::Env => "env",
            Self::Tool => "tool",
            Self::Log => "log",
        }
    }

    /// Whether this capability is classified as *dangerous*.
    ///
    /// Dangerous capabilities default to Deny in Strict/Prompt modes and
    /// require explicit opt-in or user confirmation.
    pub const fn is_dangerous(self) -> bool {
        matches!(self, Self::Exec | Self::Env)
    }

    /// List of all dangerous capabilities.
    pub const fn dangerous_list() -> &'static [Self] {
        &[Self::Exec, Self::Env]
    }

    /// Ordinal index for array-based snapshot lookups.
    pub const fn index(self) -> usize {
        match self {
            Self::Read => 0,
            Self::Write => 1,
            Self::Http => 2,
            Self::Events => 3,
            Self::Session => 4,
            Self::Ui => 5,
            Self::Exec => 6,
            Self::Env => 7,
            Self::Tool => 8,
            Self::Log => 9,
        }
    }
}

/// Number of known capabilities (must match [`ALL_CAPABILITIES`] length).
pub const NUM_CAPABILITIES: usize = 10;

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Policy profile presets
// ---------------------------------------------------------------------------

/// Named policy profiles providing curated defaults.
///
/// Profiles are convenience constructors for [`ExtensionPolicy`] — once
/// constructed the policy is fully mutable and can be further customised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyProfile {
    /// Safe defaults: only non-dangerous capabilities allowed, dangerous
    /// denied. Mode = Strict.
    Safe,
    /// Standard defaults (current production behaviour): non-dangerous
    /// allowed, dangerous prompt. Mode = Prompt.
    Standard,
    /// Everything allowed, nothing denied. Mode = Permissive.
    Permissive,
}

impl PolicyProfile {
    /// Expand this profile into a concrete [`ExtensionPolicy`].
    pub fn to_policy(self) -> ExtensionPolicy {
        match self {
            Self::Safe => ExtensionPolicy {
                mode: ExtensionPolicyMode::Strict,
                max_memory_mb: 256,
                default_caps: vec![
                    "read".to_string(),
                    "write".to_string(),
                    "http".to_string(),
                    "events".to_string(),
                    "session".to_string(),
                ],
                deny_caps: vec!["exec".to_string(), "env".to_string()],
                per_extension: HashMap::new(),
                exec_mediation: ExecMediationPolicy::strict(),
                secret_broker: SecretBrokerPolicy::default(),
            },
            Self::Standard => ExtensionPolicy::default(),
            Self::Permissive => ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                per_extension: HashMap::new(),
                exec_mediation: ExecMediationPolicy::permissive(),
                secret_broker: SecretBrokerPolicy::default(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Per-extension overrides
// ---------------------------------------------------------------------------

/// Per-extension policy override.
///
/// When present for an extension ID, these fields take precedence over the
/// global policy fields at the corresponding layer in the precedence chain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionOverride {
    /// Mode override for this extension. `None` inherits the global mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ExtensionPolicyMode>,
    /// Additional capabilities to allow for this extension (merged with
    /// global `default_caps`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Additional capabilities to deny for this extension (merged with
    /// global `deny_caps`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Per-extension resource quota overrides (SEC-4.1).
    /// `None` inherits the global quota defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<ExtensionQuotaConfig>,
}

// ---------------------------------------------------------------------------
// Core policy types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionPolicyMode {
    Strict,
    Prompt,
    Permissive,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RepairPolicyMode {
    Off,
    Suggest,
    AutoSafe,
    AutoStrict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionPolicy {
    pub mode: ExtensionPolicyMode,
    pub max_memory_mb: u32,
    pub default_caps: Vec<String>,
    pub deny_caps: Vec<String>,
    /// Per-extension overrides keyed by extension ID.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub per_extension: HashMap<String, ExtensionOverride>,
    /// Exec mediation policy (SEC-4.3). Controls command-level allow/deny
    /// after capability-level exec is granted.
    #[serde(default)]
    pub exec_mediation: ExecMediationPolicy,
    /// Secret broker policy (SEC-4.3). Controls redaction of secret env vars
    /// and prevents raw disclosure when policy forbids it.
    #[serde(default)]
    pub secret_broker: SecretBrokerPolicy,
}

impl Default for ExtensionPolicy {
    fn default() -> Self {
        Self {
            mode: ExtensionPolicyMode::Prompt,
            max_memory_mb: 256,
            default_caps: vec![
                "read".to_string(),
                "write".to_string(),
                "http".to_string(),
                "events".to_string(),
                "session".to_string(),
            ],
            deny_caps: vec!["exec".to_string(), "env".to_string()],
            per_extension: HashMap::new(),
            exec_mediation: ExecMediationPolicy::default(),
            secret_broker: SecretBrokerPolicy::default(),
        }
    }
}

/// Deterministic runtime risk-controller settings for extension hostcalls.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RuntimeRiskConfig {
    /// Master switch for runtime risk decisions.
    pub enabled: bool,
    /// When `true`, risk decisions are enforced (deny/terminate block calls).
    /// When `false` (shadow mode), calls are scored and telemetry is recorded
    /// but enforcement actions are downgraded to `Allow` — letting the call
    /// proceed while capturing what action *would* have been taken.
    pub enforce: bool,
    /// Type-I error budget for sequential detection (0 < alpha < 1).
    pub alpha: f64,
    /// Sliding-window size for residual/drift checks.
    pub window_size: usize,
    /// Max in-memory entries retained in the risk evidence ledger.
    pub ledger_limit: usize,
    /// Max decision budget per hostcall (ms) before fallback action.
    pub decision_timeout_ms: u64,
    /// If true, controller failures/timeouts fail closed.
    pub fail_closed: bool,
}

impl Default for RuntimeRiskConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            enforce: true,
            alpha: 0.01,
            window_size: 128,
            ledger_limit: 2048,
            decision_timeout_ms: 50,
            fail_closed: true,
        }
    }
}

// ---------------------------------------------------------------------------
// SEC-7.2: Graduated enforcement rollout with rollback guards
// ---------------------------------------------------------------------------

/// Rollout phases for graduated enforcement. Operators progress through phases
/// to build confidence before full enforcement.
///
/// Phase ordering: `Shadow` → `LogOnly` → `EnforceNew` → `EnforceAll`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RolloutPhase {
    /// Risk scoring runs, telemetry recorded, but no enforcement actions
    /// taken. Equivalent to `enforce = false`.
    Shadow = 0,
    /// Risk decisions are logged with would-be actions but calls proceed.
    /// Operator can review logs before enabling enforcement.
    LogOnly = 1,
    /// Enforcement applies only to extensions loaded after the phase
    /// transition. Pre-existing extensions remain in log-only mode.
    EnforceNew = 2,
    /// Full enforcement for all extensions regardless of when they were
    /// loaded.
    EnforceAll = 3,
}

impl RolloutPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::LogOnly => "log_only",
            Self::EnforceNew => "enforce_new",
            Self::EnforceAll => "enforce_all",
        }
    }

    /// Whether this phase actually enforces (blocks) calls.
    pub const fn is_enforcing(self) -> bool {
        matches!(self, Self::EnforceNew | Self::EnforceAll)
    }
}

impl std::fmt::Display for RolloutPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Automatic rollback trigger conditions. When any condition is met, the
/// rollout automatically reverts to `Shadow` phase.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RollbackTrigger {
    /// Maximum allowed false-positive rate (blocked calls that should have
    /// been allowed) over the evaluation window. When exceeded, rollback
    /// fires.
    pub max_false_positive_rate: f64,
    /// Maximum allowed error rate (controller failures / total decisions)
    /// over the evaluation window.
    pub max_error_rate: f64,
    /// Evaluation window size in number of recent decisions.
    pub window_size: usize,
    /// Maximum detection latency in milliseconds. If the average decision
    /// latency in the window exceeds this, rollback fires.
    pub max_latency_ms: u64,
}

impl Default for RollbackTrigger {
    fn default() -> Self {
        Self {
            max_false_positive_rate: 0.05,
            max_error_rate: 0.10,
            window_size: 100,
            max_latency_ms: 200,
        }
    }
}

/// Snapshot of graduated rollout state for operator inspection (SEC-7.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RolloutState {
    /// Current rollout phase.
    pub phase: RolloutPhase,
    /// Whether the `RuntimeRiskConfig` enforce flag is active.
    pub enforce: bool,
    /// Whether the risk controller is enabled.
    pub enabled: bool,
    /// Timestamp (ms since epoch) of the last phase transition.
    pub last_transition_ms: i64,
    /// Number of phase transitions since system start.
    pub transition_count: u32,
    /// If a rollback occurred, the phase it rolled back from.
    pub rolled_back_from: Option<RolloutPhase>,
    /// Current evaluation window statistics for rollback triggers.
    pub window_stats: RollbackWindowStats,
}

/// Rolling statistics over the rollback evaluation window.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RollbackWindowStats {
    /// Total decisions evaluated in the current window.
    pub total_decisions: u64,
    /// Decisions where the risk controller returned an error.
    pub error_count: u64,
    /// Decisions flagged as false positives (operator-overridden denials).
    pub false_positive_count: u64,
    /// Average decision latency in milliseconds across the window.
    pub avg_latency_ms: f64,
}

/// Mutable rollout tracking state stored inside `ExtensionManagerInner`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutTracker {
    pub phase: RolloutPhase,
    pub last_transition_ms: i64,
    pub transition_count: u32,
    pub rolled_back_from: Option<RolloutPhase>,
    pub trigger: RollbackTrigger,
    /// Rolling window of recent decision outcomes for rollback evaluation.
    pub recent_decisions: VecDeque<RolloutDecisionSample>,
}

/// A single decision sample in the rollback evaluation window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutDecisionSample {
    pub ts_ms: i64,
    pub latency_ms: u64,
    pub was_error: bool,
    pub was_false_positive: bool,
}

impl Default for RolloutTracker {
    fn default() -> Self {
        Self {
            phase: RolloutPhase::EnforceAll,
            last_transition_ms: runtime_risk_now_ms(),
            transition_count: 0,
            rolled_back_from: None,
            trigger: RollbackTrigger::default(),
            recent_decisions: VecDeque::new(),
        }
    }
}

impl RolloutTracker {
    /// Create a tracker starting in the given phase.
    pub fn new(phase: RolloutPhase) -> Self {
        Self {
            phase,
            ..Self::default()
        }
    }

    /// Advance to the next phase. Returns `true` if the phase changed.
    pub fn advance(&mut self) -> bool {
        let next = match self.phase {
            RolloutPhase::Shadow => RolloutPhase::LogOnly,
            RolloutPhase::LogOnly => RolloutPhase::EnforceNew,
            RolloutPhase::EnforceNew => RolloutPhase::EnforceAll,
            RolloutPhase::EnforceAll => return false,
        };
        self.phase = next;
        self.last_transition_ms = runtime_risk_now_ms();
        self.transition_count = self.transition_count.saturating_add(1);
        self.rolled_back_from = None;
        true
    }

    /// Roll back to `Shadow` phase, recording what phase we rolled back from.
    pub fn rollback(&mut self) {
        if self.phase != RolloutPhase::Shadow {
            self.rolled_back_from = Some(self.phase);
            self.phase = RolloutPhase::Shadow;
            self.last_transition_ms = runtime_risk_now_ms();
            self.transition_count = self.transition_count.saturating_add(1);
        }
    }

    /// Set an explicit phase (for operator override).
    pub fn set_phase(&mut self, phase: RolloutPhase) {
        if self.phase != phase {
            self.rolled_back_from = None;
            self.phase = phase;
            self.last_transition_ms = runtime_risk_now_ms();
            self.transition_count = self.transition_count.saturating_add(1);
        }
    }

    /// Record a decision sample and check rollback triggers.
    /// Returns `true` if a rollback was triggered.
    pub fn record_decision(
        &mut self,
        latency_ms: u64,
        was_error: bool,
        was_false_positive: bool,
    ) -> bool {
        let sample = RolloutDecisionSample {
            ts_ms: runtime_risk_now_ms(),
            latency_ms,
            was_error,
            was_false_positive,
        };
        self.recent_decisions.push_back(sample);
        while self.recent_decisions.len() > self.trigger.window_size {
            let _ = self.recent_decisions.pop_front();
        }
        self.check_triggers()
    }

    /// Evaluate rollback trigger conditions against the current window.
    #[allow(clippy::cast_precision_loss)]
    fn check_triggers(&mut self) -> bool {
        // Only check triggers when actually enforcing.
        if !self.phase.is_enforcing() {
            return false;
        }
        let n = self.recent_decisions.len();
        if n < 10 {
            // Not enough data to evaluate triggers.
            return false;
        }
        let stats = self.window_stats();
        let n_f64 = stats.total_decisions as f64;
        let fp_rate = stats.false_positive_count as f64 / n_f64;
        let err_rate = stats.error_count as f64 / n_f64;

        let should_rollback = fp_rate > self.trigger.max_false_positive_rate
            || err_rate > self.trigger.max_error_rate
            || stats.avg_latency_ms > self.trigger.max_latency_ms as f64;

        if should_rollback {
            self.rollback();
        }
        should_rollback
    }

    /// Compute window statistics for the current evaluation window.
    #[allow(clippy::cast_precision_loss)]
    pub fn window_stats(&self) -> RollbackWindowStats {
        let n = self.recent_decisions.len() as u64;
        if n == 0 {
            return RollbackWindowStats::default();
        }
        let mut errors = 0u64;
        let mut fps = 0u64;
        let mut total_lat = 0u64;
        for s in &self.recent_decisions {
            if s.was_error {
                errors += 1;
            }
            if s.was_false_positive {
                fps += 1;
            }
            total_lat = total_lat.saturating_add(s.latency_ms);
        }
        RollbackWindowStats {
            total_decisions: n,
            error_count: errors,
            false_positive_count: fps,
            avg_latency_ms: total_lat as f64 / n as f64,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-extension resource quota engine (SEC-4.1 / bd-b1d7o)
// ---------------------------------------------------------------------------

/// Configurable per-extension resource quotas. When a quota is `None`, the
/// corresponding limit is not enforced. All values are per-extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionQuotaConfig {
    /// Maximum hostcalls permitted per 1-second sliding window.
    pub max_hostcalls_per_second: Option<u32>,
    /// Maximum hostcalls permitted per 60-second sliding window.
    pub max_hostcalls_per_minute: Option<u32>,
    /// Maximum total hostcalls before the extension is throttled.
    pub max_hostcalls_total: Option<u64>,
    /// Maximum concurrent subprocesses spawned via exec hostcalls.
    pub max_subprocesses: Option<u32>,
    /// Maximum cumulative bytes written via fs/write hostcalls.
    pub max_write_bytes: Option<u64>,
    /// Maximum cumulative HTTP requests issued via http hostcalls.
    pub max_http_requests: Option<u64>,
}

impl Default for ExtensionQuotaConfig {
    fn default() -> Self {
        Self::for_mode(ExtensionPolicyMode::Prompt)
    }
}

impl ExtensionQuotaConfig {
    /// Create quota defaults appropriate for a given policy mode.
    ///
    /// - **Strict**: restrictive burst/rate limits and low subprocess fan-out.
    /// - **Prompt**: moderate defaults (original baseline).
    /// - **Permissive**: relaxed limits for trusted extensions.
    #[must_use]
    pub const fn for_mode(mode: ExtensionPolicyMode) -> Self {
        match mode {
            ExtensionPolicyMode::Strict => Self {
                max_hostcalls_per_second: Some(20),
                max_hostcalls_per_minute: Some(500),
                max_hostcalls_total: Some(5_000),
                max_subprocesses: Some(4),
                max_write_bytes: Some(50 * 1024 * 1024), // 50 MB
                max_http_requests: Some(200),
            },
            ExtensionPolicyMode::Prompt => Self {
                max_hostcalls_per_second: Some(100),
                max_hostcalls_per_minute: Some(2_000),
                max_hostcalls_total: None,
                max_subprocesses: Some(8),
                max_write_bytes: None,
                max_http_requests: None,
            },
            ExtensionPolicyMode::Permissive => Self {
                max_hostcalls_per_second: Some(500),
                max_hostcalls_per_minute: Some(10_000),
                max_hostcalls_total: None,
                max_subprocesses: Some(32),
                max_write_bytes: None,
                max_http_requests: None,
            },
        }
    }
}

/// Workload tier presets for the hostcall budget controller.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionBudgetTier {
    Strict,
    Balanced,
    Throughput,
}

impl ExtensionBudgetTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Balanced => "balanced",
            Self::Throughput => "throughput",
        }
    }

    pub const fn from_policy_mode(mode: ExtensionPolicyMode) -> Self {
        match mode {
            ExtensionPolicyMode::Strict => Self::Strict,
            ExtensionPolicyMode::Prompt => Self::Balanced,
            ExtensionPolicyMode::Permissive => Self::Throughput,
        }
    }
}

/// Budget controller settings for expected-loss fallback routing.
///
/// The controller promotes an extension to compatibility-lane fallback after
/// repeated overload/anomaly signals within a bounded window and returns to
/// fast lane after a recovery streak.  Optionally augmented by CUSUM/BOCPD
/// regime-shift detection for statistically-justified early triggering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ExtensionBudgetControllerConfig {
    /// Master switch for automatic compatibility-lane fallback.
    pub enabled: bool,
    /// Workload tier used to derive operational defaults.
    pub tier: ExtensionBudgetTier,
    /// Rolling window for overload signals.
    pub overload_window_ms: u64,
    /// Number of overload signals needed to enter fallback mode.
    pub overload_signals_to_fallback: u32,
    /// Consecutive successful calls required to exit fallback mode.
    pub recovery_successes_to_exit: u32,
    /// CUSUM/BOCPD regime-shift detection configuration.
    pub regime_shift: RegimeShiftConfig,
    /// Conformal + PAC-Bayes safety envelope configuration.
    pub safety_envelope: SafetyEnvelopeConfig,
    /// Online convex optimization tuner for queue/batch/time-slice budgets.
    pub oco_tuner: OcoTunerConfig,
}

impl ExtensionBudgetControllerConfig {
    #[must_use]
    pub const fn for_tier(tier: ExtensionBudgetTier) -> Self {
        match tier {
            ExtensionBudgetTier::Strict => Self {
                enabled: true,
                tier,
                overload_window_ms: 3_000,
                overload_signals_to_fallback: 2,
                recovery_successes_to_exit: 8,
                regime_shift: RegimeShiftConfig::for_tier(ExtensionBudgetTier::Strict),
                safety_envelope: SafetyEnvelopeConfig::for_tier(ExtensionBudgetTier::Strict),
                oco_tuner: OcoTunerConfig::for_tier(ExtensionBudgetTier::Strict),
            },
            ExtensionBudgetTier::Balanced => Self {
                enabled: true,
                tier,
                overload_window_ms: 8_000,
                overload_signals_to_fallback: 3,
                recovery_successes_to_exit: 16,
                regime_shift: RegimeShiftConfig::for_tier(ExtensionBudgetTier::Balanced),
                safety_envelope: SafetyEnvelopeConfig::for_tier(ExtensionBudgetTier::Balanced),
                oco_tuner: OcoTunerConfig::for_tier(ExtensionBudgetTier::Balanced),
            },
            ExtensionBudgetTier::Throughput => Self {
                enabled: true,
                tier,
                overload_window_ms: 15_000,
                overload_signals_to_fallback: 5,
                recovery_successes_to_exit: 32,
                regime_shift: RegimeShiftConfig::for_tier(ExtensionBudgetTier::Throughput),
                safety_envelope: SafetyEnvelopeConfig::for_tier(ExtensionBudgetTier::Throughput),
                oco_tuner: OcoTunerConfig::for_tier(ExtensionBudgetTier::Throughput),
            },
        }
    }

    #[must_use]
    pub const fn for_policy_mode(mode: ExtensionPolicyMode) -> Self {
        Self::for_tier(ExtensionBudgetTier::from_policy_mode(mode))
    }
}

impl Default for ExtensionBudgetControllerConfig {
    fn default() -> Self {
        Self::for_tier(ExtensionBudgetTier::Balanced)
    }
}

/// OCO controller configuration for queue, batch, and time-slice budgets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
#[allow(clippy::struct_field_names)]
pub struct OcoTunerConfig {
    /// Master switch for online updates.
    pub enabled: bool,
    /// Step size for online gradient updates.
    pub learning_rate: f64,
    /// Minimum and maximum queue budget (logical slots).
    pub min_queue_budget: f64,
    pub max_queue_budget: f64,
    /// Minimum and maximum batch budget (logical dispatch width).
    pub min_batch_budget: f64,
    pub max_batch_budget: f64,
    /// Minimum and maximum time-slice budget (milliseconds).
    pub min_time_slice_ms: f64,
    pub max_time_slice_ms: f64,
    /// Initial values for each tuned budget.
    pub initial_queue_budget: f64,
    pub initial_batch_budget: f64,
    pub initial_time_slice_ms: f64,
    /// Guardrail threshold; instantaneous loss above this triggers rollback.
    pub rollback_loss_threshold: f64,
}

impl OcoTunerConfig {
    #[must_use]
    pub const fn for_tier(tier: ExtensionBudgetTier) -> Self {
        match tier {
            ExtensionBudgetTier::Strict => Self {
                enabled: true,
                learning_rate: 0.10,
                min_queue_budget: 2.0,
                max_queue_budget: 16.0,
                min_batch_budget: 1.0,
                max_batch_budget: 8.0,
                min_time_slice_ms: 2.0,
                max_time_slice_ms: 12.0,
                initial_queue_budget: 4.0,
                initial_batch_budget: 2.0,
                initial_time_slice_ms: 4.0,
                rollback_loss_threshold: 1.35,
            },
            ExtensionBudgetTier::Balanced => Self {
                enabled: true,
                learning_rate: 0.08,
                min_queue_budget: 4.0,
                max_queue_budget: 32.0,
                min_batch_budget: 2.0,
                max_batch_budget: 16.0,
                min_time_slice_ms: 4.0,
                max_time_slice_ms: 20.0,
                initial_queue_budget: 8.0,
                initial_batch_budget: 4.0,
                initial_time_slice_ms: 8.0,
                rollback_loss_threshold: 1.45,
            },
            ExtensionBudgetTier::Throughput => Self {
                enabled: true,
                learning_rate: 0.06,
                min_queue_budget: 8.0,
                max_queue_budget: 64.0,
                min_batch_budget: 4.0,
                max_batch_budget: 32.0,
                min_time_slice_ms: 6.0,
                max_time_slice_ms: 32.0,
                initial_queue_budget: 16.0,
                initial_batch_budget: 8.0,
                initial_time_slice_ms: 12.0,
                rollback_loss_threshold: 1.60,
            },
        }
    }
}

impl Default for OcoTunerConfig {
    fn default() -> Self {
        Self::for_tier(ExtensionBudgetTier::Balanced)
    }
}

/// Mutable per-extension quota counters, reset semantics:
/// - `hostcall_timestamps_ms` is a sliding window (entries expire by time).
/// - `hostcalls_total` and cumulative counters are monotonic (session-lifetime).
/// - `active_subprocesses` increments on spawn, decrements on exit.
#[derive(Debug, Clone, Default)]
struct ExtensionQuotaState {
    hostcall_timestamps_ms: VecDeque<i64>,
    hostcalls_total: u64,
    active_subprocesses: u32,
    write_bytes_total: u64,
    http_requests_total: u64,
}

/// Configuration for CUSUM/BOCPD regime-shift detection that augments the
/// simple sliding-window counting in the budget controller.
///
/// When enabled, the detector runs alongside the existing signal-count logic
/// and can trigger fallback *before* the count threshold is reached if a
/// statistically significant regime change is detected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RegimeShiftConfig {
    /// Master switch — when false the detector is a no-op and the budget
    /// controller falls back to pure signal counting.
    pub enabled: bool,

    /// CUSUM allowance parameter `k`.  Determines how much the observed
    /// inter-arrival rate may deviate from baseline before the cumulative
    /// sum accumulates.  Expressed in multiples of baseline sigma.
    /// Lower → more sensitive, higher → fewer false positives.
    pub cusum_k: f64,

    /// CUSUM decision threshold `h`.  When the cumulative sum exceeds this
    /// value a regime shift is declared.  Expressed in multiples of baseline
    /// sigma.
    pub cusum_h: f64,

    /// BOCPD hazard constant `lambda` — the prior expected run length
    /// between change points (in number of observations).  Smaller → more
    /// sensitive.
    pub bocpd_lambda: f64,

    /// Posterior probability threshold for BOCPD.  When the probability that
    /// the current run length is 0 (= a change just happened) exceeds this
    /// value the detector fires.
    pub bocpd_threshold: f64,

    /// Maximum run-length horizon tracked by BOCPD to bound memory/CPU.
    /// Older run lengths are pruned each tick.
    pub bocpd_max_run_length: usize,
}

impl Default for RegimeShiftConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cusum_k: 0.5,
            cusum_h: 4.0,
            bocpd_lambda: 50.0,
            bocpd_threshold: 0.5,
            bocpd_max_run_length: 200,
        }
    }
}

impl RegimeShiftConfig {
    /// Tier-specific defaults.  Strict tiers are more sensitive (lower h,
    /// lower lambda) so regime shifts are detected faster at the cost of
    /// more false positives.
    #[must_use]
    pub const fn for_tier(tier: ExtensionBudgetTier) -> Self {
        match tier {
            ExtensionBudgetTier::Strict => Self {
                enabled: true,
                cusum_k: 0.3,
                cusum_h: 3.0,
                bocpd_lambda: 30.0,
                bocpd_threshold: 0.4,
                bocpd_max_run_length: 150,
            },
            ExtensionBudgetTier::Balanced => Self {
                enabled: true,
                cusum_k: 0.5,
                cusum_h: 4.0,
                bocpd_lambda: 50.0,
                bocpd_threshold: 0.5,
                bocpd_max_run_length: 200,
            },
            ExtensionBudgetTier::Throughput => Self {
                enabled: true,
                cusum_k: 0.8,
                cusum_h: 5.0,
                bocpd_lambda: 80.0,
                bocpd_threshold: 0.6,
                bocpd_max_run_length: 300,
            },
        }
    }
}

/// CUSUM (Cumulative Sum) detector state for one direction (increase).
///
/// Tracks cumulative deviation of observed inter-arrival signal rate from
/// an estimated baseline.  Alarm fires when `cumsum > h * sigma`.
#[derive(Debug, Clone)]
struct CusumState {
    /// Running cumulative sum (positive direction = rate increase).
    cumsum_high: f64,
    /// Running cumulative sum (negative direction = rate decrease).
    cumsum_low: f64,
    /// Estimated baseline inter-arrival interval (ms) from first window.
    baseline_interval_ms: f64,
    /// Estimated baseline standard deviation of inter-arrival intervals.
    baseline_sigma: f64,
    /// Number of observations used to form the baseline estimate.
    baseline_n: u32,
    /// Whether the baseline has been seeded (need >= 3 observations).
    baseline_ready: bool,
    /// Timestamp of the last observation fed into CUSUM.
    last_observation_ms: Option<i64>,
    /// Total number of alarms raised.
    alarm_count: u64,
}

impl Default for CusumState {
    fn default() -> Self {
        Self {
            cumsum_high: 0.0,
            cumsum_low: 0.0,
            baseline_interval_ms: 0.0,
            baseline_sigma: 1.0,
            baseline_n: 0,
            baseline_ready: false,
            last_observation_ms: None,
            alarm_count: 0,
        }
    }
}

impl CusumState {
    /// Minimum observations before baseline is considered valid.
    const MIN_BASELINE_OBS: u32 = 3;

    /// Feed a new inter-arrival interval and return `true` if an alarm fires.
    fn observe(&mut self, interval_ms: f64, k: f64, h: f64) -> bool {
        // Phase 1: accumulate baseline (Welford online variance).
        if !self.baseline_ready {
            self.baseline_n += 1;
            let n = f64::from(self.baseline_n);
            let delta = interval_ms - self.baseline_interval_ms;
            self.baseline_interval_ms += delta / n;
            // Online variance (M2 accumulator stored in sigma temporarily).
            if self.baseline_n == 1 {
                self.baseline_sigma = 0.0;
            } else {
                let delta2 = interval_ms - self.baseline_interval_ms;
                self.baseline_sigma = delta.mul_add(delta2, self.baseline_sigma);
            }
            if self.baseline_n >= Self::MIN_BASELINE_OBS {
                self.baseline_ready = true;
                let variance = self.baseline_sigma / (f64::from(self.baseline_n) - 1.0);
                self.baseline_sigma = variance.sqrt().max(1.0);
            }
            return false;
        }

        // Phase 2: CUSUM update.
        let z = (interval_ms - self.baseline_interval_ms) / self.baseline_sigma;
        // S_high detects a *decrease* in inter-arrival (= rate increase).
        self.cumsum_high = (self.cumsum_high + (-z - k)).max(0.0);
        // S_low detects an *increase* in inter-arrival (= rate decrease).
        self.cumsum_low = (self.cumsum_low + (z - k)).max(0.0);

        let alarm = self.cumsum_high > h || self.cumsum_low > h;
        if alarm {
            self.alarm_count += 1;
            // Reset after alarm so we can detect the next regime change.
            self.cumsum_high = 0.0;
            self.cumsum_low = 0.0;
        }
        alarm
    }

    /// Reset detector state but keep baseline.
    const fn reset_cumsum(&mut self) {
        self.cumsum_high = 0.0;
        self.cumsum_low = 0.0;
    }
}

/// Simplified BOCPD (Bayesian Online Change Point Detection) state.
///
/// Maintains a run-length distribution and detects change points when the
/// posterior probability of run_length=0 exceeds a threshold.  Uses a
/// Gaussian observation model with online mean/variance estimation.
#[derive(Debug, Clone)]
struct BocpdState {
    /// Run-length probability distribution `P(r_t | data)`.
    /// Index `i` = probability that current run length is `i`.
    run_length_probs: Vec<f64>,
    /// Online mean of observations within the current run.
    run_means: Vec<f64>,
    /// Online variance numerator (M2) within the current run.
    run_m2s: Vec<f64>,
    /// Count of observations per run length.
    run_counts: Vec<u32>,
    /// Total number of change points detected.
    changepoint_count: u64,
    /// Whether sufficient data has been seen to make decisions.
    warmed_up: bool,
}

impl Default for BocpdState {
    fn default() -> Self {
        Self {
            run_length_probs: vec![1.0],
            run_means: vec![0.0],
            run_m2s: vec![0.0],
            run_counts: vec![0],
            changepoint_count: 0,
            warmed_up: false,
        }
    }
}

impl BocpdState {
    /// Minimum observations before BOCPD starts signalling.
    const WARMUP_OBS: u32 = 5;

    /// Feed a new observation and return `true` if a change point is detected.
    fn observe(
        &mut self,
        value: f64,
        hazard_lambda: f64,
        threshold: f64,
        max_run_length: usize,
    ) -> bool {
        let n = self.run_length_probs.len();

        // 1. Compute predictive probabilities for each run length.
        let pred_probs: Vec<f64> = (0..n).map(|i| self.predictive_prob(i, value)).collect();

        // 2. Compute hazard function H(r) = 1/lambda (constant hazard).
        let h = 1.0 / hazard_lambda.max(1.0);

        // 3. Compute growth probabilities (existing runs continue).
        //    Change-point uses uninformative prior predictive (Adams & MacKay
        //    2007): the new run has no data yet, so P(x|r=0) uses a broad
        //    prior.  Growth uses the accumulated run statistics.
        let prior_pred = Self::prior_predictive(value);
        let mut new_probs = Vec::with_capacity(n + 1);
        let mut hazard_sum = 0.0_f64;
        for rl_prob in &self.run_length_probs {
            hazard_sum = rl_prob.mul_add(h, hazard_sum);
        }
        let cp_prob = prior_pred * hazard_sum;
        for (rl_prob, &pp) in self.run_length_probs.iter().zip(&pred_probs) {
            new_probs.push(rl_prob * pp * (1.0 - h));
        }

        // Insert change-point probability at position 0.
        new_probs.insert(0, cp_prob);

        // 4. Normalize.
        let total: f64 = new_probs.iter().sum();
        if total > 0.0 {
            for p in &mut new_probs {
                *p /= total;
            }
        }

        // 5. Update sufficient statistics per run length.
        let mut new_means = Vec::with_capacity(new_probs.len());
        let mut new_m2s = Vec::with_capacity(new_probs.len());
        let mut new_counts = Vec::with_capacity(new_probs.len());

        // Run length 0: fresh start.
        new_means.push(value);
        new_m2s.push(0.0);
        new_counts.push(1);

        // Run lengths 1..n: continue from previous.
        for ((&old_count, &old_mean), &old_m2) in self
            .run_counts
            .iter()
            .zip(&self.run_means)
            .zip(&self.run_m2s)
        {
            let count = old_count + 1;
            let delta = value - old_mean;
            let new_mean = old_mean + delta / f64::from(count);
            let delta2 = value - new_mean;
            let new_m2 = delta.mul_add(delta2, old_m2);
            new_means.push(new_mean);
            new_m2s.push(new_m2);
            new_counts.push(count);
        }

        // 6. Prune to max_run_length.
        let max_len = max_run_length.max(2);
        if new_probs.len() > max_len {
            new_probs.truncate(max_len);
            new_means.truncate(max_len);
            new_m2s.truncate(max_len);
            new_counts.truncate(max_len);
            // Re-normalize after pruning.
            let total: f64 = new_probs.iter().sum();
            if total > 0.0 {
                for p in &mut new_probs {
                    *p /= total;
                }
            }
        }

        self.run_length_probs = new_probs;
        self.run_means = new_means;
        self.run_m2s = new_m2s;
        self.run_counts = new_counts;

        // 7. Check warmup and change-point probability.
        let total_obs: u32 = self.run_counts.iter().max().copied().unwrap_or(0);
        if total_obs >= Self::WARMUP_OBS {
            self.warmed_up = true;
        }
        if !self.warmed_up {
            return false;
        }

        let cp_detected = self.run_length_probs.first().copied().unwrap_or(0.0) > threshold;
        if cp_detected {
            self.changepoint_count += 1;
        }
        cp_detected
    }

    /// Gaussian predictive probability for observation `x` given run-length
    /// sufficient statistics at index `i`.
    /// sqrt(2 * pi)
    const SQRT_2PI: f64 = 2.506_628_274_631_000_5;

    fn predictive_prob(&self, i: usize, x: f64) -> f64 {
        let count = self.run_counts[i];
        if count < 2 {
            return Self::prior_predictive(x);
        }
        let mean = self.run_means[i];
        let variance = (self.run_m2s[i] / f64::from(count - 1)).max(1.0);
        let pred_var = variance * (1.0 + 1.0 / f64::from(count));
        let sigma = pred_var.sqrt();
        let diff = x - mean;
        (-diff * diff / (2.0 * pred_var)).exp() / (sigma * Self::SQRT_2PI)
    }

    /// Uninformative prior predictive: broad Gaussian (sigma=1000).  Used
    /// for the change-point hypothesis so any observation is plausible.
    fn prior_predictive(_x: f64) -> f64 {
        1.0 / (1000.0 * Self::SQRT_2PI)
    }

    /// Reset detector to initial state.
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Combined regime-shift detector state for one extension.
#[derive(Debug, Clone, Default)]
struct RegimeShiftDetectorState {
    cusum: CusumState,
    bocpd: BocpdState,
    /// Whether the detector has triggered (either CUSUM or BOCPD alarm).
    triggered: bool,
    /// Reason string for the most recent trigger.
    trigger_source: Option<&'static str>,
    /// Monotonic counter of total triggers.
    trigger_count: u64,
}

/// Telemetry snapshot of the regime-shift detector for one extension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegimeShiftSnapshot {
    /// Whether the detector is currently in triggered state.
    pub triggered: bool,
    /// Source of the last trigger ("cusum" or "bocpd"), if any.
    pub trigger_source: Option<String>,
    /// Total number of triggers since creation.
    pub trigger_count: u64,
    /// CUSUM cumulative sum (high direction).
    pub cusum_high: f64,
    /// CUSUM cumulative sum (low direction).
    pub cusum_low: f64,
    /// CUSUM alarm count.
    pub cusum_alarm_count: u64,
    /// Whether the CUSUM baseline is ready.
    pub cusum_baseline_ready: bool,
    /// BOCPD change-point probability (run_length=0).
    pub bocpd_cp_prob: f64,
    /// BOCPD total change-point detections.
    pub bocpd_changepoint_count: u64,
    /// Whether BOCPD has warmed up.
    pub bocpd_warmed_up: bool,
}

impl RegimeShiftDetectorState {
    fn snapshot(&self) -> RegimeShiftSnapshot {
        RegimeShiftSnapshot {
            triggered: self.triggered,
            trigger_source: self.trigger_source.map(String::from),
            trigger_count: self.trigger_count,
            cusum_high: self.cusum.cumsum_high,
            cusum_low: self.cusum.cumsum_low,
            cusum_alarm_count: self.cusum.alarm_count,
            cusum_baseline_ready: self.cusum.baseline_ready,
            bocpd_cp_prob: self.bocpd.run_length_probs.first().copied().unwrap_or(0.0),
            bocpd_changepoint_count: self.bocpd.changepoint_count,
            bocpd_warmed_up: self.bocpd.warmed_up,
        }
    }
}

/// Configuration for conformal + PAC-Bayes safety envelopes that wrap
/// adaptive optimization decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SafetyEnvelopeConfig {
    /// Master switch — when false, no safety veto is applied.
    pub enabled: bool,
    /// Confidence level for conformal prediction intervals (0, 1).
    /// Higher → wider intervals → fewer false anomalies.
    pub conformal_confidence: f64,
    /// Maximum calibration set size for conformal prediction.
    pub conformal_calibration_size: usize,
    /// PAC-Bayes delta parameter (probability of bound violation).
    /// Smaller → tighter bound → more conservative.
    pub pac_bayes_delta: f64,
    /// PAC-Bayes KL prior weight.  Larger → more regularization
    /// toward the prior policy (conservative fallback).
    pub pac_bayes_prior_weight: f64,
    /// Maximum tolerable error rate before forcing conservative mode.
    pub safety_error_threshold: f64,
    /// Minimum observations before the safety envelope activates.
    pub min_observations: u32,
}

impl Default for SafetyEnvelopeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            conformal_confidence: 0.95,
            conformal_calibration_size: 200,
            pac_bayes_delta: 0.05,
            pac_bayes_prior_weight: 1.0,
            safety_error_threshold: 0.15,
            min_observations: 20,
        }
    }
}

impl SafetyEnvelopeConfig {
    /// Tier-specific defaults.
    #[must_use]
    pub const fn for_tier(tier: ExtensionBudgetTier) -> Self {
        match tier {
            ExtensionBudgetTier::Strict => Self {
                enabled: true,
                conformal_confidence: 0.99,
                conformal_calibration_size: 100,
                pac_bayes_delta: 0.01,
                pac_bayes_prior_weight: 2.0,
                safety_error_threshold: 0.05,
                min_observations: 10,
            },
            ExtensionBudgetTier::Balanced => Self {
                enabled: true,
                conformal_confidence: 0.95,
                conformal_calibration_size: 200,
                pac_bayes_delta: 0.05,
                pac_bayes_prior_weight: 1.0,
                safety_error_threshold: 0.15,
                min_observations: 20,
            },
            ExtensionBudgetTier::Throughput => Self {
                enabled: true,
                conformal_confidence: 0.90,
                conformal_calibration_size: 300,
                pac_bayes_delta: 0.10,
                pac_bayes_prior_weight: 0.5,
                safety_error_threshold: 0.25,
                min_observations: 30,
            },
        }
    }
}

/// Conformal prediction state for one extension.  Maintains a calibration
/// set of recent nonconformity scores and computes prediction intervals.
#[derive(Debug, Clone)]
struct ConformalState {
    /// Recent nonconformity scores (absolute residuals from running mean).
    calibration_scores: VecDeque<f64>,
    /// Online running mean of observations.
    running_mean: f64,
    /// Online running M2 (for variance computation).
    running_m2: f64,
    /// Total observation count (for Welford update).
    observation_count: u64,
    /// Number of observations that fell outside the prediction interval.
    anomaly_count: u64,
}

impl Default for ConformalState {
    fn default() -> Self {
        Self {
            calibration_scores: VecDeque::new(),
            running_mean: 0.0,
            running_m2: 0.0,
            observation_count: 0,
            anomaly_count: 0,
        }
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
impl ConformalState {
    /// Feed a new observation and return `true` if it is anomalous (outside
    /// the conformal prediction interval at the configured confidence level).
    fn observe(&mut self, value: f64, confidence: f64, max_calibration: usize) -> bool {
        // Welford online mean/variance update.
        self.observation_count += 1;
        let n = self.observation_count as f64;
        let delta = value - self.running_mean;
        self.running_mean += delta / n;
        let delta2 = value - self.running_mean;
        self.running_m2 = delta.mul_add(delta2, self.running_m2);

        // Nonconformity score = absolute residual from running mean.
        let score = delta.abs();

        // Check against current prediction interval before adding to calibration.
        let is_anomaly = if self.calibration_scores.len() >= 2 {
            let quantile_idx = self.conformal_quantile_index(confidence);
            let threshold = self.sorted_score_at(quantile_idx);
            score > threshold
        } else {
            false
        };

        if is_anomaly {
            self.anomaly_count += 1;
        }

        // Add to calibration set (bounded).
        self.calibration_scores.push_back(score);
        while self.calibration_scores.len() > max_calibration {
            let _ = self.calibration_scores.pop_front();
        }

        is_anomaly
    }

    /// Compute the quantile index for the given confidence level.
    fn conformal_quantile_index(&self, confidence: f64) -> usize {
        let n = self.calibration_scores.len();
        if n == 0 {
            return 0;
        }
        // Quantile index: ceil((n+1) * confidence) - 1, clamped to [0, n-1].
        let idx = ((n as f64 + 1.0) * confidence).ceil() as usize;
        idx.saturating_sub(1).min(n - 1)
    }

    /// Get the score at a given quantile index by partial sort.
    fn sorted_score_at(&self, idx: usize) -> f64 {
        let mut scores: Vec<f64> = self.calibration_scores.iter().copied().collect();
        scores.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        scores.get(idx).copied().unwrap_or(f64::INFINITY)
    }

    /// Current empirical anomaly rate.
    fn anomaly_rate(&self) -> f64 {
        if self.observation_count == 0 {
            return 0.0;
        }
        self.anomaly_count as f64 / self.observation_count as f64
    }

    /// Current prediction interval half-width (the conformal threshold).
    fn interval_width(&self, confidence: f64) -> f64 {
        if self.calibration_scores.len() < 2 {
            return f64::INFINITY;
        }
        let idx = self.conformal_quantile_index(confidence);
        self.sorted_score_at(idx)
    }
}

/// PAC-Bayes bound state for one extension.  Tracks empirical error rates
/// and computes the PAC-Bayes-kl bound on the true error rate.
#[derive(Debug, Clone, Default)]
struct PacBayesState {
    /// Number of successful outcomes.
    successes: u64,
    /// Number of failure outcomes.
    failures: u64,
    /// Prior error rate (before seeing data).
    prior_error_rate: f64,
}

#[allow(
    clippy::cast_precision_loss,
    clippy::missing_const_for_fn,
    clippy::manual_midpoint
)]
impl PacBayesState {
    /// Record an outcome.
    fn record(&mut self, success: bool) {
        if success {
            self.successes += 1;
        } else {
            self.failures += 1;
        }
    }

    /// Total observations.
    fn total(&self) -> u64 {
        self.successes + self.failures
    }

    /// Empirical error rate.
    fn empirical_error_rate(&self) -> f64 {
        let t = self.total();
        if t == 0 {
            return self.prior_error_rate;
        }
        self.failures as f64 / t as f64
    }

    /// Compute the PAC-Bayes-kl upper bound on the true error rate.
    ///
    /// Uses the PAC-Bayes-kl inequality:
    ///   kl(q_hat || q_bound) <= (KL(Q||P) + ln(2*sqrt(n)/delta)) / n
    ///
    /// where q_hat is the empirical error rate and q_bound is the upper bound
    /// we solve for.  We use binary search to find the tightest bound.
    fn pac_bayes_bound(&self, delta: f64, prior_weight: f64) -> f64 {
        let n = self.total();
        if n == 0 {
            return 1.0;
        }
        let n_f = n as f64;
        let q_hat = self.empirical_error_rate();

        // KL(Q||P) ≈ prior_weight * kl(q_hat || prior_error_rate).
        let kl_qp = prior_weight * kl_divergence(q_hat, self.prior_error_rate.clamp(0.01, 0.99));

        // RHS of PAC-Bayes-kl inequality.
        let rhs = (kl_qp + (2.0 * n_f.sqrt() / delta.max(1e-10)).ln()) / n_f;

        // Binary search for q_bound such that kl(q_hat || q_bound) <= rhs.
        let mut lo = q_hat;
        let mut hi = 1.0;
        for _ in 0..64 {
            let mid = (lo + hi) / 2.0;
            if kl_divergence(q_hat, mid) <= rhs {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo.min(1.0)
    }

    /// Reset state.
    fn reset(&mut self) {
        self.successes = 0;
        self.failures = 0;
    }
}

/// KL divergence between two Bernoulli distributions: kl(p || q).
fn kl_divergence(p: f64, q: f64) -> f64 {
    let p = p.clamp(1e-10, 1.0 - 1e-10);
    let q = q.clamp(1e-10, 1.0 - 1e-10);
    (1.0 - p).mul_add(((1.0 - p) / (1.0 - q)).ln(), p * (p / q).ln())
}

/// Combined safety envelope state for one extension.
#[derive(Debug, Clone, Default)]
struct SafetyEnvelopeState {
    conformal: ConformalState,
    pac_bayes: PacBayesState,
    /// Whether the safety envelope is currently vetoing aggressive optimization.
    vetoing: bool,
    /// Total number of veto activations.
    veto_count: u64,
    /// Reason for the current veto, if active.
    veto_reason: Option<&'static str>,
}

/// Telemetry snapshot of the safety envelope for one extension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SafetyEnvelopeSnapshot {
    /// Whether the safety envelope is currently vetoing.
    pub vetoing: bool,
    /// Total veto activations.
    pub veto_count: u64,
    /// Current veto reason, if active.
    pub veto_reason: Option<String>,
    /// Conformal prediction anomaly rate.
    pub conformal_anomaly_rate: f64,
    /// Conformal prediction interval half-width.
    pub conformal_interval_width: f64,
    /// Total observations in the conformal calibration set.
    pub conformal_calibration_size: usize,
    /// PAC-Bayes empirical error rate.
    pub pac_bayes_empirical_error: f64,
    /// PAC-Bayes upper bound on true error rate.
    pub pac_bayes_bound: f64,
    /// Total PAC-Bayes observations.
    pub pac_bayes_total: u64,
}

/// Snapshot of OCO-tuned budgets for one extension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OcoTunerSnapshot {
    pub queue_budget: f64,
    pub batch_budget: f64,
    pub time_slice_ms: f64,
    pub rounds: u64,
    pub cumulative_loss: f64,
    pub cumulative_regret: f64,
    pub guardrail_rollbacks: u64,
}

#[derive(Debug, Clone, Copy)]
struct OcoTunerUpdateTelemetry {
    instantaneous_loss: f64,
    cumulative_regret: f64,
    rolled_back: bool,
}

/// Per-extension online convex optimization state for budget tuning.
#[derive(Debug, Clone)]
struct OcoTunerState {
    queue_budget: f64,
    batch_budget: f64,
    time_slice_ms: f64,
    rounds: u64,
    cumulative_loss: f64,
    cumulative_regret: f64,
    guardrail_rollbacks: u64,
}

impl OcoTunerState {
    const fn from_config(config: &OcoTunerConfig) -> Self {
        let queue_budget = config
            .initial_queue_budget
            .clamp(config.min_queue_budget, config.max_queue_budget);
        let batch_budget = config
            .initial_batch_budget
            .clamp(config.min_batch_budget, config.max_batch_budget);
        let time_slice_ms = config
            .initial_time_slice_ms
            .clamp(config.min_time_slice_ms, config.max_time_slice_ms);
        Self {
            queue_budget,
            batch_budget,
            time_slice_ms,
            rounds: 0,
            cumulative_loss: 0.0,
            cumulative_regret: 0.0,
            guardrail_rollbacks: 0,
        }
    }

    const fn snapshot(&self) -> OcoTunerSnapshot {
        OcoTunerSnapshot {
            queue_budget: self.queue_budget,
            batch_budget: self.batch_budget,
            time_slice_ms: self.time_slice_ms,
            rounds: self.rounds,
            cumulative_loss: self.cumulative_loss,
            cumulative_regret: self.cumulative_regret,
            guardrail_rollbacks: self.guardrail_rollbacks,
        }
    }

    const fn rollback_to_safe_profile(&mut self, config: &OcoTunerConfig) {
        self.queue_budget = config
            .initial_queue_budget
            .clamp(config.min_queue_budget, config.max_queue_budget);
        self.batch_budget = config
            .initial_batch_budget
            .clamp(config.min_batch_budget, config.max_batch_budget);
        self.time_slice_ms = config
            .initial_time_slice_ms
            .clamp(config.min_time_slice_ms, config.max_time_slice_ms);
        self.guardrail_rollbacks = self.guardrail_rollbacks.saturating_add(1);
    }

    fn update(
        &mut self,
        overloaded: bool,
        queue_depth: Option<usize>,
        queue_capacity: Option<usize>,
        config: &OcoTunerConfig,
    ) -> OcoTunerUpdateTelemetry {
        #[allow(clippy::cast_precision_loss)]
        let utilization = match (queue_depth, queue_capacity) {
            (Some(depth), Some(capacity)) if capacity > 0 => depth as f64 / capacity as f64,
            _ => 0.0,
        };
        let loss = if overloaded {
            (1.0 + utilization).clamp(1.0, 2.0)
        } else {
            0.15 + utilization * 0.35
        };
        let baseline_loss = if overloaded { 1.0 } else { 0.2 };
        self.cumulative_loss += loss;
        self.cumulative_regret += (loss - baseline_loss).max(0.0);
        self.rounds = self.rounds.saturating_add(1);

        let grad_queue = if overloaded {
            -(1.0 + utilization)
        } else {
            0.3 + utilization * 0.2
        };
        let grad_batch = if overloaded { -0.75 } else { 0.25 };
        let grad_time_slice = if overloaded {
            -0.5 - utilization * 0.25
        } else {
            0.2
        };

        self.queue_budget = config
            .learning_rate
            .mul_add(-grad_queue, self.queue_budget)
            .clamp(config.min_queue_budget, config.max_queue_budget);
        self.batch_budget = config
            .learning_rate
            .mul_add(-grad_batch, self.batch_budget)
            .clamp(config.min_batch_budget, config.max_batch_budget);
        self.time_slice_ms = config
            .learning_rate
            .mul_add(-grad_time_slice, self.time_slice_ms)
            .clamp(config.min_time_slice_ms, config.max_time_slice_ms);

        let rolled_back = loss > config.rollback_loss_threshold;
        if rolled_back {
            self.rollback_to_safe_profile(config);
        }
        OcoTunerUpdateTelemetry {
            instantaneous_loss: loss,
            cumulative_regret: self.cumulative_regret,
            rolled_back,
        }
    }

    fn adaptive_overload_threshold(&self, base_threshold: u32) -> u32 {
        let base = base_threshold.max(1);
        let adjustment = if self.queue_budget > 0.0 {
            self.batch_budget / self.queue_budget
        } else {
            1.0
        };
        let scaled = f64::from(base) * adjustment.clamp(0.5, 1.5);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            scaled.round().max(1.0) as u32
        }
    }
}

impl SafetyEnvelopeState {
    const fn clear_veto(&mut self) {
        self.vetoing = false;
        self.veto_reason = None;
    }

    fn snapshot(&self, config: &SafetyEnvelopeConfig) -> SafetyEnvelopeSnapshot {
        SafetyEnvelopeSnapshot {
            vetoing: self.vetoing,
            veto_count: self.veto_count,
            veto_reason: self.veto_reason.map(String::from),
            conformal_anomaly_rate: self.conformal.anomaly_rate(),
            conformal_interval_width: self.conformal.interval_width(config.conformal_confidence),
            conformal_calibration_size: self.conformal.calibration_scores.len(),
            pac_bayes_empirical_error: self.pac_bayes.empirical_error_rate(),
            pac_bayes_bound: self
                .pac_bayes
                .pac_bayes_bound(config.pac_bayes_delta, config.pac_bayes_prior_weight),
            pac_bayes_total: self.pac_bayes.total(),
        }
    }

    /// Evaluate the safety envelope: update conformal + PAC-Bayes state
    /// and return `true` if aggressive optimization should be vetoed.
    fn evaluate(&mut self, latency_ms: f64, success: bool, config: &SafetyEnvelopeConfig) -> bool {
        if !config.enabled {
            self.clear_veto();
            return false;
        }

        // Update conformal state with the latency observation.
        let conformal_anomaly = self.conformal.observe(
            latency_ms,
            config.conformal_confidence,
            config.conformal_calibration_size,
        );

        // Update PAC-Bayes state with outcome.
        self.pac_bayes.record(success);

        // Not enough data yet — don't veto.
        let total = self.pac_bayes.total();
        if total < u64::from(config.min_observations) {
            self.clear_veto();
            return false;
        }

        // Check PAC-Bayes bound.
        let bound = self
            .pac_bayes
            .pac_bayes_bound(config.pac_bayes_delta, config.pac_bayes_prior_weight);
        if bound > config.safety_error_threshold {
            if !self.vetoing {
                self.veto_count += 1;
            }
            self.vetoing = true;
            self.veto_reason = Some("pac_bayes_bound_exceeded");
            return true;
        }

        // Check conformal anomaly rate.
        let anomaly_rate = self.conformal.anomaly_rate();
        let expected_anomaly = 1.0 - config.conformal_confidence;
        if anomaly_rate > expected_anomaly * 3.0 && conformal_anomaly {
            if !self.vetoing {
                self.veto_count += 1;
            }
            self.vetoing = true;
            self.veto_reason = Some("conformal_anomaly_excess");
            return true;
        }

        // All clear — release veto if previously active.
        self.clear_veto();
        false
    }

    /// Reset state (e.g. on recovery).
    fn reset(&mut self) {
        self.conformal = ConformalState::default();
        self.pac_bayes.reset();
        self.clear_veto();
    }
}

/// Runtime budget-controller state for one extension.
#[derive(Debug, Clone, Default)]
struct ExtensionBudgetFallbackState {
    overload_timestamps_ms: VecDeque<i64>,
    in_fallback: bool,
    healthy_success_streak: u32,
    last_trigger_reason: Option<String>,
    /// Regime-shift detector state (CUSUM + BOCPD).
    regime_shift: RegimeShiftDetectorState,
    /// Conformal + PAC-Bayes safety envelope state.
    safety_envelope: SafetyEnvelopeState,
    /// Online convex optimization tuner state.
    oco_tuner: Option<OcoTunerState>,
}

/// Telemetry event emitted when a quota limit is breached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaBreachEvent {
    /// Unix epoch milliseconds when the breach was detected.
    pub ts_ms: i64,
    /// Extension that triggered the breach.
    pub extension_id: String,
    /// Capability being requested (e.g. "exec", "http", "write").
    pub capability: String,
    /// Human-readable reason for the breach.
    pub reason: String,
    /// Source of the quota config: "per_extension" or "global".
    pub quota_config_source: String,
}

/// Result of a quota check before dispatching a hostcall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaCheckResult {
    /// Within quota — proceed.
    Allowed,
    /// Quota exceeded — deny with reason.
    Exceeded { reason: String },
}

/// Check per-extension quotas. Returns [`QuotaCheckResult::Exceeded`] if any
/// configured limit is breached. Called in the dispatch chokepoint before
/// the runtime risk evaluation.
fn check_extension_quota(
    config: &ExtensionQuotaConfig,
    state: &mut ExtensionQuotaState,
    now_ms: i64,
    capability: &str,
) -> QuotaCheckResult {
    // 1. Prune expired timestamps (older than 60s).
    let horizon_60s = now_ms.saturating_sub(60_000);
    while state
        .hostcall_timestamps_ms
        .front()
        .is_some_and(|&ts| ts < horizon_60s)
    {
        state.hostcall_timestamps_ms.pop_front();
    }

    // 2. Per-second burst check.
    if let Some(max_per_sec) = config.max_hostcalls_per_second {
        let horizon_1s = now_ms.saturating_sub(1_000);
        let count_1s = state
            .hostcall_timestamps_ms
            .iter()
            .rev()
            .take_while(|&&ts| ts >= horizon_1s)
            .count();
        if count_1s >= max_per_sec as usize {
            return QuotaCheckResult::Exceeded {
                reason: format!("hostcall rate {count_1s}/s exceeds limit {max_per_sec}/s"),
            };
        }
    }

    // 3. Per-minute rate check.
    if let Some(max_per_min) = config.max_hostcalls_per_minute {
        let count_60s = state.hostcall_timestamps_ms.len();
        if count_60s >= max_per_min as usize {
            return QuotaCheckResult::Exceeded {
                reason: format!("hostcall rate {count_60s}/60s exceeds limit {max_per_min}/60s"),
            };
        }
    }

    // 4. Total hostcall budget.
    if let Some(max_total) = config.max_hostcalls_total
        && state.hostcalls_total >= max_total
    {
        return QuotaCheckResult::Exceeded {
            reason: format!(
                "total hostcalls {} exceeds limit {max_total}",
                state.hostcalls_total
            ),
        };
    }

    // 5. Subprocess fan-out (only relevant for exec capability).
    if capability == "exec"
        && let Some(max_sub) = config.max_subprocesses
        && state.active_subprocesses >= max_sub
    {
        return QuotaCheckResult::Exceeded {
            reason: format!(
                "active subprocesses {} reaches limit {max_sub}",
                state.active_subprocesses
            ),
        };
    }

    // 6. HTTP request budget.
    if capability == "http"
        && let Some(max_http) = config.max_http_requests
        && state.http_requests_total >= max_http
    {
        return QuotaCheckResult::Exceeded {
            reason: format!(
                "HTTP requests {} exceeds limit {max_http}",
                state.http_requests_total
            ),
        };
    }

    // 7. Write bytes budget (tracked externally via record_write_bytes).
    if capability == "write"
        && let Some(max_wb) = config.max_write_bytes
        && state.write_bytes_total >= max_wb
    {
        return QuotaCheckResult::Exceeded {
            reason: format!(
                "write bytes {} exceeds limit {max_wb}",
                state.write_bytes_total
            ),
        };
    }

    // All checks passed; record usage.
    state.hostcall_timestamps_ms.push_back(now_ms);
    state.hostcalls_total += 1;
    if capability == "http" {
        state.http_requests_total += 1;
    }

    QuotaCheckResult::Allowed
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRiskStateLabelValue {
    SafeFast,
    Suspicious,
    Unsafe,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRiskActionValue {
    Allow,
    Harden,
    Deny,
    Terminate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRiskPosteriorEvidence {
    pub safe_fast: f64,
    pub suspicious: f64,
    #[serde(rename = "unsafe")]
    pub unsafe_: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRiskExpectedLossEvidence {
    pub allow: f64,
    pub harden: f64,
    pub deny: f64,
    pub terminate: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRiskExplanationLevelValue {
    Compact,
    #[default]
    Standard,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRiskExplanationContributor {
    pub code: String,
    pub signed_impact: f64,
    pub magnitude: f64,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RuntimeRiskExplanationBudgetState {
    pub time_budget_ms: u64,
    pub elapsed_ms: u64,
    pub term_budget: usize,
    pub terms_emitted: usize,
    pub exhausted: bool,
    pub fallback_mode: bool,
}

impl Default for RuntimeRiskExplanationBudgetState {
    fn default() -> Self {
        Self {
            time_budget_ms: RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
            elapsed_ms: 0,
            term_budget: RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
            terms_emitted: 0,
            exhausted: false,
            fallback_mode: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRiskLedgerArtifactEntry {
    pub ts_ms: i64,
    pub extension_id: String,
    pub call_id: String,
    pub capability: String,
    pub method: String,
    pub params_hash: String,
    pub policy_reason: String,
    pub risk_score: f64,
    pub posterior: RuntimeRiskPosteriorEvidence,
    pub expected_loss: RuntimeRiskExpectedLossEvidence,
    pub selected_action: RuntimeRiskActionValue,
    pub derived_state: RuntimeRiskStateLabelValue,
    pub triggers: Vec<String>,
    pub fallback_reason: Option<String>,
    pub e_process: f64,
    pub e_threshold: f64,
    pub conformal_residual: f64,
    pub conformal_quantile: f64,
    pub drift_detected: bool,
    pub outcome_error_code: Option<String>,
    #[serde(default = "runtime_risk_explanation_schema_default")]
    pub explanation_schema: String,
    #[serde(default)]
    pub explanation_level: RuntimeRiskExplanationLevelValue,
    #[serde(default)]
    pub explanation_summary: String,
    #[serde(default)]
    pub top_contributors: Vec<RuntimeRiskExplanationContributor>,
    #[serde(default)]
    pub budget_state: RuntimeRiskExplanationBudgetState,
    pub ledger_hash: String,
    pub prev_ledger_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRiskLedgerArtifact {
    pub schema: String,
    pub generated_at_ms: i64,
    pub entry_count: usize,
    pub head_ledger_hash: Option<String>,
    pub tail_ledger_hash: Option<String>,
    pub data_hash: String,
    pub entries: Vec<RuntimeRiskLedgerArtifactEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeRiskLedgerIntegrityError {
    pub index: usize,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeRiskLedgerVerificationReport {
    pub schema: String,
    pub entry_count: usize,
    pub head_ledger_hash: Option<String>,
    pub tail_ledger_hash: Option<String>,
    pub artifact_data_hash: String,
    pub computed_data_hash: String,
    pub valid: bool,
    pub errors: Vec<RuntimeRiskLedgerIntegrityError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRiskReplayStep {
    pub index: usize,
    pub call_id: String,
    pub extension_id: String,
    pub capability: String,
    pub method: String,
    pub policy_reason: String,
    pub selected_action: RuntimeRiskActionValue,
    pub derived_state: RuntimeRiskStateLabelValue,
    pub risk_score: f64,
    pub reason_codes: Vec<String>,
    #[serde(default)]
    pub explanation_level: RuntimeRiskExplanationLevelValue,
    #[serde(default)]
    pub explanation_summary: String,
    #[serde(default)]
    pub top_contributors: Vec<RuntimeRiskExplanationContributor>,
    #[serde(default)]
    pub budget_state: RuntimeRiskExplanationBudgetState,
    pub fallback_reason: Option<String>,
    pub drift_detected: bool,
    pub e_process: f64,
    pub e_threshold: f64,
    pub conformal_residual: f64,
    pub conformal_quantile: f64,
    pub ledger_hash: String,
    pub prev_ledger_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRiskReplayArtifact {
    pub schema: String,
    pub source_schema: String,
    pub source_data_hash: String,
    pub entry_count: usize,
    pub tail_ledger_hash: Option<String>,
    pub steps: Vec<RuntimeRiskReplayStep>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRiskCalibrationObjective {
    MinExpectedLoss,
    MinFalsePositives,
    #[default]
    BalancedAccuracy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRiskThresholdCalibration {
    pub threshold: f64,
    pub objective_score: f64,
    pub expected_loss: f64,
    pub false_positive_rate: f64,
    pub false_negative_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RuntimeRiskCalibrationConfig {
    pub objective: RuntimeRiskCalibrationObjective,
    pub baseline_threshold: f64,
    pub threshold_grid: Vec<f64>,
    pub false_positive_weight: f64,
    pub false_negative_weight: f64,
}

impl Default for RuntimeRiskCalibrationConfig {
    fn default() -> Self {
        let threshold_grid = (1..=19).map(|step| f64::from(step) * 0.05_f64).collect();
        Self {
            objective: RuntimeRiskCalibrationObjective::BalancedAccuracy,
            baseline_threshold: 0.65,
            threshold_grid,
            false_positive_weight: 1.0,
            false_negative_weight: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRiskCalibrationReport {
    pub schema: String,
    pub source_schema: String,
    pub source_data_hash: String,
    pub objective: RuntimeRiskCalibrationObjective,
    pub baseline_threshold: f64,
    pub recommended_threshold: f64,
    pub recommended_delta: f64,
    pub baseline: RuntimeRiskThresholdCalibration,
    pub recommended: RuntimeRiskThresholdCalibration,
    pub candidates: Vec<RuntimeRiskThresholdCalibration>,
}

// ============================================================================
// Baseline Modeling (bd-153pv)
// ============================================================================

/// Per-capability robust statistics from approved traces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaselineCapabilityProfile {
    /// Capability name (e.g. "log", "exec", "http").
    pub capability: String,
    /// Number of observations.
    pub sample_count: usize,
    /// Median of risk scores.
    pub risk_score_median: f64,
    /// Median Absolute Deviation of risk scores.
    pub risk_score_mad: f64,
    /// 5th percentile of risk scores.
    pub risk_score_p5: f64,
    /// 95th percentile of risk scores.
    pub risk_score_p95: f64,
    /// Median error rate across calls.
    pub error_rate_median: f64,
    /// Median burst density (1s window).
    pub burst_density_1s_median: f64,
    /// Median burst density (10s window).
    pub burst_density_10s_median: f64,
}

/// Markov transition matrix over risk state labels.
///
/// States: `SafeFast`=0, `Suspicious`=1, `Unsafe`=2.
/// `counts[i][j]` = number of observed transitions from state `i` to state `j`.
/// `probabilities[i][j]` = smoothed transition probability.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaselineMarkovTransitionMatrix {
    /// Raw transition counts `[from][to]`, 3x3.
    pub counts: [[u64; 3]; 3],
    /// Smoothed probabilities `[from][to]`, 3x3. Rows sum to 1.0.
    pub probabilities: [[f64; 3]; 3],
    /// Dirichlet smoothing prior per cell (default: 1.0).
    pub smoothing_prior: f64,
    /// Total transitions observed.
    pub total_transitions: u64,
    /// Stationary distribution `[SafeFast, Suspicious, Unsafe]`.
    pub stationary_distribution: [f64; 3],
}

/// Single drift anomaly detected when comparing live features to baseline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaselineDriftAnomaly {
    /// Which metric deviated (e.g. "risk_score", "error_rate", "burst_density_1s").
    pub metric: String,
    /// Observed value.
    pub observed: f64,
    /// Baseline median for this metric.
    pub baseline_median: f64,
    /// Baseline MAD for this metric.
    pub baseline_mad: f64,
    /// Number of MAD units from median (z-score analog).
    pub deviation_mads: f64,
    /// Human-readable explanation.
    pub explanation: String,
}

/// Result of comparing live features against a baseline model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaselineDriftReport {
    /// Extension ID this report pertains to.
    pub extension_id: String,
    /// Capability being evaluated.
    pub capability: String,
    /// Whether any anomaly exceeded the threshold.
    pub drift_detected: bool,
    /// Individual anomalies found.
    pub anomalies: Vec<BaselineDriftAnomaly>,
    /// Markov transition anomaly score (KL divergence from baseline).
    pub transition_divergence: f64,
    /// Whether transition pattern is anomalous (KL > threshold).
    pub transition_anomalous: bool,
}

/// Complete baseline model for an extension, built from approved traces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRiskBaselineModel {
    /// Schema version.
    pub schema: String,
    /// Extension ID this baseline covers.
    pub extension_id: String,
    /// Timestamp when baseline was generated (ms since epoch).
    pub generated_at_ms: i64,
    /// Source data hash from the ledger used to build this baseline.
    pub source_data_hash: String,
    /// Number of ledger entries used to build the baseline.
    pub source_entry_count: usize,
    /// Per-capability robust statistics.
    pub capability_profiles: Vec<BaselineCapabilityProfile>,
    /// Markov transition matrix over risk states.
    pub transition_matrix: BaselineMarkovTransitionMatrix,
    /// MAD deviation threshold for flagging anomalies (default: 3.0).
    pub anomaly_threshold_mads: f64,
    /// KL divergence threshold for transition anomalies (default: 0.5).
    pub transition_divergence_threshold: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct RuntimeHostcallSequenceContext {
    pub sequence_id: u64,
    pub previous_capability: Option<String>,
    pub previous_method: Option<String>,
    pub previous_resource_target_class: Option<String>,
    pub burst_count_1s: u32,
    pub burst_count_10s: u32,
    pub recent_error_count: u32,
    pub recent_window_count: u32,
    pub prior_failure_streak: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RuntimeHostcallFeatureVector {
    pub schema: String,
    pub base_score: f64,
    pub recent_mean_score: f64,
    pub recent_error_rate: f64,
    pub burst_density_1s: f64,
    pub burst_density_10s: f64,
    pub prior_failure_streak_norm: f64,
    pub dangerous_capability: f64,
    pub timeout_requested: f64,
    pub policy_prompt_bias: f64,
}

impl Default for RuntimeHostcallFeatureVector {
    fn default() -> Self {
        Self {
            schema: RUNTIME_HOSTCALL_FEATURE_SCHEMA_VERSION.to_string(),
            base_score: 0.0,
            recent_mean_score: 0.0,
            recent_error_rate: 0.0,
            burst_density_1s: 0.0,
            burst_density_10s: 0.0,
            prior_failure_streak_norm: 0.0,
            dangerous_capability: 0.0,
            timeout_requested: 0.0,
            policy_prompt_bias: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RuntimeHostcallTelemetryEvent {
    pub schema: String,
    pub ts_ms: i64,
    pub extension_id: String,
    pub call_id: String,
    pub capability: String,
    pub method: String,
    pub params_hash: String,
    pub args_shape_hash: String,
    pub resource_target_class: String,
    pub policy_reason: String,
    pub policy_profile: String,
    pub risk_score: f64,
    pub timeout_ms: Option<u64>,
    pub latency_ms: u64,
    /// Dispatch lane selected for this hostcall (`fast`, `compat`, or `unknown`).
    #[serde(default = "runtime_hostcall_lane_default")]
    pub lane: String,
    /// Deterministic lane decision reason code.
    #[serde(default)]
    pub lane_decision_reason: String,
    /// Fallback reason code when compat lane is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_fallback_reason: Option<String>,
    /// Lane matrix key (`method|opcode_or_fallback|capability_class`).
    #[serde(default)]
    pub lane_matrix_key: String,
    /// Portion of latency attributed to lane dispatch execution.
    #[serde(default)]
    pub lane_dispatch_latency_ms: u64,
    /// Lane dispatch share of total call latency in basis points (0..=10000).
    #[serde(default)]
    pub lane_latency_share_bps: u64,
    /// Marshalling path identifier (`interned_opcode_arena_v1`, `canonical_*`).
    #[serde(default = "runtime_hostcall_marshalling_path_default")]
    pub marshalling_path: String,
    /// Time spent in marshalling/hashing stage before dispatch.
    #[serde(default)]
    pub marshalling_latency_us: u64,
    /// Fallback reason when marshalling exits the fast opcode path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marshalling_fallback_reason: Option<String>,
    /// Per-extension running count of marshalling fast-path fallbacks.
    #[serde(default)]
    pub marshalling_fallback_count: u64,
    /// Signature of the recent opcode trace window used for superinstruction matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marshalling_superinstruction_trace_signature: Option<String>,
    /// Selected superinstruction plan id, when a fused plan hit is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marshalling_superinstruction_plan_id: Option<String>,
    /// Estimated cost reduction from selected superinstruction plan.
    #[serde(default)]
    pub marshalling_superinstruction_expected_cost_delta: i64,
    /// Observed/measured cost reduction for current call (or 0 when not applicable).
    #[serde(default)]
    pub marshalling_superinstruction_observed_cost_delta: i64,
    /// Deoptimization reason when superinstruction plan selection falls back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marshalling_superinstruction_deopt_reason: Option<String>,
    /// Whether the tier-2 trace-JIT dispatched this call.
    #[serde(default)]
    pub marshalling_superinstruction_jit_hit: bool,
    /// Tier-2 JIT cost improvement delta over tier-1 fused cost.
    #[serde(default)]
    pub marshalling_superinstruction_jit_cost_delta: i64,
    pub outcome: String,
    pub outcome_error_code: Option<String>,
    pub selected_action: RuntimeRiskActionValue,
    pub reason_codes: Vec<String>,
    pub explanation_level: RuntimeRiskExplanationLevelValue,
    pub explanation_summary: String,
    pub top_contributors: Vec<RuntimeRiskExplanationContributor>,
    pub budget_state: RuntimeRiskExplanationBudgetState,
    pub sequence: RuntimeHostcallSequenceContext,
    pub features: RuntimeHostcallFeatureVector,
    pub extraction_latency_us: u64,
    pub extraction_budget_us: u64,
    pub extraction_budget_exceeded: bool,
    pub redaction_summary: String,
}

fn runtime_hostcall_lane_default() -> String {
    "unknown".to_string()
}

fn runtime_hostcall_marshalling_path_default() -> String {
    HOSTCALL_MARSHALLING_PATH_CANONICAL_GENERIC.to_string()
}

impl Default for RuntimeHostcallTelemetryEvent {
    fn default() -> Self {
        Self {
            schema: RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION.to_string(),
            ts_ms: 0,
            extension_id: String::new(),
            call_id: String::new(),
            capability: String::new(),
            method: String::new(),
            params_hash: String::new(),
            args_shape_hash: String::new(),
            resource_target_class: "unknown".to_string(),
            policy_reason: String::new(),
            policy_profile: String::new(),
            risk_score: 0.0,
            timeout_ms: None,
            latency_ms: 0,
            lane: runtime_hostcall_lane_default(),
            lane_decision_reason: String::new(),
            lane_fallback_reason: None,
            lane_matrix_key: "unknown|fallback|unknown".to_string(),
            lane_dispatch_latency_ms: 0,
            lane_latency_share_bps: 0,
            marshalling_path: runtime_hostcall_marshalling_path_default(),
            marshalling_latency_us: 0,
            marshalling_fallback_reason: None,
            marshalling_fallback_count: 0,
            marshalling_superinstruction_trace_signature: None,
            marshalling_superinstruction_plan_id: None,
            marshalling_superinstruction_expected_cost_delta: 0,
            marshalling_superinstruction_observed_cost_delta: 0,
            marshalling_superinstruction_deopt_reason: None,
            marshalling_superinstruction_jit_hit: false,
            marshalling_superinstruction_jit_cost_delta: 0,
            outcome: "success".to_string(),
            outcome_error_code: None,
            selected_action: RuntimeRiskActionValue::Allow,
            reason_codes: Vec::new(),
            explanation_level: RuntimeRiskExplanationLevelValue::Standard,
            explanation_summary: "no explanation generated".to_string(),
            top_contributors: Vec::new(),
            budget_state: RuntimeRiskExplanationBudgetState::default(),
            sequence: RuntimeHostcallSequenceContext::default(),
            features: RuntimeHostcallFeatureVector::default(),
            extraction_latency_us: 0,
            extraction_budget_us: RUNTIME_HOSTCALL_FEATURE_BUDGET_US,
            extraction_budget_exceeded: false,
            redaction_summary: "params redacted via hash-only telemetry".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RuntimeHostcallTelemetryArtifact {
    pub schema: String,
    pub generated_at_ms: i64,
    pub entry_count: usize,
    pub entries: Vec<RuntimeHostcallTelemetryEvent>,
}

impl Default for RuntimeHostcallTelemetryArtifact {
    fn default() -> Self {
        Self {
            schema: RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION.to_string(),
            generated_at_ms: 0,
            entry_count: 0,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AdaptiveHostcallPolicyDiffConfig {
    pub min_sample_count: usize,
    pub min_matched_coverage_bps: u64,
    pub min_latency_improvement_bps: u64,
    pub max_compat_rate_increase_bps: u64,
    pub max_error_rate_increase_bps: u64,
    pub max_detailed_changes: usize,
}

impl Default for AdaptiveHostcallPolicyDiffConfig {
    fn default() -> Self {
        Self {
            min_sample_count: 5,
            min_matched_coverage_bps: 8_000,
            min_latency_improvement_bps: 500,
            max_compat_rate_increase_bps: 250,
            max_error_rate_increase_bps: 100,
            max_detailed_changes: 64,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdaptiveHostcallPolicyDiffVerdict {
    Accept,
    Monitor,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdaptiveHostcallPolicySampleSupport {
    pub baseline_samples: usize,
    pub candidate_samples: usize,
    pub matched_samples: usize,
    pub min_required_samples: usize,
    pub matched_coverage_bps: u64,
    pub min_matched_coverage_bps: u64,
    pub sufficient: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdaptiveHostcallPolicyTelemetryMetrics {
    pub sample_count: usize,
    pub fast_lane_count: u64,
    pub compat_lane_count: u64,
    pub unknown_lane_count: u64,
    pub fallback_count: u64,
    pub forced_compat_count: u64,
    pub error_count: u64,
    pub deny_or_terminate_count: u64,
    pub mean_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub mean_risk_score: f64,
    pub compat_rate_bps: u64,
    pub fallback_rate_bps: u64,
    pub error_rate_bps: u64,
    pub action_counts: BTreeMap<String, u64>,
    pub fallback_reason_counts: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdaptiveHostcallPolicyLatencyEffect {
    pub baseline_mean_latency_ms: u64,
    pub candidate_mean_latency_ms: u64,
    pub delta_ms: i64,
    pub delta_bps: i64,
    pub expected_effect: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdaptiveHostcallPolicyLaneChange {
    pub comparison_key: String,
    pub extension_id: String,
    pub capability: String,
    pub method: String,
    pub baseline_lane: String,
    pub candidate_lane: String,
    pub baseline_fallback_reason: Option<String>,
    pub candidate_fallback_reason: Option<String>,
    pub baseline_lane_decision_reason: String,
    pub candidate_lane_decision_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdaptiveHostcallPolicyActionChange {
    pub comparison_key: String,
    pub extension_id: String,
    pub capability: String,
    pub method: String,
    pub baseline_action: RuntimeRiskActionValue,
    pub candidate_action: RuntimeRiskActionValue,
    pub baseline_risk_score: f64,
    pub candidate_risk_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdaptiveHostcallPolicyThresholdChange {
    pub field: String,
    pub baseline_value: String,
    pub candidate_value: String,
    pub direction: String,
    pub risk_note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdaptiveHostcallPolicyRollbackCondition {
    pub code: String,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdaptiveHostcallPolicyDiffReport {
    pub schema: String,
    pub generated_at_ms: i64,
    pub baseline_policy_id: String,
    pub candidate_policy_id: String,
    pub baseline_source_schema: String,
    pub candidate_source_schema: String,
    pub verdict: AdaptiveHostcallPolicyDiffVerdict,
    pub reason_codes: Vec<String>,
    pub sample_support: AdaptiveHostcallPolicySampleSupport,
    pub baseline_metrics: AdaptiveHostcallPolicyTelemetryMetrics,
    pub candidate_metrics: AdaptiveHostcallPolicyTelemetryMetrics,
    pub latency_effect: AdaptiveHostcallPolicyLatencyEffect,
    pub risk_threshold_changes: Vec<AdaptiveHostcallPolicyThresholdChange>,
    pub lane_changes: Vec<AdaptiveHostcallPolicyLaneChange>,
    pub action_changes: Vec<AdaptiveHostcallPolicyActionChange>,
    pub rollback_conditions: Vec<AdaptiveHostcallPolicyRollbackCondition>,
}

pub struct AdaptiveHostcallPolicyDiffRequest<'a> {
    pub baseline_policy_id: &'a str,
    pub candidate_policy_id: &'a str,
    pub baseline_config: &'a RuntimeRiskConfig,
    pub candidate_config: &'a RuntimeRiskConfig,
    pub baseline_telemetry: &'a RuntimeHostcallTelemetryArtifact,
    pub candidate_telemetry: &'a RuntimeHostcallTelemetryArtifact,
    pub config: &'a AdaptiveHostcallPolicyDiffConfig,
    pub generated_at_ms: i64,
}

// ==========================================================================
// SEC-5.1: Security alert types and alert stream
// ==========================================================================

/// Category of a security alert, enabling consumers to distinguish policy
/// denials from anomaly-based denials at a glance.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SecurityAlertCategory {
    /// Denied by static capability policy (deny_caps, per-extension deny).
    PolicyDenial,
    /// Denied or hardened by the runtime risk scorer (anomaly detection).
    AnomalyDenial,
    /// Exec mediation blocked a dangerous command.
    ExecMediation,
    /// Secret broker detected or redacted a sensitive environment variable.
    SecretBroker,
    /// Quota limit was breached (rate or resource).
    QuotaBreach,
    /// Enforcement state machine escalated to terminate/quarantine.
    Quarantine,
    /// Profile transition was attempted (e.g. downgrade).
    ProfileTransition,
}

/// Severity level for security alerts, ordered from lowest to highest.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SecurityAlertSeverity {
    /// Informational — no action required.
    Info,
    /// Warning — user should review.
    Warning,
    /// Error — action was blocked.
    Error,
    /// Critical — extension quarantined or terminated.
    Critical,
}

/// A structured security alert with who/what/why/action fields.
///
/// Designed for both interactive display and downstream integrations (RPC,
/// SIEM, structured logging).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecurityAlert {
    /// Schema version tag for stable deserialization.
    pub schema: String,
    /// Unix epoch milliseconds when the alert was generated.
    pub ts_ms: i64,
    /// Monotonically increasing alert sequence number.
    pub sequence_id: u64,

    // -- WHO --
    /// Extension that triggered the alert (empty for global events).
    pub extension_id: String,

    // -- WHAT --
    /// Alert category for quick classification.
    pub category: SecurityAlertCategory,
    /// Severity level.
    pub severity: SecurityAlertSeverity,
    /// Capability involved (e.g. "exec", "env", "http").
    pub capability: String,
    /// Method or sub-operation (e.g. "spawn", "get", "set").
    pub method: String,

    // -- WHY --
    /// Structured reason codes (machine-readable).
    pub reason_codes: Vec<String>,
    /// Human-readable summary of why the alert was raised.
    pub summary: String,
    /// Policy source that caused the decision (e.g. "deny_caps",
    /// "exec_mediation", "risk_scorer", "quota").
    pub policy_source: String,

    // -- ACTION --
    /// Enforcement action taken.
    pub action: SecurityAlertAction,
    /// Suggested remediation for the user.
    pub remediation: String,

    // -- CONTEXT --
    /// Risk score at the time of the alert (0.0 if not applicable).
    pub risk_score: f64,
    /// Derived risk state label (if from risk scorer).
    pub risk_state: Option<RuntimeRiskStateLabelValue>,
    /// Hash of the related command or params (redacted).
    pub context_hash: String,
}

/// Action taken in response to a security event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecurityAlertAction {
    /// Request was allowed.
    Allow,
    /// Request was allowed but with extra logging/auditing.
    Harden,
    /// User was prompted for approval.
    Prompt,
    /// Request was denied.
    Deny,
    /// Extension was quarantined/terminated.
    Terminate,
    /// Sensitive value was redacted.
    Redact,
}

/// Container artifact for a stream of security alerts, suitable for export
/// and downstream integration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecurityAlertArtifact {
    /// Schema version tag.
    pub schema: String,
    /// Unix epoch milliseconds when the artifact was generated.
    pub generated_at_ms: i64,
    /// Total number of alerts in this artifact.
    pub alert_count: usize,
    /// Summary counts by category.
    pub category_counts: SecurityAlertCategoryCounts,
    /// Summary counts by severity.
    pub severity_counts: SecurityAlertSeverityCounts,
    /// The alert entries.
    pub alerts: Vec<SecurityAlert>,
}

/// Per-category alert counts for quick triage.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityAlertCategoryCounts {
    pub policy_denial: usize,
    pub anomaly_denial: usize,
    pub exec_mediation: usize,
    pub secret_broker: usize,
    pub quota_breach: usize,
    pub quarantine: usize,
    pub profile_transition: usize,
}

/// Per-severity alert counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityAlertSeverityCounts {
    pub info: usize,
    pub warning: usize,
    pub error: usize,
    pub critical: usize,
}

impl SecurityAlertCategoryCounts {
    pub const fn increment(&mut self, cat: SecurityAlertCategory) {
        match cat {
            SecurityAlertCategory::PolicyDenial => self.policy_denial += 1,
            SecurityAlertCategory::AnomalyDenial => self.anomaly_denial += 1,
            SecurityAlertCategory::ExecMediation => self.exec_mediation += 1,
            SecurityAlertCategory::SecretBroker => self.secret_broker += 1,
            SecurityAlertCategory::QuotaBreach => self.quota_breach += 1,
            SecurityAlertCategory::Quarantine => self.quarantine += 1,
            SecurityAlertCategory::ProfileTransition => self.profile_transition += 1,
        }
    }
}

impl SecurityAlertSeverityCounts {
    pub const fn increment(&mut self, sev: SecurityAlertSeverity) {
        match sev {
            SecurityAlertSeverity::Info => self.info += 1,
            SecurityAlertSeverity::Warning => self.warning += 1,
            SecurityAlertSeverity::Error => self.error += 1,
            SecurityAlertSeverity::Critical => self.critical += 1,
        }
    }
}

// ---------------------------------------------------------------------------
// SEC-5.3: Incident Evidence Bundle – filter, redaction, and bundle types
// ---------------------------------------------------------------------------

/// Filter criteria for scoping an incident evidence bundle.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncidentBundleFilter {
    /// If set, only include entries with `ts_ms >= start_ms`.
    pub start_ms: Option<i64>,
    /// If set, only include entries with `ts_ms <= end_ms`.
    pub end_ms: Option<i64>,
    /// If set, only include entries matching this extension id.
    pub extension_id: Option<String>,
    /// If set, only include alerts of these categories.
    pub alert_categories: Option<Vec<SecurityAlertCategory>>,
    /// If set, only include alerts at or above this severity.
    pub min_severity: Option<SecurityAlertSeverity>,
}

/// Redaction policy applied when exporting a bundle.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncidentBundleRedactionPolicy {
    /// Redact `params_hash` fields (default true).
    pub redact_params_hash: bool,
    /// Redact `context_hash` fields (default true).
    pub redact_context_hash: bool,
    /// Redact `args_shape_hash` fields (default true).
    pub redact_args_shape_hash: bool,
    /// Redact `command_hash` in exec mediation entries (default true).
    pub redact_command_hash: bool,
    /// Redact `name_hash` in secret broker entries (default true).
    pub redact_name_hash: bool,
    /// Redact remediation text in alerts (default false).
    pub redact_remediation: bool,
}

impl Default for IncidentBundleRedactionPolicy {
    fn default() -> Self {
        Self {
            redact_params_hash: true,
            redact_context_hash: true,
            redact_args_shape_hash: true,
            redact_command_hash: true,
            redact_name_hash: true,
            redact_remediation: false,
        }
    }
}

/// A self-contained incident evidence bundle containing all security artifacts
/// for a filtered scope. Deterministic for the same scope and data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IncidentEvidenceBundle {
    /// Schema version tag.
    pub schema: String,
    /// Unix epoch milliseconds when the bundle was generated.
    pub generated_at_ms: i64,
    /// SHA-256 hash of the serialised content sections (integrity seal).
    pub bundle_hash: String,
    /// Filter that was applied to produce this bundle.
    pub filter: IncidentBundleFilter,
    /// Redaction policy that was applied.
    pub redaction: IncidentBundleRedactionPolicy,
    /// Runtime risk decision ledger (hash-chained).
    pub risk_ledger: RuntimeRiskLedgerArtifact,
    /// Security alerts matching the filter.
    pub security_alerts: SecurityAlertArtifact,
    /// Hostcall telemetry events matching the filter.
    pub hostcall_telemetry: RuntimeHostcallTelemetryArtifact,
    /// Exec mediation decisions matching the filter.
    pub exec_mediation: ExecMediationArtifact,
    /// Secret broker decisions matching the filter.
    pub secret_broker: SecretBrokerArtifact,
    /// Quota breach events matching the filter.
    pub quota_breaches: Vec<QuotaBreachEvent>,
    /// Forensic replay steps derived from the filtered ledger.
    pub risk_replay: Option<RuntimeRiskReplayArtifact>,
    /// Summary statistics for quick triage.
    pub summary: IncidentBundleSummary,
}

/// High-level summary statistics for an incident evidence bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IncidentBundleSummary {
    /// Total ledger entries in the bundle.
    pub ledger_entry_count: usize,
    /// Total alerts in the bundle.
    pub alert_count: usize,
    /// Total telemetry events in the bundle.
    pub telemetry_event_count: usize,
    /// Total exec mediation entries.
    pub exec_mediation_count: usize,
    /// Total secret broker entries.
    pub secret_broker_count: usize,
    /// Total quota breach events.
    pub quota_breach_count: usize,
    /// Number of distinct extensions in scope.
    pub distinct_extensions: usize,
    /// Peak risk score observed in the ledger slice.
    pub peak_risk_score: f64,
    /// Count of deny/terminate actions in the ledger.
    pub deny_or_terminate_count: usize,
    /// Whether the ledger hash chain is intact.
    pub ledger_chain_intact: bool,
}

/// Verification report for an incident evidence bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncidentBundleVerificationReport {
    /// Whether the bundle passes all integrity checks.
    pub valid: bool,
    /// The bundle hash that was checked.
    pub bundle_hash: String,
    /// Recomputed hash (should match bundle_hash if valid).
    pub recomputed_hash: String,
    /// Schema check result.
    pub schema_valid: bool,
    /// Ledger chain integrity result.
    pub ledger_chain_intact: bool,
    /// List of integrity errors found (empty if valid).
    pub errors: Vec<String>,
}

impl SecurityAlertAction {
    /// Convert from an [`EnforcementState`].
    pub const fn from_enforcement(state: EnforcementState) -> Self {
        match state {
            EnforcementState::Allow => Self::Allow,
            EnforcementState::Harden => Self::Harden,
            EnforcementState::Prompt => Self::Prompt,
            EnforcementState::Deny => Self::Deny,
            EnforcementState::Terminate => Self::Terminate,
        }
    }

    /// String representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Harden => "harden",
            Self::Prompt => "prompt",
            Self::Deny => "deny",
            Self::Terminate => "terminate",
            Self::Redact => "redact",
        }
    }
}

impl SecurityAlert {
    /// Create a policy-denial alert.
    pub fn from_policy_denial(
        extension_id: &str,
        capability: &str,
        method: &str,
        reason: &str,
        policy_source: &str,
    ) -> Self {
        Self {
            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
            ts_ms: i64::try_from(wall_now().as_millis()).unwrap_or(i64::MAX),
            sequence_id: 0,
            extension_id: extension_id.to_string(),
            category: SecurityAlertCategory::PolicyDenial,
            severity: SecurityAlertSeverity::Error,
            capability: capability.to_string(),
            method: method.to_string(),
            reason_codes: vec![reason.to_string()],
            summary: format!(
                "Capability `{capability}` denied for extension `{extension_id}` by {policy_source}"
            ),
            policy_source: policy_source.to_string(),
            action: SecurityAlertAction::Deny,
            remediation: format!(
                "Use `--extension-policy permissive` or grant `{capability}` via per-extension override."
            ),
            risk_score: 0.0,
            risk_state: None,
            context_hash: String::new(),
        }
    }

    /// Create an exec-mediation alert.
    pub fn from_exec_mediation(
        extension_id: &str,
        command: &str,
        class_label: Option<&str>,
        reason: &str,
    ) -> Self {
        let summary = class_label.map_or_else(
            || format!("Command blocked by exec mediation deny pattern: {reason}"),
            |label| format!("Command classified as `{label}` and blocked by exec mediation"),
        );
        Self {
            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
            ts_ms: i64::try_from(wall_now().as_millis()).unwrap_or(i64::MAX),
            sequence_id: 0,
            extension_id: extension_id.to_string(),
            category: SecurityAlertCategory::ExecMediation,
            severity: SecurityAlertSeverity::Error,
            capability: "exec".to_string(),
            method: "spawn".to_string(),
            reason_codes: vec![reason.to_string()],
            summary,
            policy_source: "exec_mediation".to_string(),
            action: SecurityAlertAction::Deny,
            remediation: "Add the command to `exec_mediation.allow_patterns` if this is expected."
                .to_string(),
            risk_score: 0.0,
            risk_state: None,
            context_hash: sha256_short(command),
        }
    }

    /// Create a secret-broker redaction alert.
    pub fn from_secret_redaction(extension_id: &str, var_name: &str) -> Self {
        Self {
            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
            ts_ms: i64::try_from(wall_now().as_millis()).unwrap_or(i64::MAX),
            sequence_id: 0,
            extension_id: extension_id.to_string(),
            category: SecurityAlertCategory::SecretBroker,
            severity: SecurityAlertSeverity::Info,
            capability: "env".to_string(),
            method: "get".to_string(),
            reason_codes: vec!["secret_redacted".to_string()],
            summary: format!("Environment variable `{var_name}` redacted by secret broker"),
            policy_source: "secret_broker".to_string(),
            action: SecurityAlertAction::Redact,
            remediation: "Add to `secret_broker.disclosure_allowlist` if disclosure is safe."
                .to_string(),
            risk_score: 0.0,
            risk_state: None,
            context_hash: sha256_short(var_name),
        }
    }

    /// Create a risk-scorer anomaly alert.
    #[allow(clippy::too_many_arguments)]
    pub fn from_anomaly_detection(
        extension_id: &str,
        capability: &str,
        method: &str,
        risk_score: f64,
        risk_state: RuntimeRiskStateLabelValue,
        enforcement_action: SecurityAlertAction,
        reason_codes: Vec<String>,
        summary: String,
    ) -> Self {
        let severity = match enforcement_action {
            SecurityAlertAction::Terminate => SecurityAlertSeverity::Critical,
            SecurityAlertAction::Deny => SecurityAlertSeverity::Error,
            SecurityAlertAction::Harden | SecurityAlertAction::Prompt => {
                SecurityAlertSeverity::Warning
            }
            _ => SecurityAlertSeverity::Info,
        };
        Self {
            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
            ts_ms: i64::try_from(wall_now().as_millis()).unwrap_or(i64::MAX),
            sequence_id: 0,
            extension_id: extension_id.to_string(),
            category: SecurityAlertCategory::AnomalyDenial,
            severity,
            capability: capability.to_string(),
            method: method.to_string(),
            reason_codes,
            summary,
            policy_source: "risk_scorer".to_string(),
            action: enforcement_action,
            remediation:
                "Review the extension's recent behavior. Restart the session to clear risk state."
                    .to_string(),
            risk_score,
            risk_state: Some(risk_state),
            context_hash: String::new(),
        }
    }

    /// Create a quarantine alert.
    pub fn from_quarantine(extension_id: &str, reason: &str, risk_score: f64) -> Self {
        Self {
            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
            ts_ms: i64::try_from(wall_now().as_millis()).unwrap_or(i64::MAX),
            sequence_id: 0,
            extension_id: extension_id.to_string(),
            category: SecurityAlertCategory::Quarantine,
            severity: SecurityAlertSeverity::Critical,
            capability: String::new(),
            method: String::new(),
            reason_codes: vec![reason.to_string()],
            summary: format!("Extension `{extension_id}` quarantined: {reason}"),
            policy_source: "enforcement_state_machine".to_string(),
            action: SecurityAlertAction::Terminate,
            remediation: "Restart the session to re-enable the extension.".to_string(),
            risk_score,
            risk_state: None,
            context_hash: String::new(),
        }
    }

    /// Create an enforcement state transition alert.
    pub fn from_enforcement_transition(
        extension_id: &str,
        transition: &EnforcementTransition,
    ) -> Self {
        let severity = match transition.to {
            EnforcementState::Terminate => SecurityAlertSeverity::Critical,
            EnforcementState::Deny => SecurityAlertSeverity::Error,
            EnforcementState::Prompt | EnforcementState::Harden => SecurityAlertSeverity::Warning,
            EnforcementState::Allow => SecurityAlertSeverity::Info,
        };
        Self {
            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
            ts_ms: i64::try_from(wall_now().as_millis()).unwrap_or(i64::MAX),
            sequence_id: 0,
            extension_id: extension_id.to_string(),
            category: SecurityAlertCategory::ProfileTransition,
            severity,
            capability: String::new(),
            method: String::new(),
            reason_codes: vec![format!(
                "enforcement_transition:{}->{}",
                transition.from.as_str(),
                transition.to.as_str()
            )],
            summary: format!(
                "Enforcement state changed from `{}` to `{}` (score: {:.2})",
                transition.from, transition.to, transition.score
            ),
            policy_source: "enforcement_state_machine".to_string(),
            action: SecurityAlertAction::from_enforcement(transition.to),
            remediation: if transition.to > EnforcementState::Harden {
                "Review extension behavior. Restart session to reset enforcement state.".to_string()
            } else {
                String::new()
            },
            risk_score: transition.score,
            risk_state: None,
            context_hash: String::new(),
        }
    }
}

/// Compute a short hash for context identification.
fn sha256_short(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    // Return first 16 hex chars (8 bytes) of the SHA-256 hash
    let mut hex = String::with_capacity(16);
    for byte in &result[..8] {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// Record a security alert and emit a tracing event at the appropriate
/// level.
///
/// This is the primary entry point for all security alert emission.
pub fn emit_security_alert(manager: &ExtensionManager, alert: SecurityAlert) {
    match alert.severity {
        SecurityAlertSeverity::Critical => {
            tracing::error!(
                category = %serde_json::to_string(&alert.category).unwrap_or_else(|_| format!("{:?}", alert.category)),
                extension_id = %alert.extension_id,
                capability = %alert.capability,
                action = %serde_json::to_string(&alert.action).unwrap_or_else(|_| format!("{:?}", alert.action)),
                risk_score = alert.risk_score,
                "SECURITY ALERT: {}",
                alert.summary
            );
        }
        SecurityAlertSeverity::Error => {
            tracing::warn!(
                category = %serde_json::to_string(&alert.category).unwrap_or_else(|_| format!("{:?}", alert.category)),
                extension_id = %alert.extension_id,
                capability = %alert.capability,
                action = %serde_json::to_string(&alert.action).unwrap_or_else(|_| format!("{:?}", alert.action)),
                "Security alert: {}",
                alert.summary
            );
        }
        SecurityAlertSeverity::Warning => {
            tracing::info!(
                category = %serde_json::to_string(&alert.category).unwrap_or_else(|_| format!("{:?}", alert.category)),
                extension_id = %alert.extension_id,
                capability = %alert.capability,
                "Security notice: {}",
                alert.summary
            );
        }
        SecurityAlertSeverity::Info => {
            tracing::debug!(
                extension_id = %alert.extension_id,
                capability = %alert.capability,
                "Security info: {}",
                alert.summary
            );
        }
    }
    manager.record_security_alert(alert);
}

/// Query the alert stream with optional filters.
pub fn query_security_alerts(
    manager: &ExtensionManager,
    filter: &SecurityAlertFilter,
) -> Vec<SecurityAlert> {
    let artifact = manager.security_alert_artifact();
    artifact
        .alerts
        .into_iter()
        .filter(|a| {
            if let Some(cat) = &filter.category
                && a.category != *cat
            {
                return false;
            }
            if let Some(sev) = &filter.min_severity
                && a.severity < *sev
            {
                return false;
            }
            if let Some(ext) = &filter.extension_id
                && a.extension_id != *ext
            {
                return false;
            }
            if let Some(after) = filter.after_ts_ms
                && a.ts_ms < after
            {
                return false;
            }
            true
        })
        .collect()
}

/// Filter criteria for querying security alerts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityAlertFilter {
    /// Only return alerts of this category.
    pub category: Option<SecurityAlertCategory>,
    /// Only return alerts at or above this severity.
    pub min_severity: Option<SecurityAlertSeverity>,
    /// Only return alerts for this extension.
    pub extension_id: Option<String>,
    /// Only return alerts after this timestamp (ms).
    pub after_ts_ms: Option<i64>,
}

// ------------------------------------------------------------------
// SEC-5.2: Kill-switch and trust onboarding
// ------------------------------------------------------------------

/// Trust state for an extension.
///
/// Tracks the trust lifecycle from initial onboarding through
/// full trust or quarantine.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionTrustState {
    /// Extension installed but user has not acknowledged risk.
    Pending,
    /// User acknowledged risk, extension runs with monitoring.
    Acknowledged,
    /// Extension demonstrated safe behavior over time.
    Trusted,
    /// Extension killed via manual kill-switch or auto-quarantine.
    Killed,
}

impl std::fmt::Display for ExtensionTrustState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => f.write_str("pending"),
            Self::Acknowledged => f.write_str("acknowledged"),
            Self::Trusted => f.write_str("trusted"),
            Self::Killed => f.write_str("killed"),
        }
    }
}

/// Audit entry for a kill-switch activation or deactivation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitchAuditEntry {
    /// Timestamp (ms since epoch).
    pub ts_ms: i64,
    /// Extension that was killed or revived.
    pub extension_id: String,
    /// Whether this was an activation (`true`) or deactivation (`false`).
    pub activated: bool,
    /// Reason provided by the operator.
    pub reason: String,
    /// Who triggered the kill-switch (e.g. `"user"`, `"system"`, agent name).
    pub operator: String,
    /// Trust state before the action.
    pub previous_state: ExtensionTrustState,
    /// Trust state after the action.
    pub new_state: ExtensionTrustState,
}

/// Audit entry for a trust onboarding decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustOnboardingDecision {
    /// Timestamp (ms since epoch).
    pub ts_ms: i64,
    /// Extension whose trust level was decided.
    pub extension_id: String,
    /// Risk level the user acknowledged (e.g. `"high"`, `"medium"`, `"low"`).
    pub acknowledged_risk_level: String,
    /// Whether the user accepted or rejected the extension.
    pub accepted: bool,
    /// Who made the decision.
    pub operator: String,
    /// Resulting trust state.
    pub resulting_state: ExtensionTrustState,
}

/// Result of a kill-switch operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitchResult {
    /// Whether the operation succeeded.
    pub success: bool,
    /// Previous trust state.
    pub previous_state: ExtensionTrustState,
    /// New trust state.
    pub new_state: ExtensionTrustState,
    /// Explanation if operation was a no-op or failed.
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RuntimeRiskStateLabel {
    SafeFast,
    Suspicious,
    Unsafe,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RuntimeRiskAction {
    Allow,
    Harden,
    Deny,
    Terminate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeRiskPosterior {
    safe_fast: f64,
    suspicious: f64,
    unsafe_: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeRiskExpectedLoss {
    allow: f64,
    harden: f64,
    deny: f64,
    terminate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeRiskLedgerEntry {
    ts_ms: i64,
    extension_id: String,
    call_id: String,
    capability: String,
    method: String,
    params_hash: String,
    policy_reason: String,
    risk_score: f64,
    posterior: RuntimeRiskPosterior,
    expected_loss: RuntimeRiskExpectedLoss,
    selected_action: RuntimeRiskAction,
    derived_state: RuntimeRiskStateLabel,
    triggers: Vec<String>,
    fallback_reason: Option<String>,
    e_process: f64,
    e_threshold: f64,
    conformal_residual: f64,
    conformal_quantile: f64,
    drift_detected: bool,
    outcome_error_code: Option<String>,
    explanation_schema: String,
    explanation_level: RuntimeRiskExplanationLevelValue,
    explanation_summary: String,
    top_contributors: Vec<RuntimeRiskExplanationContributor>,
    budget_state: RuntimeRiskExplanationBudgetState,
    ledger_hash: String,
    prev_ledger_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct RuntimeRiskDecision {
    action: RuntimeRiskAction,
    reason: String,
    capability: String,
    method: String,
    params_hash: String,
    args_shape_hash: String,
    resource_target_class: String,
    policy_profile: String,
    timeout_ms: Option<u64>,
    risk_score: f64,
    posterior: RuntimeRiskPosterior,
    expected_loss: RuntimeRiskExpectedLoss,
    e_process: f64,
    e_threshold: f64,
    // Decision-time conformal values are retained for replay diagnostics; the
    // outcome ledger records the realized residual after dispatch.
    #[allow(dead_code)]
    conformal_residual: f64,
    #[allow(dead_code)]
    conformal_quantile: f64,
    drift_detected: bool,
    triggers: Vec<String>,
    explanation_schema: String,
    explanation_level: RuntimeRiskExplanationLevelValue,
    explanation_summary: String,
    top_contributors: Vec<RuntimeRiskExplanationContributor>,
    budget_state: RuntimeRiskExplanationBudgetState,
    fallback_reason: Option<String>,
    // Measures decision budget pressure; current telemetry persists the
    // extraction and dispatch latencies separately.
    #[allow(dead_code)]
    elapsed_ms: u64,
    state_label: RuntimeRiskStateLabel,
    sequence_context: RuntimeHostcallSequenceContext,
    features: RuntimeHostcallFeatureVector,
    feature_extraction_latency_us: u64,
    feature_budget_exceeded: bool,
}

#[derive(Debug, Clone, Copy)]
struct RuntimeRiskCallMetadata<'a> {
    args_shape_hash: &'a str,
    resource_target_class: &'a str,
    params: &'a Value,
    timeout_ms: Option<u64>,
    policy_profile: &'a str,
}

#[derive(Debug, Clone)]
struct RuntimeRiskState {
    alpha_safe: f64,
    alpha_suspicious: f64,
    alpha_unsafe: f64,
    log_e_process: f64,
    recent_scores: VecDeque<f64>,
    recent_call_timestamps_ms: VecDeque<i64>,
    recent_outcome_errors: VecDeque<bool>,
    residual_window: VecDeque<f64>,
    previous_residual_quantile: f64,
    consecutive_unsafe: u32,
    consecutive_failures: u32,
    quarantined: bool,
    last_decision: Option<RuntimeRiskAction>,
    last_capability: Option<String>,
    last_method: Option<String>,
    last_resource_target_class: Option<String>,
    sequence_counter: u64,
}

impl Default for RuntimeRiskState {
    fn default() -> Self {
        Self {
            alpha_safe: 8.0,
            alpha_suspicious: 1.5,
            alpha_unsafe: 0.5,
            log_e_process: 0.0,
            recent_scores: VecDeque::new(),
            recent_call_timestamps_ms: VecDeque::new(),
            recent_outcome_errors: VecDeque::new(),
            residual_window: VecDeque::new(),
            previous_residual_quantile: 0.0,
            consecutive_unsafe: 0,
            consecutive_failures: 0,
            quarantined: false,
            last_decision: None,
            last_capability: None,
            last_method: None,
            last_resource_target_class: None,
            sequence_counter: 0,
        }
    }
}

impl From<RuntimeRiskStateLabel> for RuntimeRiskStateLabelValue {
    fn from(value: RuntimeRiskStateLabel) -> Self {
        match value {
            RuntimeRiskStateLabel::SafeFast => Self::SafeFast,
            RuntimeRiskStateLabel::Suspicious => Self::Suspicious,
            RuntimeRiskStateLabel::Unsafe => Self::Unsafe,
        }
    }
}

impl From<RuntimeRiskStateLabelValue> for RuntimeRiskStateLabel {
    fn from(value: RuntimeRiskStateLabelValue) -> Self {
        match value {
            RuntimeRiskStateLabelValue::SafeFast => Self::SafeFast,
            RuntimeRiskStateLabelValue::Suspicious => Self::Suspicious,
            RuntimeRiskStateLabelValue::Unsafe => Self::Unsafe,
        }
    }
}

impl From<RuntimeRiskAction> for RuntimeRiskActionValue {
    fn from(value: RuntimeRiskAction) -> Self {
        match value {
            RuntimeRiskAction::Allow => Self::Allow,
            RuntimeRiskAction::Harden => Self::Harden,
            RuntimeRiskAction::Deny => Self::Deny,
            RuntimeRiskAction::Terminate => Self::Terminate,
        }
    }
}

impl From<RuntimeRiskActionValue> for RuntimeRiskAction {
    fn from(value: RuntimeRiskActionValue) -> Self {
        match value {
            RuntimeRiskActionValue::Allow => Self::Allow,
            RuntimeRiskActionValue::Harden => Self::Harden,
            RuntimeRiskActionValue::Deny => Self::Deny,
            RuntimeRiskActionValue::Terminate => Self::Terminate,
        }
    }
}

impl From<&RuntimeRiskPosterior> for RuntimeRiskPosteriorEvidence {
    fn from(value: &RuntimeRiskPosterior) -> Self {
        Self {
            safe_fast: value.safe_fast,
            suspicious: value.suspicious,
            unsafe_: value.unsafe_,
        }
    }
}

impl From<&RuntimeRiskExpectedLoss> for RuntimeRiskExpectedLossEvidence {
    fn from(value: &RuntimeRiskExpectedLoss) -> Self {
        Self {
            allow: value.allow,
            harden: value.harden,
            deny: value.deny,
            terminate: value.terminate,
        }
    }
}

impl From<&RuntimeRiskLedgerEntry> for RuntimeRiskLedgerArtifactEntry {
    fn from(value: &RuntimeRiskLedgerEntry) -> Self {
        Self {
            ts_ms: value.ts_ms,
            extension_id: value.extension_id.clone(),
            call_id: value.call_id.clone(),
            capability: value.capability.clone(),
            method: value.method.clone(),
            params_hash: value.params_hash.clone(),
            policy_reason: value.policy_reason.clone(),
            risk_score: value.risk_score,
            posterior: RuntimeRiskPosteriorEvidence::from(&value.posterior),
            expected_loss: RuntimeRiskExpectedLossEvidence::from(&value.expected_loss),
            selected_action: RuntimeRiskActionValue::from(value.selected_action),
            derived_state: RuntimeRiskStateLabelValue::from(value.derived_state),
            triggers: value.triggers.clone(),
            fallback_reason: value.fallback_reason.clone(),
            e_process: value.e_process,
            e_threshold: value.e_threshold,
            conformal_residual: value.conformal_residual,
            conformal_quantile: value.conformal_quantile,
            drift_detected: value.drift_detected,
            outcome_error_code: value.outcome_error_code.clone(),
            explanation_schema: value.explanation_schema.clone(),
            explanation_level: value.explanation_level,
            explanation_summary: value.explanation_summary.clone(),
            top_contributors: value.top_contributors.clone(),
            budget_state: value.budget_state.clone(),
            ledger_hash: value.ledger_hash.clone(),
            prev_ledger_hash: value.prev_ledger_hash.clone(),
        }
    }
}

impl From<&RuntimeRiskLedgerArtifactEntry> for RuntimeRiskLedgerEntry {
    fn from(value: &RuntimeRiskLedgerArtifactEntry) -> Self {
        Self {
            ts_ms: value.ts_ms,
            extension_id: value.extension_id.clone(),
            call_id: value.call_id.clone(),
            capability: value.capability.clone(),
            method: value.method.clone(),
            params_hash: value.params_hash.clone(),
            policy_reason: value.policy_reason.clone(),
            risk_score: value.risk_score,
            posterior: RuntimeRiskPosterior {
                safe_fast: value.posterior.safe_fast,
                suspicious: value.posterior.suspicious,
                unsafe_: value.posterior.unsafe_,
            },
            expected_loss: RuntimeRiskExpectedLoss {
                allow: value.expected_loss.allow,
                harden: value.expected_loss.harden,
                deny: value.expected_loss.deny,
                terminate: value.expected_loss.terminate,
            },
            selected_action: RuntimeRiskAction::from(value.selected_action),
            derived_state: RuntimeRiskStateLabel::from(value.derived_state),
            triggers: value.triggers.clone(),
            fallback_reason: value.fallback_reason.clone(),
            e_process: value.e_process,
            e_threshold: value.e_threshold,
            conformal_residual: value.conformal_residual,
            conformal_quantile: value.conformal_quantile,
            drift_detected: value.drift_detected,
            outcome_error_code: value.outcome_error_code.clone(),
            explanation_schema: value.explanation_schema.clone(),
            explanation_level: value.explanation_level,
            explanation_summary: value.explanation_summary.clone(),
            top_contributors: value.top_contributors.clone(),
            budget_state: value.budget_state.clone(),
            ledger_hash: value.ledger_hash.clone(),
            prev_ledger_hash: value.prev_ledger_hash.clone(),
        }
    }
}

#[cfg(test)]
thread_local! {
    static RUNTIME_RISK_TEST_NOW_MS: std::cell::Cell<Option<i64>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
struct RuntimeRiskTestClockGuard {
    previous: Option<i64>,
}

#[cfg(test)]
impl RuntimeRiskTestClockGuard {
    fn set(now_ms: i64) -> Self {
        let previous = RUNTIME_RISK_TEST_NOW_MS.with(|slot| {
            let previous = slot.get();
            slot.set(Some(now_ms));
            previous
        });
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for RuntimeRiskTestClockGuard {
    fn drop(&mut self) {
        RUNTIME_RISK_TEST_NOW_MS.with(|slot| slot.set(self.previous));
    }
}

fn runtime_risk_now_ms() -> i64 {
    #[cfg(test)]
    if let Some(now_ms) = RUNTIME_RISK_TEST_NOW_MS.with(std::cell::Cell::get) {
        return now_ms;
    }

    i64::try_from(wall_now().as_millis()).unwrap_or(i64::MAX)
}

#[allow(clippy::missing_const_for_fn)]
fn runtime_risk_clamp01(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// SEC-3.4: Enforcement State Machine with Hysteresis
// ---------------------------------------------------------------------------

/// Enforcement states ordered by severity.
///
/// Allow < Harden < Prompt < Deny < Terminate. `Prompt` sits between
/// `Harden` and `Deny`: the action is not blocked, but the user / operator
/// must explicitly approve continued execution.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementState {
    Allow = 0,
    Harden = 1,
    Prompt = 2,
    Deny = 3,
    Terminate = 4,
}

impl EnforcementState {
    /// Convert from a [`RuntimeRiskAction`] (which lacks `Prompt`).
    #[cfg(test)]
    const fn from_risk_action(action: RuntimeRiskAction) -> Self {
        match action {
            RuntimeRiskAction::Allow => Self::Allow,
            RuntimeRiskAction::Harden => Self::Harden,
            RuntimeRiskAction::Deny => Self::Deny,
            RuntimeRiskAction::Terminate => Self::Terminate,
        }
    }

    /// Map back to the nearest [`RuntimeRiskAction`] (Prompt → Harden).
    #[cfg(test)]
    const fn to_risk_action(self) -> RuntimeRiskAction {
        match self {
            Self::Allow => RuntimeRiskAction::Allow,
            Self::Harden | Self::Prompt => RuntimeRiskAction::Harden,
            Self::Deny => RuntimeRiskAction::Deny,
            Self::Terminate => RuntimeRiskAction::Terminate,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Harden => "harden",
            Self::Prompt => "prompt",
            Self::Deny => "deny",
            Self::Terminate => "terminate",
        }
    }
}

impl std::fmt::Display for EnforcementState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Score band thresholds for each enforcement state. A score *at or above*
/// the threshold triggers that state. Thresholds must satisfy
/// `allow < harden < prompt < deny < terminate`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EnforcementScoreBands {
    /// Scores below this are Allow (always 0.0 for completeness).
    pub allow: f64,
    /// Scores at or above this trigger Harden.
    pub harden: f64,
    /// Scores at or above this trigger Prompt.
    pub prompt: f64,
    /// Scores at or above this trigger Deny.
    pub deny: f64,
    /// Scores at or above this trigger Terminate.
    pub terminate: f64,
}

impl EnforcementScoreBands {
    /// Score bands for the `safe` policy profile.
    /// More aggressive: lower thresholds = quicker escalation.
    pub const fn safe() -> Self {
        Self {
            allow: 0.0,
            harden: 0.30,
            prompt: 0.50,
            deny: 0.65,
            terminate: 0.80,
        }
    }

    /// Score bands for the `balanced` (standard) policy profile.
    pub const fn balanced() -> Self {
        Self {
            allow: 0.0,
            harden: 0.40,
            prompt: 0.60,
            deny: 0.75,
            terminate: 0.90,
        }
    }

    /// Score bands for the `permissive` policy profile.
    /// More tolerant: higher thresholds = slower escalation.
    pub const fn permissive() -> Self {
        Self {
            allow: 0.0,
            harden: 0.55,
            prompt: 0.70,
            deny: 0.85,
            terminate: 0.95,
        }
    }

    /// Select score bands for a named profile.
    pub fn for_profile(profile: &str) -> Self {
        match profile {
            "safe" | "strict" => Self::safe(),
            "permissive" => Self::permissive(),
            _ => Self::balanced(),
        }
    }

    /// Map a risk score to the corresponding enforcement state.
    pub fn classify(&self, score: f64) -> EnforcementState {
        if score >= self.terminate {
            EnforcementState::Terminate
        } else if score >= self.deny {
            EnforcementState::Deny
        } else if score >= self.prompt {
            EnforcementState::Prompt
        } else if score >= self.harden {
            EnforcementState::Harden
        } else {
            EnforcementState::Allow
        }
    }
}

/// Hysteresis configuration to prevent rapid oscillation (flapping) between
/// enforcement states.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EnforcementHysteresis {
    /// De-escalation requires the score to drop this far below the entry
    /// threshold. For example, if `harden` threshold is 0.40 and margin
    /// is 0.10, the score must drop below 0.30 to de-escalate from Harden
    /// to Allow.
    pub de_escalation_margin: f64,
    /// Minimum number of consecutive evaluations in a lower band before
    /// de-escalation is permitted. Prevents a single good call from
    /// immediately dropping the state.
    pub cooldown_calls: u32,
}

impl Default for EnforcementHysteresis {
    fn default() -> Self {
        Self {
            de_escalation_margin: 0.10,
            cooldown_calls: 3,
        }
    }
}

/// Result of an enforcement state machine evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnforcementTransition {
    /// The previous enforcement state.
    pub from: EnforcementState,
    /// The new enforcement state.
    pub to: EnforcementState,
    /// Whether hysteresis prevented a de-escalation.
    pub hysteresis_active: bool,
    /// The raw score band before hysteresis was applied.
    pub raw_band: EnforcementState,
    /// The risk score that triggered this evaluation.
    pub score: f64,
    /// Number of consecutive calls in the lower band (cooldown counter).
    pub cooldown_counter: u32,
}

/// Per-extension enforcement state machine with hysteresis tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnforcementStateMachine {
    /// Current enforcement state.
    state: EnforcementState,
    /// Score bands used for classification.
    bands: EnforcementScoreBands,
    /// Hysteresis configuration.
    hysteresis: EnforcementHysteresis,
    /// Counter of consecutive evaluations in a band strictly below `state`.
    /// Reset to 0 whenever a score maps to `state` or higher.
    cooldown_counter: u32,
    /// Total number of evaluations processed.
    evaluation_count: u64,
}

impl EnforcementStateMachine {
    /// Create a new state machine for the given policy profile.
    pub fn new(profile: &str) -> Self {
        Self {
            state: EnforcementState::Allow,
            bands: EnforcementScoreBands::for_profile(profile),
            hysteresis: EnforcementHysteresis::default(),
            cooldown_counter: 0,
            evaluation_count: 0,
        }
    }

    /// Create with custom bands and hysteresis.
    pub const fn with_config(
        bands: EnforcementScoreBands,
        hysteresis: EnforcementHysteresis,
    ) -> Self {
        Self {
            state: EnforcementState::Allow,
            bands,
            hysteresis,
            cooldown_counter: 0,
            evaluation_count: 0,
        }
    }

    /// Current enforcement state.
    pub const fn state(&self) -> EnforcementState {
        self.state
    }

    /// Total evaluations processed.
    pub const fn evaluation_count(&self) -> u64 {
        self.evaluation_count
    }

    /// Evaluate a risk score and return the enforcement transition.
    ///
    /// **Escalation** (moving to a more severe state) is immediate.
    /// **De-escalation** (moving to a less severe state) requires:
    /// 1. The score to fall below (entry_threshold - `de_escalation_margin`).
    /// 2. At least `cooldown_calls` consecutive evaluations in the lower
    ///    band.
    ///
    /// Terminate is a terminal state — once entered, it cannot be
    /// de-escalated.
    pub fn evaluate(&mut self, score: f64) -> EnforcementTransition {
        self.evaluation_count += 1;
        let raw_band = self.bands.classify(score);
        let previous = self.state;

        // Terminate is terminal — no de-escalation.
        if self.state == EnforcementState::Terminate {
            self.cooldown_counter = 0;
            return EnforcementTransition {
                from: previous,
                to: self.state,
                hysteresis_active: false,
                raw_band,
                score,
                cooldown_counter: 0,
            };
        }

        // Escalation is immediate.
        if raw_band > self.state {
            self.state = raw_band;
            self.cooldown_counter = 0;
            return EnforcementTransition {
                from: previous,
                to: self.state,
                hysteresis_active: false,
                raw_band,
                score,
                cooldown_counter: 0,
            };
        }

        // Same band — reset cooldown counter.
        if raw_band == self.state {
            self.cooldown_counter = 0;
            return EnforcementTransition {
                from: previous,
                to: self.state,
                hysteresis_active: false,
                raw_band,
                score,
                cooldown_counter: 0,
            };
        }

        // raw_band < self.state → potential de-escalation.
        // Check hysteresis: score must be below entry threshold minus
        // margin, AND cooldown_calls must be satisfied.
        let entry_threshold = self.entry_threshold_for(self.state);
        let de_escalation_floor = entry_threshold - self.hysteresis.de_escalation_margin;

        if score < de_escalation_floor {
            self.cooldown_counter += 1;
            if self.cooldown_counter >= self.hysteresis.cooldown_calls {
                // De-escalation permitted — drop one level at a time.
                self.state = Self::one_level_down(self.state);
                self.cooldown_counter = 0;
                return EnforcementTransition {
                    from: previous,
                    to: self.state,
                    hysteresis_active: false,
                    raw_band,
                    score,
                    cooldown_counter: 0,
                };
            }
            // Hysteresis still holding — stay in current state.
            return EnforcementTransition {
                from: previous,
                to: self.state,
                hysteresis_active: true,
                raw_band,
                score,
                cooldown_counter: self.cooldown_counter,
            };
        }

        // Score is in a lower band but not far enough below the entry
        // threshold for hysteresis to allow de-escalation. Reset cooldown.
        self.cooldown_counter = 0;
        EnforcementTransition {
            from: previous,
            to: self.state,
            hysteresis_active: true,
            raw_band,
            score,
            cooldown_counter: 0,
        }
    }

    /// Look up the entry threshold for a given state.
    const fn entry_threshold_for(&self, state: EnforcementState) -> f64 {
        match state {
            EnforcementState::Allow => self.bands.allow,
            EnforcementState::Harden => self.bands.harden,
            EnforcementState::Prompt => self.bands.prompt,
            EnforcementState::Deny => self.bands.deny,
            EnforcementState::Terminate => self.bands.terminate,
        }
    }

    /// Drop exactly one severity level.
    const fn one_level_down(state: EnforcementState) -> EnforcementState {
        match state {
            EnforcementState::Allow | EnforcementState::Harden => EnforcementState::Allow,
            EnforcementState::Prompt => EnforcementState::Harden,
            EnforcementState::Deny => EnforcementState::Prompt,
            EnforcementState::Terminate => EnforcementState::Terminate,
        }
    }

    /// Combine the enforcement state machine decision with a capability
    /// policy decision. The most restrictive outcome wins.
    ///
    /// - If the policy denies the capability outright, the result is Deny
    ///   regardless of the risk score.
    /// - If the enforcement machine says Terminate, that overrides
    ///   everything.
    pub fn merge_with_policy(
        enforcement: EnforcementState,
        policy: PolicyDecision,
    ) -> EnforcementState {
        let policy_floor = match policy {
            PolicyDecision::Allow => EnforcementState::Allow,
            PolicyDecision::Prompt => EnforcementState::Prompt,
            PolicyDecision::Deny => EnforcementState::Deny,
        };
        // Take the more restrictive of the two.
        if enforcement > policy_floor {
            enforcement
        } else {
            policy_floor
        }
    }
}

fn runtime_risk_is_dangerous(capability: &str) -> bool {
    matches!(capability, "exec" | "env" | "http")
}

fn runtime_risk_harden_should_block_dangerous(decision: &RuntimeRiskDecision) -> bool {
    if !runtime_risk_is_dangerous(&decision.capability) {
        return false;
    }
    if decision.risk_score >= 0.82 {
        return true;
    }
    decision.triggers.iter().any(|code| {
        matches!(
            code.as_str(),
            "suspicious_exec_detail"
                | "dcg_rule_hit"
                | "dcg_heredoc_hit"
                | "sensitive_path_target"
                | "public_network_target"
                | "secret_env_access"
        )
    })
}

fn runtime_risk_base_score(capability: &str, method: &str, policy_reason: &str) -> f64 {
    let capability_score = match capability {
        "exec" => 0.48,
        "env" => 0.40,
        "http" => 0.32,
        "write" => 0.28,
        "tool" => 0.24,
        "session" => 0.18,
        "events" => 0.11,
        "ui" => 0.08,
        "read" => 0.06,
        _ => 0.12,
    };

    let method_bonus = match method {
        "exec" => 0.10,
        "http" => 0.08,
        "tool" => 0.04,
        _ => 0.0,
    };

    let policy_bonus = if policy_reason.starts_with("prompt_user_") {
        0.15
    } else if policy_reason.starts_with("prompt_cache_") {
        0.08
    } else {
        0.0
    };

    runtime_risk_clamp01(capability_score + method_bonus + policy_bonus)
}

/// Gap D3: returns `&'static str` — avoids one heap allocation per hostcall.
const fn runtime_hostcall_policy_profile(mode: ExtensionPolicyMode) -> &'static str {
    match mode {
        ExtensionPolicyMode::Strict => "strict",
        ExtensionPolicyMode::Prompt => "balanced",
        ExtensionPolicyMode::Permissive => "permissive",
    }
}

fn runtime_hostcall_count_recent(window: &VecDeque<i64>, now_ms: i64, horizon_ms: i64) -> u32 {
    let count = window
        .iter()
        .rev()
        .take_while(|ts| now_ms.saturating_sub(**ts) <= horizon_ms)
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn runtime_hostcall_sequence_context(
    state: &RuntimeRiskState,
    now_ms: i64,
) -> RuntimeHostcallSequenceContext {
    let recent_error_count = state
        .recent_outcome_errors
        .iter()
        .filter(|value| **value)
        .count();
    RuntimeHostcallSequenceContext {
        sequence_id: state.sequence_counter.saturating_add(1),
        previous_capability: state.last_capability.clone(),
        previous_method: state.last_method.clone(),
        previous_resource_target_class: state.last_resource_target_class.clone(),
        burst_count_1s: runtime_hostcall_count_recent(
            &state.recent_call_timestamps_ms,
            now_ms,
            1_000,
        ),
        burst_count_10s: runtime_hostcall_count_recent(
            &state.recent_call_timestamps_ms,
            now_ms,
            10_000,
        ),
        recent_error_count: u32::try_from(recent_error_count).unwrap_or(u32::MAX),
        recent_window_count: u32::try_from(state.recent_outcome_errors.len()).unwrap_or(u32::MAX),
        prior_failure_streak: state.consecutive_failures,
    }
}

fn runtime_hostcall_extract_features(
    base_score: f64,
    recent_mean_score: f64,
    sequence: &RuntimeHostcallSequenceContext,
    capability: &str,
    policy_reason: &str,
    timeout_ms: Option<u64>,
) -> RuntimeHostcallFeatureVector {
    let recent_error_rate = if sequence.recent_window_count == 0 {
        0.0
    } else {
        f64::from(sequence.recent_error_count) / f64::from(sequence.recent_window_count)
    };

    RuntimeHostcallFeatureVector {
        schema: RUNTIME_HOSTCALL_FEATURE_SCHEMA_VERSION.to_string(),
        base_score: runtime_risk_clamp01(base_score),
        recent_mean_score: runtime_risk_clamp01(recent_mean_score),
        recent_error_rate: runtime_risk_clamp01(recent_error_rate),
        burst_density_1s: runtime_risk_clamp01(f64::from(sequence.burst_count_1s) / 8.0),
        burst_density_10s: runtime_risk_clamp01(f64::from(sequence.burst_count_10s) / 24.0),
        prior_failure_streak_norm: runtime_risk_clamp01(
            f64::from(sequence.prior_failure_streak) / 8.0,
        ),
        dangerous_capability: if runtime_risk_is_dangerous(capability) {
            1.0
        } else {
            0.0
        },
        timeout_requested: if timeout_ms.unwrap_or(0) > 0 {
            1.0
        } else {
            0.0
        },
        policy_prompt_bias: if policy_reason.starts_with("prompt_") {
            1.0
        } else {
            0.0
        },
    }
}

fn runtime_hostcall_is_private_ip(host: &str) -> Option<bool> {
    let parsed = host.parse::<IpAddr>().ok()?;
    Some(match parsed {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_unique_local() || v6.is_unicast_link_local() || v6.is_loopback(),
    })
}

/// Gap D3: returns `&'static str` — all branches are known static strings,
/// eliminating one heap allocation per hostcall.
fn runtime_hostcall_resource_target_class(method: &str, params: &Value) -> &'static str {
    match method {
        "http" => {
            let Some(url_raw) = params.get("url").and_then(Value::as_str) else {
                return "network.unknown";
            };
            let parsed = Url::parse(url_raw).ok();
            let Some(url) = parsed else {
                return "network.unknown";
            };
            let Some(host) = url.host_str() else {
                return "network.unknown";
            };
            if host.eq_ignore_ascii_case("localhost") {
                return "network.loopback";
            }
            if let Some(is_private) = runtime_hostcall_is_private_ip(host) {
                if is_private {
                    return "network.private";
                }
                return "network.public";
            }
            "network.public"
        }
        "exec" => "subprocess.exec",
        "tool" => {
            let tool_name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match tool_name {
                "read" | "write" | "edit" | "grep" | "find" | "ls" => "filesystem.tool",
                "bash" => "subprocess.tool",
                _ => "tool.unknown",
            }
        }
        "session" => "session.state",
        "ui" => "ui.interaction",
        "events" => "event.bus",
        "log" => "telemetry.log",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RuntimeHostcallArgumentSignals {
    risk_delta: f64,
    flags: u8,
}

const ARG_FLAG_SUSPICIOUS_EXEC: u8 = 1 << 0;
const ARG_FLAG_DCG_PATTERN_HIT: u8 = 1 << 1;
const ARG_FLAG_DCG_HEREDOC_HIT: u8 = 1 << 2;
const ARG_FLAG_SENSITIVE_PATH: u8 = 1 << 3;
const ARG_FLAG_PUBLIC_NETWORK: u8 = 1 << 4;
const ARG_FLAG_SECRET_ENV_ACCESS: u8 = 1 << 5;

impl RuntimeHostcallArgumentSignals {
    const fn set(&mut self, flag: u8) {
        self.flags |= flag;
    }

    const fn has(self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

fn runtime_hostcall_is_sensitive_path(path: &str) -> bool {
    let lower = path.trim().to_ascii_lowercase();
    let sensitive_prefixes = [
        "/etc", "/usr", "/bin", "/sbin", "/var", "/root", "/dev", "/proc", "/sys", "/boot",
    ];
    sensitive_prefixes
        .iter()
        .any(|prefix| lower == *prefix || lower.starts_with(&format!("{prefix}/")))
        || lower.contains("/.ssh/")
        || lower.ends_with("/.ssh")
}

fn runtime_hostcall_is_safe_utility_command(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let cmd = words
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        cmd.as_str(),
        "ls" | "pwd" | "echo" | "cat" | "head" | "tail" | "wc"
    ) {
        return true;
    }
    // git is only safe with read-only subcommands; destructive git subcommands
    // (push --force, reset --hard, clean, etc.) must NOT receive a risk reduction.
    if cmd == "git" {
        let sub = words.next().unwrap_or_default().to_ascii_lowercase();
        return matches!(
            sub.as_str(),
            "status"
                | "log"
                | "diff"
                | "show"
                | "branch"
                | "tag"
                | "remote"
                | "rev-parse"
                | "describe"
                | "shortlog"
                | "blame"
                | "ls-files"
                | "ls-tree"
                | "ls-remote"
                | "cat-file"
                | "name-rev"
        );
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RuntimeHeredocScriptLanguage {
    Bash,
    Python,
    JavaScript,
    TypeScript,
    Ruby,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeHeredocAstSeverity {
    Critical,
    High,
    Medium,
}

impl RuntimeHeredocAstSeverity {
    const fn score(self) -> f64 {
        match self {
            Self::Critical => 0.34,
            Self::High => 0.24,
            Self::Medium => 0.12,
        }
    }

    const fn is_blocking(self) -> bool {
        matches!(self, Self::Critical | Self::High)
    }
}

#[derive(Debug)]
struct RuntimeHeredocAstPattern {
    pattern: Pattern,
    severity: RuntimeHeredocAstSeverity,
}

#[derive(Debug, Clone)]
struct RuntimeExtractedHeredoc {
    body: String,
    invocation_prefix: String,
}

fn runtime_hostcall_heredoc_ast_patterns()
-> &'static HashMap<RuntimeHeredocScriptLanguage, Vec<RuntimeHeredocAstPattern>> {
    static HEREDOC_AST_PATTERNS: OnceLock<
        HashMap<RuntimeHeredocScriptLanguage, Vec<RuntimeHeredocAstPattern>>,
    > = OnceLock::new();
    HEREDOC_AST_PATTERNS.get_or_init(runtime_hostcall_build_heredoc_ast_patterns)
}

fn runtime_hostcall_compile_ast_patterns(
    ast_lang: SupportLang,
    specs: &[(&str, RuntimeHeredocAstSeverity)],
) -> Vec<RuntimeHeredocAstPattern> {
    let mut compiled = Vec::with_capacity(specs.len());
    for (pattern_str, severity) in specs {
        if let Ok(pattern) = Pattern::try_new(pattern_str, ast_lang) {
            compiled.push(RuntimeHeredocAstPattern {
                pattern,
                severity: *severity,
            });
        }
    }
    compiled
}

#[allow(clippy::too_many_lines)]
fn runtime_hostcall_build_heredoc_ast_patterns()
-> HashMap<RuntimeHeredocScriptLanguage, Vec<RuntimeHeredocAstPattern>> {
    let mut out: HashMap<RuntimeHeredocScriptLanguage, Vec<RuntimeHeredocAstPattern>> =
        HashMap::new();

    let bash_specs = [
        ("rm -rf $$$", RuntimeHeredocAstSeverity::Critical),
        ("rm -r $$$", RuntimeHeredocAstSeverity::High),
        ("git reset --hard", RuntimeHeredocAstSeverity::Critical),
        ("git clean -fd", RuntimeHeredocAstSeverity::High),
        ("git clean -fdx", RuntimeHeredocAstSeverity::High),
    ];
    let bash = runtime_hostcall_compile_ast_patterns(SupportLang::Bash, &bash_specs);
    if !bash.is_empty() {
        out.insert(RuntimeHeredocScriptLanguage::Bash, bash);
    }

    let python_specs = [
        ("shutil.rmtree($$$)", RuntimeHeredocAstSeverity::Critical),
        ("os.remove($$$)", RuntimeHeredocAstSeverity::High),
        ("os.rmdir($$$)", RuntimeHeredocAstSeverity::High),
        ("os.unlink($$$)", RuntimeHeredocAstSeverity::High),
        (
            "pathlib.Path($$$).unlink($$$)",
            RuntimeHeredocAstSeverity::High,
        ),
        ("Path($$$).unlink($$$)", RuntimeHeredocAstSeverity::High),
        (
            "pathlib.Path($$$).rmdir($$$)",
            RuntimeHeredocAstSeverity::High,
        ),
        ("Path($$$).rmdir($$$)", RuntimeHeredocAstSeverity::High),
        ("os.system($$$)", RuntimeHeredocAstSeverity::Medium),
        ("subprocess.run($$$)", RuntimeHeredocAstSeverity::Medium),
    ];
    let python = runtime_hostcall_compile_ast_patterns(SupportLang::Python, &python_specs);
    if !python.is_empty() {
        out.insert(RuntimeHeredocScriptLanguage::Python, python);
    }

    let javascript_specs = [
        ("fs.rmSync($$$)", RuntimeHeredocAstSeverity::High),
        ("fs.rmdirSync($$$)", RuntimeHeredocAstSeverity::High),
        ("fs.rm($$$)", RuntimeHeredocAstSeverity::Medium),
        ("fs.rmdir($$$)", RuntimeHeredocAstSeverity::Medium),
        (
            "child_process.execSync($$$)",
            RuntimeHeredocAstSeverity::Medium,
        ),
        (
            "require('child_process').execSync($$$)",
            RuntimeHeredocAstSeverity::Medium,
        ),
        (
            "child_process.spawnSync($$$)",
            RuntimeHeredocAstSeverity::Medium,
        ),
    ];
    let javascript =
        runtime_hostcall_compile_ast_patterns(SupportLang::JavaScript, &javascript_specs);
    if !javascript.is_empty() {
        out.insert(RuntimeHeredocScriptLanguage::JavaScript, javascript);
    }

    let typescript_specs = [
        ("fs.rmSync($$$)", RuntimeHeredocAstSeverity::High),
        ("fs.rmdirSync($$$)", RuntimeHeredocAstSeverity::High),
        ("Deno.remove($$$)", RuntimeHeredocAstSeverity::High),
        ("fs.rm($$$)", RuntimeHeredocAstSeverity::Medium),
        ("fs.rmdir($$$)", RuntimeHeredocAstSeverity::Medium),
        (
            "child_process.execSync($$$)",
            RuntimeHeredocAstSeverity::Medium,
        ),
        (
            "require('child_process').execSync($$$)",
            RuntimeHeredocAstSeverity::Medium,
        ),
        (
            "child_process.spawnSync($$$)",
            RuntimeHeredocAstSeverity::Medium,
        ),
    ];
    let typescript =
        runtime_hostcall_compile_ast_patterns(SupportLang::TypeScript, &typescript_specs);
    if !typescript.is_empty() {
        out.insert(RuntimeHeredocScriptLanguage::TypeScript, typescript);
    }

    let ruby_specs = [
        ("FileUtils.rm_rf($$$)", RuntimeHeredocAstSeverity::Critical),
        ("FileUtils.rm($$$)", RuntimeHeredocAstSeverity::High),
        ("File.delete($$$)", RuntimeHeredocAstSeverity::High),
        ("File.unlink($$$)", RuntimeHeredocAstSeverity::High),
        ("Dir.rmdir($$$)", RuntimeHeredocAstSeverity::High),
        ("system($$$)", RuntimeHeredocAstSeverity::Medium),
        ("exec($$$)", RuntimeHeredocAstSeverity::Medium),
    ];
    let ruby = runtime_hostcall_compile_ast_patterns(SupportLang::Ruby, &ruby_specs);
    if !ruby.is_empty() {
        out.insert(RuntimeHeredocScriptLanguage::Ruby, ruby);
    }

    out
}

const fn runtime_hostcall_script_language_to_ast_lang(
    language: RuntimeHeredocScriptLanguage,
) -> Option<SupportLang> {
    match language {
        RuntimeHeredocScriptLanguage::Bash => Some(SupportLang::Bash),
        RuntimeHeredocScriptLanguage::Python => Some(SupportLang::Python),
        RuntimeHeredocScriptLanguage::JavaScript => Some(SupportLang::JavaScript),
        RuntimeHeredocScriptLanguage::TypeScript => Some(SupportLang::TypeScript),
        RuntimeHeredocScriptLanguage::Ruby => Some(SupportLang::Ruby),
        RuntimeHeredocScriptLanguage::Unknown => None,
    }
}

fn runtime_hostcall_matches_interpreter(base: &str, token: &str) -> bool {
    if token == base {
        return true;
    }
    token.strip_prefix(base).is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix.chars().all(|c| c.is_ascii_digit() || c == '.')
            && suffix.chars().next().is_some_and(|c| c.is_ascii_digit())
    })
}

fn runtime_hostcall_script_language_from_command_token(
    token: &str,
) -> RuntimeHeredocScriptLanguage {
    let basename = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    if runtime_hostcall_matches_interpreter("python", &basename) {
        RuntimeHeredocScriptLanguage::Python
    } else if runtime_hostcall_matches_interpreter("node", &basename)
        || runtime_hostcall_matches_interpreter("nodejs", &basename)
    {
        RuntimeHeredocScriptLanguage::JavaScript
    } else if runtime_hostcall_matches_interpreter("deno", &basename)
        || runtime_hostcall_matches_interpreter("bun", &basename)
        || runtime_hostcall_matches_interpreter("tsx", &basename)
        || runtime_hostcall_matches_interpreter("ts-node", &basename)
    {
        RuntimeHeredocScriptLanguage::TypeScript
    } else if runtime_hostcall_matches_interpreter("ruby", &basename)
        || runtime_hostcall_matches_interpreter("irb", &basename)
    {
        RuntimeHeredocScriptLanguage::Ruby
    } else if runtime_hostcall_matches_interpreter("bash", &basename)
        || runtime_hostcall_matches_interpreter("sh", &basename)
        || runtime_hostcall_matches_interpreter("zsh", &basename)
        || runtime_hostcall_matches_interpreter("fish", &basename)
    {
        RuntimeHeredocScriptLanguage::Bash
    } else {
        RuntimeHeredocScriptLanguage::Unknown
    }
}

fn runtime_hostcall_script_language_from_invocation(
    invocation_prefix: &str,
) -> RuntimeHeredocScriptLanguage {
    for segment in invocation_prefix.split('|').rev() {
        let mut tokens = segment.split_whitespace().peekable();
        while let Some(raw) = tokens.next() {
            let token = raw.trim_matches(['\'', '"', '(', ')']);
            if token.is_empty() {
                continue;
            }
            let token_lower = token.to_ascii_lowercase();
            if token_lower == "env" {
                while let Some(next_raw) = tokens.peek().copied() {
                    let next = next_raw.trim_matches(['\'', '"', '(', ')']);
                    if next.starts_with('-') || next.contains('=') {
                        let _ = tokens.next();
                        continue;
                    }
                    return runtime_hostcall_script_language_from_command_token(next);
                }
                break;
            }
            if matches!(
                token_lower.as_str(),
                "sudo" | "command" | "nohup" | "time" | "builtin"
            ) || token.contains('=')
            {
                continue;
            }
            return runtime_hostcall_script_language_from_command_token(token);
        }
    }
    RuntimeHeredocScriptLanguage::Unknown
}

fn runtime_hostcall_script_language_from_shebang(content: &str) -> RuntimeHeredocScriptLanguage {
    let Some(first_line) = content.lines().next() else {
        return RuntimeHeredocScriptLanguage::Unknown;
    };
    let Some(shebang) = first_line.strip_prefix("#!") else {
        return RuntimeHeredocScriptLanguage::Unknown;
    };
    let mut parts = shebang.split_whitespace();
    let Some(first) = parts.next() else {
        return RuntimeHeredocScriptLanguage::Unknown;
    };
    let basename = first.rsplit(['/', '\\']).next().unwrap_or(first);
    if basename.eq_ignore_ascii_case("env") {
        for part in parts {
            if part.starts_with('-') || part.contains('=') {
                continue;
            }
            return runtime_hostcall_script_language_from_command_token(part);
        }
        return RuntimeHeredocScriptLanguage::Unknown;
    }
    runtime_hostcall_script_language_from_command_token(basename)
}

fn runtime_hostcall_script_language_from_content(content: &str) -> RuntimeHeredocScriptLanguage {
    let window = content.lines().take(24).collect::<Vec<_>>().join("\n");
    if window.contains("import ") && (window.contains(" os") || window.contains(" shutil")) {
        return RuntimeHeredocScriptLanguage::Python;
    }
    if window.contains("require(")
        || window.contains("module.exports")
        || window.contains("child_process")
    {
        return RuntimeHeredocScriptLanguage::JavaScript;
    }
    if window.contains(": string")
        || window.contains(": number")
        || window.contains("interface ")
        || window.contains("type ")
    {
        return RuntimeHeredocScriptLanguage::TypeScript;
    }
    if window.contains("FileUtils.")
        || window.contains("require '")
        || window.contains("require \"")
        || window.contains("def ")
    {
        return RuntimeHeredocScriptLanguage::Ruby;
    }
    if window.contains("rm -rf")
        || window.contains("git reset --hard")
        || window.contains("set -e")
        || window.contains("#!/bin/bash")
    {
        return RuntimeHeredocScriptLanguage::Bash;
    }
    RuntimeHeredocScriptLanguage::Unknown
}

fn runtime_hostcall_detect_heredoc_script_language(
    heredoc: &RuntimeExtractedHeredoc,
) -> RuntimeHeredocScriptLanguage {
    let from_invocation =
        runtime_hostcall_script_language_from_invocation(&heredoc.invocation_prefix);
    if from_invocation != RuntimeHeredocScriptLanguage::Unknown {
        return from_invocation;
    }
    let from_shebang = runtime_hostcall_script_language_from_shebang(&heredoc.body);
    if from_shebang != RuntimeHeredocScriptLanguage::Unknown {
        return from_shebang;
    }
    runtime_hostcall_script_language_from_content(&heredoc.body)
}

fn runtime_hostcall_dcg_command_score(command: &str) -> (f64, bool) {
    let lower = command.to_ascii_lowercase();
    let mut score = 0.0f64;
    let mut matched = false;

    // Adapted high-signal signatures from destructive_command_guard core git/filesystem packs.
    let critical_git = [
        "git reset --hard",
        "git clean -fd",
        "git clean -xdf",
        "git clean -fdx",
        "git push --force",
        "git push -f",
        "git stash clear",
    ];
    for needle in critical_git {
        if lower.contains(needle) {
            score += 0.36;
            matched = true;
        }
    }
    let high_git = [
        "git checkout --",
        "git restore --worktree",
        "git stash drop",
        "git branch -d",
        "git branch -D",
    ];
    for needle in high_git {
        if lower.contains(needle) {
            score += 0.22;
            matched = true;
        }
    }

    if lower.contains("rm -rf /")
        || lower.contains("rm -fr /")
        || lower.contains("--no-preserve-root")
    {
        score += 0.50;
        matched = true;
    } else if lower.contains("rm -rf")
        || lower.contains("rm -fr")
        || lower.contains("rm --recursive --force")
    {
        score += 0.26;
        matched = true;
    }

    if lower.contains("dd ") && lower.contains("of=/dev/") {
        score += 0.34;
        matched = true;
    }
    if lower.contains("mkfs") || lower.contains("wipefs") || lower.contains("shred ") {
        score += 0.32;
        matched = true;
    }

    (score.min(0.55), matched)
}

fn runtime_hostcall_extract_heredoc_blocks(command: &str) -> Vec<RuntimeExtractedHeredoc> {
    let mut payloads = Vec::new();
    let lines = command.lines().collect::<Vec<_>>();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let Some(op_index) = line.find("<<") else {
            i += 1;
            continue;
        };
        if line
            .get(op_index + 2..)
            .is_some_and(|remainder| remainder.starts_with('<'))
        {
            i += 1;
            continue;
        }
        let invocation_prefix = line[..op_index].trim().to_string();
        let mut delimiter = line[op_index + 2..].trim();
        if delimiter.starts_with('-') || delimiter.starts_with('~') {
            delimiter = delimiter[1..].trim_start();
        }
        let Some(raw_token) = delimiter.split_whitespace().next() else {
            i += 1;
            continue;
        };
        let token = raw_token.trim_matches('\'').trim_matches('"').to_string();
        if token.is_empty() {
            i += 1;
            continue;
        }
        let mut body = String::new();
        let mut j = i + 1;
        while j < lines.len() {
            if lines[j].trim() == token {
                break;
            }
            body.push_str(lines[j]);
            body.push('\n');
            j += 1;
        }
        if !body.trim().is_empty() {
            payloads.push(RuntimeExtractedHeredoc {
                body,
                invocation_prefix,
            });
        }
        i = j.saturating_add(1);
    }
    payloads
}

// Test/debug helper for inspecting heredoc bodies; scoring uses the richer
// block representation to retain invocation context.
#[allow(dead_code)]
fn runtime_hostcall_extract_heredoc_payloads(command: &str) -> Vec<String> {
    runtime_hostcall_extract_heredoc_blocks(command)
        .into_iter()
        .map(|payload| payload.body)
        .collect()
}

fn runtime_hostcall_dcg_heredoc_ast_score(heredoc: &RuntimeExtractedHeredoc) -> (f64, bool) {
    let language = runtime_hostcall_detect_heredoc_script_language(heredoc);
    let Some(ast_lang) = runtime_hostcall_script_language_to_ast_lang(language) else {
        return (0.0, false);
    };
    let Some(patterns) = runtime_hostcall_heredoc_ast_patterns().get(&language) else {
        return (0.0, false);
    };
    if patterns.is_empty() {
        return (0.0, false);
    }

    let ast = AstGrep::new(heredoc.body.as_str(), ast_lang);
    let root = ast.root();

    let mut has_match = false;
    let mut has_blocking_match = false;
    let mut max_score = 0.0f64;
    for pattern in patterns {
        if root.find_all(&pattern.pattern).next().is_some() {
            has_match = true;
            max_score = max_score.max(pattern.severity.score());
            if pattern.severity.is_blocking() {
                has_blocking_match = true;
            }
        }
    }

    if has_match {
        (max_score, has_blocking_match)
    } else {
        (0.0, false)
    }
}

fn runtime_hostcall_dcg_heredoc_score(command: &str) -> (f64, bool) {
    if !command.contains("<<") {
        return (0.0, false);
    }

    let payloads = runtime_hostcall_extract_heredoc_blocks(command);
    if payloads.is_empty() {
        // Heredoc operator present but payload not extractable: suspicious but low-confidence.
        return (0.05, false);
    }

    let mut matched = false;
    let mut score = 0.0f64;
    for payload in payloads {
        for line in payload.body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (line_score, line_hit) = runtime_hostcall_dcg_command_score(trimmed);
            if line_hit {
                matched = true;
                score = score.max(line_score + 0.12);
            }
        }

        let (ast_score, ast_blocking_hit) = runtime_hostcall_dcg_heredoc_ast_score(&payload);
        if ast_score > 0.0 {
            score = score.max(ast_score);
        }
        if ast_blocking_hit {
            matched = true;
        }
    }

    if matched {
        (score.min(0.68), true)
    } else if score > 0.0 {
        (score.min(0.30), false)
    } else {
        (0.08, false)
    }
}

fn runtime_hostcall_extract_exec_command(method: &str, params: &Value) -> Option<String> {
    if method.eq_ignore_ascii_case("exec") {
        if let Some(command) = params.get("command").and_then(Value::as_str) {
            let command = command.trim();
            if !command.is_empty() {
                return Some(command.to_string());
            }
        }
        if let Some(cmd) = params.get("cmd").and_then(Value::as_str) {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                return None;
            }
            let args = params
                .get("args")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            if args.is_empty() {
                return Some(cmd.to_string());
            }
            return Some(format!("{cmd} {args}"));
        }
        return None;
    }

    if !method.eq_ignore_ascii_case("tool") {
        return None;
    }

    let is_bash = params
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name.trim().eq_ignore_ascii_case("bash"));
    if !is_bash {
        return None;
    }
    let input = params.get("input")?;
    let command = input.get("command").and_then(Value::as_str)?;
    let command = command.trim();
    if command.is_empty() {
        None
    } else {
        Some(command.to_string())
    }
}

fn runtime_hostcall_extract_path(method: &str, params: &Value) -> Option<String> {
    if method.eq_ignore_ascii_case("fs") {
        let path = params.get("path").and_then(Value::as_str)?;
        let path = path.trim();
        if path.is_empty() {
            return None;
        }
        return Some(path.to_string());
    }

    if !method.eq_ignore_ascii_case("tool") {
        return None;
    }

    let tool_name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(
        tool_name.as_str(),
        "read" | "write" | "edit" | "grep" | "find" | "ls"
    ) {
        return None;
    }
    let input = params.get("input")?;
    for key in ["path", "file", "file_path"] {
        if let Some(path) = input.get(key).and_then(Value::as_str) {
            let path = path.trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn runtime_hostcall_extract_env_names(method: &str, params: &Value) -> Vec<String> {
    if !method.eq_ignore_ascii_case("env") {
        return Vec::new();
    }

    let mut names = Vec::new();
    if let Some(name) = params.get("name").and_then(Value::as_str) {
        let value = name.trim();
        if !value.is_empty() {
            names.push(value.to_string());
        }
    }
    if let Some(items) = params.get("names").and_then(Value::as_array) {
        for item in items {
            if let Some(name) = item.as_str() {
                let value = name.trim();
                if !value.is_empty() {
                    names.push(value.to_string());
                }
            }
        }
    }
    names
}

fn runtime_hostcall_is_secret_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    let needles = [
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "PASSWD",
        "AUTH",
        "COOKIE",
        "SESSION",
        "PRIVATE",
        "KEY",
        "AWS_",
        "OPENAI_",
        "ANTHROPIC_",
    ];
    needles.iter().any(|needle| upper.contains(needle))
}

fn runtime_hostcall_argument_signals(
    capability: &str,
    method: &str,
    params: &Value,
    resource_target_class: &str,
) -> RuntimeHostcallArgumentSignals {
    let mut signals = RuntimeHostcallArgumentSignals::default();

    if let Some(command) = runtime_hostcall_extract_exec_command(method, params) {
        let tokens = command
            .split_whitespace()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let (cmd, args) = if let Some((first, rest)) = tokens.split_first() {
            (first.as_str(), rest.to_vec())
        } else {
            ("", Vec::new())
        };
        let classifications = classify_dangerous_command(cmd, &args);
        let highest = classifications.iter().map(|class| class.risk_tier()).max();
        if let Some(tier) = highest {
            signals.set(ARG_FLAG_SUSPICIOUS_EXEC);
            signals.risk_delta += match tier {
                ExecRiskTier::Critical => 0.42,
                ExecRiskTier::High => 0.30,
                ExecRiskTier::Medium => 0.18,
                ExecRiskTier::Low => 0.10,
            };
        } else if runtime_hostcall_is_safe_utility_command(&command) {
            signals.risk_delta -= 0.18;
        } else {
            signals.risk_delta += 0.04;
        }

        let (dcg_score, dcg_hit) = runtime_hostcall_dcg_command_score(&command);
        signals.risk_delta += dcg_score;
        if dcg_hit {
            signals.set(ARG_FLAG_SUSPICIOUS_EXEC);
            signals.set(ARG_FLAG_DCG_PATTERN_HIT);
        }

        let (heredoc_score, heredoc_hit) = runtime_hostcall_dcg_heredoc_score(&command);
        signals.risk_delta += heredoc_score;
        if heredoc_hit {
            signals.set(ARG_FLAG_SUSPICIOUS_EXEC);
            signals.set(ARG_FLAG_DCG_HEREDOC_HIT);
        }
    }

    if let Some(path) = runtime_hostcall_extract_path(method, params) {
        if runtime_hostcall_is_sensitive_path(&path) {
            signals.set(ARG_FLAG_SENSITIVE_PATH);
            signals.risk_delta += if capability.eq_ignore_ascii_case("write") {
                0.30
            } else {
                0.20
            };
        } else if path.contains("../") {
            signals.risk_delta += 0.10;
        } else if path.starts_with('/') {
            signals.risk_delta += 0.04;
        } else {
            signals.risk_delta -= 0.03;
        }
    }

    if capability.eq_ignore_ascii_case("http") {
        if resource_target_class == "network.public" {
            signals.set(ARG_FLAG_PUBLIC_NETWORK);
            signals.risk_delta += 0.14;
        } else if matches!(
            resource_target_class,
            "network.private" | "network.loopback"
        ) {
            signals.risk_delta -= 0.06;
        } else {
            signals.risk_delta += 0.02;
        }
    }

    let env_names = runtime_hostcall_extract_env_names(method, params);
    if !env_names.is_empty() {
        if env_names
            .iter()
            .any(|name| runtime_hostcall_is_secret_env_key(name))
        {
            signals.set(ARG_FLAG_SECRET_ENV_ACCESS);
            signals.risk_delta += 0.22;
        } else {
            signals.risk_delta += 0.06;
        }
    }

    signals.risk_delta = signals.risk_delta.clamp(-0.30, 0.55);
    signals
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn runtime_risk_quantile(mut values: Vec<f64>, q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = runtime_risk_clamp01(q);
    let idx = ((values.len() - 1) as f64 * q).round() as usize;
    values[idx.min(values.len() - 1)]
}

fn runtime_risk_compute_ledger_hash(
    entry: &RuntimeRiskLedgerEntry,
    prev_hash: Option<&str>,
) -> String {
    let mut canonical = entry.clone();
    canonical.ledger_hash.clear();
    canonical.prev_ledger_hash = prev_hash.map(ToString::to_string);

    let mut hasher = sha2::Sha256::new();
    if let Some(prev) = prev_hash {
        hasher.update(prev.as_bytes());
    }
    let payload = serde_json::to_string(&canonical).unwrap_or_default();
    hasher.update(payload.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn runtime_risk_compute_ledger_hash_artifact(
    entry: &RuntimeRiskLedgerArtifactEntry,
    prev_hash: Option<&str>,
) -> String {
    let internal = RuntimeRiskLedgerEntry::from(entry);
    runtime_risk_compute_ledger_hash(&internal, prev_hash)
}

pub fn runtime_risk_ledger_data_hash(entries: &[RuntimeRiskLedgerArtifactEntry]) -> String {
    let mut hasher = sha2::Sha256::new();
    for entry in entries {
        hasher.update(entry.ledger_hash.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

pub fn verify_runtime_risk_ledger_artifact(
    artifact: &RuntimeRiskLedgerArtifact,
) -> RuntimeRiskLedgerVerificationReport {
    let mut errors = Vec::new();
    if artifact.schema != RUNTIME_RISK_LEDGER_SCHEMA_VERSION {
        errors.push(RuntimeRiskLedgerIntegrityError {
            index: 0,
            code: "schema_mismatch".to_string(),
            message: format!(
                "expected schema {}, got {}",
                RUNTIME_RISK_LEDGER_SCHEMA_VERSION, artifact.schema
            ),
        });
    }

    if artifact.entry_count != artifact.entries.len() {
        errors.push(RuntimeRiskLedgerIntegrityError {
            index: artifact.entries.len(),
            code: "entry_count_mismatch".to_string(),
            message: format!(
                "entry_count={}, actual={}",
                artifact.entry_count,
                artifact.entries.len()
            ),
        });
    }

    let head_ledger_hash = artifact
        .entries
        .first()
        .map(|entry| entry.ledger_hash.clone());
    let tail_ledger_hash = artifact
        .entries
        .last()
        .map(|entry| entry.ledger_hash.clone());

    if artifact.head_ledger_hash != head_ledger_hash {
        errors.push(RuntimeRiskLedgerIntegrityError {
            index: 0,
            code: "head_hash_mismatch".to_string(),
            message: "head_ledger_hash does not match first entry".to_string(),
        });
    }

    if artifact.tail_ledger_hash != tail_ledger_hash {
        errors.push(RuntimeRiskLedgerIntegrityError {
            index: artifact.entries.len(),
            code: "tail_hash_mismatch".to_string(),
            message: "tail_ledger_hash does not match last entry".to_string(),
        });
    }

    let mut expected_prev_hash = artifact
        .entries
        .first()
        .and_then(|entry| entry.prev_ledger_hash.clone());

    for (idx, entry) in artifact.entries.iter().enumerate() {
        if entry.prev_ledger_hash != expected_prev_hash {
            errors.push(RuntimeRiskLedgerIntegrityError {
                index: idx,
                code: "prev_hash_mismatch".to_string(),
                message: format!(
                    "expected prev {:?}, got {:?}",
                    expected_prev_hash, entry.prev_ledger_hash
                ),
            });
        }

        let expected_hash =
            runtime_risk_compute_ledger_hash_artifact(entry, expected_prev_hash.as_deref());
        if entry.ledger_hash != expected_hash {
            errors.push(RuntimeRiskLedgerIntegrityError {
                index: idx,
                code: "hash_mismatch".to_string(),
                message: format!("expected {}, got {}", expected_hash, entry.ledger_hash),
            });
        }

        expected_prev_hash = Some(entry.ledger_hash.clone());
    }

    let computed_data_hash = runtime_risk_ledger_data_hash(&artifact.entries);
    if artifact.data_hash != computed_data_hash {
        errors.push(RuntimeRiskLedgerIntegrityError {
            index: artifact.entries.len(),
            code: "data_hash_mismatch".to_string(),
            message: format!(
                "expected {}, got {}",
                computed_data_hash, artifact.data_hash
            ),
        });
    }

    RuntimeRiskLedgerVerificationReport {
        schema: RUNTIME_RISK_LEDGER_SCHEMA_VERSION.to_string(),
        entry_count: artifact.entries.len(),
        head_ledger_hash,
        tail_ledger_hash,
        artifact_data_hash: artifact.data_hash.clone(),
        computed_data_hash,
        valid: errors.is_empty(),
        errors,
    }
}

pub fn replay_runtime_risk_ledger_artifact(
    artifact: &RuntimeRiskLedgerArtifact,
) -> Result<RuntimeRiskReplayArtifact> {
    let verification = verify_runtime_risk_ledger_artifact(artifact);
    if !verification.valid {
        let summary = verification
            .errors
            .iter()
            .take(3)
            .map(|err| format!("[{}] {}: {}", err.index, err.code, err.message))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(Error::validation(format!(
            "runtime risk ledger integrity verification failed: {summary}"
        )));
    }

    let steps = artifact
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| RuntimeRiskReplayStep {
            index,
            call_id: entry.call_id.clone(),
            extension_id: entry.extension_id.clone(),
            capability: entry.capability.clone(),
            method: entry.method.clone(),
            policy_reason: entry.policy_reason.clone(),
            selected_action: entry.selected_action,
            derived_state: entry.derived_state,
            risk_score: entry.risk_score,
            reason_codes: entry.triggers.clone(),
            explanation_level: entry.explanation_level,
            explanation_summary: entry.explanation_summary.clone(),
            top_contributors: entry.top_contributors.clone(),
            budget_state: entry.budget_state.clone(),
            fallback_reason: entry.fallback_reason.clone(),
            drift_detected: entry.drift_detected,
            e_process: entry.e_process,
            e_threshold: entry.e_threshold,
            conformal_residual: entry.conformal_residual,
            conformal_quantile: entry.conformal_quantile,
            ledger_hash: entry.ledger_hash.clone(),
            prev_ledger_hash: entry.prev_ledger_hash.clone(),
        })
        .collect();

    Ok(RuntimeRiskReplayArtifact {
        schema: RUNTIME_RISK_REPLAY_SCHEMA_VERSION.to_string(),
        source_schema: artifact.schema.clone(),
        source_data_hash: artifact.data_hash.clone(),
        entry_count: artifact.entries.len(),
        tail_ledger_hash: artifact.tail_ledger_hash.clone(),
        steps,
    })
}

// ---------------------------------------------------------------------------
// SEC-5.3: Incident Evidence Bundle – free functions
// ---------------------------------------------------------------------------

/// Severity ordering for filter comparisons.
const fn security_alert_severity_ordinal(sev: SecurityAlertSeverity) -> u8 {
    match sev {
        SecurityAlertSeverity::Info => 0,
        SecurityAlertSeverity::Warning => 1,
        SecurityAlertSeverity::Error => 2,
        SecurityAlertSeverity::Critical => 3,
    }
}

/// Returns true if `entry_sev` is at or above `min_sev`.
const fn severity_at_or_above(
    entry_sev: SecurityAlertSeverity,
    min_sev: SecurityAlertSeverity,
) -> bool {
    security_alert_severity_ordinal(entry_sev) >= security_alert_severity_ordinal(min_sev)
}

/// Apply redaction policy to a ledger artifact entry (in place).
fn redact_ledger_entry(
    entry: &mut RuntimeRiskLedgerArtifactEntry,
    policy: &IncidentBundleRedactionPolicy,
) {
    if policy.redact_params_hash {
        entry.params_hash = "[REDACTED]".to_string();
    }
}

/// Apply redaction policy to a telemetry event (in place).
fn redact_telemetry_event(
    event: &mut RuntimeHostcallTelemetryEvent,
    policy: &IncidentBundleRedactionPolicy,
) {
    if policy.redact_params_hash {
        event.params_hash = "[REDACTED]".to_string();
    }
    if policy.redact_args_shape_hash {
        event.args_shape_hash = "[REDACTED]".to_string();
    }
}

/// Apply redaction policy to an exec mediation entry (in place).
fn redact_exec_mediation_entry(
    entry: &mut ExecMediationLedgerEntry,
    policy: &IncidentBundleRedactionPolicy,
) {
    if policy.redact_command_hash {
        entry.command_hash = "[REDACTED]".to_string();
    }
}

/// Apply redaction policy to a secret broker entry (in place).
fn redact_secret_broker_entry(
    entry: &mut SecretBrokerLedgerEntry,
    policy: &IncidentBundleRedactionPolicy,
) {
    if policy.redact_name_hash {
        entry.name_hash = "[REDACTED]".to_string();
    }
}

/// Apply redaction policy to a security alert (in place).
fn redact_security_alert(alert: &mut SecurityAlert, policy: &IncidentBundleRedactionPolicy) {
    if policy.redact_context_hash {
        alert.context_hash = "[REDACTED]".to_string();
    }
    if policy.redact_remediation {
        alert.remediation = "[REDACTED]".to_string();
    }
}

/// Compute the SHA-256 integrity hash of a bundle's content and metadata.
///
/// Covers all sections including summary, filter, redaction policy, schema,
/// and timestamp to prevent metadata tampering.
pub fn compute_incident_bundle_hash(bundle: &IncidentEvidenceBundle) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    // Include metadata fields to prevent tampering with summary, filter, etc.
    hasher.update(bundle.schema.as_bytes());
    hasher.update(b"|");
    hasher.update(bundle.generated_at_ms.to_le_bytes());
    hasher.update(b"|");
    hasher.update(
        serde_json::to_string(&bundle.filter)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"|");
    hasher.update(
        serde_json::to_string(&bundle.redaction)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"|");
    hasher.update(
        serde_json::to_string(&bundle.risk_ledger)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"|");
    hasher.update(
        serde_json::to_string(&bundle.security_alerts)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"|");
    hasher.update(
        serde_json::to_string(&bundle.hostcall_telemetry)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"|");
    hasher.update(
        serde_json::to_string(&bundle.exec_mediation)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"|");
    hasher.update(
        serde_json::to_string(&bundle.secret_broker)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"|");
    hasher.update(
        serde_json::to_string(&bundle.quota_breaches)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"|");
    hasher.update(
        serde_json::to_string(&bundle.risk_replay)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"|");
    hasher.update(
        serde_json::to_string(&bundle.summary)
            .unwrap_or_default()
            .as_bytes(),
    );
    format!("{:x}", hasher.finalize())
}

/// Build an incident evidence bundle from raw artifacts with filtering and
/// redaction applied. Deterministic: same inputs produce the same bundle.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn build_incident_evidence_bundle(
    ledger_artifact: &RuntimeRiskLedgerArtifact,
    alert_artifact: &SecurityAlertArtifact,
    telemetry_artifact: &RuntimeHostcallTelemetryArtifact,
    exec_artifact: &ExecMediationArtifact,
    secret_artifact: &SecretBrokerArtifact,
    quota_breaches: &[QuotaBreachEvent],
    filter: &IncidentBundleFilter,
    redaction: &IncidentBundleRedactionPolicy,
    generated_at_ms: i64,
) -> IncidentEvidenceBundle {
    // -- 1. Filter ledger entries --
    let mut filtered_ledger_entries: Vec<RuntimeRiskLedgerArtifactEntry> = ledger_artifact
        .entries
        .iter()
        .filter(|e| {
            filter.start_ms.is_none_or(|s| e.ts_ms >= s)
                && filter.end_ms.is_none_or(|end| e.ts_ms <= end)
                && filter
                    .extension_id
                    .as_ref()
                    .is_none_or(|ext| e.extension_id == *ext)
        })
        .cloned()
        .collect();

    // Check ledger chain on ORIGINAL entries before redaction.
    let ledger_chain_intact = {
        let mut intact = true;
        for i in 1..filtered_ledger_entries.len() {
            if filtered_ledger_entries[i].prev_ledger_hash.as_deref()
                != Some(filtered_ledger_entries[i - 1].ledger_hash.as_str())
            {
                intact = false;
                break;
            }
        }
        intact
    };

    for entry in &mut filtered_ledger_entries {
        redact_ledger_entry(entry, redaction);
    }

    let ledger_data_hash = runtime_risk_ledger_data_hash(&filtered_ledger_entries);
    let head_hash = filtered_ledger_entries
        .first()
        .map(|e| e.ledger_hash.clone());
    let tail_hash = filtered_ledger_entries
        .last()
        .map(|e| e.ledger_hash.clone());

    let filtered_ledger = RuntimeRiskLedgerArtifact {
        schema: RUNTIME_RISK_LEDGER_SCHEMA_VERSION.to_string(),
        generated_at_ms,
        entry_count: filtered_ledger_entries.len(),
        head_ledger_hash: head_hash,
        tail_ledger_hash: tail_hash,
        data_hash: ledger_data_hash,
        entries: filtered_ledger_entries,
    };

    // -- 2. Filter alerts --
    let mut filtered_alerts: Vec<SecurityAlert> = alert_artifact
        .alerts
        .iter()
        .filter(|a| {
            filter.start_ms.is_none_or(|s| a.ts_ms >= s)
                && filter.end_ms.is_none_or(|end| a.ts_ms <= end)
                && filter
                    .extension_id
                    .as_ref()
                    .is_none_or(|ext| a.extension_id == *ext)
                && filter
                    .alert_categories
                    .as_ref()
                    .is_none_or(|cats| cats.contains(&a.category))
                && filter
                    .min_severity
                    .is_none_or(|min_sev| severity_at_or_above(a.severity, min_sev))
        })
        .cloned()
        .collect();

    for alert in &mut filtered_alerts {
        redact_security_alert(alert, redaction);
    }

    let mut category_counts = SecurityAlertCategoryCounts::default();
    let mut severity_counts = SecurityAlertSeverityCounts::default();
    for alert in &filtered_alerts {
        category_counts.increment(alert.category);
        severity_counts.increment(alert.severity);
    }

    let filtered_alert_artifact = SecurityAlertArtifact {
        schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
        generated_at_ms,
        alert_count: filtered_alerts.len(),
        category_counts,
        severity_counts,
        alerts: filtered_alerts,
    };

    // -- 3. Filter telemetry --
    let mut filtered_telemetry: Vec<RuntimeHostcallTelemetryEvent> = telemetry_artifact
        .entries
        .iter()
        .filter(|t| {
            filter.start_ms.is_none_or(|s| t.ts_ms >= s)
                && filter.end_ms.is_none_or(|end| t.ts_ms <= end)
                && filter
                    .extension_id
                    .as_ref()
                    .is_none_or(|ext| t.extension_id == *ext)
        })
        .cloned()
        .collect();

    for event in &mut filtered_telemetry {
        redact_telemetry_event(event, redaction);
    }

    let filtered_telemetry_artifact = RuntimeHostcallTelemetryArtifact {
        schema: RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION.to_string(),
        generated_at_ms,
        entry_count: filtered_telemetry.len(),
        entries: filtered_telemetry,
    };

    // -- 4. Filter exec mediation --
    let mut filtered_exec: Vec<ExecMediationLedgerEntry> = exec_artifact
        .entries
        .iter()
        .filter(|e| {
            filter.start_ms.is_none_or(|s| e.ts_ms >= s)
                && filter.end_ms.is_none_or(|end| e.ts_ms <= end)
                && filter
                    .extension_id
                    .as_ref()
                    .is_none_or(|ext| e.extension_id.as_deref() == Some(ext.as_str()))
        })
        .cloned()
        .collect();

    for entry in &mut filtered_exec {
        redact_exec_mediation_entry(entry, redaction);
    }

    let filtered_exec_artifact = ExecMediationArtifact {
        schema: "pi.ext.exec_mediation.v1".to_string(),
        generated_at_ms,
        entry_count: filtered_exec.len(),
        entries: filtered_exec,
    };

    // -- 5. Filter secret broker --
    let mut filtered_secret: Vec<SecretBrokerLedgerEntry> = secret_artifact
        .entries
        .iter()
        .filter(|e| {
            filter.start_ms.is_none_or(|s| e.ts_ms >= s)
                && filter.end_ms.is_none_or(|end| e.ts_ms <= end)
                && filter
                    .extension_id
                    .as_ref()
                    .is_none_or(|ext| e.extension_id.as_deref() == Some(ext.as_str()))
        })
        .cloned()
        .collect();

    for entry in &mut filtered_secret {
        redact_secret_broker_entry(entry, redaction);
    }

    let filtered_secret_artifact = SecretBrokerArtifact {
        schema: "pi.ext.secret_broker.v1".to_string(),
        generated_at_ms,
        entry_count: filtered_secret.len(),
        entries: filtered_secret,
    };

    // -- 6. Filter quota breaches --
    let filtered_quotas: Vec<QuotaBreachEvent> = quota_breaches
        .iter()
        .filter(|q| {
            filter.start_ms.is_none_or(|s| q.ts_ms >= s)
                && filter.end_ms.is_none_or(|end| q.ts_ms <= end)
                && filter
                    .extension_id
                    .as_ref()
                    .is_none_or(|ext| q.extension_id == *ext)
        })
        .cloned()
        .collect();

    // -- 7. Build replay from filtered ledger --
    let risk_replay = replay_runtime_risk_ledger_artifact(&filtered_ledger).ok();

    // -- 8. Compute summary --
    let mut distinct_ext_set = std::collections::HashSet::new();
    for e in &filtered_ledger.entries {
        distinct_ext_set.insert(e.extension_id.clone());
    }
    for a in &filtered_alert_artifact.alerts {
        distinct_ext_set.insert(a.extension_id.clone());
    }

    let peak_risk_score = filtered_ledger
        .entries
        .iter()
        .map(|e| e.risk_score)
        .fold(0.0_f64, f64::max);

    let deny_or_terminate_count = filtered_ledger
        .entries
        .iter()
        .filter(|e| {
            matches!(
                e.selected_action,
                RuntimeRiskActionValue::Deny | RuntimeRiskActionValue::Terminate
            )
        })
        .count();

    let summary = IncidentBundleSummary {
        ledger_entry_count: filtered_ledger.entries.len(),
        alert_count: filtered_alert_artifact.alerts.len(),
        telemetry_event_count: filtered_telemetry_artifact.entries.len(),
        exec_mediation_count: filtered_exec_artifact.entries.len(),
        secret_broker_count: filtered_secret_artifact.entries.len(),
        quota_breach_count: filtered_quotas.len(),
        distinct_extensions: distinct_ext_set.len(),
        peak_risk_score,
        deny_or_terminate_count,
        ledger_chain_intact,
    };

    // -- 9. Assemble and seal --
    let mut bundle = IncidentEvidenceBundle {
        schema: INCIDENT_EVIDENCE_BUNDLE_SCHEMA_VERSION.to_string(),
        generated_at_ms,
        bundle_hash: String::new(),
        filter: filter.clone(),
        redaction: redaction.clone(),
        risk_ledger: filtered_ledger,
        security_alerts: filtered_alert_artifact,
        hostcall_telemetry: filtered_telemetry_artifact,
        exec_mediation: filtered_exec_artifact,
        secret_broker: filtered_secret_artifact,
        quota_breaches: filtered_quotas,
        risk_replay,
        summary,
    };

    bundle.bundle_hash = compute_incident_bundle_hash(&bundle);
    bundle
}

/// Verify the integrity of an incident evidence bundle.
pub fn verify_incident_evidence_bundle(
    bundle: &IncidentEvidenceBundle,
) -> IncidentBundleVerificationReport {
    let mut errors = Vec::new();

    let schema_valid = bundle.schema == INCIDENT_EVIDENCE_BUNDLE_SCHEMA_VERSION;
    if !schema_valid {
        errors.push(format!(
            "schema mismatch: expected {}, got {}",
            INCIDENT_EVIDENCE_BUNDLE_SCHEMA_VERSION, bundle.schema
        ));
    }

    let recomputed_hash = compute_incident_bundle_hash(bundle);
    let hash_valid = bundle.bundle_hash == recomputed_hash;
    if !hash_valid {
        errors.push(format!(
            "bundle_hash mismatch: stored {}, recomputed {}",
            bundle.bundle_hash, recomputed_hash
        ));
    }

    let ledger_chain_intact = bundle.summary.ledger_chain_intact;
    if !ledger_chain_intact {
        errors.push("ledger hash chain has discontinuities".to_string());
    }

    if bundle.summary.ledger_entry_count != bundle.risk_ledger.entries.len() {
        errors.push("summary.ledger_entry_count mismatch".to_string());
    }
    if bundle.summary.alert_count != bundle.security_alerts.alerts.len() {
        errors.push("summary.alert_count mismatch".to_string());
    }

    IncidentBundleVerificationReport {
        valid: errors.is_empty(),
        bundle_hash: bundle.bundle_hash.clone(),
        recomputed_hash,
        schema_valid,
        ledger_chain_intact,
        errors,
    }
}

// ---------------------------------------------------------------------------
fn runtime_risk_calibration_threshold_grid(config: &RuntimeRiskCalibrationConfig) -> Vec<f64> {
    let mut thresholds = if config.threshold_grid.is_empty() {
        RuntimeRiskCalibrationConfig::default().threshold_grid
    } else {
        config.threshold_grid.clone()
    };
    for threshold in &mut thresholds {
        *threshold = runtime_risk_clamp01(*threshold);
    }
    thresholds.sort_by(f64::total_cmp);
    thresholds.dedup_by(|left, right| left.total_cmp(right).is_eq());
    if thresholds.is_empty() {
        thresholds.push(runtime_risk_clamp01(config.baseline_threshold));
    }
    thresholds
}

const fn runtime_risk_calibration_is_positive(entry: &RuntimeRiskLedgerArtifactEntry) -> bool {
    entry.outcome_error_code.is_some()
        || matches!(
            entry.selected_action,
            RuntimeRiskActionValue::Deny | RuntimeRiskActionValue::Terminate
        )
        || matches!(entry.derived_state, RuntimeRiskStateLabelValue::Unsafe)
}

fn runtime_risk_calibration_candidate(
    entries: &[RuntimeRiskLedgerArtifactEntry],
    threshold: f64,
    config: &RuntimeRiskCalibrationConfig,
) -> RuntimeRiskThresholdCalibration {
    let threshold = runtime_risk_clamp01(threshold);
    let fp_weight = config.false_positive_weight.max(0.0);
    let fn_weight = config.false_negative_weight.max(0.0);
    let mut true_positive = 0.0_f64;
    let mut false_positive = 0.0_f64;
    let mut true_negative = 0.0_f64;
    let mut false_negative = 0.0_f64;

    for entry in entries {
        let actual_positive = runtime_risk_calibration_is_positive(entry);
        let predicted_positive = entry.risk_score >= threshold;
        match (actual_positive, predicted_positive) {
            (true, true) => true_positive += 1.0,
            (false, true) => false_positive += 1.0,
            (false, false) => true_negative += 1.0,
            (true, false) => false_negative += 1.0,
        }
    }

    let positives = true_positive + false_negative;
    let negatives = true_negative + false_positive;
    let false_positive_rate = if negatives == 0.0 {
        0.0
    } else {
        false_positive / negatives
    };
    let false_negative_rate = if positives == 0.0 {
        0.0
    } else {
        false_negative / positives
    };

    let false_negative_cost = false_negative * 12.0 * fn_weight;
    let false_positive_cost = false_positive * 3.0 * fp_weight;
    let true_positive_cost = true_positive;
    let true_negative_cost = true_negative * 0.2;
    let expected_loss =
        false_negative_cost + false_positive_cost + true_positive_cost + true_negative_cost;

    let objective_score = match config.objective {
        RuntimeRiskCalibrationObjective::MinExpectedLoss => expected_loss,
        RuntimeRiskCalibrationObjective::MinFalsePositives => {
            (false_negative_rate * fn_weight).mul_add(0.25, false_positive_rate * fp_weight)
        }
        RuntimeRiskCalibrationObjective::BalancedAccuracy => {
            (false_positive_rate * fp_weight) + (false_negative_rate * fn_weight)
        }
    };

    RuntimeRiskThresholdCalibration {
        threshold,
        objective_score,
        expected_loss,
        false_positive_rate,
        false_negative_rate,
    }
}

pub fn calibrate_runtime_risk_from_ledger(
    artifact: &RuntimeRiskLedgerArtifact,
    config: &RuntimeRiskCalibrationConfig,
) -> Result<RuntimeRiskCalibrationReport> {
    let verification = verify_runtime_risk_ledger_artifact(artifact);
    if !verification.valid {
        return Err(Error::validation(
            "runtime risk ledger failed integrity verification".to_string(),
        ));
    }
    if artifact.entries.is_empty() {
        return Err(Error::validation(
            "runtime risk ledger calibration requires at least one entry".to_string(),
        ));
    }

    let baseline_threshold = runtime_risk_clamp01(config.baseline_threshold);
    let baseline =
        runtime_risk_calibration_candidate(&artifact.entries, baseline_threshold, config);
    let mut candidates = runtime_risk_calibration_threshold_grid(config)
        .into_iter()
        .map(|threshold| runtime_risk_calibration_candidate(&artifact.entries, threshold, config))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.threshold.total_cmp(&right.threshold));

    let mut recommended = candidates
        .first()
        .cloned()
        .unwrap_or_else(|| baseline.clone());
    for candidate in candidates.iter().skip(1) {
        let better_score = candidate
            .objective_score
            .total_cmp(&recommended.objective_score)
            .is_lt();
        let equal_score = candidate
            .objective_score
            .total_cmp(&recommended.objective_score)
            .is_eq();
        let candidate_distance = (candidate.threshold - baseline_threshold).abs();
        let recommended_distance = (recommended.threshold - baseline_threshold).abs();
        let better_distance = candidate_distance.total_cmp(&recommended_distance).is_lt();
        let equal_distance = candidate_distance.total_cmp(&recommended_distance).is_eq();
        let better_threshold = candidate
            .threshold
            .total_cmp(&recommended.threshold)
            .is_lt();

        if better_score
            || (equal_score && (better_distance || (equal_distance && better_threshold)))
        {
            recommended = candidate.clone();
        }
    }

    Ok(RuntimeRiskCalibrationReport {
        schema: RUNTIME_RISK_CALIBRATION_SCHEMA_VERSION.to_string(),
        source_schema: artifact.schema.clone(),
        source_data_hash: artifact.data_hash.clone(),
        objective: config.objective,
        baseline_threshold,
        recommended_threshold: recommended.threshold,
        recommended_delta: recommended.threshold - baseline_threshold,
        baseline,
        recommended,
        candidates,
    })
}

fn adaptive_policy_usize_to_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

fn adaptive_policy_rate_bps(count: u64, total: usize) -> u64 {
    if total == 0 {
        return 0;
    }

    let count = u128::from(count);
    let total = u128::from(u64::try_from(total).unwrap_or(u64::MAX));
    let rate = (count.saturating_mul(10_000)).saturating_add(total / 2) / total;
    u64::try_from(rate).unwrap_or(u64::MAX)
}

fn adaptive_policy_signed_delta_bps(candidate: u64, baseline: u64) -> i64 {
    if baseline == 0 {
        return if candidate == 0 { 0 } else { 10_000 };
    }

    let delta = i128::from(candidate) - i128::from(baseline);
    let scaled = delta.saturating_mul(10_000) / i128::from(baseline);
    i64::try_from(scaled).unwrap_or_else(|_| {
        if scaled.is_negative() {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}

const fn runtime_risk_action_value_code(action: RuntimeRiskActionValue) -> &'static str {
    match action {
        RuntimeRiskActionValue::Allow => "allow",
        RuntimeRiskActionValue::Harden => "harden",
        RuntimeRiskActionValue::Deny => "deny",
        RuntimeRiskActionValue::Terminate => "terminate",
    }
}

fn adaptive_policy_is_forced_compat_reason(reason: &str) -> bool {
    reason.starts_with("forced_compat_")
}

fn adaptive_policy_increment(map: &mut BTreeMap<String, u64>, key: impl Into<String>) {
    let count = map.entry(key.into()).or_insert(0);
    *count = count.saturating_add(1);
}

fn adaptive_policy_mean_latency_ms(entries: &[RuntimeHostcallTelemetryEvent]) -> u64 {
    if entries.is_empty() {
        return 0;
    }

    let sum = entries.iter().fold(0_u128, |acc, entry| {
        acc.saturating_add(u128::from(entry.latency_ms))
    });
    let count = u128::from(u64::try_from(entries.len()).unwrap_or(u64::MAX));
    u64::try_from(sum / count).unwrap_or(u64::MAX)
}

fn adaptive_policy_p95_latency_ms(entries: &[RuntimeHostcallTelemetryEvent]) -> u64 {
    if entries.is_empty() {
        return 0;
    }

    let mut latencies = entries
        .iter()
        .map(|entry| entry.latency_ms)
        .collect::<Vec<_>>();
    latencies.sort_unstable();
    let index = latencies.len().saturating_mul(95).div_ceil(100);
    latencies[index.saturating_sub(1).min(latencies.len() - 1)]
}

fn adaptive_policy_comparison_key(entry: &RuntimeHostcallTelemetryEvent) -> String {
    if entry.call_id.trim().is_empty() {
        format!(
            "trace:{}|{}|{}|{}",
            entry.extension_id, entry.capability, entry.method, entry.params_hash
        )
    } else {
        format!("call:{}", entry.call_id)
    }
}

fn adaptive_policy_event_map(
    entries: &[RuntimeHostcallTelemetryEvent],
) -> BTreeMap<String, &RuntimeHostcallTelemetryEvent> {
    let mut by_key = BTreeMap::new();
    for entry in entries {
        by_key
            .entry(adaptive_policy_comparison_key(entry))
            .or_insert(entry);
    }
    by_key
}

fn adaptive_policy_telemetry_metrics(
    artifact: &RuntimeHostcallTelemetryArtifact,
) -> AdaptiveHostcallPolicyTelemetryMetrics {
    let mut fast_lane_count = 0_u64;
    let mut compat_lane_count = 0_u64;
    let mut unknown_lane_count = 0_u64;
    let mut fallback_count = 0_u64;
    let mut forced_compat_count = 0_u64;
    let mut error_count = 0_u64;
    let mut deny_or_terminate_count = 0_u64;
    let mut risk_score_sum = 0.0_f64;
    let mut action_counts = BTreeMap::new();
    let mut fallback_reason_counts = BTreeMap::new();

    for entry in &artifact.entries {
        match entry.lane.as_str() {
            "fast" => fast_lane_count = fast_lane_count.saturating_add(1),
            "compat" => compat_lane_count = compat_lane_count.saturating_add(1),
            _ => unknown_lane_count = unknown_lane_count.saturating_add(1),
        }

        if entry.outcome == "error" {
            error_count = error_count.saturating_add(1);
        }
        if matches!(
            entry.selected_action,
            RuntimeRiskActionValue::Deny | RuntimeRiskActionValue::Terminate
        ) {
            deny_or_terminate_count = deny_or_terminate_count.saturating_add(1);
        }

        risk_score_sum += entry.risk_score;
        adaptive_policy_increment(
            &mut action_counts,
            runtime_risk_action_value_code(entry.selected_action),
        );

        if let Some(reason) = entry
            .lane_fallback_reason
            .as_deref()
            .filter(|reason| !reason.is_empty())
        {
            fallback_count = fallback_count.saturating_add(1);
            adaptive_policy_increment(&mut fallback_reason_counts, reason);
            if adaptive_policy_is_forced_compat_reason(reason) {
                forced_compat_count = forced_compat_count.saturating_add(1);
            }
        } else if adaptive_policy_is_forced_compat_reason(&entry.lane_decision_reason) {
            forced_compat_count = forced_compat_count.saturating_add(1);
        }
    }

    let sample_count = artifact.entries.len();
    let mean_risk_score = if sample_count == 0 {
        0.0
    } else {
        risk_score_sum / adaptive_policy_usize_to_f64(sample_count)
    };

    AdaptiveHostcallPolicyTelemetryMetrics {
        sample_count,
        fast_lane_count,
        compat_lane_count,
        unknown_lane_count,
        fallback_count,
        forced_compat_count,
        error_count,
        deny_or_terminate_count,
        mean_latency_ms: adaptive_policy_mean_latency_ms(&artifact.entries),
        p95_latency_ms: adaptive_policy_p95_latency_ms(&artifact.entries),
        mean_risk_score,
        compat_rate_bps: adaptive_policy_rate_bps(compat_lane_count, sample_count),
        fallback_rate_bps: adaptive_policy_rate_bps(fallback_count, sample_count),
        error_rate_bps: adaptive_policy_rate_bps(error_count, sample_count),
        action_counts,
        fallback_reason_counts,
    }
}

fn adaptive_policy_sample_support(
    baseline: &RuntimeHostcallTelemetryArtifact,
    candidate: &RuntimeHostcallTelemetryArtifact,
    config: &AdaptiveHostcallPolicyDiffConfig,
) -> AdaptiveHostcallPolicySampleSupport {
    let candidate_by_key = adaptive_policy_event_map(&candidate.entries);
    let matched_samples = baseline
        .entries
        .iter()
        .filter(|entry| candidate_by_key.contains_key(&adaptive_policy_comparison_key(entry)))
        .count();
    let matched_coverage_bps = adaptive_policy_rate_bps(
        u64::try_from(matched_samples).unwrap_or(u64::MAX),
        baseline.entries.len(),
    );
    let sufficient = baseline.entries.len() >= config.min_sample_count
        && candidate.entries.len() >= config.min_sample_count
        && matched_coverage_bps >= config.min_matched_coverage_bps;

    AdaptiveHostcallPolicySampleSupport {
        baseline_samples: baseline.entries.len(),
        candidate_samples: candidate.entries.len(),
        matched_samples,
        min_required_samples: config.min_sample_count,
        matched_coverage_bps,
        min_matched_coverage_bps: config.min_matched_coverage_bps,
        sufficient,
    }
}

fn adaptive_policy_push_threshold_change(
    changes: &mut Vec<AdaptiveHostcallPolicyThresholdChange>,
    field: &str,
    baseline_value: impl Into<String>,
    candidate_value: impl Into<String>,
    direction: &str,
    risk_note: &str,
) {
    changes.push(AdaptiveHostcallPolicyThresholdChange {
        field: field.to_string(),
        baseline_value: baseline_value.into(),
        candidate_value: candidate_value.into(),
        direction: direction.to_string(),
        risk_note: risk_note.to_string(),
    });
}

fn adaptive_policy_mode_threshold_changes(
    baseline: &RuntimeRiskConfig,
    candidate: &RuntimeRiskConfig,
    changes: &mut Vec<AdaptiveHostcallPolicyThresholdChange>,
) {
    if baseline.enabled != candidate.enabled {
        let direction = if candidate.enabled {
            "enabled"
        } else {
            "disabled"
        };
        adaptive_policy_push_threshold_change(
            changes,
            "enabled",
            baseline.enabled.to_string(),
            candidate.enabled.to_string(),
            direction,
            "runtime risk scoring master switch changed",
        );
    }

    if baseline.enforce != candidate.enforce {
        let direction = if candidate.enforce {
            "enforced"
        } else {
            "shadowed"
        };
        adaptive_policy_push_threshold_change(
            changes,
            "enforce",
            baseline.enforce.to_string(),
            candidate.enforce.to_string(),
            direction,
            "runtime risk enforcement mode changed",
        );
    }

    if baseline.fail_closed != candidate.fail_closed {
        let direction = if candidate.fail_closed {
            "tightened"
        } else {
            "relaxed"
        };
        adaptive_policy_push_threshold_change(
            changes,
            "fail_closed",
            baseline.fail_closed.to_string(),
            candidate.fail_closed.to_string(),
            direction,
            "controller fallback behavior changed",
        );
    }
}

fn adaptive_policy_numeric_threshold_changes(
    baseline: &RuntimeRiskConfig,
    candidate: &RuntimeRiskConfig,
    changes: &mut Vec<AdaptiveHostcallPolicyThresholdChange>,
) {
    if baseline.alpha.total_cmp(&candidate.alpha) != std::cmp::Ordering::Equal {
        let direction = if candidate.alpha > baseline.alpha {
            "relaxed"
        } else {
            "tightened"
        };
        adaptive_policy_push_threshold_change(
            changes,
            "alpha",
            format!("{:.6}", baseline.alpha),
            format!("{:.6}", candidate.alpha),
            direction,
            "sequential detector type-I error budget changed",
        );
    }

    if baseline.window_size != candidate.window_size {
        let direction = if candidate.window_size < baseline.window_size {
            "more_sensitive"
        } else {
            "less_sensitive"
        };
        adaptive_policy_push_threshold_change(
            changes,
            "window_size",
            baseline.window_size.to_string(),
            candidate.window_size.to_string(),
            direction,
            "sliding window size changed",
        );
    }

    if baseline.ledger_limit != candidate.ledger_limit {
        let direction = if candidate.ledger_limit < baseline.ledger_limit {
            "shorter_retention"
        } else {
            "longer_retention"
        };
        adaptive_policy_push_threshold_change(
            changes,
            "ledger_limit",
            baseline.ledger_limit.to_string(),
            candidate.ledger_limit.to_string(),
            direction,
            "risk evidence retention changed",
        );
    }

    if baseline.decision_timeout_ms != candidate.decision_timeout_ms {
        let direction = if candidate.decision_timeout_ms < baseline.decision_timeout_ms {
            "tightened"
        } else {
            "relaxed"
        };
        adaptive_policy_push_threshold_change(
            changes,
            "decision_timeout_ms",
            baseline.decision_timeout_ms.to_string(),
            candidate.decision_timeout_ms.to_string(),
            direction,
            "per-hostcall risk decision budget changed",
        );
    }
}

fn adaptive_policy_threshold_changes(
    baseline: &RuntimeRiskConfig,
    candidate: &RuntimeRiskConfig,
) -> Vec<AdaptiveHostcallPolicyThresholdChange> {
    let mut changes = Vec::new();
    adaptive_policy_mode_threshold_changes(baseline, candidate, &mut changes);
    adaptive_policy_numeric_threshold_changes(baseline, candidate, &mut changes);

    changes
}

fn adaptive_policy_lane_changes(
    baseline: &RuntimeHostcallTelemetryArtifact,
    candidate: &RuntimeHostcallTelemetryArtifact,
    max_changes: usize,
) -> Vec<AdaptiveHostcallPolicyLaneChange> {
    let candidate_by_key = adaptive_policy_event_map(&candidate.entries);
    let mut changes = Vec::new();

    for baseline_entry in &baseline.entries {
        if changes.len() >= max_changes {
            break;
        }
        let key = adaptive_policy_comparison_key(baseline_entry);
        let Some(candidate_entry) = candidate_by_key.get(&key) else {
            continue;
        };
        if baseline_entry.lane == candidate_entry.lane
            && baseline_entry.lane_fallback_reason == candidate_entry.lane_fallback_reason
            && baseline_entry.lane_decision_reason == candidate_entry.lane_decision_reason
        {
            continue;
        }

        changes.push(AdaptiveHostcallPolicyLaneChange {
            comparison_key: key,
            extension_id: baseline_entry.extension_id.clone(),
            capability: baseline_entry.capability.clone(),
            method: baseline_entry.method.clone(),
            baseline_lane: baseline_entry.lane.clone(),
            candidate_lane: candidate_entry.lane.clone(),
            baseline_fallback_reason: baseline_entry.lane_fallback_reason.clone(),
            candidate_fallback_reason: candidate_entry.lane_fallback_reason.clone(),
            baseline_lane_decision_reason: baseline_entry.lane_decision_reason.clone(),
            candidate_lane_decision_reason: candidate_entry.lane_decision_reason.clone(),
        });
    }

    changes
}

fn adaptive_policy_action_changes(
    baseline: &RuntimeHostcallTelemetryArtifact,
    candidate: &RuntimeHostcallTelemetryArtifact,
    max_changes: usize,
) -> Vec<AdaptiveHostcallPolicyActionChange> {
    let candidate_by_key = adaptive_policy_event_map(&candidate.entries);
    let mut changes = Vec::new();

    for baseline_entry in &baseline.entries {
        if changes.len() >= max_changes {
            break;
        }
        let key = adaptive_policy_comparison_key(baseline_entry);
        let Some(candidate_entry) = candidate_by_key.get(&key) else {
            continue;
        };
        if baseline_entry.selected_action == candidate_entry.selected_action {
            continue;
        }

        changes.push(AdaptiveHostcallPolicyActionChange {
            comparison_key: key,
            extension_id: baseline_entry.extension_id.clone(),
            capability: baseline_entry.capability.clone(),
            method: baseline_entry.method.clone(),
            baseline_action: baseline_entry.selected_action,
            candidate_action: candidate_entry.selected_action,
            baseline_risk_score: baseline_entry.risk_score,
            candidate_risk_score: candidate_entry.risk_score,
        });
    }

    changes
}

fn adaptive_policy_latency_effect(
    baseline: &AdaptiveHostcallPolicyTelemetryMetrics,
    candidate: &AdaptiveHostcallPolicyTelemetryMetrics,
    config: &AdaptiveHostcallPolicyDiffConfig,
) -> AdaptiveHostcallPolicyLatencyEffect {
    let delta_bps =
        adaptive_policy_signed_delta_bps(candidate.mean_latency_ms, baseline.mean_latency_ms);
    let improvement_bps = i64::try_from(config.min_latency_improvement_bps).unwrap_or(i64::MAX);
    let expected_effect = if delta_bps <= -improvement_bps {
        "improved"
    } else if delta_bps > 0 {
        "regressed"
    } else {
        "neutral"
    };

    AdaptiveHostcallPolicyLatencyEffect {
        baseline_mean_latency_ms: baseline.mean_latency_ms,
        candidate_mean_latency_ms: candidate.mean_latency_ms,
        delta_ms: i64::try_from(candidate.mean_latency_ms).unwrap_or(i64::MAX)
            - i64::try_from(baseline.mean_latency_ms).unwrap_or(i64::MAX),
        delta_bps,
        expected_effect: expected_effect.to_string(),
    }
}

fn adaptive_policy_push_condition(
    conditions: &mut Vec<AdaptiveHostcallPolicyRollbackCondition>,
    code: &str,
    severity: &str,
    message: &str,
) {
    conditions.push(AdaptiveHostcallPolicyRollbackCondition {
        code: code.to_string(),
        severity: severity.to_string(),
        message: message.to_string(),
    });
}

#[allow(clippy::too_many_lines)]
pub fn build_adaptive_hostcall_policy_diff_report(
    request: &AdaptiveHostcallPolicyDiffRequest<'_>,
) -> AdaptiveHostcallPolicyDiffReport {
    let baseline_metrics = adaptive_policy_telemetry_metrics(request.baseline_telemetry);
    let candidate_metrics = adaptive_policy_telemetry_metrics(request.candidate_telemetry);
    let sample_support = adaptive_policy_sample_support(
        request.baseline_telemetry,
        request.candidate_telemetry,
        request.config,
    );
    let lane_changes = adaptive_policy_lane_changes(
        request.baseline_telemetry,
        request.candidate_telemetry,
        request.config.max_detailed_changes,
    );
    let action_changes = adaptive_policy_action_changes(
        request.baseline_telemetry,
        request.candidate_telemetry,
        request.config.max_detailed_changes,
    );
    let risk_threshold_changes =
        adaptive_policy_threshold_changes(request.baseline_config, request.candidate_config);
    let latency_effect =
        adaptive_policy_latency_effect(&baseline_metrics, &candidate_metrics, request.config);

    let mut rollback_conditions = Vec::new();
    if !sample_support.sufficient {
        adaptive_policy_push_condition(
            &mut rollback_conditions,
            "weak_sample_support",
            "warning",
            "candidate policy lacks the minimum replay sample support or matched coverage",
        );
    }
    if candidate_metrics.forced_compat_count > 0 {
        adaptive_policy_push_condition(
            &mut rollback_conditions,
            "forced_compat_kill_switch_active",
            "error",
            "candidate telemetry contains forced compatibility lane decisions",
        );
    }
    if !action_changes.is_empty() {
        adaptive_policy_push_condition(
            &mut rollback_conditions,
            "policy_action_divergence",
            "error",
            "candidate selected different runtime risk actions for replayed hostcalls",
        );
    }
    if candidate_metrics.compat_rate_bps
        > baseline_metrics
            .compat_rate_bps
            .saturating_add(request.config.max_compat_rate_increase_bps)
    {
        adaptive_policy_push_condition(
            &mut rollback_conditions,
            "compat_lane_regression",
            "error",
            "candidate increased compatibility lane routing beyond the configured tolerance",
        );
    }
    if candidate_metrics.error_rate_bps
        > baseline_metrics
            .error_rate_bps
            .saturating_add(request.config.max_error_rate_increase_bps)
    {
        adaptive_policy_push_condition(
            &mut rollback_conditions,
            "error_rate_regression",
            "error",
            "candidate increased hostcall error rate beyond the configured tolerance",
        );
    }
    if request.baseline_config.fail_closed && !request.candidate_config.fail_closed {
        adaptive_policy_push_condition(
            &mut rollback_conditions,
            "fail_closed_disabled",
            "error",
            "candidate disables fail-closed controller fallback behavior",
        );
    }
    if request.candidate_config.alpha > request.baseline_config.alpha {
        adaptive_policy_push_condition(
            &mut rollback_conditions,
            "risk_error_budget_relaxed",
            "warning",
            "candidate relaxes the runtime risk type-I error budget",
        );
    }

    let has_error = rollback_conditions
        .iter()
        .any(|condition| condition.severity == "error");
    let verdict = if has_error {
        AdaptiveHostcallPolicyDiffVerdict::Rollback
    } else if rollback_conditions.is_empty() && latency_effect.expected_effect == "improved" {
        AdaptiveHostcallPolicyDiffVerdict::Accept
    } else {
        AdaptiveHostcallPolicyDiffVerdict::Monitor
    };

    let mut reason_codes = Vec::new();
    if sample_support.sufficient {
        reason_codes.push("sufficient_sample_support".to_string());
    }
    reason_codes.push(format!("latency_effect_{}", latency_effect.expected_effect));
    if lane_changes.is_empty() {
        reason_codes.push("lane_output_stable".to_string());
    } else {
        reason_codes.push("lane_output_changed".to_string());
    }
    if action_changes.is_empty() {
        reason_codes.push("risk_actions_stable".to_string());
    }
    for condition in &rollback_conditions {
        reason_codes.push(condition.code.clone());
    }

    AdaptiveHostcallPolicyDiffReport {
        schema: ADAPTIVE_HOSTCALL_POLICY_DIFF_SCHEMA_VERSION.to_string(),
        generated_at_ms: request.generated_at_ms,
        baseline_policy_id: request.baseline_policy_id.to_string(),
        candidate_policy_id: request.candidate_policy_id.to_string(),
        baseline_source_schema: request.baseline_telemetry.schema.clone(),
        candidate_source_schema: request.candidate_telemetry.schema.clone(),
        verdict,
        reason_codes,
        sample_support,
        baseline_metrics,
        candidate_metrics,
        latency_effect,
        risk_threshold_changes,
        lane_changes,
        action_changes,
        rollback_conditions,
    }
}

// ============================================================================
// Baseline Model Builder (bd-153pv)
// ============================================================================

/// Compute the median of a sorted slice. Returns 0.0 for empty slices.
fn baseline_median(sorted: &[f64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        // Use midpoint arithmetic to avoid overflow lint
        let a = sorted[mid - 1];
        let b = sorted[mid];
        a + (b - a) / 2.0
    } else {
        sorted[mid]
    }
}

/// Compute the Median Absolute Deviation of a sorted slice.
/// MAD = median(|x_i - median(x)|).
fn baseline_mad(sorted: &[f64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let med = baseline_median(sorted);
    let mut deviations: Vec<f64> = sorted.iter().map(|x| (x - med).abs()).collect();
    deviations.sort_by(f64::total_cmp);
    baseline_median(&deviations)
}

/// Map a `RuntimeRiskStateLabelValue` to a 0-indexed state for the Markov matrix.
const fn state_label_to_index(label: RuntimeRiskStateLabelValue) -> usize {
    match label {
        RuntimeRiskStateLabelValue::SafeFast => 0,
        RuntimeRiskStateLabelValue::Suspicious => 1,
        RuntimeRiskStateLabelValue::Unsafe => 2,
    }
}

/// Build a Markov transition matrix from a sequence of state labels.
/// Uses Dirichlet smoothing (additive prior) for sparse data.
#[allow(clippy::cast_precision_loss)]
fn build_markov_transition_matrix(
    states: &[RuntimeRiskStateLabelValue],
    smoothing_prior: f64,
) -> BaselineMarkovTransitionMatrix {
    let mut counts = [[0u64; 3]; 3];
    let mut total_transitions = 0u64;
    for window in states.windows(2) {
        let from = state_label_to_index(window[0]);
        let to = state_label_to_index(window[1]);
        counts[from][to] += 1;
        total_transitions += 1;
    }

    // Smooth and normalize
    let mut probabilities = [[0.0f64; 3]; 3];
    for (i, row) in counts.iter().enumerate() {
        let row_sum: f64 = row.iter().map(|&c| c as f64).sum::<f64>();
        let row_total = 3.0f64.mul_add(smoothing_prior, row_sum);
        for (j, &count) in row.iter().enumerate() {
            probabilities[i][j] = (count as f64 + smoothing_prior) / row_total;
        }
    }

    // Compute stationary distribution via power iteration.
    let stationary = markov_stationary_distribution(&probabilities);

    BaselineMarkovTransitionMatrix {
        counts,
        probabilities,
        smoothing_prior,
        total_transitions,
        stationary_distribution: stationary,
    }
}

/// Compute stationary distribution of a 3x3 transition matrix via power iteration.
fn markov_stationary_distribution(prob: &[[f64; 3]; 3]) -> [f64; 3] {
    let mut pi = [1.0 / 3.0; 3];
    for _ in 0..200 {
        let mut next = [0.0f64; 3];
        for (j, next_j) in next.iter_mut().enumerate() {
            for (i, pi_i) in pi.iter().enumerate() {
                *next_j += pi_i * prob[i][j];
            }
        }
        // Normalize
        let sum: f64 = next.iter().sum();
        if sum > 0.0 {
            for v in &mut next {
                *v /= sum;
            }
        }
        pi = next;
    }
    pi
}

/// Compute KL divergence D_KL(p || q) for two discrete distributions.
/// Returns 0.0 if distributions are identical. Uses floor of 1e-12 for q to
/// avoid log(0).
fn kl_divergence_discrete3(p: &[f64; 3], q: &[f64; 3]) -> f64 {
    let mut kl = 0.0f64;
    for (i, &p_i) in p.iter().enumerate() {
        if p_i > 0.0 {
            let q_i = q[i].max(1e-12);
            kl = p_i.mul_add((p_i / q_i).ln(), kl);
        }
    }
    kl.max(0.0)
}

/// Build a per-capability profile from ledger entries for a single capability.
fn build_capability_profile(
    capability: &str,
    entries: &[&RuntimeRiskLedgerArtifactEntry],
) -> BaselineCapabilityProfile {
    let mut risk_scores: Vec<f64> = entries.iter().map(|e| e.risk_score).collect();
    risk_scores.sort_by(f64::total_cmp);

    let median = baseline_median(&risk_scores);
    let mad = baseline_mad(&risk_scores);
    let p5 = runtime_risk_quantile(risk_scores.clone(), 0.05);
    let p95 = runtime_risk_quantile(risk_scores, 0.95);

    // Compute error rate from outcome_error_code presence
    let error_count = entries
        .iter()
        .filter(|e| e.outcome_error_code.is_some())
        .count();
    #[allow(clippy::cast_precision_loss)]
    let error_rate = if entries.is_empty() {
        0.0
    } else {
        error_count as f64 / entries.len() as f64
    };

    // burst_density is harder to compute from ledger entries (no feature vector),
    // but we can estimate from timestamp clustering
    let mut timestamps: Vec<i64> = entries.iter().map(|e| e.ts_ms).collect();
    timestamps.sort_unstable();
    let burst_1s_median = estimate_burst_density(&timestamps, 1000);
    let burst_10s_median = estimate_burst_density(&timestamps, 10_000);

    BaselineCapabilityProfile {
        capability: capability.to_string(),
        sample_count: entries.len(),
        risk_score_median: median,
        risk_score_mad: mad,
        risk_score_p5: p5,
        risk_score_p95: p95,
        error_rate_median: error_rate,
        burst_density_1s_median: burst_1s_median,
        burst_density_10s_median: burst_10s_median,
    }
}

/// Estimate median burst density for a set of sorted timestamps within a given
/// window size (in ms). Returns 0.0 if there are fewer than 2 timestamps.
fn estimate_burst_density(sorted_timestamps: &[i64], window_ms: i64) -> f64 {
    if sorted_timestamps.len() < 2 {
        return 0.0;
    }
    let mut densities = Vec::with_capacity(sorted_timestamps.len());
    for (i, &ts) in sorted_timestamps.iter().enumerate() {
        let count = sorted_timestamps[i..]
            .iter()
            .take_while(|&&t| t - ts <= window_ms)
            .count();
        // Normalize: 8 for 1s, 24 for 10s (same as feature extraction)
        let normalizer = if window_ms <= 1000 { 8.0 } else { 24.0 };
        #[allow(clippy::cast_precision_loss)]
        densities.push(runtime_risk_clamp01(count as f64 / normalizer));
    }
    densities.sort_by(f64::total_cmp);
    baseline_median(&densities)
}

/// Build a complete baseline model from a runtime risk ledger artifact.
///
/// The baseline captures per-capability robust statistics (median/MAD/quantiles)
/// and a Markov transition matrix over risk state labels, both of which can be
/// used by the online scorer for drift detection.
pub fn build_baseline_from_ledger(
    artifact: &RuntimeRiskLedgerArtifact,
    extension_id: &str,
) -> Result<RuntimeRiskBaselineModel> {
    build_baseline_from_ledger_with_options(artifact, extension_id, 3.0, 0.5, 1.0)
}

/// Build a baseline model with customizable thresholds.
pub fn build_baseline_from_ledger_with_options(
    artifact: &RuntimeRiskLedgerArtifact,
    extension_id: &str,
    anomaly_threshold_mads: f64,
    transition_divergence_threshold: f64,
    smoothing_prior: f64,
) -> Result<RuntimeRiskBaselineModel> {
    let verification = verify_runtime_risk_ledger_artifact(artifact);
    if !verification.valid {
        return Err(Error::validation(
            "cannot build baseline from invalid ledger".to_string(),
        ));
    }
    if artifact.entries.is_empty() {
        return Err(Error::validation(
            "baseline requires at least one ledger entry".to_string(),
        ));
    }

    // Filter to this extension only
    let ext_entries: Vec<&RuntimeRiskLedgerArtifactEntry> = artifact
        .entries
        .iter()
        .filter(|e| e.extension_id == extension_id)
        .collect();

    if ext_entries.is_empty() {
        return Err(Error::validation(format!(
            "no ledger entries found for extension '{extension_id}'"
        )));
    }

    // Group by capability
    let mut by_capability: std::collections::BTreeMap<
        String,
        Vec<&RuntimeRiskLedgerArtifactEntry>,
    > = std::collections::BTreeMap::new();
    for entry in &ext_entries {
        by_capability
            .entry(entry.capability.clone())
            .or_default()
            .push(entry);
    }

    let capability_profiles: Vec<BaselineCapabilityProfile> = by_capability
        .iter()
        .map(|(cap, entries)| build_capability_profile(cap, entries))
        .collect();

    // Build Markov transition matrix from state sequence
    let states: Vec<RuntimeRiskStateLabelValue> =
        ext_entries.iter().map(|e| e.derived_state).collect();
    let transition_matrix = build_markov_transition_matrix(&states, smoothing_prior);

    Ok(RuntimeRiskBaselineModel {
        schema: RUNTIME_RISK_BASELINE_SCHEMA_VERSION.to_string(),
        extension_id: extension_id.to_string(),
        generated_at_ms: runtime_risk_now_ms(),
        source_data_hash: artifact.data_hash.clone(),
        source_entry_count: ext_entries.len(),
        capability_profiles,
        transition_matrix,
        anomaly_threshold_mads,
        transition_divergence_threshold,
    })
}

/// Detect drift in live features compared to a baseline model.
///
/// Returns a drift report with individual anomalies (metrics exceeding the MAD
/// threshold) and Markov transition divergence.
#[allow(clippy::too_many_arguments)]
pub fn detect_baseline_drift(
    baseline: &RuntimeRiskBaselineModel,
    extension_id: &str,
    capability: &str,
    live_risk_score: f64,
    live_error_rate: f64,
    live_burst_1s: f64,
    live_burst_10s: f64,
    recent_states: &[RuntimeRiskStateLabelValue],
) -> BaselineDriftReport {
    let profile = baseline
        .capability_profiles
        .iter()
        .find(|p| p.capability == capability);

    let mut anomalies = Vec::new();
    let mut drift_detected = false;

    if let Some(prof) = profile {
        // Check risk score deviation
        let mad_threshold = baseline.anomaly_threshold_mads;

        let check_metric = |metric: &str,
                            observed: f64,
                            median: f64,
                            mad: f64,
                            anomalies: &mut Vec<BaselineDriftAnomaly>|
         -> bool {
            // Use a floor of 0.01 for MAD to avoid infinite deviation on constant data
            let effective_mad = mad.max(0.01);
            let deviation = (observed - median).abs() / effective_mad;
            if deviation > mad_threshold {
                anomalies.push(BaselineDriftAnomaly {
                    metric: metric.to_string(),
                    observed,
                    baseline_median: median,
                    baseline_mad: mad,
                    deviation_mads: deviation,
                    explanation: format!(
                        "{metric} = {observed:.4} is {deviation:.1} MADs from baseline median \
                         {median:.4} (MAD={mad:.4})"
                    ),
                });
                true
            } else {
                false
            }
        };

        drift_detected |= check_metric(
            "risk_score",
            live_risk_score,
            prof.risk_score_median,
            prof.risk_score_mad,
            &mut anomalies,
        );
        drift_detected |= check_metric(
            "error_rate",
            live_error_rate,
            prof.error_rate_median,
            prof.error_rate_median.max(0.01), // Use median as proxy MAD when no explicit MAD
            &mut anomalies,
        );
        drift_detected |= check_metric(
            "burst_density_1s",
            live_burst_1s,
            prof.burst_density_1s_median,
            prof.burst_density_1s_median.max(0.01),
            &mut anomalies,
        );
        drift_detected |= check_metric(
            "burst_density_10s",
            live_burst_10s,
            prof.burst_density_10s_median,
            prof.burst_density_10s_median.max(0.01),
            &mut anomalies,
        );
    }

    // Check Markov transition divergence
    let mut transition_divergence = 0.0;
    let mut transition_anomalous = false;
    if recent_states.len() >= 2 {
        // Build observed transition matrix from recent states
        let observed_matrix = build_markov_transition_matrix(recent_states, 0.5);
        // Compare stationary distributions via KL divergence
        transition_divergence = kl_divergence_discrete3(
            &observed_matrix.stationary_distribution,
            &baseline.transition_matrix.stationary_distribution,
        );
        if transition_divergence > baseline.transition_divergence_threshold {
            transition_anomalous = true;
            drift_detected = true;
        }
    }

    BaselineDriftReport {
        extension_id: extension_id.to_string(),
        capability: capability.to_string(),
        drift_detected,
        anomalies,
        transition_divergence,
        transition_anomalous,
    }
}

fn runtime_risk_choose_action(
    posterior: &RuntimeRiskPosterior,
    e_process_breach: bool,
    drift_detected: bool,
) -> (
    RuntimeRiskAction,
    RuntimeRiskExpectedLoss,
    Vec<String>,
    RuntimeRiskStateLabel,
) {
    // Asymmetric loss matrix:
    // - false allow on unsafe is very costly
    // - denying known-safe calls has meaningful UX/compat cost
    let allow_loss = 120.0f64.mul_add(posterior.unsafe_, 8.0 * posterior.suspicious);
    let harden_loss = 35.0f64.mul_add(
        posterior.unsafe_,
        3.0f64.mul_add(posterior.safe_fast, 2.0 * posterior.suspicious),
    );
    let deny_loss = 2.0f64.mul_add(
        posterior.unsafe_,
        20.0f64.mul_add(posterior.safe_fast, 4.0 * posterior.suspicious),
    );
    let terminate_loss = 1.0f64.mul_add(
        posterior.unsafe_,
        35.0f64.mul_add(posterior.safe_fast, 8.0 * posterior.suspicious),
    );

    let expected = RuntimeRiskExpectedLoss {
        allow: allow_loss,
        harden: harden_loss,
        deny: deny_loss,
        terminate: terminate_loss,
    };

    let mut best = RuntimeRiskAction::Allow;
    let mut best_loss = allow_loss;
    if harden_loss < best_loss {
        best = RuntimeRiskAction::Harden;
        best_loss = harden_loss;
    }
    if deny_loss < best_loss {
        best = RuntimeRiskAction::Deny;
        best_loss = deny_loss;
    }
    if terminate_loss < best_loss {
        best = RuntimeRiskAction::Terminate;
    }

    let mut triggers = Vec::new();
    if e_process_breach {
        triggers.push("e_process_breach".to_string());
        if matches!(best, RuntimeRiskAction::Allow) {
            best = RuntimeRiskAction::Harden;
        }
    }
    if drift_detected {
        triggers.push("drift_detected".to_string());
        if matches!(best, RuntimeRiskAction::Allow) {
            best = RuntimeRiskAction::Harden;
        }
    }

    let state_label = if posterior.unsafe_ >= 0.55 {
        RuntimeRiskStateLabel::Unsafe
    } else if posterior.suspicious >= 0.40 {
        RuntimeRiskStateLabel::Suspicious
    } else {
        RuntimeRiskStateLabel::SafeFast
    };

    (best, expected, triggers, state_label)
}

const fn runtime_risk_action_code(action: RuntimeRiskAction) -> &'static str {
    match action {
        RuntimeRiskAction::Allow => "allow",
        RuntimeRiskAction::Harden => "harden",
        RuntimeRiskAction::Deny => "deny",
        RuntimeRiskAction::Terminate => "terminate",
    }
}

const fn runtime_risk_selected_expected_loss(
    action: RuntimeRiskAction,
    expected_loss: &RuntimeRiskExpectedLoss,
) -> f64 {
    match action {
        RuntimeRiskAction::Allow => expected_loss.allow,
        RuntimeRiskAction::Harden => expected_loss.harden,
        RuntimeRiskAction::Deny => expected_loss.deny,
        RuntimeRiskAction::Terminate => expected_loss.terminate,
    }
}

const fn runtime_risk_default_explanation_level(
    action: RuntimeRiskAction,
    triggers: &[String],
    fallback_reason: Option<&str>,
) -> RuntimeRiskExplanationLevelValue {
    if fallback_reason.is_some()
        || matches!(
            action,
            RuntimeRiskAction::Deny | RuntimeRiskAction::Terminate
        )
    {
        RuntimeRiskExplanationLevelValue::Full
    } else if matches!(action, RuntimeRiskAction::Harden) || !triggers.is_empty() {
        RuntimeRiskExplanationLevelValue::Standard
    } else {
        RuntimeRiskExplanationLevelValue::Compact
    }
}

fn runtime_risk_sort_contributors(contributors: &mut [RuntimeRiskExplanationContributor]) {
    contributors.sort_by(|left, right| {
        right
            .magnitude
            .total_cmp(&left.magnitude)
            .then_with(|| left.code.cmp(&right.code))
    });
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn runtime_risk_build_explanation(
    action: RuntimeRiskAction,
    risk_score: f64,
    posterior: &RuntimeRiskPosterior,
    expected_loss: &RuntimeRiskExpectedLoss,
    features: &RuntimeHostcallFeatureVector,
    triggers: &[String],
    fallback_reason: Option<&str>,
    term_budget: usize,
    time_budget_ms: u64,
) -> (
    RuntimeRiskExplanationLevelValue,
    String,
    Vec<RuntimeRiskExplanationContributor>,
    RuntimeRiskExplanationBudgetState,
) {
    let started = Instant::now();
    let normalized_term_budget = term_budget.max(1);
    let mut level = runtime_risk_default_explanation_level(action, triggers, fallback_reason);
    let mut contributors = vec![
        RuntimeRiskExplanationContributor {
            code: "feature_base_score".to_string(),
            signed_impact: 0.50 * features.base_score,
            magnitude: (0.50 * features.base_score).abs(),
            rationale: "base capability/method/detail risk contribution".to_string(),
        },
        RuntimeRiskExplanationContributor {
            code: "feature_recent_mean_score".to_string(),
            signed_impact: 0.30 * features.recent_mean_score,
            magnitude: (0.30 * features.recent_mean_score).abs(),
            rationale: "recent moving-average risk contribution".to_string(),
        },
        RuntimeRiskExplanationContributor {
            code: "feature_recent_error_rate".to_string(),
            signed_impact: 0.12 * features.recent_error_rate,
            magnitude: (0.12 * features.recent_error_rate).abs(),
            rationale: "recent hostcall failure-rate contribution".to_string(),
        },
        RuntimeRiskExplanationContributor {
            code: "feature_burst_density_1s".to_string(),
            signed_impact: 0.08 * features.burst_density_1s,
            magnitude: (0.08 * features.burst_density_1s).abs(),
            rationale: "short-horizon call burst density contribution".to_string(),
        },
        RuntimeRiskExplanationContributor {
            code: "feature_prior_failure_streak".to_string(),
            signed_impact: 0.05 * features.prior_failure_streak_norm,
            magnitude: (0.05 * features.prior_failure_streak_norm).abs(),
            rationale: "prior failure streak contribution".to_string(),
        },
        RuntimeRiskExplanationContributor {
            code: "posterior_unsafe".to_string(),
            signed_impact: posterior.unsafe_,
            magnitude: posterior.unsafe_.abs(),
            rationale: "posterior probability of unsafe behavior".to_string(),
        },
        RuntimeRiskExplanationContributor {
            code: "posterior_suspicious".to_string(),
            signed_impact: 0.5 * posterior.suspicious,
            magnitude: (0.5 * posterior.suspicious).abs(),
            rationale: "posterior probability of suspicious behavior".to_string(),
        },
    ];

    let selected_loss = runtime_risk_selected_expected_loss(action, expected_loss);
    let loss_delta_vs_allow = expected_loss.allow - selected_loss;
    contributors.push(RuntimeRiskExplanationContributor {
        code: "expected_loss_delta_vs_allow".to_string(),
        signed_impact: loss_delta_vs_allow,
        magnitude: loss_delta_vs_allow.abs(),
        rationale: "expected-loss improvement versus allow action".to_string(),
    });

    for trigger in triggers {
        contributors.push(RuntimeRiskExplanationContributor {
            code: format!("trigger_{trigger}"),
            signed_impact: 0.1,
            magnitude: 0.1,
            rationale: format!("trigger `{trigger}` tightened action selection"),
        });
    }

    if let Some(reason) = fallback_reason {
        contributors.push(RuntimeRiskExplanationContributor {
            code: format!("fallback_{reason}"),
            signed_impact: 0.25,
            magnitude: 0.25,
            rationale: format!("fallback reason `{reason}` constrained decision output"),
        });
    }

    runtime_risk_sort_contributors(&mut contributors);

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let budget_exhausted_by_terms = contributors.len() > normalized_term_budget;
    let budget_exhausted_by_time = elapsed_ms > time_budget_ms;
    let exhausted = budget_exhausted_by_terms || budget_exhausted_by_time;
    let mut fallback_mode = false;

    if exhausted {
        level = RuntimeRiskExplanationLevelValue::Compact;
        fallback_mode = true;
        let action_code = runtime_risk_action_code(action);
        contributors = vec![
            RuntimeRiskExplanationContributor {
                code: format!("action_{action_code}"),
                signed_impact: 1.0,
                magnitude: 1.0,
                rationale: "conservative fallback preserves deterministic selected action"
                    .to_string(),
            },
            RuntimeRiskExplanationContributor {
                code: "budget_exhausted".to_string(),
                signed_impact: 1.0,
                magnitude: 1.0,
                rationale: "explanation budget exhausted; omitted speculative contributor terms"
                    .to_string(),
            },
        ];
    }

    let mut trigger_labels = triggers.to_vec();
    trigger_labels.sort();
    let trigger_summary = if trigger_labels.is_empty() {
        "none".to_string()
    } else {
        trigger_labels.join("|")
    };
    let action_code = runtime_risk_action_code(action);
    let summary = if fallback_mode {
        format!("action={action_code} score={risk_score:.3} conservative_explanation_fallback=true")
    } else {
        format!(
            "action={action_code} score={risk_score:.3} unsafe={:.3} suspicious={:.3} triggers={trigger_summary}",
            posterior.unsafe_, posterior.suspicious
        )
    };

    let terms_emitted = contributors.len();
    (
        level,
        summary,
        contributors,
        RuntimeRiskExplanationBudgetState {
            time_budget_ms,
            elapsed_ms,
            term_budget: normalized_term_budget,
            terms_emitted,
            exhausted,
            fallback_mode,
        },
    )
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Prompt,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCheck {
    pub decision: PolicyDecision,
    pub capability: String,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Policy explanation types (SEC-4.4)
// ---------------------------------------------------------------------------

/// Structured explanation of a single capability decision within a policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityExplanation {
    pub capability: String,
    pub decision: PolicyDecision,
    pub reason: String,
    pub is_dangerous: bool,
}

/// Full structured explanation of an effective policy, suitable for runtime
/// diagnostics and audit logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyExplanation {
    pub mode: ExtensionPolicyMode,
    pub default_caps: Vec<String>,
    pub deny_caps: Vec<String>,
    pub exec_mediation_enabled: bool,
    pub secret_broker_enabled: bool,
    pub capability_decisions: Vec<CapabilityExplanation>,
    /// Dangerous capabilities that the effective policy allows.
    pub dangerous_allowed: Vec<String>,
    /// Dangerous capabilities that the effective policy denies.
    pub dangerous_denied: Vec<String>,
    /// Extension ID used for evaluation, if any.
    pub extension_id: Option<String>,
}

/// Result of checking whether a profile transition constitutes a valid
/// downgrade (tightening of security posture).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileTransitionCheck {
    pub is_valid_downgrade: bool,
    pub exec_before: PolicyDecision,
    pub exec_after: PolicyDecision,
    pub env_before: PolicyDecision,
    pub env_after: PolicyDecision,
    pub mode_before: ExtensionPolicyMode,
    pub mode_after: ExtensionPolicyMode,
}

/// Audit trail entry for dangerous-capability opt-in via `allow_dangerous`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DangerousOptInAuditEntry {
    /// Source of the `allow_dangerous` flag (e.g. "config", "env").
    pub source: String,
    /// The effective profile at the time of opt-in.
    pub profile: String,
    /// Capabilities removed from the deny list.
    pub capabilities_unblocked: Vec<String>,
}

/// Map a [`PolicyDecision`] to a numeric strictness level.
/// Higher = stricter.
const fn decision_strictness(d: PolicyDecision) -> u8 {
    match d {
        PolicyDecision::Allow => 0,
        PolicyDecision::Prompt => 1,
        PolicyDecision::Deny => 2,
    }
}

/// Map a policy mode to a numeric strictness level.
const fn mode_strictness(m: ExtensionPolicyMode) -> u8 {
    match m {
        ExtensionPolicyMode::Permissive => 0,
        ExtensionPolicyMode::Prompt => 1,
        ExtensionPolicyMode::Strict => 2,
    }
}

// ---------------------------------------------------------------------------
// Precedence chain
// ---------------------------------------------------------------------------
//
// Policy evaluation follows a strict precedence order. Each layer either
// produces a terminal decision (Allow / Deny) or defers to the next layer.
//
//   1. **Per-extension deny** — if the capability is in the extension
//      override's `deny` list → Deny ("extension_deny").
//   2. **Global deny_caps** — if the capability is in the global `deny_caps`
//      list → Deny ("deny_caps").
//   3. **Per-extension allow** — if the capability is in the extension
//      override's `allow` list → Allow ("extension_allow").
//   4. **Global default_caps** — if the capability is in `default_caps`
//      → Allow ("default_caps").
//   5. **Mode fallback** — Strict → Deny, Prompt → Prompt, Permissive →
//      Allow.
//
// The effective mode is the per-extension override mode if set, otherwise
// the global mode.

impl ExtensionPolicy {
    /// Evaluate policy for a capability without extension context.
    ///
    /// Equivalent to `evaluate_for(capability, None)`.
    pub fn evaluate(&self, capability: &str) -> PolicyCheck {
        self.evaluate_for(capability, None)
    }

    /// Evaluate policy for a capability with optional extension context.
    ///
    /// Applies the full precedence chain documented above.
    #[allow(clippy::too_many_lines)]
    pub fn evaluate_for(&self, capability: &str, extension_id: Option<&str>) -> PolicyCheck {
        let normalized = capability.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return PolicyCheck {
                decision: PolicyDecision::Deny,
                capability: String::new(),
                reason: "empty_capability".to_string(),
            };
        }

        let ext_override = extension_id.and_then(|id| self.per_extension.get(id));

        // Layer 1: per-extension deny.
        if let Some(ovr) = ext_override
            && ovr
                .deny
                .iter()
                .any(|cap| cap.eq_ignore_ascii_case(&normalized))
        {
            return PolicyCheck {
                decision: PolicyDecision::Deny,
                capability: normalized,
                reason: "extension_deny".to_string(),
            };
        }

        // Layer 2: global deny_caps.
        if self
            .deny_caps
            .iter()
            .any(|cap| cap.eq_ignore_ascii_case(&normalized))
        {
            return PolicyCheck {
                decision: PolicyDecision::Deny,
                capability: normalized,
                reason: "deny_caps".to_string(),
            };
        }

        // Layer 3: per-extension allow.
        if let Some(ovr) = ext_override
            && ovr
                .allow
                .iter()
                .any(|cap| cap.eq_ignore_ascii_case(&normalized))
        {
            return PolicyCheck {
                decision: PolicyDecision::Allow,
                capability: normalized,
                reason: "extension_allow".to_string(),
            };
        }

        // Layer 4: global default_caps.
        let in_default_caps = self
            .default_caps
            .iter()
            .any(|cap| cap.eq_ignore_ascii_case(&normalized));

        // Layer 5: mode fallback (use per-extension mode if set).
        let effective_mode = ext_override.and_then(|ovr| ovr.mode).unwrap_or(self.mode);

        match effective_mode {
            ExtensionPolicyMode::Strict => PolicyCheck {
                decision: if in_default_caps {
                    PolicyDecision::Allow
                } else {
                    PolicyDecision::Deny
                },
                capability: normalized,
                reason: if in_default_caps {
                    "default_caps".to_string()
                } else {
                    "not_in_default_caps".to_string()
                },
            },
            ExtensionPolicyMode::Prompt => PolicyCheck {
                decision: if in_default_caps {
                    PolicyDecision::Allow
                } else {
                    PolicyDecision::Prompt
                },
                capability: normalized,
                reason: if in_default_caps {
                    "default_caps".to_string()
                } else {
                    "prompt_required".to_string()
                },
            },
            ExtensionPolicyMode::Permissive => PolicyCheck {
                decision: PolicyDecision::Allow,
                capability: normalized,
                reason: "permissive".to_string(),
            },
        }
    }

    /// Check whether a specific extension has any overrides configured.
    pub fn has_override(&self, extension_id: &str) -> bool {
        self.per_extension.contains_key(extension_id)
    }

    /// Create a policy from a named profile.
    pub fn from_profile(profile: PolicyProfile) -> Self {
        profile.to_policy()
    }

    /// Produce a structured explanation of the effective policy for all
    /// known capabilities. This is the runtime-callable counterpart to the
    /// CLI `--explain-extension-policy` flag — it can be invoked at any
    /// point during execution to inspect the live policy state.
    pub fn explain_effective_policy(&self, extension_id: Option<&str>) -> PolicyExplanation {
        let capability_decisions: Vec<CapabilityExplanation> = ALL_CAPABILITIES
            .iter()
            .map(|cap| {
                let check = self.evaluate_for(cap.as_str(), extension_id);
                CapabilityExplanation {
                    capability: cap.as_str().to_string(),
                    decision: check.decision,
                    reason: check.reason,
                    is_dangerous: cap.is_dangerous(),
                }
            })
            .collect();

        let dangerous_allowed = capability_decisions
            .iter()
            .filter(|c| c.is_dangerous && c.decision == PolicyDecision::Allow)
            .map(|c| c.capability.clone())
            .collect::<Vec<_>>();

        let dangerous_denied = capability_decisions
            .iter()
            .filter(|c| c.is_dangerous && c.decision == PolicyDecision::Deny)
            .map(|c| c.capability.clone())
            .collect::<Vec<_>>();

        PolicyExplanation {
            mode: self.mode,
            default_caps: self.default_caps.clone(),
            deny_caps: self.deny_caps.clone(),
            exec_mediation_enabled: self.exec_mediation.enabled,
            secret_broker_enabled: self.secret_broker.enabled,
            capability_decisions,
            dangerous_allowed,
            dangerous_denied,
            extension_id: extension_id.map(String::from),
        }
    }

    /// Verify that a profile transition from `from` to `to` produces a
    /// strictly tighter policy for dangerous capabilities. Returns `true`
    /// if the downgrade is valid (all dangerous caps that were denied in
    /// `from` are still denied in `to`, AND `to` denies at least as many).
    pub fn is_valid_downgrade(from: &Self, to: &Self) -> ProfileTransitionCheck {
        let from_exec = from.evaluate("exec").decision;
        let to_exec = to.evaluate("exec").decision;
        let from_env = from.evaluate("env").decision;
        let to_env = to.evaluate("env").decision;

        let exec_tightened = decision_strictness(to_exec) >= decision_strictness(from_exec);
        let env_tightened = decision_strictness(to_env) >= decision_strictness(from_env);

        let mode_tightened = mode_strictness(to.mode) >= mode_strictness(from.mode);

        ProfileTransitionCheck {
            is_valid_downgrade: exec_tightened && env_tightened && mode_tightened,
            exec_before: from_exec,
            exec_after: to_exec,
            env_before: from_env,
            env_after: to_env,
            mode_before: from.mode,
            mode_after: to.mode,
        }
    }
}

// ============================================================================
// PolicySnapshot — O(1) precomputed capability authorization
// ============================================================================

/// A single precomputed policy decision for a known capability.
#[derive(Debug, Clone, Copy)]
struct SnapshotEntry {
    decision: PolicyDecision,
    /// Static reason string (avoids allocation on the hot path).
    reason: &'static str,
}

impl Default for SnapshotEntry {
    fn default() -> Self {
        Self {
            decision: PolicyDecision::Deny,
            reason: "not_computed",
        }
    }
}

/// Precomputed per-extension capability decision table for O(1) hostcall authorization.
///
/// Built once from an [`ExtensionPolicy`] at dispatcher creation time; all
/// subsequent lookups are constant-time array reads. For unknown capabilities
/// not in [`ALL_CAPABILITIES`], falls back to the original `evaluate_for()`
/// path.
#[derive(Debug, Clone)]
pub struct PolicySnapshot {
    /// Decisions for known capabilities evaluated without extension context.
    global: [SnapshotEntry; NUM_CAPABILITIES],
    /// Per-extension decisions keyed by extension ID.
    per_extension: HashMap<String, [SnapshotEntry; NUM_CAPABILITIES]>,
    /// The original policy for fallback on unknown capabilities.
    fallback: ExtensionPolicy,
}

impl PolicySnapshot {
    /// Compile a snapshot from the given policy.
    ///
    /// Precomputes decisions for every known capability in both the global
    /// context and each per-extension override.
    pub fn compile(policy: &ExtensionPolicy) -> Self {
        let global = Self::compute_decisions(policy, None);

        let per_extension: HashMap<String, [SnapshotEntry; NUM_CAPABILITIES]> = policy
            .per_extension
            .keys()
            .map(|ext_id| {
                let decisions = Self::compute_decisions(policy, Some(ext_id.as_str()));
                (ext_id.clone(), decisions)
            })
            .collect();

        Self {
            global,
            per_extension,
            fallback: policy.clone(),
        }
    }

    /// O(1) capability lookup. Returns a [`PolicyCheck`] for the given
    /// capability and optional extension context.
    ///
    /// Known capabilities (read, write, http, etc.) are resolved from the
    /// precomputed table. Unknown capabilities fall back to `evaluate_for()`.
    pub fn lookup(&self, capability: &str, extension_id: Option<&str>) -> PolicyCheck {
        Capability::parse(capability).map_or_else(
            // Unknown capability — fall back to full evaluation.
            || self.fallback.evaluate_for(capability, extension_id),
            |cap| {
                let idx = cap.index();
                let entry = extension_id
                    .and_then(|id| self.per_extension.get(id))
                    .map_or(&self.global[idx], |arr| &arr[idx]);
                PolicyCheck {
                    decision: entry.decision,
                    capability: capability.to_string(),
                    reason: entry.reason.to_string(),
                }
            },
        )
    }

    /// Build the decision array for all known capabilities.
    fn compute_decisions(
        policy: &ExtensionPolicy,
        extension_id: Option<&str>,
    ) -> [SnapshotEntry; NUM_CAPABILITIES] {
        let mut decisions = [SnapshotEntry::default(); NUM_CAPABILITIES];
        for cap in ALL_CAPABILITIES {
            let check = policy.evaluate_for(cap.as_str(), extension_id);
            decisions[cap.index()] = SnapshotEntry {
                decision: check.decision,
                reason: Self::intern_reason(&check.reason),
            };
        }
        decisions
    }

    /// Map dynamic reason strings to static equivalents to avoid per-lookup
    /// allocations. Unknown reasons get a generic fallback.
    fn intern_reason(reason: &str) -> &'static str {
        match reason {
            "default_caps" => "default_caps",
            "deny_caps" => "deny_caps",
            "extension_deny" => "extension_deny",
            "extension_allow" => "extension_allow",
            "not_in_default_caps" => "not_in_default_caps",
            "prompt_required" => "prompt_required",
            "permissive" => "permissive",
            "empty_capability" => "empty_capability",
            _ => "precomputed",
        }
    }
}

#[cfg(test)]
mod policy_snapshot_tests;

fn required_capability_for_host_call_static_legacy(call: &HostCallPayload) -> Option<&'static str> {
    let method = call.method.trim();
    if method.is_empty() {
        return None;
    }

    if method.eq_ignore_ascii_case("fs") {
        let op = call
            .params
            .get("op")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let op = FsOp::parse(op)?;
        return Some(op.required_capability());
    }

    if method.eq_ignore_ascii_case("tool") {
        let tool_name = call
            .params
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)?;
        if tool_name.is_empty() {
            return None;
        }

        if tool_name.eq_ignore_ascii_case("read")
            || tool_name.eq_ignore_ascii_case("grep")
            || tool_name.eq_ignore_ascii_case("find")
            || tool_name.eq_ignore_ascii_case("ls")
        {
            return Some("read");
        }
        if tool_name.eq_ignore_ascii_case("write") || tool_name.eq_ignore_ascii_case("edit") {
            return Some("write");
        }
        if tool_name.eq_ignore_ascii_case("bash") {
            return Some("exec");
        }
        return Some("tool");
    }

    if method.eq_ignore_ascii_case("exec") {
        Some("exec")
    } else if method.eq_ignore_ascii_case("env") {
        Some("env")
    } else if method.eq_ignore_ascii_case("http") {
        Some("http")
    } else if method.eq_ignore_ascii_case("session") {
        Some("session")
    } else if method.eq_ignore_ascii_case("ui") {
        Some("ui")
    } else if method.eq_ignore_ascii_case("events") {
        Some("events")
    } else if method.eq_ignore_ascii_case("log") {
        Some("log")
    } else {
        None
    }
}

pub(crate) fn required_capability_for_host_call_static(
    call: &HostCallPayload,
) -> Option<&'static str> {
    if let Ok(HostcallOpcodeResolution::FastPath { opcode, .. }) = resolve_hostcall_opcode(call) {
        return Some(opcode.required_capability());
    }
    required_capability_for_host_call_static_legacy(call)
}

pub fn required_capability_for_host_call(call: &HostCallPayload) -> Option<String> {
    required_capability_for_host_call_static(call).map(str::to_string)
}

// ============================================================================
// WASM Host Scaffold (minimal)
// ============================================================================

#[cfg(feature = "wasm-host")]
#[derive(Debug, Clone)]
pub struct WasmExtension {
    pub path: PathBuf,
}

#[cfg(feature = "wasm-host")]
#[allow(clippy::trait_duplication_in_bounds)]
mod wasm_host;

#[cfg(feature = "wasm-host")]
pub struct WasmExtensionHost {
    policy: ExtensionPolicy,
    cwd: PathBuf,
    engine: wasmtime::Engine,
}

#[cfg(feature = "wasm-host")]
impl WasmExtensionHost {
    pub fn new(cwd: &Path, policy: ExtensionPolicy) -> Result<Self> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);

        let engine = wasmtime::Engine::new(&config)
            .map_err(|err| Error::extension(format!("Failed to create WASM engine: {err}")))?;

        Ok(Self {
            policy,
            cwd: cwd.to_path_buf(),
            engine,
        })
    }

    pub const fn policy(&self) -> &ExtensionPolicy {
        &self.policy
    }

    pub fn load_from_path(&self, path: &Path) -> Result<WasmExtension> {
        if !path.exists() {
            return Err(Error::validation(format!(
                "Extension artifact not found: {}",
                path.display()
            )));
        }
        Ok(WasmExtension {
            path: path.to_path_buf(),
        })
    }

    pub async fn instantiate(&self, extension: &WasmExtension) -> Result<wasm_host::Instance> {
        wasm_host::Instance::instantiate(
            &self.engine,
            &extension.path,
            wasm_host::HostState::new(self.policy.clone(), self.cwd.clone())?,
        )
        .await
    }

    async fn instantiate_with(
        &self,
        extension: &WasmExtension,
        tools: Arc<ToolRegistry>,
        manager: Option<ExtensionManagerHandle>,
    ) -> Result<wasm_host::Instance> {
        wasm_host::Instance::instantiate(
            &self.engine,
            &extension.path,
            wasm_host::HostState::new_with_tools(
                self.policy.clone(),
                self.cwd.clone(),
                tools,
                manager,
            )?,
        )
        .await
    }
}

// ============================================================================
// Extension Event System
// ============================================================================

/// Default cancellation budget for extension event handlers (ms).
pub const EXTENSION_EVENT_TIMEOUT_MS: u64 = 5_000;

/// Tight cancellation budget for informational (fire-and-forget) event handlers.
///
/// This covers lifecycle notifications, telemetry pokes, and post-hoc updates.
/// A misbehaving or deadlocked extension on an info-only event shouldn't
/// stall the agent for the full general budget. See [`ExtensionEventName::is_informational`].
pub const EXTENSION_INFO_EVENT_TIMEOUT_MS: u64 = 500;

/// Default cancellation budget for extension tool execution (ms).
pub const EXTENSION_TOOL_BUDGET_MS: u64 = 30_000;

/// Default cancellation budget for extension command execution (ms).
pub const EXTENSION_COMMAND_BUDGET_MS: u64 = 30_000;

/// Default cancellation budget for extension shortcut execution (ms).
pub const EXTENSION_SHORTCUT_BUDGET_MS: u64 = 30_000;

/// Default cancellation budget for UI dialog operations (ms).
pub const EXTENSION_UI_BUDGET_MS: u64 = 1_000;

/// Default cancellation budget for provider stream operations (ms).
pub const EXTENSION_PROVIDER_BUDGET_MS: u64 = 120_000;

/// Default cancellation budget for extension queries (get tools, pump, flags) (ms).
pub const EXTENSION_QUERY_BUDGET_MS: u64 = 10_000;

/// Default cancellation budget for extension loading (ms).
pub const EXTENSION_LOAD_BUDGET_MS: u64 = 60_000;

/// Create a [`Cx`] with a deadline budget derived from `timeout_ms`.
///
/// The returned context will cancel any async operation that exceeds the
/// deadline, integrating with asupersync's structured concurrency protocol.
fn cx_with_deadline(timeout_ms: u64) -> Cx {
    let budget = Budget {
        deadline: Some(wall_now() + Duration::from_millis(timeout_ms)),
        ..Budget::INFINITE
    };
    Cx::for_request_with_budget(budget)
}

fn js_runtime_request_deadline(timeout_ms: u64) -> Instant {
    Instant::now()
        .checked_add(Duration::from_millis(timeout_ms))
        .unwrap_or_else(Instant::now)
}

fn js_runtime_remaining_timeout_ms(deadline: Instant, operation: &str) -> Result<u64> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            Error::extension(format!(
                "JS extension runtime {operation} expired before actor execution"
            ))
        })?;
    Ok(u64::try_from(remaining.as_millis())
        .unwrap_or(u64::MAX)
        .max(1))
}

/// Event names for the extension lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionEventName {
    /// Agent startup (once per session).
    Startup,
    /// Input from the user.
    Input,
    /// Before the agent starts processing.
    BeforeAgentStart,
    /// Before provider call; can modify context messages.
    Context,
    /// Agent started processing.
    AgentStart,
    /// Agent ended processing.
    AgentEnd,
    /// Turn lifecycle start.
    TurnStart,
    /// Turn lifecycle end.
    TurnEnd,
    /// Message lifecycle start.
    MessageStart,
    /// Message lifecycle update (assistant streaming).
    MessageUpdate,
    /// Message lifecycle end.
    MessageEnd,
    /// Tool execution start.
    ToolExecutionStart,
    /// Tool execution update.
    ToolExecutionUpdate,
    /// Tool execution end.
    ToolExecutionEnd,
    /// Tool call (pre-exec; can block).
    ToolCall,
    /// Tool result (post-exec; can modify).
    ToolResult,
    /// Session start.
    SessionStart,
    /// Session before switch.
    SessionBeforeSwitch,
    /// Session switched.
    SessionSwitch,
    /// Session before fork.
    SessionBeforeFork,
    /// Session forked.
    SessionFork,
    /// Session before compact.
    SessionBeforeCompact,
    /// Session compacted.
    SessionCompact,
    /// Resource discovery request.
    ResourcesDiscover,
    /// Model selection changed.
    ModelSelect,
    /// User-initiated bash command.
    UserBash,
    /// Session before tree view.
    SessionBeforeTree,
    /// Session tree navigation.
    SessionTree,
    /// Session shutdown.
    SessionShutdown,
}

impl std::fmt::Display for ExtensionEventName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Startup => "startup",
            Self::Input => "input",
            Self::BeforeAgentStart => "before_agent_start",
            Self::Context => "context",
            Self::AgentStart => "agent_start",
            Self::AgentEnd => "agent_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::MessageStart => "message_start",
            Self::MessageUpdate => "message_update",
            Self::MessageEnd => "message_end",
            Self::ToolExecutionStart => "tool_execution_start",
            Self::ToolExecutionUpdate => "tool_execution_update",
            Self::ToolExecutionEnd => "tool_execution_end",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::SessionStart => "session_start",
            Self::SessionBeforeSwitch => "session_before_switch",
            Self::SessionSwitch => "session_switch",
            Self::SessionBeforeFork => "session_before_fork",
            Self::SessionFork => "session_fork",
            Self::SessionBeforeCompact => "session_before_compact",
            Self::SessionCompact => "session_compact",
            Self::ResourcesDiscover => "resources_discover",
            Self::ModelSelect => "model_select",
            Self::UserBash => "user_bash",
            Self::SessionBeforeTree => "session_before_tree",
            Self::SessionTree => "session_tree",
            Self::SessionShutdown => "session_shutdown",
        };
        write!(f, "{name}")
    }
}

impl ExtensionEventName {
    /// Returns `true` for fire-and-forget lifecycle/telemetry events where
    /// the dispatcher doesn't consume the handler's response to block,
    /// cancel, or transform anything.
    ///
    /// The per-extension deadline for these events is
    /// [`EXTENSION_INFO_EVENT_TIMEOUT_MS`] so one deadlocked extension on
    /// an info-only hook can't stall the agent for the full
    /// [`EXTENSION_EVENT_TIMEOUT_MS`].
    ///
    /// **Actionable** events (not listed here) still use the longer
    /// budget so handlers have room to do meaningful work before a
    /// decision is made:
    /// `BeforeAgentStart`, `Context`, `ToolCall`, `ToolResult` (can
    /// modify the tool's result payload), `Input`,
    /// `SessionBeforeSwitch`, `SessionBeforeFork`, `SessionBeforeCompact`,
    /// `SessionBeforeTree`, `ResourcesDiscover`.
    #[must_use]
    pub const fn is_informational(self) -> bool {
        // Exhaustive match (no `_ => …` fallthrough) so that adding a
        // new `ExtensionEventName` variant forces the author to
        // classify it here. Without this, a new variant would silently
        // fall into the "actionable" default and get a 5s timeout —
        // probably harmless, but easy to miss.
        match self {
            Self::Startup
            | Self::AgentStart
            | Self::AgentEnd
            | Self::TurnStart
            | Self::TurnEnd
            | Self::MessageStart
            | Self::MessageUpdate
            | Self::MessageEnd
            | Self::ToolExecutionStart
            | Self::ToolExecutionUpdate
            | Self::ToolExecutionEnd
            | Self::SessionStart
            | Self::SessionSwitch
            | Self::SessionFork
            | Self::SessionCompact
            | Self::SessionTree
            | Self::SessionShutdown
            | Self::ModelSelect
            | Self::UserBash => true,
            Self::Input
            | Self::BeforeAgentStart
            | Self::Context
            | Self::ToolCall
            | Self::ToolResult
            | Self::SessionBeforeSwitch
            | Self::SessionBeforeFork
            | Self::SessionBeforeCompact
            | Self::SessionBeforeTree
            | Self::ResourcesDiscover => false,
        }
    }

    /// Deadline this event should be dispatched with when no explicit
    /// caller-provided timeout is in play. See [`Self::is_informational`].
    #[must_use]
    pub const fn default_timeout_ms(self) -> u64 {
        if self.is_informational() {
            EXTENSION_INFO_EVENT_TIMEOUT_MS
        } else {
            EXTENSION_EVENT_TIMEOUT_MS
        }
    }
}

// ============================================================================
// Extension Manifest + Load Specs
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionRuntime {
    Js,
    #[serde(rename = "native-rust")]
    NativeRust,
    Wasm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    pub schema: String,
    pub extension_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub api_version: String,
    pub runtime: ExtensionRuntime,
    pub entrypoint: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_manifest: Option<CapabilityManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ExtensionManifest {
    fn normalize(
        mut self,
        package_name: Option<String>,
        package_version: Option<String>,
    ) -> Result<Self> {
        if self.name.trim().is_empty()
            && let Some(name) = package_name
        {
            self.name = name;
        }

        if self.version.trim().is_empty()
            && let Some(version) = package_version
        {
            self.version = version;
        }

        if self.api_version.trim().is_empty() {
            self.api_version = PROTOCOL_VERSION.to_string();
        }

        validate_extension_manifest(&self)?;
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub struct ExtensionManifestSource {
    pub manifest: ExtensionManifest,
    pub manifest_json: String,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
}

impl ExtensionManifestSource {
    pub fn entry_path(&self) -> PathBuf {
        self.root.join(self.manifest.entrypoint.trim())
    }
}

#[derive(Debug, Clone)]
pub enum ExtensionLoadSpec {
    Js(JsExtensionLoadSpec),
    NativeRust(NativeRustExtensionLoadSpec),
    #[cfg(feature = "wasm-host")]
    Wasm(WasmExtensionLoadSpec),
}

#[cfg(feature = "wasm-host")]
#[derive(Debug, Clone)]
pub struct WasmExtensionLoadSpec {
    pub manifest: ExtensionManifest,
    pub manifest_json: String,
    pub root: PathBuf,
    pub entry_path: PathBuf,
}

fn extension_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9._-]{0,63}$").expect("regex"))
}

fn validate_extension_manifest(manifest: &ExtensionManifest) -> Result<()> {
    if manifest.schema != "pi.ext.manifest.v1" {
        return Err(Error::validation(format!(
            "Unsupported extension manifest schema: {}",
            manifest.schema
        )));
    }

    let extension_id = manifest.extension_id.trim();
    if extension_id.is_empty() {
        return Err(Error::validation(
            "Extension manifest extension_id is empty",
        ));
    }
    if !extension_id_regex().is_match(extension_id) {
        return Err(Error::validation(format!(
            "Invalid extension_id '{extension_id}'"
        )));
    }

    if manifest.name.trim().is_empty() {
        return Err(Error::validation("Extension manifest name is empty"));
    }
    if manifest.version.trim().is_empty() {
        return Err(Error::validation("Extension manifest version is empty"));
    }
    if manifest.api_version.trim().is_empty() {
        return Err(Error::validation("Extension manifest api_version is empty"));
    }
    if manifest.entrypoint.trim().is_empty() {
        return Err(Error::validation("Extension manifest entrypoint is empty"));
    }
    let entry_path = Path::new(manifest.entrypoint.trim());
    if entry_path.is_absolute()
        || entry_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(Error::validation(format!(
            "Extension manifest entrypoint must be a relative path inside the extension root: {}",
            manifest.entrypoint
        )));
    }

    if let Some(capability_manifest) = &manifest.capability_manifest {
        validate_capability_manifest(capability_manifest)?;
    }

    Ok(())
}

fn read_package_json_meta(root: &Path) -> Option<(Option<String>, Option<String>, Option<Value>)> {
    let package_json = root.join("package.json");
    if !package_json.exists() {
        return None;
    }
    let raw = fs::read_to_string(package_json).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    let name = json.get("name").and_then(Value::as_str).map(str::to_string);
    let version = json
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_string);
    let pi = json.get("pi").cloned();
    Some((name, version, pi))
}

fn parse_extension_manifest_value(
    value: Value,
    package_name: Option<String>,
    package_version: Option<String>,
) -> Result<ExtensionManifest> {
    let manifest: ExtensionManifest = serde_json::from_value(value)
        .map_err(|err| Error::validation(format!("Invalid extension manifest: {err}")))?;
    manifest.normalize(package_name, package_version)
}

pub fn load_extension_manifest(root: &Path) -> Result<Option<ExtensionManifestSource>> {
    let (package_name, package_version, package_pi) =
        read_package_json_meta(root).unwrap_or((None, None, None));

    let extension_json = root.join("extension.json");
    if extension_json.exists() {
        let raw = fs::read_to_string(&extension_json).map_err(|err| {
            Error::validation(format!(
                "Failed to read extension manifest {}: {err}",
                extension_json.display()
            ))
        })?;
        let value: Value = serde_json::from_str(&raw).map_err(|err| {
            Error::validation(format!(
                "Failed to parse extension manifest {}: {err}",
                extension_json.display()
            ))
        })?;
        let manifest = parse_extension_manifest_value(value, package_name, package_version)?;
        let manifest_json = serde_json::to_string(&manifest)
            .map_err(|err| Error::validation(format!("Serialize manifest: {err}")))?;
        return Ok(Some(ExtensionManifestSource {
            manifest,
            manifest_json,
            root: root.to_path_buf(),
            manifest_path: extension_json,
        }));
    }

    if let Some(pi) = package_pi
        && pi.get("schema").and_then(Value::as_str) == Some("pi.ext.manifest.v1")
    {
        let manifest = parse_extension_manifest_value(pi, package_name, package_version)?;
        let manifest_json = serde_json::to_string(&manifest)
            .map_err(|err| Error::validation(format!("Serialize manifest: {err}")))?;
        let manifest_path = root.join("package.json");
        return Ok(Some(ExtensionManifestSource {
            manifest,
            manifest_json,
            root: root.to_path_buf(),
            manifest_path,
        }));
    }

    Ok(None)
}

fn resolve_extension_index(root: &Path) -> Option<PathBuf> {
    let index_native = root.join("index.native.json");
    if index_native.exists() {
        return Some(index_native);
    }
    for ext in JS_EXTENSION_ENTRY_EXTS {
        let candidate = root.join(format!("index.{ext}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

impl ExtensionManifestSource {
    fn to_load_spec(&self) -> Result<ExtensionLoadSpec> {
        let entry_path = self.entry_path();
        if !entry_path.exists() {
            return Err(Error::validation(format!(
                "Extension entrypoint not found: {}",
                entry_path.display()
            )));
        }

        match self.manifest.runtime {
            ExtensionRuntime::Js => Ok(ExtensionLoadSpec::Js(JsExtensionLoadSpec::from_manifest(
                &self.manifest,
                &self.root,
            )?)),
            ExtensionRuntime::NativeRust => Ok(ExtensionLoadSpec::NativeRust(
                NativeRustExtensionLoadSpec::from_manifest(&self.manifest, &self.root)?,
            )),
            ExtensionRuntime::Wasm => {
                #[cfg(feature = "wasm-host")]
                {
                    Ok(ExtensionLoadSpec::Wasm(WasmExtensionLoadSpec {
                        manifest: self.manifest.clone(),
                        manifest_json: self.manifest_json.clone(),
                        root: self.root.clone(),
                        entry_path,
                    }))
                }
                #[cfg(not(feature = "wasm-host"))]
                {
                    Err(Error::validation(
                        "WASM extensions require the `wasm-host` feature".to_string(),
                    ))
                }
            }
        }
    }
}

pub fn resolve_extension_load_spec(entry: &Path) -> Result<ExtensionLoadSpec> {
    if entry.is_dir() {
        if let Some(source) = load_extension_manifest(entry)? {
            return source.to_load_spec();
        }
        if let Some(index) = resolve_extension_index(entry) {
            if index
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "index.native.json")
            {
                return Ok(ExtensionLoadSpec::NativeRust(
                    NativeRustExtensionLoadSpec::from_entry_path(index)?,
                ));
            }
            return Ok(ExtensionLoadSpec::Js(JsExtensionLoadSpec::from_entry_path(
                index,
            )?));
        }
        return Err(Error::validation(format!(
            "Extension directory has no manifest or entrypoint: {}",
            entry.display()
        )));
    }

    if entry.is_file() {
        if entry
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s == "extension.json")
        {
            let root = entry.parent().unwrap_or(entry);
            if let Some(source) = load_extension_manifest(root)? {
                return source.to_load_spec();
            }
        }

        if entry
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with(".native.json"))
        {
            return Ok(ExtensionLoadSpec::NativeRust(
                NativeRustExtensionLoadSpec::from_entry_path(entry)?,
            ));
        }

        if let Some(ext) = entry.extension().and_then(|s| s.to_str()) {
            match ext {
                "wasm" => {
                    #[cfg(feature = "wasm-host")]
                    {
                        if let Some(source) =
                            load_extension_manifest(entry.parent().unwrap_or(entry))?
                        {
                            let spec = source.to_load_spec()?;
                            if let ExtensionLoadSpec::Wasm(wasm_spec) = spec {
                                if wasm_spec.entry_path != entry {
                                    return Err(Error::validation(format!(
                                        "WASM entrypoint mismatch: manifest entrypoint is {}, but got {}",
                                        wasm_spec.entry_path.display(),
                                        entry.display()
                                    )));
                                }
                                return Ok(ExtensionLoadSpec::Wasm(wasm_spec));
                            }
                            return Err(Error::validation(format!(
                                "Extension manifest runtime is not wasm for {}",
                                entry.display()
                            )));
                        }
                        return Err(Error::validation(format!(
                            "WASM extension requires extension.json or package.json#pi manifest: {}",
                            entry.display()
                        )));
                    }
                    #[cfg(not(feature = "wasm-host"))]
                    {
                        return Err(Error::validation(
                            "WASM extensions require the `wasm-host` feature".to_string(),
                        ));
                    }
                }
                "js" | "ts" | "mjs" | "cjs" | "tsx" | "mts" | "cts" => {
                    return Ok(ExtensionLoadSpec::Js(JsExtensionLoadSpec::from_entry_path(
                        entry,
                    )?));
                }
                _ => {}
            }
        }
    }

    Err(Error::validation(format!(
        "Unsupported extension entry: {}",
        entry.display()
    )))
}

// ============================================================================
// JS Extension Runtime (QuickJS via PiJsRuntime)
// ============================================================================

#[derive(Debug, Clone)]
pub struct JsExtensionLoadSpec {
    pub extension_id: String,
    pub entry_path: PathBuf,
    pub name: String,
    pub version: String,
    pub api_version: String,
}

impl JsExtensionLoadSpec {
    pub fn from_entry_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(Error::validation(format!(
                "Extension entry does not exist: {}",
                path.display()
            )));
        }

        let entry_path = safe_canonicalize(path);

        let file_stem = entry_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if file_stem.is_empty() {
            return Err(Error::validation(format!(
                "Extension entry has no filename: {}",
                entry_path.display()
            )));
        }

        let extension_id = if file_stem == "index" {
            entry_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .trim()
                .to_string()
        } else {
            file_stem
        };

        if extension_id.is_empty() {
            return Err(Error::validation(format!(
                "Could not derive extension id from entry path: {}",
                entry_path.display()
            )));
        }

        let mut name = extension_id.clone();
        let mut version = "0.0.0".to_string();

        if let Some(parent) = entry_path.parent() {
            let manifest_path = parent.join("package.json");
            if manifest_path.exists()
                && let Ok(raw) = fs::read_to_string(&manifest_path)
                && let Ok(json) = serde_json::from_str::<Value>(&raw)
            {
                if let Some(manifest_name) = json.get("name").and_then(Value::as_str)
                    && !manifest_name.trim().is_empty()
                {
                    name = manifest_name.trim().to_string();
                }
                if let Some(manifest_version) = json.get("version").and_then(Value::as_str)
                    && !manifest_version.trim().is_empty()
                {
                    version = manifest_version.trim().to_string();
                }
            }
        }

        Ok(Self {
            extension_id,
            entry_path,
            name,
            version,
            api_version: PROTOCOL_VERSION.to_string(),
        })
    }

    pub fn from_manifest(manifest: &ExtensionManifest, root: &Path) -> Result<Self> {
        let entry_path = root.join(manifest.entrypoint.trim());
        if !entry_path.exists() {
            return Err(Error::validation(format!(
                "Extension entry does not exist: {}",
                entry_path.display()
            )));
        }

        let entry_path = safe_canonicalize(&entry_path);

        if manifest.extension_id.trim().is_empty() {
            return Err(Error::validation(
                "Extension manifest extension_id is empty".to_string(),
            ));
        }

        Ok(Self {
            extension_id: manifest.extension_id.clone(),
            entry_path,
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            api_version: manifest.api_version.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct NativeRustExtensionLoadSpec {
    pub extension_id: String,
    pub entry_path: PathBuf,
    pub name: String,
    pub version: String,
    pub api_version: String,
}

impl NativeRustExtensionLoadSpec {
    pub fn from_entry_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(Error::validation(format!(
                "Native extension entry does not exist: {}",
                path.display()
            )));
        }

        let entry_path = safe_canonicalize(path);
        let mut extension_id = entry_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if let Some(stripped) = extension_id.strip_suffix(".native") {
            extension_id = stripped.to_string();
        }
        if extension_id.is_empty() {
            extension_id = entry_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .trim()
                .to_string();
        }
        if extension_id.is_empty() {
            return Err(Error::validation(format!(
                "Native extension entry has no resolvable id: {}",
                entry_path.display()
            )));
        }

        let mut name = extension_id.clone();
        let mut version = "0.0.0".to_string();
        let mut api_version = PROTOCOL_VERSION.to_string();
        if let Some(parent) = entry_path.parent()
            && let Ok(Some(manifest)) = load_extension_manifest(parent)
            && manifest.manifest.runtime == ExtensionRuntime::NativeRust
        {
            name.clone_from(&manifest.manifest.name);
            version.clone_from(&manifest.manifest.version);
            api_version.clone_from(&manifest.manifest.api_version);
        }

        Ok(Self {
            extension_id,
            entry_path,
            name,
            version,
            api_version,
        })
    }

    pub fn from_manifest(manifest: &ExtensionManifest, root: &Path) -> Result<Self> {
        let entry_path = root.join(manifest.entrypoint.trim());
        if !entry_path.exists() {
            return Err(Error::validation(format!(
                "Native extension entry does not exist: {}",
                entry_path.display()
            )));
        }

        let entry_path = safe_canonicalize(&entry_path);
        if manifest.extension_id.trim().is_empty() {
            return Err(Error::validation(
                "Native extension manifest extension_id is empty".to_string(),
            ));
        }

        Ok(Self {
            extension_id: manifest.extension_id.clone(),
            entry_path,
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            api_version: manifest.api_version.clone(),
        })
    }
}

#[cfg(any())]
mod native_runtime_experimental;

#[derive(Debug, Clone, Deserialize)]
struct JsExtensionSnapshot {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    api_version: String,
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(default)]
    slash_commands: Vec<Value>,
    #[serde(default)]
    shortcuts: Vec<Value>,
    #[serde(default)]
    providers: Vec<Value>,
    #[serde(default)]
    mcp_servers: Vec<Value>,
    #[serde(default)]
    flags: Vec<Value>,
    #[serde(default)]
    event_hooks: Vec<String>,
    #[serde(default)]
    active_tools: Option<Vec<String>>,
}

#[cfg(feature = "wasm-host")]
#[derive(Clone)]
pub struct WasmExtensionHandle {
    instance: Arc<AsyncMutex<wasm_host::Instance>>,
    registration: RegisterPayload,
    tool_defs: Vec<ExtensionToolDef>,
}

#[cfg(feature = "wasm-host")]
impl WasmExtensionHandle {
    fn new(instance: wasm_host::Instance, registration: RegisterPayload) -> Self {
        let tool_defs = parse_extension_tool_defs(&registration.tools);
        Self {
            instance: Arc::new(AsyncMutex::new(instance)),
            registration,
            tool_defs,
        }
    }

    pub fn tool_defs(&self) -> &[ExtensionToolDef] {
        &self.tool_defs
    }

    pub fn event_hooks(&self) -> &[String] {
        &self.registration.event_hooks
    }

    pub const fn registration(&self) -> &RegisterPayload {
        &self.registration
    }

    pub async fn handle_tool(&self, name: &str, input: &Value) -> Result<String> {
        let input_json = serde_json::to_string(input)
            .map_err(|err| Error::extension(format!("Serialize tool input: {err}")))?;
        let cx = Cx::for_request();
        let mut instance = OwnedMutexGuard::lock(Arc::clone(&self.instance), &cx)
            .await
            .map_err(|err| Error::extension(format!("Lock wasm instance: {err}")))?;
        instance.handle_tool(name, &input_json).await
    }

    pub async fn handle_slash(
        &self,
        command: &str,
        args: &[String],
        input: &Value,
    ) -> Result<String> {
        let input_json = serde_json::to_string(input)
            .map_err(|err| Error::extension(format!("Serialize slash input: {err}")))?;
        let cx = Cx::for_request();
        let mut instance = OwnedMutexGuard::lock(Arc::clone(&self.instance), &cx)
            .await
            .map_err(|err| Error::extension(format!("Lock wasm instance: {err}")))?;
        instance.handle_slash(command, args, &input_json).await
    }

    pub async fn handle_event_value(
        &self,
        event: &Value,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        let event_json = serde_json::to_string(event)
            .map_err(|err| Error::extension(format!("Serialize event: {err}")))?;
        let cx = Cx::for_request();
        let fut = async {
            let mut instance = OwnedMutexGuard::lock(Arc::clone(&self.instance), &cx)
                .await
                .map_err(|err| Error::extension(format!("Lock wasm instance: {err}")))?;
            instance.handle_event(&event_json).await
        };

        let response_json = if timeout_ms > 0 {
            match timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut)).await {
                Ok(value) => value?,
                Err(_) => {
                    return Err(Error::extension(format!(
                        "WASM event timed out after {timeout_ms}ms"
                    )));
                }
            }
        } else {
            fut.await?
        };

        if response_json.trim().is_empty() {
            return Ok(None);
        }

        let value: Value = serde_json::from_str(&response_json)
            .map_err(|err| Error::extension(format!("Parse event response: {err}")))?;
        if value.is_null() {
            Ok(None)
        } else {
            Ok(Some(value))
        }
    }
}

fn parse_extension_tool_defs(tools: &[Value]) -> Vec<ExtensionToolDef> {
    let mut defs = Vec::new();
    for value in tools {
        match serde_json::from_value::<ExtensionToolDef>(value.clone()) {
            Ok(def) => defs.push(def),
            Err(err) => {
                tracing::warn!(error = %err, "Invalid extension tool definition; ignoring");
            }
        }
    }
    defs
}

/// Trait allowing tests to intercept hostcalls before they reach real dispatch.
/// Return `Some(outcome)` to short-circuit, or `None` to fall through to real dispatch.
pub trait HostcallInterceptor: Send + Sync {
    fn intercept(&self, request: &HostcallRequest) -> Option<HostcallOutcome>;
}

#[derive(Clone)]
struct JsRuntimeHost {
    tools: Arc<ToolRegistry>,
    /// Weak reference to avoid Arc cycle with the runtime thread.
    /// The thread holds a `JsRuntimeHost` which would otherwise prevent
    /// `ExtensionManager` from being dropped (and the channel from closing).
    manager_ref: Weak<Mutex<ExtensionManagerInner>>,
    /// Shared RCU snapshot so managers reconstructed from the weak reference
    /// read and write the same snapshot as the original `ExtensionManager`.
    manager_snapshot: Arc<RwLock<Arc<RegistrySnapshot>>>,
    manager_snapshot_version: Arc<AtomicU64>,
    http: Arc<HttpConnector>,
    policy: ExtensionPolicy,
    interceptor: Option<Arc<dyn HostcallInterceptor>>,
}

thread_local! {
    /// Per-thread AMAC batch executor for interleaved hostcall dispatch.
    /// Persists telemetry across `pump_js_runtime_once` cycles on the
    /// JS runtime thread.
    static AMAC_EXECUTOR: RefCell<AmacBatchExecutor> =
        RefCell::new(AmacBatchExecutor::default());
}

/// Query the AMAC batch executor telemetry for the current thread.
///
/// Returns `None` if called from a thread that has never run the JS
/// runtime pump (the thread-local executor was never initialized with
/// any observations).
#[must_use]
pub fn amac_telemetry_snapshot() -> Option<crate::hostcall_amac::AmacStallTelemetrySnapshot> {
    AMAC_EXECUTOR.with(|cell| {
        let executor = cell.borrow();
        let snap = executor.telemetry().snapshot();
        if snap.total_calls == 0 {
            None
        } else {
            Some(snap)
        }
    })
}

thread_local! {
    /// Per-thread trace-JIT compiler for tier-2 superinstruction dispatch.
    /// Persists compiled traces and profiling data across pump cycles.
    static TRACE_JIT: RefCell<TraceJitCompiler> =
        RefCell::new(TraceJitCompiler::default());
}

/// Query the trace-JIT compiler telemetry for the current thread.
///
/// Returns `None` if no plans have been evaluated yet.
#[must_use]
pub fn trace_jit_telemetry_snapshot() -> Option<crate::hostcall_trace_jit::TraceJitTelemetry> {
    TRACE_JIT.with(|cell| {
        let jit = cell.borrow();
        let t = jit.telemetry().clone();
        if t.plans_evaluated == 0 {
            None
        } else {
            Some(t)
        }
    })
}

impl JsRuntimeHost {
    /// Upgrade the weak manager reference.  Returns `None` if the
    /// `ExtensionManager` has already been dropped (shutdown in progress).
    fn manager(&self) -> Option<ExtensionManager> {
        self.manager_ref.upgrade().map(|inner| ExtensionManager {
            inner,
            snapshot: Arc::clone(&self.manager_snapshot),
            snapshot_version: Arc::clone(&self.manager_snapshot_version),
        })
    }
}

#[derive(Debug)]
enum JsRuntimeCommand {
    LoadExtensions {
        specs: Vec<JsExtensionLoadSpec>,
        deadline: Instant,
        reply: oneshot::Sender<Result<Vec<JsExtensionSnapshot>>>,
    },
    GetRegisteredTools {
        deadline: Instant,
        reply: oneshot::Sender<Result<Vec<ExtensionToolDef>>>,
    },
    PumpOnce {
        deadline: Instant,
        reply: oneshot::Sender<Result<bool>>,
    },
    DispatchEvent {
        event_name: String,
        event_payload: Value,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<Value>>,
    },
    /// Dispatch multiple events in a single JS bridge call with shared context.
    DispatchEventBatch {
        events: Vec<(String, Value)>,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<Vec<Result<Value>>>>,
    },
    ExecuteTool {
        tool_name: String,
        tool_call_id: String,
        input: Value,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<Value>>,
    },
    ExecuteCommand {
        command_name: String,
        args: String,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<Value>>,
    },
    ExecuteShortcut {
        key_id: String,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<Value>>,
    },
    ProviderStreamSimpleStart {
        provider_id: String,
        model: Value,
        context: Value,
        options: Value,
        timeout_ms: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<String>>,
    },
    ProviderStreamSimpleNext {
        stream_id: String,
        timeout_ms: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<Option<Value>>>,
    },
    ProviderStreamSimpleCancel {
        stream_id: String,
        timeout_ms: u64,
        deadline: Instant,
        reply: Option<oneshot::Sender<Result<()>>>,
    },
    SetFlagValue {
        extension_id: String,
        flag_name: String,
        value: Value,
        deadline: Instant,
        reply: oneshot::Sender<Result<()>>,
    },
    RegisterMcpServer {
        extension_id: String,
        name: String,
        spec: Value,
        deadline: Instant,
        reply: oneshot::Sender<Result<Value>>,
    },
    /// Drain accumulated auto-repair events from the runtime.
    DrainRepairEvents {
        deadline: Instant,
        reply: oneshot::Sender<Vec<ExtensionRepairEvent>>,
    },
    /// Reset every realm and remove all shards from actor routing.
    ///
    /// The next load is always a cold transactional rebuild; no realm-local
    /// registry, task, timer, VFS, stream, or module-cache state is retained.
    ResetTransientState {
        deadline: Instant,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Request the runtime thread to shut down gracefully.
    Shutdown,
}

struct JsRuntimeShard {
    extension_id: String,
    runtime: PiJsRuntime,
    snapshot: JsExtensionSnapshot,
    /// Terminal pump failure retained so unrelated shards can continue while
    /// future calls targeting this shard still receive the original failure.
    pump_fault: Option<String>,
}

#[derive(Debug, Clone)]
struct JsProviderStreamRoute {
    shard_index: usize,
    inner_stream_id: String,
}

#[derive(Default)]
struct JsRuntimeShardSet {
    /// Stable extension load order. Every execution and event route is an index
    /// into this vector so reloads cannot inherit realm-local state.
    shards: Vec<JsRuntimeShard>,
    extension_owner: HashMap<String, usize>,
    tool_owner: HashMap<String, usize>,
    command_owner: HashMap<String, usize>,
    shortcut_owner: HashMap<String, usize>,
    provider_owner: HashMap<String, usize>,
    mcp_server_owner: HashMap<String, usize>,
    event_owners: HashMap<String, Vec<usize>>,
    provider_stream_routes: HashMap<String, JsProviderStreamRoute>,
    next_provider_stream_id: u64,
    pump_cursor: usize,
}

#[derive(Default)]
struct JsRuntimeShardIndexes {
    extension_owner: HashMap<String, usize>,
    tool_owner: HashMap<String, usize>,
    command_owner: HashMap<String, usize>,
    shortcut_owner: HashMap<String, usize>,
    provider_owner: HashMap<String, usize>,
    mcp_server_owner: HashMap<String, usize>,
    event_owners: HashMap<String, Vec<usize>>,
}

impl JsRuntimeShardIndexes {
    fn insert_unique_route(
        routes: &mut HashMap<String, usize>,
        name: &str,
        shard_index: usize,
        collision_kind: &str,
    ) -> Result<()> {
        if let Some(previous) = routes.insert(name.to_string(), shard_index)
            && previous != shard_index
        {
            return Err(Error::extension(format!("{collision_kind}: {name}")));
        }
        Ok(())
    }

    fn index_extension(&mut self, shard_index: usize, shard: &JsRuntimeShard) -> Result<()> {
        if self
            .extension_owner
            .insert(shard.extension_id.clone(), shard_index)
            .is_some()
        {
            return Err(Error::extension(format!(
                "Duplicate JS extension id: {}",
                shard.extension_id
            )));
        }
        Ok(())
    }

    fn index_tools(&mut self, shard_index: usize, shard: &JsRuntimeShard) -> Result<()> {
        for tool in &shard.snapshot.tools {
            let Some(name) = tool
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            Self::insert_unique_route(
                &mut self.tool_owner,
                name,
                shard_index,
                "registerTool: tool name collision",
            )?;
        }
        Ok(())
    }

    fn index_commands(&mut self, shard_index: usize, shard: &JsRuntimeShard) -> Result<()> {
        for command in &shard.snapshot.slash_commands {
            let Some(name) = extract_slash_command_name(command) else {
                continue;
            };
            let name = js_command_route_name(&name);
            if name.is_empty() {
                continue;
            }
            Self::insert_unique_route(
                &mut self.command_owner,
                name,
                shard_index,
                "registerCommand: command name collision",
            )?;
        }
        Ok(())
    }

    fn index_shortcuts(&mut self, shard_index: usize, shard: &JsRuntimeShard) {
        for shortcut in &shard.snapshot.shortcuts {
            let Some(key_id) = shortcut
                .get("key_id")
                .or_else(|| shortcut.get("shortcut"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|key_id| !key_id.is_empty())
            else {
                continue;
            };
            // The shared-realm bridge historically overwrote shortcut
            // registrations, so preserve deterministic last-loaded-wins.
            self.shortcut_owner
                .insert(key_id.to_ascii_lowercase(), shard_index);
        }
    }

    fn index_providers(&mut self, shard_index: usize, shard: &JsRuntimeShard) -> Result<()> {
        for provider in &shard.snapshot.providers {
            let Some(provider_id) = provider
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|provider_id| !provider_id.is_empty())
            else {
                continue;
            };
            Self::insert_unique_route(
                &mut self.provider_owner,
                provider_id,
                shard_index,
                "registerProvider: provider id collision",
            )?;
        }
        Ok(())
    }

    fn index_mcp_servers(&mut self, shard_index: usize, shard: &JsRuntimeShard) -> Result<()> {
        for server in &shard.snapshot.mcp_servers {
            let Some(name) = server
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            Self::insert_unique_route(
                &mut self.mcp_server_owner,
                name,
                shard_index,
                "registerMcpServer: server name collision",
            )?;
        }
        Ok(())
    }

    fn index_events(&mut self, shard_index: usize, shard: &JsRuntimeShard) {
        let mut seen_hooks = HashSet::new();
        for event_name in &shard.snapshot.event_hooks {
            let event_name = event_name.trim();
            if event_name.is_empty() || !seen_hooks.insert(event_name) {
                continue;
            }
            self.event_owners
                .entry(event_name.to_string())
                .or_default()
                .push(shard_index);
        }
    }

    fn index_shard(&mut self, shard_index: usize, shard: &JsRuntimeShard) -> Result<()> {
        self.index_extension(shard_index, shard)?;
        self.index_tools(shard_index, shard)?;
        self.index_commands(shard_index, shard)?;
        self.index_shortcuts(shard_index, shard);
        self.index_providers(shard_index, shard)?;
        self.index_mcp_servers(shard_index, shard)?;
        self.index_events(shard_index, shard);
        Ok(())
    }
}

impl JsRuntimeShardSet {
    fn snapshots(&self) -> Vec<JsExtensionSnapshot> {
        self.shards
            .iter()
            .map(|shard| shard.snapshot.clone())
            .collect()
    }

    fn provider_stream_id_was_issued(&self, stream_id: &str) -> bool {
        let Some(sequence) = stream_id
            .strip_prefix("provider-stream-")
            .and_then(|suffix| suffix.parse::<u64>().ok())
        else {
            return false;
        };

        sequence > 0
            && sequence <= self.next_provider_stream_id
            && stream_id == format!("provider-stream-{sequence}")
    }

    fn shard_index_for_extension(&self, extension_id: &str) -> Result<usize> {
        let shard_index = self
            .extension_owner
            .get(extension_id)
            .copied()
            .ok_or_else(|| Error::extension(format!("Unknown JS extension: {extension_id}")))?;
        self.ensure_shard_healthy(shard_index)?;
        Ok(shard_index)
    }

    fn shard_index_for_route(
        &self,
        routes: &HashMap<String, usize>,
        route_kind: &str,
        route_name: &str,
    ) -> Result<usize> {
        let shard_index = routes.get(route_name).copied().ok_or_else(|| {
            Error::extension(format!("Unknown JS extension {route_kind}: {route_name}"))
        })?;
        self.ensure_shard_healthy(shard_index)?;
        Ok(shard_index)
    }

    fn ensure_shard_healthy(&self, shard_index: usize) -> Result<()> {
        let shard = self
            .shards
            .get(shard_index)
            .ok_or_else(|| Error::extension("JS runtime shard disappeared"))?;
        if let Some(fault) = shard.pump_fault.as_deref() {
            return Err(Error::extension(fault.to_string()));
        }
        Ok(())
    }

    fn rebuild_indexes(&mut self) -> Result<()> {
        let mut indexes = JsRuntimeShardIndexes::default();
        for (shard_index, shard) in self.shards.iter().enumerate() {
            indexes.index_shard(shard_index, shard)?;
        }

        self.extension_owner = indexes.extension_owner;
        self.tool_owner = indexes.tool_owner;
        self.command_owner = indexes.command_owner;
        self.shortcut_owner = indexes.shortcut_owner;
        self.provider_owner = indexes.provider_owner;
        self.mcp_server_owner = indexes.mcp_server_owner;
        self.event_owners = indexes.event_owners;

        Ok(())
    }
}

fn duration_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn file_change_token(path: &Path) -> Value {
    let metadata = std::fs::metadata(path).ok();
    let modified_nanos = metadata
        .as_ref()
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    json!({
        "path": safe_canonicalize(path).display().to_string(),
        "len": metadata.as_ref().map(std::fs::Metadata::len),
        "modified_nanos": modified_nanos.to_string(),
    })
}

fn js_extension_spec_token(spec: &JsExtensionLoadSpec) -> Value {
    let mut sidecars = Vec::new();
    if let Some(parent) = spec.entry_path.parent() {
        for filename in ["extension.json", "package.json"] {
            let candidate = parent.join(filename);
            if candidate.exists() {
                sidecars.push(file_change_token(&candidate));
            }
        }
    }

    json!({
        "extension_id": &spec.extension_id,
        "entry": file_change_token(&spec.entry_path),
        "name": &spec.name,
        "version": &spec.version,
        "api_version": &spec.api_version,
        "sidecars": sidecars,
    })
}

fn warm_runtime_pool_fingerprint(
    config: &PiJsRuntimeConfig,
    policy: &ExtensionPolicy,
    specs: &[JsExtensionLoadSpec],
) -> String {
    let env_hashes = config
        .env
        .iter()
        .map(|(key, value)| (key.clone(), sha256_hex_standalone(value)))
        .collect::<BTreeMap<_, _>>();
    let mut spec_tokens = specs
        .iter()
        .map(js_extension_spec_token)
        .collect::<Vec<_>>();
    spec_tokens.sort_by(|a, b| {
        let a_key = a
            .get("extension_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let b_key = b
            .get("extension_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        a_key.cmp(b_key)
    });

    let policy_value = serde_json::to_value(policy).unwrap_or_else(|_| json!("policy-unavailable"));
    let payload = json!({
        "cwd": &config.cwd,
        "args": &config.args,
        "env": env_hashes,
        "limits": {
            "memory_limit_bytes": config.limits.memory_limit_bytes,
            "module_cache_limit_bytes": config.limits.module_cache_limit_bytes,
            "max_stack_bytes": config.limits.max_stack_bytes,
            "interrupt_budget": config.limits.interrupt_budget,
            "hostcall_timeout_ms": config.limits.hostcall_timeout_ms,
            "hostcall_fast_queue_capacity": config.limits.hostcall_fast_queue_capacity,
            "hostcall_overflow_queue_capacity": config.limits.hostcall_overflow_queue_capacity,
        },
        "repair_mode": format!("{:?}", config.repair_mode),
        "allow_unsafe_sync_exec": config.allow_unsafe_sync_exec,
        "deny_env": config.deny_env,
        "disk_cache_dir": config
            .disk_cache_dir
            .as_ref()
            .map(|path| path.display().to_string()),
        "policy": policy_value,
        "specs": spec_tokens,
    });
    let encoded = serde_json::to_string(&payload)
        .unwrap_or_else(|_| "{\"error\":\"warm_pool_fingerprint\"}".to_string());
    sha256_hex_standalone(&encoded)
}

fn short_warm_pool_fingerprint(fingerprint: &str) -> &str {
    fingerprint.get(..16).unwrap_or(fingerprint)
}

/// Handle to the JS extension runtime thread.
///
/// Cloning shares the same underlying runtime. Call [`shutdown`](Self::shutdown)
/// to request a graceful exit; the runtime thread will finish the current
/// command, break out of the event loop, and signal completion via
/// `exit_signal`.
pub struct JsExtensionRuntimeHandle {
    sender: mpsc::Sender<JsRuntimeCommand>,
    /// Frozen compatibility policy from the runtime config. This is shared
    /// with the manager-side static-registration fallback so every
    /// conformance-only behavior observes the same per-runtime decision.
    compat_scan_mode: bool,
    /// Receives `()` when the runtime thread exits its event loop.
    /// Wrapped in `Arc<Mutex<Option<_>>>` so only the first `shutdown()`
    /// caller actually awaits the signal.
    exit_signal: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

impl Clone for JsExtensionRuntimeHandle {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            compat_scan_mode: self.compat_scan_mode,
            exit_signal: Arc::clone(&self.exit_signal),
        }
    }
}

impl JsExtensionRuntimeHandle {
    #[allow(clippy::too_many_lines)]
    pub async fn start(
        config: PiJsRuntimeConfig,
        tools: Arc<ToolRegistry>,
        manager: ExtensionManager,
    ) -> Result<Self> {
        Self::start_inner(config, tools, manager, None, None).await
    }

    /// Like [`start`](Self::start) but uses a specific [`ExtensionPolicy`].
    pub async fn start_with_policy(
        config: PiJsRuntimeConfig,
        tools: Arc<ToolRegistry>,
        manager: ExtensionManager,
        policy: ExtensionPolicy,
    ) -> Result<Self> {
        Self::start_inner(config, tools, manager, None, Some(policy)).await
    }

    /// Like [`start`](Self::start) but installs a [`HostcallInterceptor`] that
    /// can short-circuit hostcalls before they reach real dispatch handlers.
    /// Used by conformance tests to provide deterministic exec/http/ui stubs.
    pub async fn start_with_interceptor(
        config: PiJsRuntimeConfig,
        tools: Arc<ToolRegistry>,
        manager: ExtensionManager,
        interceptor: Arc<dyn HostcallInterceptor>,
    ) -> Result<Self> {
        Self::start_inner(config, tools, manager, Some(interceptor), None).await
    }

    /// Like [`start_with_interceptor`](Self::start_with_interceptor) but with
    /// an explicit [`ExtensionPolicy`].
    pub async fn start_with_interceptor_and_policy(
        config: PiJsRuntimeConfig,
        tools: Arc<ToolRegistry>,
        manager: ExtensionManager,
        interceptor: Arc<dyn HostcallInterceptor>,
        policy: ExtensionPolicy,
    ) -> Result<Self> {
        Self::start_inner(config, tools, manager, Some(interceptor), Some(policy)).await
    }

    #[allow(clippy::too_many_lines)]
    async fn start_inner(
        mut config: PiJsRuntimeConfig,
        tools: Arc<ToolRegistry>,
        manager: ExtensionManager,
        interceptor: Option<Arc<dyn HostcallInterceptor>>,
        policy: Option<ExtensionPolicy>,
    ) -> Result<Self> {
        let compat_scan_mode = config.compat_scan_mode();
        let (tx, mut rx) = mpsc::channel(32);
        let (init_tx, mut init_rx) = oneshot::channel();
        let (exit_tx, exit_rx) = oneshot::channel();
        let policy = policy.unwrap_or_default();
        let runtime_policy = policy.clone();

        if !policy.deny_caps.contains(&"env".to_string()) {
            config.deny_env = false;
        }

        let host = JsRuntimeHost {
            tools,
            manager_ref: Arc::downgrade(&manager.inner),
            manager_snapshot: Arc::clone(&manager.snapshot),
            manager_snapshot_version: Arc::clone(&manager.snapshot_version),
            http: Arc::new(HttpConnector::with_defaults()),
            policy,
            interceptor,
        };

        thread::spawn(move || {
            let runtime = RuntimeBuilder::current_thread()
                .build()
                .expect("extension runtime build");
            runtime.block_on(async move {
                let cx = Cx::for_request();
                let runtime_config = config.clone();
                let warm_pool = crate::extensions_js::WarmIsolatePool::new(runtime_config.clone());
                let cold_init_started = Instant::now();
                let init_config = warm_pool.make_config();
                let init = PiJsRuntime::with_clock_and_config_with_policy(
                    crate::scheduler::WallClock,
                    init_config.clone(),
                    Some(runtime_policy.clone()),
                )
                .await;
                let cold_init_latency_ms = duration_ms_u64(cold_init_started.elapsed());
                match init {
                    Ok(runtime_probe) => {
                        tracing::info!(
                            event = "extension_runtime.shards.startup",
                            phase = "cold_init",
                            cold_init_latency_ms,
                            memory_limit_bytes = init_config.limits.memory_limit_bytes.unwrap_or(0),
                            module_cache_limit_bytes =
                                init_config.limits.module_cache_limit_bytes.unwrap_or(0),
                            pool_created_count = warm_pool.created_count(),
                            pool_reset_count = warm_pool.reset_count(),
                            "QuickJS shard runtime configuration validated"
                        );
                        // The probe executes no extension code. Production realms are
                        // created below with an immutable Rust-owned extension id.
                        drop(runtime_probe);
                        let _ = init_tx.send(&cx, Ok(()));
                    }
                    Err(err) => {
                        let _ = init_tx.send(&cx, Err(err));
                        return;
                    }
                }

                let mut shard_set = JsRuntimeShardSet::default();

                while let Ok(cmd) = rx.recv(&cx).await {
                    match cmd {
                        JsRuntimeCommand::Shutdown => break,
                        JsRuntimeCommand::LoadExtensions {
                            specs,
                            deadline,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let timeout_ms = match js_runtime_remaining_timeout_ms(deadline, "load") {
                                Ok(timeout_ms) => timeout_ms,
                                Err(err) => {
                                    let _ = reply.send(&cx, Err(err));
                                    continue;
                                }
                            };
                            let startup_started = Instant::now();
                            let load_fingerprint = warm_runtime_pool_fingerprint(
                                &runtime_config,
                                &runtime_policy,
                                &specs,
                            );
                            let fingerprint_short =
                                short_warm_pool_fingerprint(&load_fingerprint).to_string();
                            let next_provider_stream_id = shard_set.next_provider_stream_id;
                            let build_result = timeout(
                                wall_now(),
                                Duration::from_millis(timeout_ms),
                                Box::pin(build_js_runtime_shards(
                                    &warm_pool,
                                    &runtime_policy,
                                    &host,
                                    &specs,
                                )),
                            )
                            .await;
                            let result = match build_result {
                                Err(_) => Err(Error::extension(
                                    "JS extension runtime load expired during actor execution",
                                )),
                                Ok(Err(err)) => Err(err),
                                Ok(Ok(mut candidate)) => {
                                    if reply.is_closed() {
                                        continue;
                                    }
                                    if let Err(err) =
                                        js_runtime_remaining_timeout_ms(deadline, "load")
                                    {
                                        let _ = reply.send(&cx, Err(err));
                                        continue;
                                    }
                                    let cleanup_budget = deadline
                                        .checked_duration_since(Instant::now())
                                        .map_or(Duration::ZERO, |remaining| {
                                            remaining.min(ExtensionManager::DEFAULT_CLEANUP_BUDGET)
                                        });
                                    if cleanup_budget.is_zero() {
                                        let dropped_routes = shard_set.provider_stream_routes.len();
                                        shard_set.provider_stream_routes.clear();
                                        if dropped_routes > 0 {
                                            tracing::warn!(
                                                event = "extension_runtime.provider_stream.reload_cleanup_budget_exhausted",
                                                total = dropped_routes,
                                                attempted = 0,
                                                skipped = dropped_routes,
                                                cleanup_budget_ms = 0,
                                                "Provider stream cleanup budget expired before cold shard replacement"
                                            );
                                        }
                                    } else {
                                        cancel_active_provider_streams_for_replacement(
                                            &mut shard_set,
                                            &host,
                                            cleanup_budget,
                                        )
                                        .await;
                                    }
                                    candidate.next_provider_stream_id = next_provider_stream_id;
                                    let snapshots = candidate.snapshots();
                                    let shard_count = candidate.shards.len();
                                    shard_set = candidate;
                                    tracing::info!(
                                        event = "extension_runtime.shards.reload",
                                        reload_mode = "cold_transactional",
                                        shard_count,
                                        pool_fingerprint = %fingerprint_short,
                                        startup_latency_ms = duration_ms_u64(startup_started.elapsed()),
                                        "Installed isolated JS extension runtime shards"
                                    );
                                    Ok(snapshots)
                                }
                            };
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::GetRegisteredTools { deadline, reply } => {
                            if reply.is_closed() {
                                continue;
                            }
                            if let Err(err) =
                                js_runtime_remaining_timeout_ms(deadline, "tools query")
                            {
                                let _ = reply.send(&cx, Err(err));
                                continue;
                            }
                            let result = get_registered_tools_from_shards(&shard_set).await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::PumpOnce { deadline, reply } => {
                            if reply.is_closed() {
                                continue;
                            }
                            if let Err(err) = js_runtime_remaining_timeout_ms(deadline, "pump") {
                                let _ = reply.send(&cx, Err(err));
                                continue;
                            }
                            let result = pump_js_runtime_shards_once(&mut shard_set, &host).await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::DispatchEvent {
                            event_name,
                            event_payload,
                            ctx_payload,
                            timeout_ms: _,
                            deadline,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let timeout_ms = match js_runtime_remaining_timeout_ms(deadline, "event") {
                                Ok(timeout_ms) => timeout_ms,
                                Err(err) => {
                                    let _ = reply.send(&cx, Err(err));
                                    continue;
                                }
                            };
                            let result = dispatch_extension_event_across_shards(
                                &mut shard_set,
                                &host,
                                &event_name,
                                event_payload,
                                ctx_payload.as_ref(),
                                timeout_ms,
                            )
                            .await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::DispatchEventBatch {
                            events,
                            ctx_payload,
                            timeout_ms: _,
                            deadline,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let timeout_ms = match js_runtime_remaining_timeout_ms(deadline, "event batch") {
                                Ok(timeout_ms) => timeout_ms,
                                Err(err) => {
                                    let _ = reply.send(&cx, Err(err));
                                    continue;
                                }
                            };
                            let result = dispatch_extension_event_batch_across_shards(
                                &mut shard_set,
                                &host,
                                events,
                                ctx_payload.as_ref(),
                                timeout_ms,
                            )
                            .await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::ExecuteTool {
                            tool_name,
                            tool_call_id,
                            input,
                            ctx_payload,
                            timeout_ms: _,
                            deadline,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let timeout_ms = match js_runtime_remaining_timeout_ms(deadline, "tool") {
                                Ok(timeout_ms) => timeout_ms,
                                Err(err) => {
                                    let _ = reply.send(&cx, Err(err));
                                    continue;
                                }
                            };
                            let result = match shard_set.shard_index_for_route(
                                &shard_set.tool_owner,
                                "tool",
                                &tool_name,
                            ) {
                                Ok(shard_index) => {
                                    execute_extension_tool_sharded(
                                        &mut shard_set,
                                        &host,
                                        JsToolExecution {
                                            shard_index,
                                            tool_name: &tool_name,
                                            tool_call_id: &tool_call_id,
                                            input,
                                            ctx_payload: ctx_payload.as_ref(),
                                            timeout_ms,
                                        },
                                    )
                                    .await
                                }
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::ExecuteCommand {
                            command_name,
                            args,
                            ctx_payload,
                            timeout_ms: _,
                            deadline,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let timeout_ms = match js_runtime_remaining_timeout_ms(deadline, "command") {
                                Ok(timeout_ms) => timeout_ms,
                                Err(err) => {
                                    let _ = reply.send(&cx, Err(err));
                                    continue;
                                }
                            };
                            let route_name = js_command_route_name(&command_name);
                            let result = match shard_set.shard_index_for_route(
                                &shard_set.command_owner,
                                "command",
                                route_name,
                            ) {
                                Ok(shard_index) => {
                                    execute_extension_command_sharded(
                                        &mut shard_set,
                                        &host,
                                        shard_index,
                                        route_name,
                                        &args,
                                        ctx_payload.as_ref(),
                                        timeout_ms,
                                    )
                                    .await
                                }
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::ExecuteShortcut {
                            key_id,
                            ctx_payload,
                            timeout_ms: _,
                            deadline,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let timeout_ms = match js_runtime_remaining_timeout_ms(deadline, "shortcut") {
                                Ok(timeout_ms) => timeout_ms,
                                Err(err) => {
                                    let _ = reply.send(&cx, Err(err));
                                    continue;
                                }
                            };
                            let route_name = key_id.trim().to_ascii_lowercase();
                            let result = match shard_set.shard_index_for_route(
                                &shard_set.shortcut_owner,
                                "shortcut",
                                &route_name,
                            ) {
                                Ok(shard_index) => {
                                    execute_extension_shortcut_sharded(
                                        &mut shard_set,
                                        &host,
                                        shard_index,
                                        &route_name,
                                        ctx_payload.as_ref(),
                                        timeout_ms,
                                    )
                                    .await
                                }
                                Err(err) => Err(err),
                            };
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::ProviderStreamSimpleStart {
                            provider_id,
                            model,
                            context,
                            options,
                            timeout_ms: _,
                            deadline,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let timeout_ms = match js_runtime_remaining_timeout_ms(
                                deadline,
                                "provider stream start",
                            ) {
                                Ok(timeout_ms) => timeout_ms,
                                Err(err) => {
                                    let _ = reply.send(&cx, Err(err));
                                    continue;
                                }
                            };
                            let result = async {
                                let shard_index = shard_set.shard_index_for_route(
                                    &shard_set.provider_owner,
                                    "provider",
                                    &provider_id,
                                )?;
                                let inner_stream_id =
                                    start_extension_provider_stream_simple_sharded(
                                        &mut shard_set,
                                        &host,
                                        JsProviderStreamStart {
                                            shard_index,
                                            provider_id: &provider_id,
                                            model,
                                            context,
                                            options,
                                            timeout_ms,
                                        },
                                    )
                                    .await?;
                                let continuation_timeout_ms = match js_runtime_remaining_timeout_ms(
                                    deadline,
                                    "provider stream start",
                                ) {
                                    Ok(timeout_ms) if !reply.is_closed() => timeout_ms,
                                    Ok(timeout_ms) => {
                                        if let Err(cleanup_err) =
                                            cancel_extension_provider_stream_simple_best_effort(
                                                &mut shard_set,
                                                &host,
                                                shard_index,
                                                &inner_stream_id,
                                                timeout_ms,
                                            )
                                            .await
                                        {
                                            tracing::warn!(
                                                event = "extension_runtime.provider_stream.closed_reply_cleanup_failed",
                                                shard_index,
                                                inner_stream_id,
                                                error = %cleanup_err,
                                                "Failed to cancel inner provider stream after caller abandoned start"
                                            );
                                        }
                                        return Err(Error::extension(
                                            "Provider stream start caller closed before route publication",
                                        ));
                                    }
                                    Err(err) => {
                                        if let Err(cleanup_err) =
                                            cancel_extension_provider_stream_simple_best_effort(
                                                &mut shard_set,
                                                &host,
                                                shard_index,
                                                &inner_stream_id,
                                                1,
                                            )
                                            .await
                                        {
                                            tracing::warn!(
                                                event = "extension_runtime.provider_stream.expired_start_cleanup_failed",
                                                shard_index,
                                                inner_stream_id,
                                                error = %cleanup_err,
                                                "Failed to cancel inner provider stream after start deadline expired"
                                            );
                                        }
                                        return Err(err);
                                    }
                                };
                                let Some(sequence) =
                                    shard_set.next_provider_stream_id.checked_add(1)
                                else {
                                    if let Err(cleanup_err) =
                                        cancel_extension_provider_stream_simple_best_effort(
                                            &mut shard_set,
                                            &host,
                                            shard_index,
                                            &inner_stream_id,
                                            continuation_timeout_ms,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            event = "extension_runtime.provider_stream.exhaustion_cleanup_failed",
                                            shard_index,
                                            inner_stream_id,
                                            error = %cleanup_err,
                                            "Failed to cancel inner provider stream after outer id exhaustion"
                                        );
                                    }
                                    return Err(Error::extension(
                                        "provider stream id space exhausted".to_string(),
                                    ));
                                };
                                shard_set.next_provider_stream_id = sequence;
                                let outer_stream_id = format!("provider-stream-{sequence}");
                                shard_set.provider_stream_routes.insert(
                                    outer_stream_id.clone(),
                                    JsProviderStreamRoute {
                                        shard_index,
                                        inner_stream_id,
                                    },
                                );
                                Ok(outer_stream_id)
                            }
                            .await;
                            if reply.is_closed() {
                                if let Ok(outer_stream_id) = result.as_ref()
                                    && let Some(route) = shard_set
                                        .provider_stream_routes
                                        .remove(outer_stream_id)
                                {
                                    let cleanup_timeout_ms = deadline
                                        .checked_duration_since(Instant::now())
                                        .map_or(1, |remaining| {
                                            u64::try_from(remaining.as_millis())
                                                .unwrap_or(u64::MAX)
                                                .max(1)
                                        });
                                    if let Err(cleanup_err) =
                                        cancel_extension_provider_stream_simple_best_effort(
                                            &mut shard_set,
                                            &host,
                                            route.shard_index,
                                            &route.inner_stream_id,
                                            cleanup_timeout_ms,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            event = "extension_runtime.provider_stream.route_publish_cleanup_failed",
                                            outer_stream_id,
                                            inner_stream_id = %route.inner_stream_id,
                                            error = %cleanup_err,
                                            "Failed to cancel provider stream whose caller closed during route publication"
                                        );
                                    }
                                }
                                continue;
                            }
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::ProviderStreamSimpleNext {
                            stream_id,
                            timeout_ms: _,
                            deadline,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let timeout_ms = match js_runtime_remaining_timeout_ms(
                                deadline,
                                "provider stream next",
                            ) {
                                Ok(timeout_ms) => timeout_ms,
                                Err(err) => {
                                    let _ = reply.send(&cx, Err(err));
                                    continue;
                                }
                            };
                            let result = async {
                                let Some(route) = shard_set
                                    .provider_stream_routes
                                    .get(&stream_id)
                                    .cloned()
                                else {
                                    if shard_set.provider_stream_id_was_issued(&stream_id) {
                                        return Ok(None);
                                    }
                                    return Err(Error::extension(format!(
                                        "Unknown extension provider stream: {stream_id}"
                                    )));
                                };
                                let result = next_extension_provider_stream_simple_sharded(
                                    &mut shard_set,
                                    &host,
                                    route.shard_index,
                                    &route.inner_stream_id,
                                    timeout_ms,
                                )
                                .await;
                                match result {
                                    Ok(Some(value)) => Ok(Some(value)),
                                    Ok(None) => {
                                        shard_set.provider_stream_routes.remove(&stream_id);
                                        Ok(None)
                                    }
                                    Err(err) => {
                                        shard_set.provider_stream_routes.remove(&stream_id);
                                        let cleanup_timeout_ms = deadline
                                            .checked_duration_since(Instant::now())
                                            .map_or(1, |remaining| {
                                                u64::try_from(remaining.as_millis())
                                                    .unwrap_or(u64::MAX)
                                                    .max(1)
                                            });
                                        if let Err(cleanup_err) =
                                            cancel_extension_provider_stream_simple_best_effort(
                                                &mut shard_set,
                                                &host,
                                                route.shard_index,
                                                &route.inner_stream_id,
                                                cleanup_timeout_ms,
                                            )
                                            .await
                                        {
                                            tracing::warn!(
                                                event = "extension_runtime.provider_stream.next_cleanup_failed",
                                                outer_stream_id = %stream_id,
                                                inner_stream_id = %route.inner_stream_id,
                                                error = %cleanup_err,
                                                "Failed to cancel inner provider stream after next error"
                                            );
                                        }
                                        Err(err)
                                    }
                                }
                            }
                            .await;
                            if reply.is_closed() {
                                if let Some(route) =
                                    shard_set.provider_stream_routes.remove(&stream_id)
                                {
                                    let cleanup_timeout_ms = deadline
                                        .checked_duration_since(Instant::now())
                                        .map_or(1, |remaining| {
                                            u64::try_from(remaining.as_millis())
                                                .unwrap_or(u64::MAX)
                                                .max(1)
                                        });
                                    if let Err(cleanup_err) =
                                        cancel_extension_provider_stream_simple_best_effort(
                                            &mut shard_set,
                                            &host,
                                            route.shard_index,
                                            &route.inner_stream_id,
                                            cleanup_timeout_ms,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            event = "extension_runtime.provider_stream.abandoned_next_cleanup_failed",
                                            outer_stream_id = %stream_id,
                                            inner_stream_id = %route.inner_stream_id,
                                            error = %cleanup_err,
                                            "Failed to cancel provider stream after its next caller closed"
                                        );
                                    }
                                }
                                continue;
                            }
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::ProviderStreamSimpleCancel {
                            stream_id,
                            timeout_ms: _,
                            deadline,
                            mut reply,
                        } => {
                            if reply.as_ref().is_some_and(oneshot::Sender::is_closed) {
                                continue;
                            }
                            let timeout_ms = match js_runtime_remaining_timeout_ms(
                                deadline,
                                "provider stream cancel",
                            ) {
                                Ok(timeout_ms) => timeout_ms,
                                Err(err) => {
                                    if let Some(reply) = reply.take() {
                                        let _ = reply.send(&cx, Err(err));
                                        continue;
                                    }
                                    // Fire-and-forget cancellation remains a cleanup
                                    // command even when it waited in the queue. Give it
                                    // a minimal bounded attempt rather than turning a
                                    // caller timeout into a permanent resource leak.
                                    1
                                }
                            };
                            let result = async {
                                let route = shard_set
                                    .provider_stream_routes
                                    .get(&stream_id)
                                    .cloned()
                                    .ok_or_else(|| {
                                        Error::extension(format!(
                                            "Unknown extension provider stream: {stream_id}"
                                        ))
                                    })?;
                                let result = cancel_extension_provider_stream_simple_best_effort(
                                    &mut shard_set,
                                    &host,
                                    route.shard_index,
                                    &route.inner_stream_id,
                                    timeout_ms,
                                )
                                .await;
                                shard_set.provider_stream_routes.remove(&stream_id);
                                result
                            }
                            .await;
                            if let Some(reply) = reply {
                                let _ = reply.send(&cx, result);
                            }
                        }
                        JsRuntimeCommand::SetFlagValue {
                            extension_id,
                            flag_name,
                            value,
                            deadline,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            if let Err(err) =
                                js_runtime_remaining_timeout_ms(deadline, "flag update")
                            {
                                let _ = reply.send(&cx, Err(err));
                                continue;
                            }
                            let result = async {
                                let shard_index =
                                    shard_set.shard_index_for_extension(&extension_id)?;
                                set_extension_flag_value(
                                    &shard_set.shards[shard_index].runtime,
                                    &extension_id,
                                    &flag_name,
                                    &value,
                                )
                                .await
                            }
                            .await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::RegisterMcpServer {
                            extension_id,
                            name,
                            spec,
                            deadline,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            if let Err(err) =
                                js_runtime_remaining_timeout_ms(deadline, "MCP registration")
                            {
                                let _ = reply.send(&cx, Err(err));
                                continue;
                            }
                            let result = async {
                                let shard_index =
                                    shard_set.shard_index_for_extension(&extension_id)?;
                                if let Some(previous) =
                                    shard_set.mcp_server_owner.get(name.trim()).copied()
                                    && previous != shard_index
                                {
                                    return Err(Error::extension(format!(
                                        "registerMcpServer: server name collision: {}",
                                        name.trim()
                                    )));
                                }
                                let value = register_extension_mcp_server(
                                    &shard_set.shards[shard_index].runtime,
                                    &extension_id,
                                    &name,
                                    &spec,
                                )
                                .await?;
                                refresh_runtime_shard_snapshot(&mut shard_set, shard_index).await?;
                                Ok(value)
                            }
                            .await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::DrainRepairEvents { deadline, reply } => {
                            if reply.is_closed() {
                                continue;
                            }
                            if js_runtime_remaining_timeout_ms(deadline, "repair-event drain")
                                .is_err()
                            {
                                let _ = reply.send(&cx, Vec::new());
                                continue;
                            }
                            let events = shard_set
                                .shards
                                .iter()
                                .flat_map(|shard| shard.runtime.drain_repair_events())
                                .collect();
                            let _ = reply.send(&cx, events);
                        }
                        JsRuntimeCommand::ResetTransientState { deadline, reply } => {
                            if reply.is_closed() {
                                continue;
                            }
                            if let Err(err) =
                                js_runtime_remaining_timeout_ms(deadline, "transient reset")
                            {
                                let _ = reply.send(&cx, Err(err));
                                continue;
                            }
                            let result = scrub_and_drop_runtime_shards(&mut shard_set, &warm_pool).await;
                            let _ = reply.send(&cx, result);
                        }
                    }
                }
                // Signal that the runtime thread has exited its event loop.
                let _ = exit_tx.send(&cx, ());
                tracing::info!(
                    event = "extension_runtime.exit",
                    "JS extension runtime thread exiting"
                );
            });
        });

        let cx = Cx::for_request();
        init_rx
            .recv(&cx)
            .await
            .map_err(|_| Error::extension("JS extension runtime init cancelled"))??;

        Ok(Self {
            sender: tx,
            compat_scan_mode,
            exit_signal: Arc::new(Mutex::new(Some(exit_rx))),
        })
    }

    pub(crate) const fn compat_scan_mode(&self) -> bool {
        self.compat_scan_mode
    }

    /// Request the JS runtime thread to shut down gracefully.
    ///
    /// Sends a `Shutdown` command and waits up to `budget` for the thread
    /// to exit its event loop.  Returns `true` if the runtime exited
    /// within the budget.
    pub async fn shutdown(&self, budget: Duration) -> bool {
        let cx = Cx::for_request();
        let budget_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX);

        // Send shutdown command (ignore error if channel already closed).
        let _ = self.sender.send(&cx, JsRuntimeCommand::Shutdown).await;

        // Take the exit signal — only the first caller can await it.
        let exit_rx = {
            let Ok(mut guard) = self.exit_signal.lock() else {
                return false;
            };
            guard.take()
        };

        let Some(mut rx) = exit_rx else {
            // Already shut down or another caller is waiting.
            return true;
        };

        match timeout(wall_now(), budget, rx.recv(&cx)).await {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                // Sender dropped without explicit ack: runtime is gone, so cleanup is
                // complete, but log for postmortem visibility.
                tracing::warn!(
                    event = "extension_runtime.shutdown_exit_signal_dropped",
                    budget_ms,
                    error = %err,
                    "JS extension runtime exit signal channel closed before ack"
                );
                true
            }
            Err(_) => {
                tracing::warn!(
                    event = "extension_runtime.shutdown_timeout",
                    budget_ms,
                    "JS extension runtime did not exit within cleanup budget"
                );
                false
            }
        }
    }

    async fn load_extensions_snapshots(
        &self,
        specs: Vec<JsExtensionLoadSpec>,
    ) -> Result<Vec<JsExtensionSnapshot>> {
        let timeout_ms = EXTENSION_LOAD_BUDGET_MS;
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::LoadExtensions {
            specs,
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime load timed out after {timeout_ms}ms"
                )))
            })
    }

    pub async fn get_registered_tools(&self) -> Result<Vec<ExtensionToolDef>> {
        let timeout_ms = EXTENSION_QUERY_BUDGET_MS;
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::GetRegisteredTools {
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime tools query timed out after {timeout_ms}ms"
                )))
            })
    }

    pub async fn pump_once(&self) -> Result<bool> {
        let timeout_ms = EXTENSION_QUERY_BUDGET_MS;
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::PumpOnce {
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime pump timed out after {timeout_ms}ms"
                )))
            })
    }

    pub async fn dispatch_event(
        &self,
        event_name: String,
        event_payload: Value,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Value> {
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::DispatchEvent {
            event_name,
            event_payload,
            ctx_payload,
            timeout_ms,
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime event timed out after {timeout_ms}ms"
                )))
            })
    }

    /// Dispatch multiple events in a single JS bridge call with shared context.
    pub async fn dispatch_event_batch(
        &self,
        events: Vec<(String, Value)>,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Vec<Result<Value>>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::DispatchEventBatch {
            events,
            ctx_payload,
            timeout_ms,
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime batch event timed out after {timeout_ms}ms"
                )))
            })
    }

    pub async fn execute_tool(
        &self,
        tool_name: String,
        tool_call_id: String,
        input: Value,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Value> {
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::ExecuteTool {
            tool_name,
            tool_call_id,
            input,
            ctx_payload,
            timeout_ms,
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime tool timed out after {timeout_ms}ms"
                )))
            })
    }

    pub async fn execute_command(
        &self,
        command_name: String,
        args: String,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Value> {
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::ExecuteCommand {
            command_name,
            args,
            ctx_payload,
            timeout_ms,
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime command timed out after {timeout_ms}ms"
                )))
            })
    }

    pub async fn execute_shortcut(
        &self,
        key_id: String,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Value> {
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::ExecuteShortcut {
            key_id,
            ctx_payload,
            timeout_ms,
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime shortcut timed out after {timeout_ms}ms"
                )))
            })
    }

    pub async fn set_flag_value(
        &self,
        extension_id: String,
        flag_name: String,
        value: Value,
    ) -> Result<()> {
        let timeout_ms = EXTENSION_QUERY_BUDGET_MS;
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::SetFlagValue {
            extension_id,
            flag_name,
            value,
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime flag update timed out after {timeout_ms}ms"
                )))
            })
    }

    pub async fn register_mcp_server(
        &self,
        extension_id: String,
        name: String,
        spec: Value,
    ) -> Result<Value> {
        let timeout_ms = EXTENSION_QUERY_BUDGET_MS;
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::RegisterMcpServer {
            extension_id,
            name,
            spec,
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime MCP registration timed out after {timeout_ms}ms"
                )))
            })
    }

    /// Drain all accumulated auto-repair events from the JS runtime.
    pub async fn drain_repair_events(&self) -> Vec<ExtensionRepairEvent> {
        let timeout_ms = EXTENSION_QUERY_BUDGET_MS;
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::DrainRepairEvents {
            deadline,
            reply: reply_tx,
        };
        let Ok(()) = self.sender.send(&cx, command).await else {
            return Vec::new();
        };
        reply_rx.recv(&cx).await.unwrap_or_default()
    }

    /// Fully reset every isolated extension realm's transient state.
    ///
    /// This invokes the JS registry/task/timer/VFS/provider-stream reset gate,
    /// validates its clean-reset report, and removes reset shards from actor
    /// routing. Subsequent extension loading remains cold and transactional.
    pub async fn reset_transient_state(&self) -> Result<()> {
        let timeout_ms = EXTENSION_QUERY_BUDGET_MS;
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::ResetTransientState {
            deadline,
            reply: reply_tx,
        };
        self.sender
            .send(&cx, command)
            .await
            .map_err(|_| Error::extension("runtime channel closed during reset"))?;
        reply_rx
            .recv(&cx)
            .await
            .map_err(|_| Error::extension("reset reply channel closed"))?
    }

    pub async fn provider_stream_simple_start(
        &self,
        provider_id: String,
        model: Value,
        context: Value,
        options: Value,
        timeout_ms: u64,
    ) -> Result<String> {
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::ProviderStreamSimpleStart {
            provider_id,
            model,
            context,
            options,
            timeout_ms,
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime provider stream start timed out after {timeout_ms}ms"
                )))
            })
    }

    pub async fn provider_stream_simple_next(
        &self,
        stream_id: String,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::ProviderStreamSimpleNext {
            stream_id,
            timeout_ms,
            deadline,
            reply: reply_tx,
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime provider stream next timed out after {timeout_ms}ms"
                )))
            })
    }

    pub async fn provider_stream_simple_cancel(
        &self,
        stream_id: String,
        timeout_ms: u64,
    ) -> Result<()> {
        let deadline = js_runtime_request_deadline(timeout_ms);
        let cx = cx_with_deadline(timeout_ms);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let command = JsRuntimeCommand::ProviderStreamSimpleCancel {
            stream_id,
            timeout_ms,
            deadline,
            reply: Some(reply_tx),
        };
        let fut = async move {
            self.sender
                .send(&cx, command)
                .await
                .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
            reply_rx
                .recv(&cx)
                .await
                .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
        };

        timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut))
            .await
            .unwrap_or_else(|_| {
                Err(Error::extension(format!(
                    "JS extension runtime provider stream cancel timed out after {timeout_ms}ms"
                )))
            })
    }

    pub fn provider_stream_simple_cancel_best_effort(&self, stream_id: String) {
        let timeout_ms = 5000;
        let deadline = js_runtime_request_deadline(timeout_ms);
        if self
            .sender
            .try_send(JsRuntimeCommand::ProviderStreamSimpleCancel {
                stream_id: stream_id.clone(),
                timeout_ms,
                deadline,
                reply: None,
            })
            .is_ok()
        {
            return;
        }

        // Fall back to an async send if the command channel is full.
        let sender = self.sender.clone();
        let _ = std::thread::Builder::new()
            .name("pi-js-stream-cancel".to_owned())
            .spawn(move || {
                let Ok(runtime) = asupersync::runtime::RuntimeBuilder::current_thread().build()
                else {
                    return;
                };
                runtime.block_on(async move {
                    let cx = Cx::for_request();
                    let _ = sender
                        .send(
                            &cx,
                            JsRuntimeCommand::ProviderStreamSimpleCancel {
                                stream_id,
                                timeout_ms,
                                deadline,
                                reply: None,
                            },
                        )
                        .await;
                });
            });
    }
}

mod native_runtime_duplicate_scaffold;

pub type ExtensionRuntimeEngineSelection =
    native_runtime_duplicate_scaffold::ExtensionRuntimeEngineSelection;
pub type ExtensionRuntimeHandle = native_runtime_duplicate_scaffold::ExtensionRuntimeHandle;
pub type NativeRustExtensionRuntimeHandle =
    native_runtime_duplicate_scaffold::NativeRustExtensionRuntimeHandle;

const JS_EXTENSION_ENTRY_EXTS: &[&str] = &["ts", "tsx", "jsx", "js", "mjs", "cjs", "mts", "cts"];
const MAX_BUNDLE_CLUSTER_DIRS: usize = 40;
const MAX_AUXILIARY_EXAMPLE_ENTRIES: usize = 24;
const AUXILIARY_EXTENSION_DIR_NAMES: &[&str] = &["examples", "example", "demos", "demo"];

fn is_supported_js_extension_entry(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            JS_EXTENSION_ENTRY_EXTS
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        })
}

fn resolve_extension_entry_file(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        if is_supported_js_extension_entry(path) {
            return Some(safe_canonicalize(path));
        }
        return None;
    }
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            JS_EXTENSION_ENTRY_EXTS
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        })
    {
        return None;
    }

    JS_EXTENSION_ENTRY_EXTS.iter().find_map(|ext| {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(".");
        candidate.push(ext);
        let candidate = PathBuf::from(candidate);
        if candidate.is_file() {
            Some(safe_canonicalize(&candidate))
        } else {
            None
        }
    })
}

fn collect_extension_entries_from_dir(dir: &Path) -> Vec<PathBuf> {
    if !dir.is_dir() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut seen = BTreeSet::new();

    for ext in JS_EXTENSION_ENTRY_EXTS {
        let candidate = dir.join(format!("index.{ext}"));
        if let Some(path) = resolve_extension_entry_file(&candidate)
            && seen.insert(path.clone())
        {
            out.push(path);
        }
    }

    let mut extras = Vec::new();
    let mut nested = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(index_path) = resolve_extension_entry_file(&path.join("index")) {
                    nested.push(index_path);
                }
                if let Some(dir_name) = path.file_name().and_then(|name| name.to_str())
                    && let Some(named_path) = resolve_extension_entry_file(&path.join(dir_name))
                {
                    nested.push(named_path);
                }
                continue;
            }
            if !path.is_file() || !is_supported_js_extension_entry(&path) {
                continue;
            }
            if path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.eq_ignore_ascii_case("index"))
            {
                continue;
            }
            extras.push(path);
        }
    }
    extras.sort();
    nested.sort();
    for path in extras {
        let canonical = safe_canonicalize(&path);
        if seen.insert(canonical.clone()) {
            out.push(canonical);
        }
    }
    for path in nested {
        if seen.insert(path.clone()) {
            out.push(path);
        }
    }

    out
}

fn is_likely_auxiliary_extension_entry(path: &Path) -> bool {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if file_name.contains("extension")
        || file_name.contains("plugin")
        || file_name.contains("command")
    {
        return true;
    }

    let Ok(raw) = fs::read(path) else {
        return false;
    };
    let preview_len = raw.len().min(32_768);
    let preview = String::from_utf8_lossy(&raw[..preview_len]);
    [
        "registerCommand(",
        "registerTool(",
        "registerProvider(",
        "registerShortcut(",
        "registerFlag(",
        "pi.registerCommand(",
        "pi.registerTool(",
        "pi.registerProvider(",
        "export default function",
    ]
    .iter()
    .any(|needle| preview.contains(needle))
}

fn is_likely_flat_extension_entry(path: &Path) -> bool {
    let Ok(raw) = fs::read(path) else {
        return false;
    };
    let preview_len = raw.len().min(32_768);
    let preview = String::from_utf8_lossy(&raw[..preview_len]);

    let has_default_initializer = [
        "export default function",
        "export default async function",
        "export default(",
        "export default (",
        "export default async(",
        "export default async (",
    ]
    .iter()
    .any(|needle| preview.contains(needle));

    let has_named_initializer = named_flat_extension_initializer_regex().is_match(&preview);
    let has_default_object_initializer =
        default_object_flat_extension_initializer_regex().is_match(&preview);

    let has_registration = [
        "registerCommand(",
        "registerTool(",
        "registerProvider(",
        "registerShortcut(",
        "registerFlag(",
        "pi.registerCommand(",
        "pi.registerTool(",
        "pi.registerProvider(",
        "pi.registerShortcut(",
        "pi.registerFlag(",
    ]
    .iter()
    .any(|needle| preview.contains(needle));

    has_default_initializer
        || has_named_initializer
        || has_default_object_initializer
        || has_registration
}

const FLAT_EXTENSION_INITIALIZER_NAMES: &str =
    r"(?:activate|init(?:ialize)?|setup|register|plugin|main)";

fn named_flat_extension_initializer_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?m)\bexport\s+(?:async\s+)?function\s+{FLAT_EXTENSION_INITIALIZER_NAMES}\b|\bexport\s+(?:const|let|var)\s+{FLAT_EXTENSION_INITIALIZER_NAMES}\s*=\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*=>|[A-Za-z_$][\w$]*\s*=>)"
        ))
        .expect("named flat extension initializer regex")
    })
}

fn default_object_flat_extension_initializer_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r#"(?ms)\bexport\s+default\s*\{{.*?(?:\b(?:async\s+)?{FLAT_EXTENSION_INITIALIZER_NAMES}\s*\(|\b{FLAT_EXTENSION_INITIALIZER_NAMES}\s*:\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*=>|[A-Za-z_$][\w$]*\s*=>)|["'`]{FLAT_EXTENSION_INITIALIZER_NAMES}["'`]\s*(?:\(|:\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*=>|[A-Za-z_$][\w$]*\s*=>)))"#
        ))
        .expect("default object flat extension initializer regex")
    })
}

fn discover_auxiliary_example_entries(
    package_dir: &Path,
    canonical_primary: &Path,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();

    for dir_name in AUXILIARY_EXTENSION_DIR_NAMES {
        let candidate_dir = package_dir.join(dir_name);
        for candidate in collect_extension_entries_from_dir(&candidate_dir) {
            if candidate == canonical_primary {
                continue;
            }
            if !is_likely_auxiliary_extension_entry(&candidate) {
                continue;
            }
            if seen.insert(candidate.clone()) {
                out.push(candidate);
                if out.len() >= MAX_AUXILIARY_EXAMPLE_ENTRIES {
                    return out;
                }
            }
        }
    }

    out
}

fn read_pi_extensions_from_package(package_json_path: &Path) -> Result<Option<Vec<String>>> {
    if !package_json_path.is_file() {
        return Ok(None);
    }

    let raw = fs::read_to_string(package_json_path).map_err(|err| {
        Error::config(format!(
            "Failed to read package manifest {}: {err}",
            package_json_path.display()
        ))
    })?;
    let json = serde_json::from_str::<Value>(&raw).map_err(|err| {
        Error::config(format!(
            "Failed to parse package manifest {}: {err}",
            package_json_path.display()
        ))
    })?;
    let Some(pi) = json.get("pi") else {
        return Ok(None);
    };
    let Some(pi) = pi.as_object() else {
        return Err(Error::config(format!(
            "Invalid package manifest {}: `pi` must be an object",
            package_json_path.display()
        )));
    };
    let Some(entries_value) = pi.get("extensions") else {
        return Ok(None);
    };

    match entries_value {
        Value::String(entry) => {
            let entry = entry.trim();
            if entry.is_empty() {
                return Err(Error::config(format!(
                    "Invalid package manifest {}: `pi.extensions` entries must be non-empty paths",
                    package_json_path.display()
                )));
            }
            Ok(Some(vec![entry.to_owned()]))
        }
        Value::Array(entries) => {
            let mut out = Vec::with_capacity(entries.len());
            for entry in entries {
                let Some(entry) = entry.as_str() else {
                    return Err(Error::config(format!(
                        "Invalid package manifest {}: `pi.extensions` must be a string or array of strings",
                        package_json_path.display()
                    )));
                };
                let entry = entry.trim();
                if entry.is_empty() {
                    return Err(Error::config(format!(
                        "Invalid package manifest {}: `pi.extensions` entries must be non-empty paths",
                        package_json_path.display()
                    )));
                }
                out.push(entry.to_owned());
            }
            Ok(Some(out))
        }
        _ => Err(Error::config(format!(
            "Invalid package manifest {}: `pi.extensions` must be a string or array of strings",
            package_json_path.display()
        ))),
    }
}

fn parse_package_name_from_package(package_json_path: &Path) -> Option<String> {
    let raw = fs::read_to_string(package_json_path).ok()?;
    let json = serde_json::from_str::<Value>(&raw).ok()?;
    json.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn find_package_json_ancestors(mut dir: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    while let Some(current) = dir {
        let candidate = current.join("package.json");
        if candidate.is_file() {
            let canonical = safe_canonicalize(&candidate);
            if seen.insert(canonical.clone()) {
                out.push(canonical);
            }
        }
        dir = current.parent();
    }
    out
}

fn collect_extension_roots_from_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    for entry_path in paths {
        let Some(parent) = entry_path.parent() else {
            continue;
        };
        let root = safe_canonicalize(parent);
        if seen.insert(root.clone()) {
            roots.push(root);
        }

        for package_json in find_package_json_ancestors(Some(parent)) {
            if let Some(package_dir) = package_json.parent() {
                let root = safe_canonicalize(package_dir);
                if seen.insert(root.clone()) {
                    roots.push(root);
                }
            }
        }
    }
    roots
}

fn extract_node_modules_package_name(entry: &str) -> Option<String> {
    let normalized = entry.replace('\\', "/");
    let marker = "node_modules/";
    let start = normalized.find(marker)?;
    let mut parts = normalized[start + marker.len()..].split('/');
    let first = parts.next()?;
    if first.starts_with('@') {
        let second = parts.next()?;
        Some(format!("{first}/{second}"))
    } else {
        Some(first.to_string())
    }
}

fn find_workspace_package_dir_by_name(
    workspace_root: &Path,
    package_name: &str,
) -> Option<PathBuf> {
    let entries = fs::read_dir(workspace_root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let package_json = path.join("package.json");
        if !package_json.is_file() {
            continue;
        }
        if parse_package_name_from_package(&package_json).is_some_and(|name| name == package_name) {
            return Some(path);
        }
    }
    None
}

fn resolve_package_declared_entries(
    package_dir: &Path,
    package_entries: &[String],
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let workspace_root = package_dir.parent();

    for raw_entry in package_entries {
        let mut resolved = Vec::new();
        let declared_path = package_dir.join(raw_entry);
        if declared_path.is_dir() {
            resolved.extend(collect_extension_entries_from_dir(&declared_path));
        } else if let Some(path) = resolve_extension_entry_file(&declared_path) {
            resolved.push(path);
        }

        if resolved.is_empty()
            && raw_entry.contains("node_modules/")
            && let Some(workspace_root) = workspace_root
            && let Some(package_name) = extract_node_modules_package_name(raw_entry)
            && let Some(workspace_package_dir) =
                find_workspace_package_dir_by_name(workspace_root, &package_name)
        {
            let nested_package_json = workspace_package_dir.join("package.json");
            match read_pi_extensions_from_package(&nested_package_json)? {
                Some(nested_entries) if !nested_entries.is_empty() => {
                    resolved.extend(resolve_package_declared_entries(
                        &workspace_package_dir,
                        &nested_entries,
                    )?);
                }
                Some(_) => {}
                None => {
                    if let Some(index_path) =
                        resolve_extension_entry_file(&workspace_package_dir.join("index"))
                    {
                        resolved.push(index_path);
                    }
                }
            }
        }

        for path in resolved {
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
    }

    Ok(out)
}

fn discover_workspace_bundle_entries(package_dir: &Path) -> Result<Vec<PathBuf>> {
    let Some(workspace_root) = package_dir.parent() else {
        return Ok(Vec::new());
    };

    let mut cluster_dirs = Vec::new();
    if let Ok(entries) = fs::read_dir(workspace_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                cluster_dirs.push(path);
            }
        }
    }
    if cluster_dirs.is_empty() || cluster_dirs.len() > MAX_BUNDLE_CLUSTER_DIRS {
        return Ok(Vec::new());
    }
    cluster_dirs.sort();

    let mut out = Vec::new();
    let mut seen = BTreeSet::new();

    for dir in &cluster_dirs {
        let package_json = dir.join("package.json");
        let Some(package_entries) = read_pi_extensions_from_package(&package_json)? else {
            continue;
        };
        for path in resolve_package_declared_entries(dir, &package_entries)? {
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
    }

    let mut root_files = Vec::new();
    if let Ok(entries) = fs::read_dir(workspace_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_supported_js_extension_entry(&path) {
                root_files.push(path);
            }
        }
    }
    root_files.sort();
    for path in root_files {
        let canonical = safe_canonicalize(&path);
        if seen.insert(canonical.clone()) {
            out.push(canonical);
        }
    }

    for dir in cluster_dirs {
        if dir.join("package.json").is_file() {
            continue;
        }
        for path in collect_extension_entries_from_dir(&dir) {
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
    }
    Ok(out)
}

fn discover_sibling_index_entries(primary: &Path) -> Vec<PathBuf> {
    let canonical_primary = safe_canonicalize(primary);
    if primary
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_none_or(|stem| !stem.eq_ignore_ascii_case("index"))
    {
        return Vec::new();
    }
    let Some(parent_dir) = primary.parent() else {
        return Vec::new();
    };
    let Some(cluster_root) = parent_dir.parent() else {
        return Vec::new();
    };

    let mut candidate_dirs = Vec::new();
    if let Ok(entries) = fs::read_dir(cluster_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                candidate_dirs.push(path);
            }
        }
    }
    if candidate_dirs.len() < 2 || candidate_dirs.len() > MAX_BUNDLE_CLUSTER_DIRS {
        return Vec::new();
    }
    candidate_dirs.sort();

    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for dir in candidate_dirs {
        for ext in JS_EXTENSION_ENTRY_EXTS {
            let candidate = dir.join(format!("index.{ext}"));
            if let Some(path) = resolve_extension_entry_file(&candidate) {
                if seen.insert(path.clone()) {
                    out.push(path);
                }
                break;
            }
        }
    }

    if out.len() < 2 || !out.iter().any(|path| path == &canonical_primary) {
        return Vec::new();
    }
    out
}

fn discover_sibling_extension_entries(primary: &Path) -> Vec<PathBuf> {
    let canonical_primary = safe_canonicalize(primary);
    let Some(parent_dir) = primary.parent() else {
        return Vec::new();
    };

    // Skip sibling discovery when the parent is a known auto-discovery root
    // (e.g., ~/.pi/agent/extensions/ or .pi/extensions/). Files in these
    // directories are independent extensions, not siblings of a single package.
    if parent_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("extensions"))
    {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut sibling_files = Vec::new();
    let mut sibling_dirs = Vec::new();
    if let Ok(entries) = fs::read_dir(parent_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_supported_js_extension_entry(&path) {
                sibling_files.push(path);
                continue;
            }
            if !path.is_dir() {
                continue;
            }
            if let Some(index_path) = resolve_extension_entry_file(&path.join("index")) {
                sibling_dirs.push(index_path);
            }
            if let Some(dir_name) = path.file_name().and_then(|name| name.to_str())
                && let Some(named_path) = resolve_extension_entry_file(&path.join(dir_name))
            {
                sibling_dirs.push(named_path);
            }
        }
    }
    sibling_files.sort();
    sibling_dirs.sort();

    for path in sibling_files {
        if !is_likely_flat_extension_entry(&path) {
            continue;
        }
        let canonical = safe_canonicalize(&path);
        if seen.insert(canonical.clone()) {
            out.push(canonical);
        }
    }
    for path in sibling_dirs {
        if seen.insert(path.clone()) {
            out.push(path);
        }
    }

    if out.len() < 2 || !out.iter().any(|path| path == &canonical_primary) {
        return Vec::new();
    }

    out
}

fn discover_related_extension_entries(primary: &Path) -> Result<Vec<PathBuf>> {
    let canonical_primary = safe_canonicalize(primary);
    let mut out = vec![canonical_primary.clone()];
    let mut seen = BTreeSet::new();
    let _ = seen.insert(canonical_primary.clone());

    let mut selected_package_dir: Option<PathBuf> = None;
    let mut selected_package_entries_len = 0usize;
    let mut selected_resolved: Vec<PathBuf> = Vec::new();
    let mut saw_manifest_extensions = false;
    for package_json in find_package_json_ancestors(primary.parent()) {
        let Some(package_dir) = package_json.parent() else {
            continue;
        };
        let Some(package_entries) = read_pi_extensions_from_package(&package_json)? else {
            continue;
        };
        saw_manifest_extensions = true;
        let resolved = resolve_package_declared_entries(package_dir, &package_entries)?;
        if !resolved.contains(&canonical_primary) {
            continue;
        }
        if resolved.len() > selected_resolved.len() {
            selected_package_dir = Some(package_dir.to_path_buf());
            selected_package_entries_len = package_entries.len();
            selected_resolved = resolved;
        }
    }

    let has_declared_package_entries = !selected_resolved.is_empty();
    if has_declared_package_entries {
        for path in selected_resolved {
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }

        let is_primary_index = canonical_primary
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case("index"));
        if selected_package_entries_len == 1
            && is_primary_index
            && let Some(package_dir) = selected_package_dir.as_deref()
        {
            let bundle_entries = discover_workspace_bundle_entries(package_dir)?;
            if bundle_entries.len() >= 2
                && bundle_entries.iter().any(|path| path == &canonical_primary)
            {
                for path in bundle_entries {
                    if seen.insert(path.clone()) {
                        out.push(path);
                    }
                }
            }
        }
    } else if saw_manifest_extensions {
        return Ok(out);
    }

    if !has_declared_package_entries {
        for path in discover_sibling_extension_entries(&canonical_primary) {
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
    }
    if let Some(package_dir) = selected_package_dir.as_deref() {
        for path in discover_auxiliary_example_entries(package_dir, &canonical_primary) {
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
    }
    if !has_declared_package_entries {
        for path in discover_sibling_index_entries(&canonical_primary) {
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
    }

    Ok(out)
}

type GroupedJsExtensionSpecs<'a> = Vec<(String, Vec<(&'a JsExtensionLoadSpec, Vec<PathBuf>)>)>;

fn group_js_extension_specs<'a>(
    specs: &'a [JsExtensionLoadSpec],
    explicit_entry_paths: &HashSet<PathBuf>,
) -> Result<GroupedJsExtensionSpecs<'a>> {
    let mut group_by_id = HashMap::<String, usize>::new();
    let mut grouped_specs = GroupedJsExtensionSpecs::new();
    for spec in specs {
        if spec.extension_id.trim().is_empty() {
            return Err(Error::extension("JS extension id cannot be empty"));
        }
        // Freeze discovery before any realm is created. The same exact entry
        // set drives both peer-boundary metadata and the subsequent load, so a
        // concurrent manifest/filesystem change cannot create an unclassified
        // extension root between two discovery passes.
        let entry_paths = resolve_extension_load_entry_paths(spec, explicit_entry_paths)?;
        let group_index = match group_by_id.entry(spec.extension_id.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let index = grouped_specs.len();
                grouped_specs.push((spec.extension_id.clone(), Vec::new()));
                entry.insert(index);
                index
            }
        };
        grouped_specs[group_index].1.push((spec, entry_paths));
    }
    Ok(grouped_specs)
}

fn collect_js_extension_roots(
    grouped_specs: &GroupedJsExtensionSpecs<'_>,
) -> Result<Vec<(String, Vec<PathBuf>)>> {
    let mut leaf_root_owner = HashMap::<PathBuf, String>::new();
    for (extension_id, extension_specs) in grouped_specs {
        for (_, entry_paths) in extension_specs {
            for entry_path in entry_paths {
                let Some(parent) = entry_path.parent() else {
                    continue;
                };
                let canonical_parent = safe_canonicalize(parent);
                if let Some(previous_owner) =
                    leaf_root_owner.insert(canonical_parent.clone(), extension_id.clone())
                    && previous_owner != *extension_id
                {
                    return Err(Error::extension(format!(
                        "Ambiguous JS extension ownership: {previous_owner} and {extension_id} both resolve entries under {}",
                        canonical_parent.display()
                    )));
                }
            }
        }
    }

    let mut roots_by_id = Vec::with_capacity(grouped_specs.len());
    for (extension_id, extension_specs) in grouped_specs {
        let mut roots = Vec::new();
        let mut seen = BTreeSet::new();
        for (_, entry_paths) in extension_specs {
            for root in collect_extension_roots_from_paths(entry_paths) {
                if seen.insert(root.clone()) {
                    roots.push(root);
                }
            }
        }
        roots_by_id.push((extension_id.clone(), roots));
    }
    Ok(roots_by_id)
}

fn js_runtime_shard_config(
    warm_pool: &crate::extensions_js::WarmIsolatePool,
    shard_count: usize,
    shard_index: usize,
) -> Result<PiJsRuntimeConfig> {
    let mut config = warm_pool.make_config();
    config.limits.memory_limit_bytes = config
        .limits
        .memory_limit_bytes
        .map(|total| split_shard_budget(total, shard_count, shard_index, "memory"))
        .transpose()?;
    config.limits.module_cache_limit_bytes = config
        .limits
        .module_cache_limit_bytes
        .map(|total| split_shard_budget(total, shard_count, shard_index, "module cache"))
        .transpose()?;
    let fast_queue_total = if config.limits.hostcall_fast_queue_capacity == 0 {
        crate::hostcall_queue::HOSTCALL_FAST_RING_CAPACITY
    } else {
        config.limits.hostcall_fast_queue_capacity
    };
    config.limits.hostcall_fast_queue_capacity = split_shard_budget(
        fast_queue_total,
        shard_count,
        shard_index,
        "hostcall fast queue",
    )?;
    let overflow_queue_total = if config.limits.hostcall_overflow_queue_capacity == 0 {
        crate::hostcall_queue::HOSTCALL_OVERFLOW_CAPACITY
    } else {
        config.limits.hostcall_overflow_queue_capacity
    };
    config.limits.hostcall_overflow_queue_capacity = split_shard_budget(
        overflow_queue_total,
        shard_count,
        shard_index,
        "hostcall overflow queue",
    )?;
    Ok(config)
}

#[allow(clippy::future_not_send)]
async fn build_js_runtime_shards(
    warm_pool: &crate::extensions_js::WarmIsolatePool,
    policy: &ExtensionPolicy,
    host: &JsRuntimeHost,
    specs: &[JsExtensionLoadSpec],
) -> Result<JsRuntimeShardSet> {
    let explicit_entry_paths = specs
        .iter()
        .map(|spec| safe_canonicalize(&spec.entry_path))
        .collect::<HashSet<_>>();

    // One logical extension can have multiple explicit entrypoints. Keep the
    // first-seen extension order and the original entrypoint order within each
    // realm so route resolution stays deterministic across reloads.
    let grouped_specs = group_js_extension_specs(specs, &explicit_entry_paths)?;
    let shard_count = grouped_specs.len();
    let extension_roots_by_id = collect_js_extension_roots(&grouped_specs)?;

    let mut candidate = JsRuntimeShardSet::default();
    for (shard_index, (extension_id, extension_specs)) in grouped_specs.into_iter().enumerate() {
        let shard_config = js_runtime_shard_config(warm_pool, shard_count, shard_index)?;

        let runtime = PiJsRuntime::with_clock_and_config_with_policy_for_extension(
            crate::scheduler::WallClock,
            shard_config,
            Some(policy.clone()),
            extension_id.clone(),
        )
        .await?;

        // Files under another extension's root must remain a protected
        // boundary even when both extensions live below the workspace cwd.
        // Register peer roots as metadata only; this intentionally grants no
        // read/write capability to the current shard.
        for (foreign_extension_id, roots) in &extension_roots_by_id {
            if foreign_extension_id == &extension_id {
                continue;
            }
            for root in roots {
                runtime.register_foreign_extension_root_boundary(root, foreign_extension_id);
            }
        }

        for (spec, entry_paths) in extension_specs {
            load_one_extension(&runtime, host, spec, &entry_paths).await?;
        }

        let snapshot =
            require_single_shard_snapshot(snapshot_extensions(&runtime).await?, &extension_id)?;
        candidate.shards.push(JsRuntimeShard {
            extension_id,
            runtime,
            snapshot,
            pump_fault: None,
        });
    }

    candidate.rebuild_indexes()?;
    Ok(candidate)
}

fn split_shard_budget(
    total: usize,
    shard_count: usize,
    shard_index: usize,
    budget_name: &str,
) -> Result<usize> {
    if shard_count == 0 || shard_index >= shard_count {
        return Err(Error::extension(format!(
            "Invalid JS runtime shard allocation for {budget_name}"
        )));
    }
    if total < shard_count {
        return Err(Error::extension(format!(
            "Configured aggregate {budget_name} budget ({total}) is too small for {shard_count} isolated extension shards"
        )));
    }
    let base = total / shard_count;
    let remainder = total % shard_count;
    Ok(base + usize::from(shard_index < remainder))
}

fn require_single_shard_snapshot(
    mut snapshots: Vec<JsExtensionSnapshot>,
    expected_extension_id: &str,
) -> Result<JsExtensionSnapshot> {
    if snapshots.len() != 1 {
        return Err(Error::extension(format!(
            "Extension runtime shard {expected_extension_id} produced {} registry snapshots; expected exactly one",
            snapshots.len()
        )));
    }
    let snapshot = snapshots.pop().expect("length checked above");
    if snapshot.id != expected_extension_id {
        return Err(Error::extension(format!(
            "Extension runtime shard owner mismatch: expected {expected_extension_id}, got {}",
            snapshot.id
        )));
    }
    Ok(snapshot)
}

#[allow(clippy::future_not_send)]
async fn get_registered_tools_from_shards(
    shards: &JsRuntimeShardSet,
) -> Result<Vec<ExtensionToolDef>> {
    let mut tools = Vec::new();
    for (shard_index, shard) in shards.shards.iter().enumerate() {
        shards.ensure_shard_healthy(shard_index)?;
        tools.extend(shard.runtime.get_registered_tools().await?);
    }
    Ok(tools)
}

#[allow(clippy::future_not_send)]
async fn scrub_and_drop_runtime_shards(
    shards: &mut JsRuntimeShardSet,
    warm_pool: &crate::extensions_js::WarmIsolatePool,
) -> Result<()> {
    let mut errors = Vec::new();
    for shard in &shards.shards {
        match shard.runtime.scrub_for_cold_drop().await {
            Ok(report) if report.inventoried_state_cleared => {
                warm_pool.record_reset();
            }
            Ok(report) => errors.push(format!(
                "{}: {} (residual_entries_after={})",
                shard.extension_id,
                report.reason_code.as_deref().unwrap_or("reset_not_clean"),
                report.residual_entries_after
            )),
            Err(err) => errors.push(format!("{}: {err}", shard.extension_id)),
        }
    }

    // A clean scrub is hygiene evidence, never realm-reuse authority. Arbitrary
    // extension JavaScript can mutate module and global singleton state outside
    // any finite registry inventory. Cold transactional loading is therefore
    // the only supported next step: drop every attempted realm and every
    // route/fault rather than retaining a Rust snapshot that cannot be proven
    // equivalent to a fresh realm.
    let next_provider_stream_id = shards.next_provider_stream_id;
    *shards = JsRuntimeShardSet {
        next_provider_stream_id,
        ..JsRuntimeShardSet::default()
    };

    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::extension(format!(
            "One or more JS extension shards failed full transient reset: {}",
            errors.join("; ")
        )))
    }
}

#[allow(clippy::future_not_send)]
async fn load_one_extension(
    runtime: &PiJsRuntime,
    host: &JsRuntimeHost,
    spec: &JsExtensionLoadSpec,
    entry_paths: &[PathBuf],
) -> Result<()> {
    if entry_paths.len() > 1 {
        tracing::info!(
            event = "ext.load.multi_entry",
            extension_id = %spec.extension_id,
            root_entry = %spec.entry_path.display(),
            resolved_entries = entry_paths.len(),
            "Loading extension package with multiple entrypoints"
        );
    }

    // Register the extension's root directory so `readFileSync` can access
    // bundled assets (HTML templates, markdown docs, etc.) within the
    // extension's own directory tree, and so the resolver can detect
    // monorepo escape patterns (Pattern 3).
    for root in collect_extension_roots_from_paths(entry_paths) {
        runtime.add_extension_root_with_id(root, Some(spec.extension_id.as_str()));
    }

    let meta = json!({
        "name": spec.name,
        "version": spec.version,
        "apiVersion": spec.api_version,
    });

    for (entry_index, entry_path) in entry_paths.iter().enumerate() {
        // QuickJS module resolver requires forward-slash paths.
        let entry_specifier = entry_path.display().to_string().replace('\\', "/");
        let task_id = next_runtime_task_id("task-load");
        let meta_value = meta.clone();
        let bridge_secret = runtime.bridge_secret().to_string();

        let bootstrap_result = runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let load_fn: rquickjs::Function<'_> = global.get("__pi_load_extension")?;
                let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
                let meta_js = json_to_js(&ctx, &meta_value)?;
                let promise: rquickjs::Value<'_> = load_fn.call((
                    bridge_secret.as_str(),
                    spec.extension_id.clone(),
                    entry_specifier.clone(),
                    meta_js,
                ))?;
                let _task: String =
                    task_start.call((bridge_secret.as_str(), task_id.as_str(), promise))?;
                Ok(())
            })
            .await;
        let load_result = match bootstrap_result {
            Ok(()) => await_js_task(
                runtime,
                host,
                Some(spec.extension_id.as_str()),
                &task_id,
                Duration::from_secs(10),
            )
            .await
            .map(|_| ()),
            Err(err) => Err(err),
        };

        match load_result {
            Ok(()) => {}
            Err(err) if entry_index == 0 => return Err(err),
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

fn resolve_extension_load_entry_paths(
    spec: &JsExtensionLoadSpec,
    explicit_entry_paths: &HashSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let explicit_primary = safe_canonicalize(&spec.entry_path);
    let mut entry_paths = discover_related_extension_entries(&spec.entry_path)?;
    if explicit_entry_paths.len() > 1 {
        entry_paths.retain(|entry_path| {
            let canonical = safe_canonicalize(entry_path);
            canonical == explicit_primary || !explicit_entry_paths.contains(&canonical)
        });
    }
    Ok(entry_paths)
}

#[allow(clippy::future_not_send)]
async fn snapshot_extensions(runtime: &PiJsRuntime) -> Result<Vec<JsExtensionSnapshot>> {
    let bridge_secret = runtime.bridge_secret().to_string();
    let json = runtime
        .with_ctx(|ctx| {
            let global = ctx.globals();
            let snapshot_fn: rquickjs::Function<'_> = global.get("__pi_snapshot_extensions")?;
            let value: rquickjs::Value<'_> = snapshot_fn.call((bridge_secret.as_str(),))?;
            js_to_json(&value)
        })
        .await?;

    let snapshots: Vec<JsExtensionSnapshot> =
        serde_json::from_value(json).map_err(|err| Error::extension(err.to_string()))?;
    Ok(snapshots)
}

#[allow(clippy::future_not_send)]
async fn refresh_runtime_shard_snapshot(
    shards: &mut JsRuntimeShardSet,
    shard_index: usize,
) -> Result<()> {
    let extension_id = shards
        .shards
        .get(shard_index)
        .ok_or_else(|| Error::extension("JS runtime shard disappeared"))?
        .extension_id
        .clone();
    let snapshots = match snapshot_extensions(&shards.shards[shard_index].runtime).await {
        Ok(snapshots) => snapshots,
        Err(err) => {
            return Err(quarantine_runtime_shard(
                shards,
                shard_index,
                &format!("registry snapshot failed: {err}"),
            ));
        }
    };
    let new_snapshot = match require_single_shard_snapshot(snapshots, &extension_id) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return Err(quarantine_runtime_shard(
                shards,
                shard_index,
                &format!("registry ownership validation failed: {err}"),
            ));
        }
    };
    let old_snapshot = std::mem::replace(&mut shards.shards[shard_index].snapshot, new_snapshot);
    if let Err(err) = shards.rebuild_indexes() {
        shards.shards[shard_index].snapshot = old_snapshot;
        let restore_error = if let Err(restore_err) = shards.rebuild_indexes() {
            tracing::error!(
                event = "extension_runtime.shards.index_restore_failed",
                extension_id,
                error = %restore_err,
                "Failed to restore JS extension route indexes after rejecting a dynamic registration"
            );
            Some(restore_err.to_string())
        } else {
            None
        };
        let reason = restore_error.map_or_else(
            || format!("registry route validation failed: {err}"),
            |restore_err| {
                format!(
                    "registry route validation failed: {err}; index restoration also failed: {restore_err}"
                )
            },
        );
        return Err(quarantine_runtime_shard(shards, shard_index, &reason));
    }
    Ok(())
}

fn quarantine_runtime_shard(
    shards: &mut JsRuntimeShardSet,
    shard_index: usize,
    reason: &str,
) -> Error {
    let extension_id = shards.shards.get(shard_index).map_or_else(
        || "<missing>".to_string(),
        |shard| shard.extension_id.clone(),
    );
    let fault = format!("JS extension shard {extension_id} quarantined: {reason}");
    if let Some(shard) = shards.shards.get_mut(shard_index) {
        shard.pump_fault = Some(fault.clone());
    }
    tracing::error!(
        event = "extension_runtime.shards.quarantined",
        extension_id,
        shard_index,
        reason,
        "Quarantined an inconsistent or unresponsive JS extension shard"
    );
    Error::extension(fault)
}

#[allow(clippy::future_not_send)]
async fn set_extension_flag_value(
    runtime: &PiJsRuntime,
    extension_id: &str,
    flag_name: &str,
    value: &Value,
) -> Result<()> {
    let bridge_secret = runtime.bridge_secret().to_string();
    runtime
        .with_ctx(|ctx| {
            let global = ctx.globals();
            let set_fn: rquickjs::Function<'_> = global.get("__pi_set_flag_value")?;
            let _: rquickjs::Value<'_> = set_fn.call((
                bridge_secret.as_str(),
                extension_id,
                flag_name,
                json_to_js(&ctx, value)?,
            ))?;
            Ok(())
        })
        .await
}

#[allow(clippy::future_not_send)]
async fn register_extension_mcp_server(
    runtime: &PiJsRuntime,
    extension_id: &str,
    name: &str,
    spec: &Value,
) -> Result<Value> {
    let bridge_secret = runtime.bridge_secret().to_string();
    runtime
        .with_ctx(|ctx| {
            let global = ctx.globals();
            let register_fn: rquickjs::Function<'_> =
                global.get("__pi_register_mcp_server_for_extension")?;
            let spec_js = json_to_js(&ctx, spec)?;
            let value: rquickjs::Value<'_> =
                register_fn.call((bridge_secret.as_str(), extension_id, name, spec_js))?;
            js_to_json(&value)
        })
        .await
}

#[inline]
fn next_runtime_task_id(prefix: &str) -> String {
    static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_TASK_ID.fetch_add(1, StdOrdering::Relaxed);
    format!("{prefix}-{id}")
}

#[derive(Debug, Deserialize)]
struct JsEventPhaseEnvelope {
    present: bool,
    #[serde(default)]
    value: Value,
}

fn remaining_js_task_timeout(deadline: Instant, operation: &str) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            Error::extension(format!(
                "JS extension {operation} timed out before dispatch completed"
            ))
        })
}

fn json_value_is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_none_or(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

struct JsEventPhaseDispatch<'a> {
    shard_index: usize,
    event_name: &'a str,
    event_payload: Value,
    ctx_payload: &'a Value,
    phase: &'a str,
    batch_id: Option<&'a str>,
    deadline: Instant,
}

#[allow(clippy::future_not_send)]
async fn dispatch_extension_event_phase_sharded(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    dispatch: JsEventPhaseDispatch<'_>,
) -> Result<Option<Value>> {
    let JsEventPhaseDispatch {
        shard_index,
        event_name,
        event_payload,
        ctx_payload,
        phase,
        batch_id,
        deadline,
    } = dispatch;
    shards.ensure_shard_healthy(shard_index)?;
    let task_id = next_runtime_task_id("task-event-phase");
    {
        let runtime = &shards
            .shards
            .get(shard_index)
            .ok_or_else(|| Error::extension("JS runtime shard disappeared"))?
            .runtime;
        let bridge_secret = runtime.bridge_secret().to_string();
        runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
                let event_js = json_to_js(&ctx, &event_payload)?;
                let promise: rquickjs::Value<'_> = if let Some(batch_id) = batch_id {
                    let dispatch_fn: rquickjs::Function<'_> =
                        global.get("__pi_dispatch_extension_event_phase_in_batch")?;
                    dispatch_fn.call((
                        bridge_secret.as_str(),
                        event_name,
                        event_js,
                        batch_id,
                        phase,
                    ))?
                } else {
                    let dispatch_fn: rquickjs::Function<'_> =
                        global.get("__pi_dispatch_extension_event_phase")?;
                    let ctx_js = json_to_js(&ctx, ctx_payload)?;
                    dispatch_fn.call((
                        bridge_secret.as_str(),
                        event_name,
                        event_js,
                        ctx_js,
                        phase,
                    ))?
                };
                let _task: String =
                    task_start.call((bridge_secret.as_str(), task_id.as_str(), promise))?;
                Ok(())
            })
            .await?;
    }

    let raw = await_js_task_in_shards_and_refresh(
        shards,
        host,
        shard_index,
        &task_id,
        remaining_js_task_timeout(deadline, "event")?,
    )
    .await?;
    let envelope: JsEventPhaseEnvelope = serde_json::from_value(raw)
        .map_err(|err| Error::extension(format!("event phase envelope: {err}")))?;
    Ok(envelope.present.then_some(envelope.value))
}

fn input_event_payload(text: &str, images: Option<&Value>, source: &Value) -> Value {
    let mut payload = serde_json::Map::from_iter([
        ("type".to_string(), Value::String("input".to_string())),
        ("text".to_string(), Value::String(text.to_string())),
        ("source".to_string(), source.clone()),
    ]);
    if let Some(images) = images {
        payload.insert("images".to_string(), images.clone());
    }
    Value::Object(payload)
}

fn before_agent_start_payload(prompt: &str, images: Option<&Value>, system_prompt: &str) -> Value {
    let mut payload = serde_json::Map::from_iter([
        (
            "type".to_string(),
            Value::String("before_agent_start".to_string()),
        ),
        ("prompt".to_string(), Value::String(prompt.to_string())),
        (
            "systemPrompt".to_string(),
            Value::String(system_prompt.to_string()),
        ),
    ]);
    if let Some(images) = images {
        payload.insert("images".to_string(), images.clone());
    }
    Value::Object(payload)
}

fn push_resource_paths(target: &mut Vec<Value>, value: Option<&Value>) {
    match value {
        Some(Value::Array(paths)) => {
            target.extend(paths.iter().filter_map(|path| {
                path.as_str()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(|path| Value::String(path.to_string()))
            }));
        }
        Some(Value::String(path)) if !path.trim().is_empty() => {
            target.push(Value::String(path.trim().to_string()));
        }
        _ => {}
    }
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn dispatch_extension_event_across_shards_until(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    event_name: &str,
    event_payload: Value,
    ctx_payload: &Value,
    batch_id: Option<&str>,
    deadline: Instant,
) -> Result<Value> {
    let owners = shards
        .event_owners
        .get(event_name)
        .cloned()
        .unwrap_or_default();
    if owners.is_empty() {
        return Ok(Value::Null);
    }

    if event_name == "input" {
        let original_text = event_payload
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| event_payload.get("content").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string();
        let original_images = event_payload
            .get("images")
            .or_else(|| event_payload.get("attachments"))
            .cloned();
        let source = event_payload
            .get("source")
            .cloned()
            .unwrap_or_else(|| Value::String("extension".to_string()));
        let mut current_text = original_text.clone();
        let mut current_images = original_images.clone();
        let mut saw_handler_result = false;

        for phase in ["direct", "event_bus"] {
            for &shard_index in &owners {
                let payload = input_event_payload(&current_text, current_images.as_ref(), &source);
                let Some(value) = dispatch_extension_event_phase_sharded(
                    shards,
                    host,
                    JsEventPhaseDispatch {
                        shard_index,
                        event_name,
                        event_payload: payload,
                        ctx_payload,
                        phase,
                        batch_id,
                        deadline,
                    },
                )
                .await?
                else {
                    continue;
                };
                saw_handler_result = true;
                if value.get("action").and_then(Value::as_str) == Some("handled") {
                    return Ok(value);
                }
                if value.get("action").and_then(Value::as_str) == Some("transform")
                    && let Some(text) = value.get("text").and_then(Value::as_str)
                {
                    current_text = text.to_string();
                    if let Some(images) = value.get("images") {
                        current_images = Some(images.clone());
                    }
                }
            }
        }

        if current_text != original_text || current_images != original_images {
            let mut result = serde_json::Map::from_iter([
                ("action".to_string(), Value::String("transform".to_string())),
                ("text".to_string(), Value::String(current_text)),
            ]);
            if let Some(images) = current_images {
                result.insert("images".to_string(), images);
            }
            return Ok(Value::Object(result));
        }
        return Ok(if saw_handler_result {
            json!({ "action": "continue" })
        } else {
            Value::Null
        });
    }

    if event_name == "before_agent_start" {
        let prompt = event_payload
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let images = event_payload.get("images").cloned();
        let mut system_prompt = event_payload
            .get("systemPrompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut modified = false;
        let mut messages = Vec::new();

        for phase in ["direct", "event_bus"] {
            for &shard_index in &owners {
                let payload = before_agent_start_payload(&prompt, images.as_ref(), &system_prompt);
                let Some(value) = dispatch_extension_event_phase_sharded(
                    shards,
                    host,
                    JsEventPhaseDispatch {
                        shard_index,
                        event_name,
                        event_payload: payload,
                        ctx_payload,
                        phase,
                        batch_id,
                        deadline,
                    },
                )
                .await?
                else {
                    continue;
                };
                if let Some(next_messages) = value.get("messages").and_then(Value::as_array) {
                    messages.extend(next_messages.iter().cloned());
                }
                if let Some(next_prompt) = value.get("systemPrompt") {
                    system_prompt = next_prompt
                        .as_str()
                        .map_or_else(|| next_prompt.to_string(), ToString::to_string);
                    modified = true;
                }
            }
        }

        if messages.is_empty() && !modified {
            return Ok(Value::Null);
        }
        let mut result = serde_json::Map::new();
        if !messages.is_empty() {
            result.insert("messages".to_string(), Value::Array(messages));
        }
        if modified {
            result.insert("systemPrompt".to_string(), Value::String(system_prompt));
        }
        return Ok(Value::Object(result));
    }

    if event_name == "resources_discover" {
        let mut skill_paths = Vec::new();
        let mut prompt_paths = Vec::new();
        let mut theme_paths = Vec::new();
        for phase in ["direct", "event_bus"] {
            for &shard_index in &owners {
                let Some(value) = dispatch_extension_event_phase_sharded(
                    shards,
                    host,
                    JsEventPhaseDispatch {
                        shard_index,
                        event_name,
                        event_payload: event_payload.clone(),
                        ctx_payload,
                        phase,
                        batch_id,
                        deadline,
                    },
                )
                .await?
                else {
                    continue;
                };
                push_resource_paths(&mut skill_paths, value.get("skillPaths"));
                push_resource_paths(&mut prompt_paths, value.get("promptPaths"));
                push_resource_paths(&mut theme_paths, value.get("themePaths"));
            }
        }
        let mut result = serde_json::Map::new();
        if !skill_paths.is_empty() {
            result.insert("skillPaths".to_string(), Value::Array(skill_paths));
        }
        if !prompt_paths.is_empty() {
            result.insert("promptPaths".to_string(), Value::Array(prompt_paths));
        }
        if !theme_paths.is_empty() {
            result.insert("themePaths".to_string(), Value::Array(theme_paths));
        }
        return Ok(if result.is_empty() {
            Value::Null
        } else {
            Value::Object(result)
        });
    }

    let mut last = None;
    for phase in ["direct", "event_bus"] {
        for &shard_index in &owners {
            let Some(value) = dispatch_extension_event_phase_sharded(
                shards,
                host,
                JsEventPhaseDispatch {
                    shard_index,
                    event_name,
                    event_payload: event_payload.clone(),
                    ctx_payload,
                    phase,
                    batch_id,
                    deadline,
                },
            )
            .await?
            else {
                continue;
            };
            if event_name == "user_bash" {
                return Ok(value);
            }
            let should_stop = (event_name == "tool_call"
                && value.get("block").is_some_and(json_value_is_truthy))
                || (event_name.starts_with("session_before_")
                    && value.get("cancel").is_some_and(json_value_is_truthy));
            last = Some(value);
            if should_stop {
                return Ok(last.expect("value assigned above"));
            }
        }
    }
    Ok(last.unwrap_or(Value::Null))
}

#[allow(clippy::future_not_send)]
async fn dispatch_extension_event_across_shards(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    event_name: &str,
    event_payload: Value,
    ctx_payload: &Value,
    timeout_ms: u64,
) -> Result<Value> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(timeout_ms))
        .ok_or_else(|| Error::extension("JS extension event deadline overflow"))?;
    dispatch_extension_event_across_shards_until(
        shards,
        host,
        event_name,
        event_payload,
        ctx_payload,
        None,
        deadline,
    )
    .await
}

#[allow(clippy::future_not_send)]
async fn delete_event_batch_contexts(
    shards: &JsRuntimeShardSet,
    shard_indexes: &[usize],
    batch_id: &str,
) -> Result<()> {
    let mut first_error = None;
    for &shard_index in shard_indexes {
        let Some(shard) = shards.shards.get(shard_index) else {
            if first_error.is_none() {
                first_error = Some(Error::extension("JS runtime shard disappeared"));
            }
            continue;
        };
        let bridge_secret = shard.runtime.bridge_secret().to_string();
        let result = shard
            .runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let delete_fn: rquickjs::Function<'_> =
                    global.get("__pi_event_batch_context_delete")?;
                let _: rquickjs::Value<'_> = delete_fn.call((bridge_secret.as_str(), batch_id))?;
                Ok(())
            })
            .await;
        if let Err(err) = result
            && first_error.is_none()
        {
            first_error = Some(err);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[allow(clippy::future_not_send)]
async fn create_event_batch_contexts(
    shards: &JsRuntimeShardSet,
    batch_id: &str,
    ctx_payload: &Value,
) -> Result<Vec<usize>> {
    let mut created = Vec::with_capacity(shards.shards.len());
    for (shard_index, shard) in shards.shards.iter().enumerate() {
        let bridge_secret = shard.runtime.bridge_secret().to_string();
        let result = shard
            .runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let create_fn: rquickjs::Function<'_> =
                    global.get("__pi_event_batch_context_create")?;
                let ctx_js = json_to_js(&ctx, ctx_payload)?;
                let _: rquickjs::Value<'_> =
                    create_fn.call((bridge_secret.as_str(), batch_id, ctx_js))?;
                Ok(())
            })
            .await;
        if let Err(err) = result {
            if let Err(cleanup_err) = delete_event_batch_contexts(shards, &created, batch_id).await
            {
                tracing::warn!(
                    event = "extension_runtime.event_batch.setup_cleanup_failed",
                    batch_id,
                    error = %cleanup_err,
                    "Failed to clean up partial extension event batch contexts"
                );
            }
            return Err(err);
        }
        created.push(shard_index);
    }
    Ok(created)
}

#[allow(clippy::future_not_send)]
async fn dispatch_extension_event_batch_across_shards(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    events: Vec<(String, Value)>,
    ctx_payload: &Value,
    timeout_ms: u64,
) -> Result<Vec<Result<Value>>> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(timeout_ms))
        .ok_or_else(|| Error::extension("JS extension batch event deadline overflow"))?;
    let batch_id = next_runtime_task_id("event-batch-context");
    let created_contexts = create_event_batch_contexts(shards, &batch_id, ctx_payload).await?;
    let mut results = Vec::with_capacity(events.len());
    for (event_name, event_payload) in events {
        results.push(
            dispatch_extension_event_across_shards_until(
                shards,
                host,
                &event_name,
                event_payload,
                ctx_payload,
                Some(batch_id.as_str()),
                deadline,
            )
            .await,
        );
    }
    delete_event_batch_contexts(shards, &created_contexts, &batch_id).await?;
    Ok(results)
}

struct JsToolExecution<'a> {
    shard_index: usize,
    tool_name: &'a str,
    tool_call_id: &'a str,
    input: Value,
    ctx_payload: &'a Value,
    timeout_ms: u64,
}

#[allow(clippy::future_not_send)]
async fn execute_extension_tool_sharded(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    execution: JsToolExecution<'_>,
) -> Result<Value> {
    let JsToolExecution {
        shard_index,
        tool_name,
        tool_call_id,
        input,
        ctx_payload,
        timeout_ms,
    } = execution;
    shards.ensure_shard_healthy(shard_index)?;
    let started_at = Instant::now();
    tracing::info!(
        event = "ext.tool.start",
        tool_name = %tool_name,
        tool_call_id = %tool_call_id,
        timeout_ms,
        "Extension tool execution start"
    );
    let task_id = next_runtime_task_id("task-tool");
    {
        let runtime = &shards
            .shards
            .get(shard_index)
            .ok_or_else(|| Error::extension("JS runtime shard disappeared"))?
            .runtime;
        let bridge_secret = runtime.bridge_secret().to_string();
        runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let exec_fn: rquickjs::Function<'_> = global.get("__pi_execute_tool")?;
                let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
                let input_js = json_to_js(&ctx, &input)?;
                let ctx_js = json_to_js(&ctx, ctx_payload)?;
                let promise: rquickjs::Value<'_> = exec_fn.call((
                    bridge_secret.as_str(),
                    tool_name,
                    tool_call_id,
                    input_js,
                    ctx_js,
                ))?;
                let _task: String =
                    task_start.call((bridge_secret.as_str(), task_id.as_str(), promise))?;
                Ok(())
            })
            .await?;
    }

    let result = await_js_task_in_shards_and_refresh(
        shards,
        host,
        shard_index,
        &task_id,
        Duration::from_millis(timeout_ms),
    )
    .await;
    let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let is_err = result.is_err();
    tracing::info!(
        event = "ext.tool.end",
        tool_name = %tool_name,
        tool_call_id = %tool_call_id,
        duration_ms,
        is_error = is_err,
        "Extension tool execution end"
    );
    result
}

#[allow(clippy::future_not_send)]
async fn execute_extension_command_sharded(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    shard_index: usize,
    command_name: &str,
    args: &str,
    ctx_payload: &Value,
    timeout_ms: u64,
) -> Result<Value> {
    shards.ensure_shard_healthy(shard_index)?;
    let started_at = Instant::now();
    tracing::info!(
        event = "ext.command.start",
        command = %command_name,
        timeout_ms,
        "Extension command execution start"
    );
    let task_id = next_runtime_task_id("task-cmd");
    {
        let runtime = &shards
            .shards
            .get(shard_index)
            .ok_or_else(|| Error::extension("JS runtime shard disappeared"))?
            .runtime;
        let bridge_secret = runtime.bridge_secret().to_string();
        runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let exec_fn: rquickjs::Function<'_> = global.get("__pi_execute_command")?;
                let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
                let ctx_js = json_to_js(&ctx, ctx_payload)?;
                let promise: rquickjs::Value<'_> =
                    exec_fn.call((bridge_secret.as_str(), command_name, args, ctx_js))?;
                let _task: String =
                    task_start.call((bridge_secret.as_str(), task_id.as_str(), promise))?;
                Ok(())
            })
            .await?;
    }

    let result = await_js_task_in_shards_and_refresh(
        shards,
        host,
        shard_index,
        &task_id,
        Duration::from_millis(timeout_ms),
    )
    .await;
    let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let is_err = result.is_err();
    tracing::info!(
        event = "ext.command.end",
        command = %command_name,
        duration_ms,
        is_error = is_err,
        "Extension command execution end"
    );
    result
}

#[allow(clippy::future_not_send)]
async fn execute_extension_shortcut_sharded(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    shard_index: usize,
    key_id: &str,
    ctx_payload: &Value,
    timeout_ms: u64,
) -> Result<Value> {
    shards.ensure_shard_healthy(shard_index)?;
    let started_at = Instant::now();
    tracing::info!(
        event = "ext.shortcut.start",
        key_id = %key_id,
        timeout_ms,
        "Extension shortcut execution start"
    );
    let task_id = next_runtime_task_id("task-shortcut");
    {
        let runtime = &shards
            .shards
            .get(shard_index)
            .ok_or_else(|| Error::extension("JS runtime shard disappeared"))?
            .runtime;
        let bridge_secret = runtime.bridge_secret().to_string();
        runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let exec_fn: rquickjs::Function<'_> = global.get("__pi_execute_shortcut")?;
                let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
                let ctx_js = json_to_js(&ctx, ctx_payload)?;
                let promise: rquickjs::Value<'_> =
                    exec_fn.call((bridge_secret.as_str(), key_id, ctx_js))?;
                let _task: String =
                    task_start.call((bridge_secret.as_str(), task_id.as_str(), promise))?;
                Ok(())
            })
            .await?;
    }

    let result = await_js_task_in_shards_and_refresh(
        shards,
        host,
        shard_index,
        &task_id,
        Duration::from_millis(timeout_ms),
    )
    .await;
    let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let is_err = result.is_err();
    tracing::info!(
        event = "ext.shortcut.end",
        key_id = %key_id,
        duration_ms,
        is_error = is_err,
        "Extension shortcut execution end"
    );
    result
}

#[derive(Debug, Deserialize)]
struct JsProviderStreamNext {
    done: bool,
    #[serde(default)]
    value: Option<Value>,
}

struct JsProviderStreamStart<'a> {
    shard_index: usize,
    provider_id: &'a str,
    model: Value,
    context: Value,
    options: Value,
    timeout_ms: u64,
}

#[allow(clippy::future_not_send)]
async fn start_extension_provider_stream_simple_sharded(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    start: JsProviderStreamStart<'_>,
) -> Result<String> {
    let JsProviderStreamStart {
        shard_index,
        provider_id,
        model,
        context,
        options,
        timeout_ms,
    } = start;
    shards.ensure_shard_healthy(shard_index)?;
    let timeout = Duration::from_millis(timeout_ms);
    let deadline = Instant::now().checked_add(timeout);
    let task_id = next_runtime_task_id("task-provider-stream-start");
    {
        let runtime = &shards
            .shards
            .get(shard_index)
            .ok_or_else(|| Error::extension("JS runtime shard disappeared"))?
            .runtime;
        let bridge_secret = runtime.bridge_secret().to_string();
        runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let start_fn: rquickjs::Function<'_> =
                    global.get("__pi_provider_stream_simple_start")?;
                let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
                let model_js = json_to_js(&ctx, &model)?;
                let context_js = json_to_js(&ctx, &context)?;
                let options_js = json_to_js(&ctx, &options)?;
                let promise: rquickjs::Value<'_> = start_fn.call((
                    bridge_secret.as_str(),
                    provider_id,
                    model_js,
                    context_js,
                    options_js,
                ))?;
                let _task: String =
                    task_start.call((bridge_secret.as_str(), task_id.as_str(), promise))?;
                Ok(())
            })
            .await?;
    }

    let value = await_js_task_in_shards(shards, host, shard_index, &task_id, timeout).await?;
    let inner_stream_id = value
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| Error::extension("provider stream start: expected stream id".to_string()))?;

    if let Err(refresh_err) = refresh_runtime_shard_snapshot(shards, shard_index).await {
        let cleanup_timeout_ms = deadline
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            .map_or(1, |remaining| {
                u64::try_from(remaining.as_millis())
                    .unwrap_or(u64::MAX)
                    .max(1)
            });
        if let Err(cleanup_err) = cancel_extension_provider_stream_simple_best_effort(
            shards,
            host,
            shard_index,
            &inner_stream_id,
            cleanup_timeout_ms,
        )
        .await
        {
            tracing::warn!(
                event = "extension_runtime.provider_stream.start_refresh_cleanup_failed",
                inner_stream_id,
                shard_index,
                error = %cleanup_err,
                "Failed to cancel an inner provider stream after registry refresh rejected its shard"
            );
        }
        return Err(refresh_err);
    }

    Ok(inner_stream_id)
}

#[allow(clippy::future_not_send)]
async fn next_extension_provider_stream_simple_sharded(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    shard_index: usize,
    stream_id: &str,
    timeout_ms: u64,
) -> Result<Option<Value>> {
    shards.ensure_shard_healthy(shard_index)?;
    let task_id = next_runtime_task_id("task-provider-stream-next");
    {
        let runtime = &shards
            .shards
            .get(shard_index)
            .ok_or_else(|| Error::extension("JS runtime shard disappeared"))?
            .runtime;
        let bridge_secret = runtime.bridge_secret().to_string();
        runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let next_fn: rquickjs::Function<'_> =
                    global.get("__pi_provider_stream_simple_next")?;
                let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
                let promise: rquickjs::Value<'_> =
                    next_fn.call((bridge_secret.as_str(), stream_id))?;
                let _task: String =
                    task_start.call((bridge_secret.as_str(), task_id.as_str(), promise))?;
                Ok(())
            })
            .await?;
    }

    let value = await_js_task_in_shards_and_refresh(
        shards,
        host,
        shard_index,
        &task_id,
        Duration::from_millis(timeout_ms),
    )
    .await?;
    let result: JsProviderStreamNext = serde_json::from_value(value)
        .map_err(|err| Error::extension(format!("provider stream next: {err}")))?;
    if result.done {
        return Ok(None);
    }
    let Some(value) = result.value else {
        return Err(Error::extension(
            "provider stream next: missing value".to_string(),
        ));
    };
    Ok(Some(value))
}

#[allow(clippy::future_not_send)]
async fn cancel_extension_provider_stream_simple_sharded(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    shard_index: usize,
    stream_id: &str,
    timeout_ms: u64,
) -> Result<()> {
    let task_id = next_runtime_task_id("task-provider-stream-cancel");
    {
        let runtime = &shards
            .shards
            .get(shard_index)
            .ok_or_else(|| Error::extension("JS runtime shard disappeared"))?
            .runtime;
        let bridge_secret = runtime.bridge_secret().to_string();
        runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let cancel_fn: rquickjs::Function<'_> =
                    global.get("__pi_provider_stream_simple_cancel")?;
                let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
                let promise: rquickjs::Value<'_> =
                    cancel_fn.call((bridge_secret.as_str(), stream_id))?;
                let _task: String =
                    task_start.call((bridge_secret.as_str(), task_id.as_str(), promise))?;
                Ok(())
            })
            .await?;
    }

    let _ = await_js_task_in_shards_and_refresh(
        shards,
        host,
        shard_index,
        &task_id,
        Duration::from_millis(timeout_ms),
    )
    .await?;
    Ok(())
}

#[allow(clippy::future_not_send)]
async fn cancel_extension_provider_stream_simple_best_effort(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    shard_index: usize,
    stream_id: &str,
    timeout_ms: u64,
) -> Result<()> {
    // Cleanup is the sole operation allowed to enter a quarantined realm. A
    // prior logical fault may still leave an async iterator that can release a
    // child process or host resource through return(). Preserve the original
    // quarantine verdict after the best-effort attempt.
    let prior_fault = shards
        .shards
        .get_mut(shard_index)
        .and_then(|shard| shard.pump_fault.take());
    let result = cancel_extension_provider_stream_simple_sharded(
        shards,
        host,
        shard_index,
        stream_id,
        timeout_ms,
    )
    .await;
    if let Some(prior_fault) = prior_fault
        && let Some(shard) = shards.shards.get_mut(shard_index)
    {
        shard.pump_fault = Some(prior_fault);
    }
    result
}

#[allow(clippy::future_not_send)]
async fn cancel_active_provider_streams_for_replacement(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    cleanup_budget: Duration,
) {
    let deadline = Instant::now().checked_add(cleanup_budget);
    let mut routes = std::mem::take(&mut shards.provider_stream_routes)
        .into_iter()
        .collect::<Vec<_>>();
    routes.sort_by(|(left, _), (right, _)| left.cmp(right));
    let total = routes.len();
    let mut attempted = 0usize;
    let mut failures = 0usize;

    for (outer_stream_id, route) in routes {
        let Some(remaining) =
            deadline.and_then(|deadline| deadline.checked_duration_since(Instant::now()))
        else {
            tracing::warn!(
                event = "extension_runtime.provider_stream.reload_cleanup_budget_exhausted",
                total,
                attempted,
                skipped = total.saturating_sub(attempted),
                cleanup_budget_ms = u64::try_from(cleanup_budget.as_millis()).unwrap_or(u64::MAX),
                "Provider stream cleanup budget expired before cold shard replacement"
            );
            break;
        };
        let timeout_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        attempted = attempted.saturating_add(1);
        if let Err(err) = cancel_extension_provider_stream_simple_best_effort(
            shards,
            host,
            route.shard_index,
            &route.inner_stream_id,
            timeout_ms,
        )
        .await
        {
            failures = failures.saturating_add(1);
            tracing::warn!(
                event = "extension_runtime.provider_stream.reload_cleanup_failed",
                outer_stream_id,
                inner_stream_id = %route.inner_stream_id,
                shard_index = route.shard_index,
                error = %err,
                "Failed to cancel an active provider stream before cold shard replacement"
            );
        }
    }

    if total > 0 {
        tracing::info!(
            event = "extension_runtime.provider_stream.reload_cleanup",
            total,
            attempted,
            failures,
            "Completed best-effort provider stream cleanup before cold shard replacement"
        );
    }
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn pump_js_runtime_once_for_owner(
    runtime: &PiJsRuntime,
    host: &JsRuntimeHost,
    expected_owner: Option<&str>,
) -> Result<bool> {
    fn drain_requests(runtime: &PiJsRuntime) -> std::collections::VecDeque<HostcallRequest> {
        runtime.drain_hostcall_requests()
    }

    /// Dispatch a single hostcall request, recording timing and returning
    /// the completion pair plus elapsed nanoseconds for AMAC telemetry.
    async fn dispatch_one(
        runtime: &PiJsRuntime,
        host: &JsRuntimeHost,
        expected_owner: Option<&str>,
        req: HostcallRequest,
    ) -> Option<(String, HostcallOutcome, u64)> {
        let call_id = req.call_id.clone();
        if !runtime.is_hostcall_active(&call_id) {
            tracing::debug!(
                event = "pijs.hostcall.skip_cancelled",
                call_id = %call_id,
                "Skipping hostcall dispatch because call is no longer pending"
            );
            return None;
        }
        let extension_id = req.extension_id.clone();
        let queue_wait_ms = runtime.hostcall_queue_wait_ms(&call_id).unwrap_or(0);
        let dispatch_started = Instant::now();
        let outcome = if let Some(expected_owner) = expected_owner
            && req.extension_id.as_deref() != Some(expected_owner)
        {
            tracing::error!(
                event = "pijs.hostcall.owner_mismatch",
                call_id = %call_id,
                expected_owner,
                claimed_owner = ?req.extension_id,
                "Rejected hostcall whose claimed extension owner does not match its runtime shard"
            );
            HostcallOutcome::Error {
                code: "extension_identity_mismatch".to_string(),
                message: format!(
                    "Runtime shard {expected_owner} rejected hostcall claiming extension {:?}",
                    req.extension_id
                ),
            }
        } else {
            dispatch_hostcall_with_runtime(Some(runtime), host, req).await
        };
        let elapsed = dispatch_started.elapsed();
        let execution_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let elapsed_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let outcome_code = match &outcome {
            HostcallOutcome::Success(_) => "success",
            HostcallOutcome::StreamChunk { .. } => "stream",
            HostcallOutcome::Error { code, .. } => code.as_str(),
        };
        tracing::debug!(
            event = "pijs.hostcall.dispatch_timing",
            call_id = %call_id,
            extension_id = ?extension_id,
            queue_wait_ms,
            execution_ms,
            outcome_code = %outcome_code,
            "Hostcall dispatch timing"
        );
        Some((call_id, outcome, elapsed_ns))
    }

    async fn dispatch_requests(
        runtime: &PiJsRuntime,
        host: &JsRuntimeHost,
        expected_owner: Option<&str>,
        pending: std::collections::VecDeque<HostcallRequest>,
    ) {
        if pending.is_empty() {
            return;
        }

        let amac_enabled = AMAC_EXECUTOR.with(|cell| cell.borrow().enabled());

        // Check safety envelope veto — if any extension's conformal+PAC-Bayes
        // envelope is in a vetoing state, disable AMAC interleaving and fall
        // back to conservative sequential dispatch.
        let safety_vetoed = host
            .manager()
            .is_some_and(|mgr| mgr.any_safety_envelope_vetoing());

        if amac_enabled && !safety_vetoed {
            dispatch_requests_amac(runtime, host, expected_owner, pending).await;
        } else {
            dispatch_requests_sequential(runtime, host, expected_owner, pending).await;
        }
    }

    /// Sequential dispatch path (AMAC disabled or fallback).
    async fn dispatch_requests_sequential(
        runtime: &PiJsRuntime,
        host: &JsRuntimeHost,
        expected_owner: Option<&str>,
        pending: std::collections::VecDeque<HostcallRequest>,
    ) {
        let mut completions = Vec::with_capacity(pending.len());
        for req in pending {
            if let Some((call_id, outcome, elapsed_ns)) =
                dispatch_one(runtime, host, expected_owner, req).await
            {
                // Feed timing to AMAC even when disabled, so telemetry
                // is ready if toggled on later.
                AMAC_EXECUTOR.with(|cell| cell.borrow_mut().observe_call(elapsed_ns));
                completions.push((call_id, outcome));
            }
        }
        if !completions.is_empty() {
            runtime.complete_hostcalls_batch(completions);
        }
    }

    /// AMAC batch dispatch path: group requests by kind, decide per-group
    /// whether to interleave, and dispatch with timing telemetry.
    async fn dispatch_requests_amac(
        runtime: &PiJsRuntime,
        host: &JsRuntimeHost,
        expected_owner: Option<&str>,
        pending: std::collections::VecDeque<HostcallRequest>,
    ) {
        let requests: Vec<HostcallRequest> = pending.into_iter().collect();
        let total = requests.len();

        // Plan the batch: group by kind, decide toggle per group.
        let plan = AMAC_EXECUTOR.with(|cell| cell.borrow_mut().plan_batch(requests));

        tracing::debug!(
            event = "pijs.amac.batch_planned",
            total_requests = total,
            groups = plan.groups.len(),
            interleaved = plan.interleaved_groups,
            sequential = plan.sequential_groups,
            "AMAC batch plan created"
        );

        let batch_start = Instant::now();
        let mut completions = Vec::with_capacity(total);

        for (group, decision) in plan.groups.into_iter().zip(plan.decisions) {
            tracing::debug!(
                event = "pijs.amac.group_dispatch",
                group_key = ?group.key,
                group_size = group.len(),
                interleave = decision.is_interleave(),
                "Dispatching AMAC group"
            );

            for req in group.requests {
                if let Some((call_id, outcome, elapsed_ns)) =
                    dispatch_one(runtime, host, expected_owner, req).await
                {
                    AMAC_EXECUTOR.with(|cell| cell.borrow_mut().observe_call(elapsed_ns));
                    completions.push((call_id, outcome));
                }
            }
        }

        let batch_elapsed_ms = u64::try_from(batch_start.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::debug!(
            event = "pijs.amac.batch_complete",
            total_dispatched = completions.len(),
            batch_elapsed_ms,
            "AMAC batch dispatch complete"
        );

        if !completions.is_empty() {
            runtime.complete_hostcalls_batch(completions);
        }
    }

    // Process any hostcalls already queued before we advance the event loop.
    dispatch_requests(runtime, host, expected_owner, drain_requests(runtime)).await;

    // Advance the event loop (may schedule hostcalls while running a task's microtasks).
    let _ = runtime.tick().await?;
    let _ = runtime.drain_microtasks().await?;

    // Process hostcalls scheduled during the tick/microtask phase. Without this, fire-and-forget
    // calls (e.g. `pi.sendMessage()` without `await`) can be lost when a JS task resolves quickly.
    let after_tick = drain_requests(runtime);
    let has_after_tick = !after_tick.is_empty();
    dispatch_requests(runtime, host, expected_owner, after_tick).await;

    // If we dispatched any hostcalls, run another tick so their completions are delivered and
    // microtasks reach a fixpoint before the caller observes the outcome.
    if has_after_tick {
        let _ = runtime.tick().await?;
        let _ = runtime.drain_microtasks().await?;
    }

    Ok(runtime.has_pending())
}

#[allow(clippy::future_not_send)]
async fn pump_js_runtime_once(runtime: &PiJsRuntime, host: &JsRuntimeHost) -> Result<bool> {
    pump_js_runtime_once_for_owner(runtime, host, None).await
}

#[allow(clippy::future_not_send)]
async fn pump_js_runtime_shards_once(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
) -> Result<bool> {
    pump_js_runtime_shards_once_for_target(shards, host, None).await
}

#[allow(clippy::future_not_send)]
async fn pump_js_runtime_shards_once_for_target(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    target_shard_index: Option<usize>,
) -> Result<bool> {
    let shard_count = shards.shards.len();
    if shard_count == 0 {
        return Ok(false);
    }

    // A target wait must not drain a peer's arbitrary hostcall queue before it
    // can re-check its own task/deadline. Pump only the target here; peers make
    // progress when they are targeted by their own command or by an explicit
    // untargeted PumpOnce round. This keeps per-extension latency independent
    // from slow or adversarial work queued in another shard.
    if let Some(shard_index) = target_shard_index {
        shards.ensure_shard_healthy(shard_index)?;
        let pending = {
            let shard = &shards.shards[shard_index];
            pump_js_runtime_once_for_owner(&shard.runtime, host, Some(&shard.extension_id)).await
        };
        return match pending {
            Ok(pending) => {
                shards.pump_cursor = (shard_index + 1) % shard_count;
                Ok(pending)
            }
            Err(err) => {
                let extension_id = shards.shards[shard_index].extension_id.clone();
                let fault = format!(
                    "JS extension shard {extension_id} quarantined after runtime pump failure: {err}"
                );
                shards.shards[shard_index].pump_fault = Some(fault.clone());
                Err(Error::extension(fault))
            }
        };
    }

    let start = shards.pump_cursor % shard_count;
    let mut has_pending = false;
    let mut first_fault = None;
    for offset in 0..shard_count {
        let shard_index = (start + offset) % shard_count;
        if shards.shards[shard_index].pump_fault.is_some() {
            continue;
        }
        let pump_result = {
            let shard = &shards.shards[shard_index];
            pump_js_runtime_once_for_owner(&shard.runtime, host, Some(shard.extension_id.as_str()))
                .await
        };
        match pump_result {
            Ok(pending) => has_pending |= pending,
            Err(err) => {
                let extension_id = shards.shards[shard_index].extension_id.clone();
                let fault = format!(
                    "JS extension shard {extension_id} quarantined after runtime pump failure: {err}"
                );
                shards.shards[shard_index].pump_fault = Some(fault.clone());
                if first_fault.is_none() {
                    first_fault = Some(fault.clone());
                }
                tracing::error!(
                    event = "extension_runtime.shards.pump_quarantined",
                    extension_id,
                    shard_index,
                    error = %fault,
                    "Quarantined a failed shard and continued pumping healthy peers"
                );
            }
        }
    }
    shards.pump_cursor = (start + 1) % shard_count;
    first_fault.map_or_else(|| Ok(has_pending), |fault| Err(Error::extension(fault)))
}

#[derive(Debug, Deserialize)]
struct JsTaskState {
    status: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    error: Option<JsTaskError>,
}

#[derive(Debug, Deserialize)]
struct JsTaskError {
    #[serde(default)]
    code: Option<String>,
    message: String,
    #[serde(default)]
    stack: Option<String>,
}

fn js_hostcall_timeout_ms(request: &HostcallRequest) -> Option<u64> {
    fn timeout_value(value: &Value) -> Option<u64> {
        value
            .get("timeout")
            .and_then(Value::as_u64)
            .or_else(|| value.get("timeoutMs").and_then(Value::as_u64))
            .or_else(|| value.get("timeout_ms").and_then(Value::as_u64))
            .filter(|ms| *ms > 0)
    }

    match request.kind {
        HostcallKind::Exec { .. } => request
            .payload
            .get("options")
            .and_then(timeout_value)
            .or_else(|| timeout_value(&request.payload)),
        HostcallKind::Http => timeout_value(&request.payload),
        _ => None,
    }
}

async fn prompt_capability_once(
    manager: &ExtensionManager,
    extension_id: &str,
    capability: &str,
) -> bool {
    let title = format!("Allow extension capability: {capability}");
    let message = format!("Extension {extension_id} requests capability '{capability}'. Allow?");
    let payload = json!({
        "title": title,
        "message": message,
        "extension_id": extension_id,
        "capability": capability,
    });
    let request = ExtensionUiRequest::new("", "confirm", payload);

    match manager.request_ui(request).await {
        Ok(Some(response)) => {
            response
                .value
                .as_ref()
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && !response.cancelled
        }
        Ok(None) | Err(_) => false,
    }
}

// NOTE: Superseded by resolve_shared_policy_prompt in dispatch_host_call_shared (bd-1uy.1.3).
#[allow(dead_code, clippy::future_not_send)]
async fn resolve_js_hostcall_policy_decision(
    host: &JsRuntimeHost,
    extension_id: Option<&str>,
    required: &str,
) -> (PolicyDecision, String, String) {
    const UNKNOWN_EXTENSION_ID: &str = "<unknown>";
    let PolicyCheck {
        mut decision,
        capability,
        mut reason,
    } = host.policy.evaluate(required);

    if decision != PolicyDecision::Prompt {
        return (decision, reason, capability);
    }

    if let Some(extension_id) = extension_id
        && let Some(allow) = host
            .manager()
            .and_then(|m| m.cached_policy_prompt_decision(extension_id, &capability))
    {
        decision = if allow {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Deny
        };
        reason = if allow {
            "prompt_cache_allow".to_string()
        } else {
            "prompt_cache_deny".to_string()
        };
        return (decision, reason, capability);
    }

    let prompt_extension_id = extension_id.unwrap_or(UNKNOWN_EXTENSION_ID);
    let Some(manager) = host.manager() else {
        return (PolicyDecision::Deny, "shutdown".to_string(), capability);
    };
    let allow = prompt_capability_once(&manager, prompt_extension_id, &capability).await;
    if let Some(extension_id) = extension_id {
        manager.cache_policy_prompt_decision(extension_id, &capability, allow);
    }
    decision = if allow {
        PolicyDecision::Allow
    } else {
        PolicyDecision::Deny
    };
    reason = if allow {
        "prompt_user_allow".to_string()
    } else {
        "prompt_user_deny".to_string()
    };
    (decision, reason, capability)
}

fn log_hostcall_start(
    runtime: &str,
    call_id: &str,
    extension_id: Option<&str>,
    required: &str,
    method: &str,
    params_hash: &str,
    call_timeout_ms: Option<u64>,
) {
    tracing::info!(
        event = "host_call.start",
        runtime = runtime,
        call_id = %call_id,
        extension_id = ?extension_id,
        capability = %required,
        method = %method,
        params_hash = %params_hash,
        timeout_ms = call_timeout_ms,
        "Hostcall start"
    );
}

fn log_policy_decision(
    runtime: &str,
    call_id: &str,
    extension_id: Option<&str>,
    capability: &str,
    decision: PolicyDecision,
    reason: &str,
    params_hash: &str,
) {
    if decision == PolicyDecision::Allow {
        tracing::info!(
            event = "policy.decision",
            runtime = runtime,
            call_id = %call_id,
            extension_id = ?extension_id,
            capability = %capability,
            decision = ?decision,
            reason = %reason,
            params_hash = %params_hash,
            "Hostcall allowed by policy"
        );
    } else {
        tracing::warn!(
            event = "policy.decision",
            runtime = runtime,
            call_id = %call_id,
            extension_id = ?extension_id,
            capability = %capability,
            decision = ?decision,
            reason = %reason,
            params_hash = %params_hash,
            "Hostcall denied by policy"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn log_hostcall_end(
    runtime: &str,
    call_id: &str,
    extension_id: Option<&str>,
    required: &str,
    method: &str,
    params_hash: &str,
    duration_ms: u64,
    lane_execution: Option<&HostcallLaneExecution>,
    marshalling: &HostcallMarshallingTelemetry,
    outcome: &HostcallOutcome,
) {
    let (is_error, error_code) = match outcome {
        HostcallOutcome::Success(_) | HostcallOutcome::StreamChunk { .. } => (false, None),
        HostcallOutcome::Error { code, .. } => (true, Some(code.as_str())),
    };
    let lane = lane_execution.map(|meta| meta.lane.as_str());
    let lane_decision_reason = lane_execution.map(|meta| meta.decision_reason.as_str());
    let lane_fallback_reason = lane_execution.and_then(|meta| meta.fallback_reason.as_deref());
    let lane_matrix_key = lane_execution.map(|meta| meta.matrix_key);
    let lane_dispatch_latency_ms = lane_execution.map_or(0, |meta| meta.dispatch_latency_ms);
    let lane_latency_share_bps = lane_dispatch_latency_ms
        .saturating_mul(10_000)
        .checked_div(duration_ms)
        .unwrap_or(0)
        .min(10_000);
    let marshalling_path = marshalling.path.as_str();
    let marshalling_latency_us = marshalling.latency_us;
    let marshalling_fallback_reason = marshalling.fallback_reason.as_deref();
    let marshalling_fallback_count = marshalling.fallback_count;
    let marshalling_rewrite_rule = marshalling.rewrite_rule.as_deref();
    let marshalling_rewrite_expected_cost_delta = marshalling.rewrite_expected_cost_delta;
    let marshalling_rewrite_observed_cost_delta = marshalling.rewrite_observed_cost_delta;
    let marshalling_rewrite_fallback_reason = marshalling.rewrite_fallback_reason.as_deref();
    let marshalling_superinstruction_trace_signature =
        marshalling.superinstruction_trace_signature.as_deref();
    let marshalling_superinstruction_plan_id = marshalling.superinstruction_plan_id.as_deref();
    let marshalling_superinstruction_expected_cost_delta =
        marshalling.superinstruction_expected_cost_delta;
    let marshalling_superinstruction_observed_cost_delta =
        marshalling.superinstruction_observed_cost_delta;
    let marshalling_superinstruction_deopt_reason =
        marshalling.superinstruction_deopt_reason.as_deref();

    if is_error {
        tracing::warn!(
            event = "host_call.end",
            runtime = runtime,
            call_id = %call_id,
            extension_id = ?extension_id,
            capability = %required,
            method = %method,
            params_hash = %params_hash,
            duration_ms,
            lane = lane,
            lane_decision_reason = lane_decision_reason,
            lane_fallback_reason = lane_fallback_reason,
            lane_matrix_key = lane_matrix_key,
            lane_dispatch_latency_ms,
            lane_latency_share_bps,
            marshalling_path = marshalling_path,
            marshalling_latency_us,
            marshalling_fallback_reason = marshalling_fallback_reason,
            marshalling_fallback_count,
            marshalling_rewrite_rule = marshalling_rewrite_rule,
            marshalling_rewrite_expected_cost_delta,
            marshalling_rewrite_observed_cost_delta,
            marshalling_rewrite_fallback_reason = marshalling_rewrite_fallback_reason,
            marshalling_superinstruction_trace_signature =
                marshalling_superinstruction_trace_signature,
            marshalling_superinstruction_plan_id = marshalling_superinstruction_plan_id,
            marshalling_superinstruction_expected_cost_delta,
            marshalling_superinstruction_observed_cost_delta,
            marshalling_superinstruction_deopt_reason =
                marshalling_superinstruction_deopt_reason,
            error_code = error_code,
            "Hostcall end (error)"
        );
    } else {
        tracing::info!(
            event = "host_call.end",
            runtime = runtime,
            call_id = %call_id,
            extension_id = ?extension_id,
            capability = %required,
            method = %method,
            params_hash = %params_hash,
            duration_ms,
            lane = lane,
            lane_decision_reason = lane_decision_reason,
            lane_fallback_reason = lane_fallback_reason,
            lane_matrix_key = lane_matrix_key,
            lane_dispatch_latency_ms,
            lane_latency_share_bps,
            marshalling_path = marshalling_path,
            marshalling_latency_us,
            marshalling_fallback_reason = marshalling_fallback_reason,
            marshalling_fallback_count,
            marshalling_rewrite_rule = marshalling_rewrite_rule,
            marshalling_rewrite_expected_cost_delta,
            marshalling_rewrite_observed_cost_delta,
            marshalling_rewrite_fallback_reason = marshalling_rewrite_fallback_reason,
            marshalling_superinstruction_trace_signature =
                marshalling_superinstruction_trace_signature,
            marshalling_superinstruction_plan_id = marshalling_superinstruction_plan_id,
            marshalling_superinstruction_expected_cost_delta,
            marshalling_superinstruction_observed_cost_delta,
            marshalling_superinstruction_deopt_reason =
                marshalling_superinstruction_deopt_reason,
            "Hostcall end (success)"
        );
    }
}

// ============================================================================
// Shared Hostcall Dispatcher (bd-1uy.1.3)
// ============================================================================

/// Dispatch a hostcall through the unified ABI surface.
///
/// This is the **single source of truth** for hostcall execution, usable by
/// JS extensions, WASM components, and protocol-based runtimes alike.
///
/// 1. Resolves the required capability from the payload.
/// 2. Evaluates policy (allow / deny / prompt).
/// 3. Routes to the appropriate type-specific handler.
/// 4. Returns a taxonomy-compliant [`HostResultPayload`].
#[allow(clippy::future_not_send)]
#[allow(clippy::too_many_lines)]
pub async fn dispatch_host_call_shared(
    ctx: &HostCallContext<'_>,
    call: HostCallPayload,
) -> HostResultPayload {
    if let Err(err) = validate_host_call(&call) {
        tracing::warn!(
            event = "host_call.validation_reject",
            runtime = ctx.runtime_name,
            call_id = %call.call_id,
            extension_id = ?ctx.extension_id,
            capability = %call.capability,
            method = %call.method,
            reason = %err,
            "Hostcall rejected during validation"
        );
        return outcome_to_host_result(
            &call.call_id,
            &HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: err.to_string(),
            },
        );
    }

    let call_id = call.call_id.as_str();
    let method = call.method.as_str();
    let opcode_hint = match resolve_hostcall_opcode(&call) {
        Ok(HostcallOpcodeResolution::FastPath { opcode, .. }) => Some(opcode),
        _ => None,
    };
    let capability = opcode_hint
        .map(CommonHostcallOpcode::required_capability)
        .or_else(|| required_capability_for_host_call_static_legacy(&call))
        .unwrap_or("internal");
    let HostcallMarshallingArtifacts {
        params_hash,
        args_shape_hash,
        mut telemetry,
    } = HostcallPayloadArena::new(method, &call.params, opcode_hint).marshal();
    if let Some(manager) = ctx.manager.as_ref() {
        telemetry.fallback_count = manager.record_hostcall_marshalling_fallback_count(
            ctx.extension_id,
            telemetry.fallback_reason.as_deref(),
        );
    }
    let marshalling_telemetry = telemetry;
    let resource_target_class = runtime_hostcall_resource_target_class(method, &call.params);
    let policy_profile = runtime_hostcall_policy_profile(ctx.policy.mode);
    let started_at = Instant::now();

    log_hostcall_start(
        ctx.runtime_name,
        call_id,
        ctx.extension_id,
        capability,
        method,
        &params_hash,
        call.timeout_ms,
    );

    // Policy check (per-extension overrides applied via extension_id).
    let policy_check = ctx.policy.evaluate_for(capability, ctx.extension_id);
    let (decision, reason) = match policy_check.decision {
        PolicyDecision::Allow => (PolicyDecision::Allow, policy_check.reason),
        PolicyDecision::Deny => (PolicyDecision::Deny, policy_check.reason),
        PolicyDecision::Prompt => {
            // Check prompt cache, then prompt the user.
            resolve_shared_policy_prompt(ctx, capability).await
        }
    };

    log_policy_decision(
        ctx.runtime_name,
        call_id,
        ctx.extension_id,
        capability,
        decision,
        &reason,
        &params_hash,
    );

    // SEC-4.1: Per-extension quota check (after policy, before risk eval).
    if decision == PolicyDecision::Allow
        && let Some(manager) = ctx.manager.as_ref()
    {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let quota_result = manager.check_quota(ctx.extension_id, capability, now_ms, ctx.policy);
        if let QuotaCheckResult::Exceeded { reason: ref qr } = quota_result {
            tracing::warn!(
                event = "ext.quota.exceeded",
                extension_id = ?ctx.extension_id,
                capability,
                method = %method,
                reason = %qr,
            );
            manager.record_budget_overload_signal(ctx.extension_id, "quota_exceeded", None, None);
            // SEC-5.1: Alert for quota breach.
            manager.record_security_alert(SecurityAlert {
                schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
                ts_ms: runtime_risk_now_ms(),
                sequence_id: 0,
                extension_id: ctx.extension_id.unwrap_or("<unknown>").to_string(),
                category: SecurityAlertCategory::QuotaBreach,
                severity: SecurityAlertSeverity::Warning,
                capability: capability.to_string(),
                method: method.to_string(),
                reason_codes: vec!["quota_exceeded".to_string()],
                summary: format!("Quota exceeded: {qr}"),
                policy_source: "quota".to_string(),
                action: SecurityAlertAction::Deny,
                remediation: "Increase quota limits or reduce extension call frequency."
                    .to_string(),
                risk_score: 0.0,
                risk_state: None,
                context_hash: params_hash.clone(),
            });
            let outcome = HostcallOutcome::Error {
                code: "quota_exceeded".to_string(),
                message: format!("Quota exceeded for extension: {qr}"),
            };
            let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            log_hostcall_end(
                ctx.runtime_name,
                call_id,
                ctx.extension_id,
                capability,
                method,
                &params_hash,
                duration_ms,
                None,
                &marshalling_telemetry,
                &outcome,
            );
            return outcome_to_host_result(call_id, &outcome);
        }
    }

    // SEC-4.1: track whether we need subprocess lifecycle recording.
    let is_exec = capability == "exec";

    let mut runtime_risk_decision = None;
    let mut lane_execution: Option<HostcallLaneExecution> = None;
    let outcome = if decision == PolicyDecision::Allow {
        if let Some(manager) = ctx.manager.as_ref() {
            runtime_risk_decision = manager.evaluate_runtime_risk(
                ctx.extension_id,
                call_id,
                capability,
                method,
                &params_hash,
                RuntimeRiskCallMetadata {
                    args_shape_hash: &args_shape_hash,
                    resource_target_class,
                    params: &call.params,
                    timeout_ms: call.timeout_ms,
                    policy_profile,
                },
                &reason,
            );
            if let Some(decision) = runtime_risk_decision.as_ref() {
                if decision.feature_budget_exceeded {
                    manager.record_budget_overload_signal(
                        ctx.extension_id,
                        "feature_extraction_budget_exceeded",
                        None,
                        None,
                    );
                }
                if decision.fallback_reason.is_some() {
                    manager.record_budget_overload_signal(
                        ctx.extension_id,
                        "runtime_risk_decision_timeout",
                        None,
                        None,
                    );
                }
            }
        }

        // SEC-7.1: In shadow mode (enabled=true, enforce=false) the risk
        // scorer runs and records telemetry but all calls are allowed through.
        let shadow_mode = ctx.manager.as_ref().is_some_and(|m| {
            let cfg = m.runtime_risk_config();
            cfg.enabled && !cfg.enforce
        });

        let will_dispatch = if shadow_mode {
            true
        } else {
            match runtime_risk_decision
                .as_ref()
                .map_or(RuntimeRiskAction::Allow, |d| d.action)
            {
                RuntimeRiskAction::Allow => true,
                RuntimeRiskAction::Harden => runtime_risk_decision
                    .as_ref()
                    .is_none_or(|decision| !runtime_risk_harden_should_block_dangerous(decision)),
                RuntimeRiskAction::Deny | RuntimeRiskAction::Terminate => false,
            }
        };

        // SEC-4.1: record subprocess spawn before exec dispatch.
        if is_exec
            && will_dispatch
            && let (Some(manager), Some(ext_id)) = (ctx.manager.as_ref(), ctx.extension_id)
        {
            manager.record_subprocess_spawn(ext_id);
        }

        let dispatched = if shadow_mode {
            // SEC-7.1: Shadow mode — score is recorded but call is always allowed.
            // Alerts are still generated with counterfactual actions for review.
            let (outcome, lane_meta) = dispatch_shared_allowed(ctx, &call).await;
            lane_execution = lane_meta;
            outcome
        } else {
            match runtime_risk_decision
                .as_ref()
                .map_or(RuntimeRiskAction::Allow, |d| d.action)
            {
                RuntimeRiskAction::Allow => {
                    let (outcome, lane_meta) = dispatch_shared_allowed(ctx, &call).await;
                    lane_execution = lane_meta;
                    outcome
                }
                RuntimeRiskAction::Harden => {
                    let should_block = runtime_risk_decision
                        .as_ref()
                        .is_some_and(runtime_risk_harden_should_block_dangerous);
                    if should_block {
                        // SEC-5.1: Alert for anomaly-based hardening denial.
                        if let Some(ref manager) = ctx.manager {
                            manager.record_security_alert(SecurityAlert {
                                schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
                                ts_ms: runtime_risk_now_ms(),
                                sequence_id: 0,
                                extension_id: ctx.extension_id.unwrap_or("<unknown>").to_string(),
                                category: SecurityAlertCategory::AnomalyDenial,
                                severity: SecurityAlertSeverity::Error,
                                capability: capability.to_string(),
                                method: method.to_string(),
                                reason_codes: runtime_risk_decision
                                    .as_ref()
                                    .map(|d| d.triggers.clone())
                                    .unwrap_or_default(),
                                summary: format!(
                                    "Dangerous capability '{capability}' denied by risk hardening"
                                ),
                                policy_source: "risk_scorer".to_string(),
                                action: SecurityAlertAction::Deny,
                                remediation:
                                    "Review extension behavior; risk scorer elevated threat level."
                                        .to_string(),
                                risk_score: runtime_risk_decision
                                    .as_ref()
                                    .map_or(0.0, |d| d.risk_score),
                                risk_state: runtime_risk_decision
                                    .as_ref()
                                    .map(|d| d.state_label.into()),
                                context_hash: params_hash.clone(),
                            });
                        }
                        HostcallOutcome::Error {
                            code: "denied".to_string(),
                            message: format!(
                                "Capability '{capability}' denied by runtime risk hardening"
                            ),
                        }
                    } else {
                        let (outcome, lane_meta) = dispatch_shared_allowed(ctx, &call).await;
                        lane_execution = lane_meta;
                        outcome
                    }
                }
                RuntimeRiskAction::Deny => {
                    // SEC-5.1: Alert for anomaly-based denial.
                    if let Some(ref manager) = ctx.manager {
                        manager.record_security_alert(SecurityAlert {
                            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
                            ts_ms: runtime_risk_now_ms(),
                            sequence_id: 0,
                            extension_id: ctx.extension_id.unwrap_or("<unknown>").to_string(),
                            category: SecurityAlertCategory::AnomalyDenial,
                            severity: SecurityAlertSeverity::Error,
                            capability: capability.to_string(),
                            method: method.to_string(),
                            reason_codes: runtime_risk_decision
                                .as_ref()
                                .map(|d| d.triggers.clone())
                                .unwrap_or_default(),
                            summary: format!(
                                "Capability '{capability}' denied by runtime risk controller"
                            ),
                            policy_source: "risk_scorer".to_string(),
                            action: SecurityAlertAction::Deny,
                            remediation: "Review extension behavior; risk scorer detected anomaly."
                                .to_string(),
                            risk_score: runtime_risk_decision
                                .as_ref()
                                .map_or(0.0, |d| d.risk_score),
                            risk_state: runtime_risk_decision
                                .as_ref()
                                .map(|d| d.state_label.into()),
                            context_hash: params_hash.clone(),
                        });
                    }
                    HostcallOutcome::Error {
                        code: "denied".to_string(),
                        message: format!(
                            "Capability '{capability}' denied by runtime risk controller"
                        ),
                    }
                }
                RuntimeRiskAction::Terminate => {
                    // SEC-5.1: Critical alert for quarantine.
                    if let Some(ref manager) = ctx.manager {
                        manager.record_security_alert(SecurityAlert {
                            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
                            ts_ms: runtime_risk_now_ms(),
                            sequence_id: 0,
                            extension_id: ctx.extension_id.unwrap_or("<unknown>").to_string(),
                            category: SecurityAlertCategory::Quarantine,
                            severity: SecurityAlertSeverity::Critical,
                            capability: capability.to_string(),
                            method: method.to_string(),
                            reason_codes: runtime_risk_decision
                                .as_ref()
                                .map(|d| d.triggers.clone())
                                .unwrap_or_default(),
                            summary: "Extension quarantined by runtime risk controller".to_string(),
                            policy_source: "risk_scorer".to_string(),
                            action: SecurityAlertAction::Terminate,
                            remediation:
                                "Extension has been quarantined. Remove or reinstall after review."
                                    .to_string(),
                            risk_score: runtime_risk_decision
                                .as_ref()
                                .map_or(0.0, |d| d.risk_score),
                            risk_state: runtime_risk_decision
                                .as_ref()
                                .map(|d| d.state_label.into()),
                            context_hash: params_hash.clone(),
                        });
                    }
                    HostcallOutcome::Error {
                        code: "denied".to_string(),
                        message: "Extension quarantined by runtime risk controller".to_string(),
                    }
                }
            }
        };

        // SEC-4.1: record subprocess exit after exec dispatch completes.
        if is_exec
            && will_dispatch
            && let (Some(manager), Some(ext_id)) = (ctx.manager.as_ref(), ctx.extension_id)
        {
            manager.record_subprocess_exit(ext_id);
        }

        dispatched
    } else {
        // SEC-5.1: Alert for static policy denial.
        if let Some(ref manager) = ctx.manager {
            manager.record_security_alert(SecurityAlert {
                schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
                ts_ms: runtime_risk_now_ms(),
                sequence_id: 0,
                extension_id: ctx.extension_id.unwrap_or("<unknown>").to_string(),
                category: SecurityAlertCategory::PolicyDenial,
                severity: SecurityAlertSeverity::Error,
                capability: capability.to_string(),
                method: method.to_string(),
                reason_codes: vec![reason.clone()],
                summary: format!("Capability '{capability}' denied by policy ({reason})"),
                policy_source: reason.clone(),
                action: SecurityAlertAction::Deny,
                remediation: format!(
                    "Grant '{capability}' in extension policy or switch to a more permissive profile."
                ),
                risk_score: 0.0,
                risk_state: None,
                context_hash: params_hash.clone(),
            });
        }
        HostcallOutcome::Error {
            code: "denied".to_string(),
            message: format!("Capability '{capability}' denied by policy ({reason})"),
        }
    };

    let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    log_hostcall_end(
        ctx.runtime_name,
        call_id,
        ctx.extension_id,
        capability,
        method,
        &params_hash,
        duration_ms,
        lane_execution.as_ref(),
        &marshalling_telemetry,
        &outcome,
    );

    let outcome_error_code = match &outcome {
        HostcallOutcome::Error { code, .. } => Some(code.as_str()),
        _ => None,
    };

    if let Some(manager) = ctx.manager.as_ref() {
        manager.record_budget_recovery_sample(ctx.extension_id, outcome_error_code.is_none());
    }

    if let (Some(manager), Some(risk_decision)) =
        (ctx.manager.as_ref(), runtime_risk_decision.as_ref())
    {
        manager.record_runtime_risk_outcome(
            ctx.extension_id,
            call_id,
            &reason,
            risk_decision,
            outcome_error_code,
            duration_ms,
            lane_execution.as_ref(),
            &marshalling_telemetry,
        );
    }

    // Replay trace recording: if the manager has replay enabled, record this dispatch.
    if let Some(manager) = ctx.manager.as_ref()
        && let Some(replay_config) = manager.replay_config()
    {
        let ext_id = ctx.extension_id.unwrap_or("unknown");
        let trace_id = format!("hc-{call_id}");
        let mut recorder = crate::extension_replay::ReplayRecorder::new(trace_id, replay_config);

        recorder.tick();
        recorder.record_scheduled(
            ext_id,
            call_id,
            replay_scheduled_attributes(
                &call,
                capability,
                method,
                &params_hash,
                &args_shape_hash,
                resource_target_class,
                policy_profile,
            ),
        );
        recorder.tick();
        recorder.record_queue_accepted(
            ext_id,
            call_id,
            replay_queue_attributes(lane_execution.as_ref(), manager),
        );
        recorder.tick();

        recorder.record_policy_decision(
            ext_id,
            call_id,
            replay_policy_attributes(decision, &reason, runtime_risk_decision.as_ref()),
        );
        recorder.tick();

        let outcome_kind = if outcome_error_code.is_some() {
            crate::extension_replay::ReplayEventKind::Failed
        } else {
            crate::extension_replay::ReplayEventKind::Completed
        };
        recorder.record(
            ext_id,
            call_id,
            outcome_kind,
            replay_outcome_attributes(&outcome, duration_ms, resource_target_class),
        );

        let observed_micros = u64::try_from(started_at.elapsed().as_micros())
            .unwrap_or(u64::MAX)
            .max(1);
        let observation = crate::extension_replay::ReplayCaptureObservation {
            baseline_micros: observed_micros,
            captured_micros: observed_micros,
            trace_bytes: 0,
        };
        if let Ok(result) = recorder.finish(observation)
            && result.gate_report.capture_allowed
        {
            manager.store_replay_bundle(result.bundle);
        }
    }

    outcome_to_host_result(call_id, &outcome)
}

const REPLAY_CONTEXT_INDEX_KEYS: &[(&str, &str)] = &[
    ("transcript_index", "transcript_index"),
    ("transcriptIndex", "transcript_index"),
    ("turn_index", "turn_index"),
    ("turnIndex", "turn_index"),
    ("message_index", "message_index"),
    ("messageIndex", "message_index"),
    ("event_index", "event_index"),
    ("eventIndex", "event_index"),
    ("session_entry_index", "session_entry_index"),
    ("sessionEntryIndex", "session_entry_index"),
];

fn replay_scheduled_attributes(
    call: &HostCallPayload,
    capability: &str,
    method: &str,
    params_hash: &str,
    args_shape_hash: &str,
    resource_target_class: &str,
    policy_profile: &str,
) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::from([
        ("capability".to_string(), capability.to_string()),
        ("method".to_string(), method.to_string()),
        ("params_hash".to_string(), params_hash.to_string()),
        ("args_shape_hash".to_string(), args_shape_hash.to_string()),
        (
            "resource_target_class".to_string(),
            resource_target_class.to_string(),
        ),
        ("policy_profile".to_string(), policy_profile.to_string()),
        (
            "cancel_token_present".to_string(),
            call.cancel_token.is_some().to_string(),
        ),
    ]);
    if let Some(timeout_ms) = call.timeout_ms {
        attrs.insert("timeout_ms".to_string(), timeout_ms.to_string());
    }
    insert_replay_context_indexes(&mut attrs, call.context.as_ref());
    attrs
}

fn replay_queue_attributes(
    lane_execution: Option<&HostcallLaneExecution>,
    manager: &ExtensionManager,
) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    match lane_execution {
        Some(meta) => {
            attrs.insert("dispatch_status".to_string(), "accepted".to_string());
            attrs.insert("selected_lane".to_string(), meta.lane.as_str().to_string());
            attrs.insert(
                "lane_decision_reason".to_string(),
                meta.decision_reason.clone(),
            );
            attrs.insert("lane_matrix_key".to_string(), meta.matrix_key.to_string());
            attrs.insert(
                "lane_dispatch_latency_ms".to_string(),
                meta.dispatch_latency_ms.to_string(),
            );
            if let Some(reason) = &meta.fallback_reason {
                attrs.insert("lane_fallback_reason".to_string(), reason.clone());
                attrs.insert("backpressure_decision".to_string(), reason.clone());
            }
        }
        None => {
            attrs.insert(
                "dispatch_status".to_string(),
                "blocked_before_lane".to_string(),
            );
        }
    }

    if let Some(telemetry) = manager.reactor_telemetry() {
        attrs.insert(
            "reactor_shard_count".to_string(),
            telemetry.shard_count.to_string(),
        );
        attrs.insert(
            "reactor_lane_capacity".to_string(),
            telemetry.lane_capacity.to_string(),
        );
        attrs.insert(
            "reactor_queue_depth_current_max".to_string(),
            telemetry
                .queue_depths
                .iter()
                .copied()
                .max()
                .unwrap_or_default()
                .to_string(),
        );
        attrs.insert(
            "reactor_queue_depth_observed_max".to_string(),
            telemetry
                .max_queue_depths
                .iter()
                .copied()
                .max()
                .unwrap_or_default()
                .to_string(),
        );
        attrs.insert(
            "reactor_rejected_enqueues".to_string(),
            telemetry.rejected_enqueues.to_string(),
        );
        attrs.insert(
            "reactor_total_dispatched".to_string(),
            telemetry.total_dispatched.to_string(),
        );
        attrs.insert(
            "reactor_overloaded".to_string(),
            telemetry.overloaded.to_string(),
        );
        if let Some(reason) = telemetry.overload_reason {
            attrs.insert("reactor_overload_reason".to_string(), reason);
        }
    }

    attrs
}

fn replay_policy_attributes(
    decision: PolicyDecision,
    reason: &str,
    runtime_risk_decision: Option<&RuntimeRiskDecision>,
) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::from([
        ("decision".to_string(), format!("{decision:?}")),
        ("reason".to_string(), reason.to_string()),
    ]);
    if let Some(risk) = runtime_risk_decision {
        attrs.insert(
            "runtime_risk_action".to_string(),
            format!("{:?}", risk.action),
        );
        attrs.insert("runtime_risk_reason".to_string(), risk.reason.clone());
        attrs.insert(
            "runtime_risk_score".to_string(),
            format!("{:.6}", risk.risk_score),
        );
        attrs.insert(
            "runtime_risk_state".to_string(),
            format!("{:?}", risk.state_label),
        );
        attrs.insert(
            "runtime_risk_drift_detected".to_string(),
            risk.drift_detected.to_string(),
        );
        attrs.insert("runtime_risk_triggers".to_string(), risk.triggers.join(","));
        if let Some(fallback_reason) = &risk.fallback_reason {
            attrs.insert(
                "runtime_risk_fallback_reason".to_string(),
                fallback_reason.clone(),
            );
        }
    }
    attrs
}

fn replay_outcome_attributes(
    outcome: &HostcallOutcome,
    duration_ms: u64,
    resource_target_class: &str,
) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::from([
        ("duration_ms".to_string(), duration_ms.to_string()),
        (
            "resource_target_class".to_string(),
            resource_target_class.to_string(),
        ),
    ]);
    match outcome {
        HostcallOutcome::Success(_) => {
            attrs.insert("outcome_kind".to_string(), "success".to_string());
            attrs.insert("is_error".to_string(), "false".to_string());
        }
        HostcallOutcome::StreamChunk {
            sequence, is_final, ..
        } => {
            attrs.insert("outcome_kind".to_string(), "stream_chunk".to_string());
            attrs.insert("is_error".to_string(), "false".to_string());
            attrs.insert("stream_sequence".to_string(), sequence.to_string());
            attrs.insert("stream_is_final".to_string(), is_final.to_string());
        }
        HostcallOutcome::Error { code, .. } => {
            attrs.insert("outcome_kind".to_string(), "error".to_string());
            attrs.insert("is_error".to_string(), "true".to_string());
            attrs.insert("error_code".to_string(), code.clone());
        }
    }
    attrs
}

fn insert_replay_context_indexes(attrs: &mut BTreeMap<String, String>, context: Option<&Value>) {
    let Some(context) = context.and_then(Value::as_object) else {
        return;
    };
    for (source_key, target_key) in REPLAY_CONTEXT_INDEX_KEYS {
        if let Some(value) = context
            .get(*source_key)
            .and_then(replay_context_index_value)
        {
            attrs.insert((*target_key).to_string(), value);
        }
    }
}

fn replay_context_index_value(value: &Value) -> Option<String> {
    if let Some(value) = value.as_u64() {
        return Some(value.to_string());
    }
    if let Some(value) = value.as_i64() {
        return Some(value.to_string());
    }
    value.as_str().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.len() <= 64
            && !trimmed.is_empty()
            && trimmed
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
        {
            Some(trimmed.to_string())
        } else {
            None
        }
    })
}

// ============================================================================
// Protocol Adapter: ExtensionMessage host_call -> host_result (bd-1uy.1.2)
// ============================================================================

/// Handle an incoming [`ExtensionMessage`] of type `host_call` by dispatching
/// through the shared hostcall ABI and returning `host_result` messages.
///
/// This is a thin wrapper around [`dispatch_host_call_shared`] with no bespoke
/// policy, timeout, or logging logic.
///
/// Returns a `Vec<ExtensionMessage>` for streaming-readiness: the initial
/// implementation always returns exactly one message.
///
/// If the message is not a `host_call`, or fails validation, a single
/// `host_result` with `invalid_request` is returned (never panics).
#[allow(clippy::future_not_send)]
pub async fn handle_extension_message(
    ctx: &HostCallContext<'_>,
    msg: ExtensionMessage,
) -> Vec<ExtensionMessage> {
    // Validate the incoming message.
    if let Err(err) = msg.validate() {
        let call_id = match &msg.body {
            ExtensionBody::HostCall(payload) => payload.call_id.clone(),
            _ => String::new(),
        };
        return vec![make_host_result_message(
            &call_id,
            HostResultPayload {
                call_id: call_id.clone(),
                output: json!({}),
                is_error: true,
                error: Some(HostCallError {
                    code: HostCallErrorCode::InvalidRequest,
                    message: format!("Message validation failed: {err}"),
                    details: None,
                    retryable: None,
                }),
                chunk: None,
            },
        )];
    }

    // Extract the `HostCallPayload`.
    let payload = match msg.body {
        ExtensionBody::HostCall(payload) => payload,
        other => {
            let type_name = extension_body_type_name(&other);
            return vec![make_host_result_message(
                "",
                HostResultPayload {
                    call_id: String::new(),
                    output: json!({}),
                    is_error: true,
                    error: Some(HostCallError {
                        code: HostCallErrorCode::InvalidRequest,
                        message: format!(
                            "handle_extension_message expects host_call, got {type_name}"
                        ),
                        details: None,
                        retryable: None,
                    }),
                    chunk: None,
                },
            )];
        }
    };

    let call_id = payload.call_id.clone();

    // Dispatch through the shared ABI surface.
    let result = dispatch_host_call_shared(ctx, payload).await;

    vec![make_host_result_message(&call_id, result)]
}

/// Build an [`ExtensionMessage`] wrapping a [`HostResultPayload`].
fn make_host_result_message(call_id: &str, result: HostResultPayload) -> ExtensionMessage {
    ExtensionMessage {
        id: format!("host_result:{call_id}"),
        version: PROTOCOL_VERSION.to_string(),
        body: ExtensionBody::HostResult(result),
    }
}

/// Return the serde tag name for an [`ExtensionBody`] variant.
const fn extension_body_type_name(body: &ExtensionBody) -> &'static str {
    match body {
        ExtensionBody::Register(_) => "register",
        ExtensionBody::ToolCall(_) => "tool_call",
        ExtensionBody::ToolResult(_) => "tool_result",
        ExtensionBody::SlashCommand(_) => "slash_command",
        ExtensionBody::SlashResult(_) => "slash_result",
        ExtensionBody::EventHook(_) => "event_hook",
        ExtensionBody::HostCall(_) => "host_call",
        ExtensionBody::HostResult(_) => "host_result",
        ExtensionBody::Log(_) => "log",
        ExtensionBody::Error(_) => "error",
    }
}

/// Resolve a policy `Prompt` decision using the extension manager cache + UI.
#[allow(clippy::future_not_send)]
async fn resolve_shared_policy_prompt(
    ctx: &HostCallContext<'_>,
    capability: &str,
) -> (PolicyDecision, String) {
    // Check prompt cache.
    if let Some(ext_id) = ctx.extension_id
        && let Some(allow) = ctx
            .manager
            .as_ref()
            .and_then(|m| m.cached_policy_prompt_decision(ext_id, capability))
    {
        let decision = if allow {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Deny
        };
        let reason = if allow {
            "prompt_cache_allow"
        } else {
            "prompt_cache_deny"
        };
        return (decision, reason.to_string());
    }

    // Prompt the user via UI.
    let Some(ref manager) = ctx.manager else {
        return (PolicyDecision::Deny, "shutdown".to_string());
    };

    let prompt_ext_id = ctx.extension_id.unwrap_or("<unknown>");
    let allow = prompt_capability_once(manager, prompt_ext_id, capability).await;

    if let Some(ext_id) = ctx.extension_id {
        manager.cache_policy_prompt_decision(ext_id, capability, allow);
    }

    let decision = if allow {
        PolicyDecision::Allow
    } else {
        PolicyDecision::Deny
    };
    let reason = if allow {
        "prompt_user_allow"
    } else {
        "prompt_user_deny"
    };
    (decision, reason.to_string())
}

/// Route an allowed hostcall to the appropriate handler based on method.
///
/// Converts the canonical [`HostCallPayload`] params back into the format
/// expected by the type-specific dispatch functions.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn dispatch_shared_allowed(
    ctx: &HostCallContext<'_>,
    call: &HostCallPayload,
) -> (HostcallOutcome, Option<HostcallLaneExecution>) {
    let lane = match select_hostcall_lane(call) {
        Ok(lane) => lane,
        Err(err) => {
            tracing::warn!(
                event = "host_call.opcode.reject",
                call_id = %call.call_id,
                extension_id = ?ctx.extension_id,
                method = %call.method,
                reason = %err,
                "Rejecting hostcall due to invalid typed opcode metadata"
            );
            return (
                HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: err.to_string(),
                },
                Some(HostcallLaneExecution {
                    lane: HostcallDispatchLane::Compat,
                    decision_reason: "typed_opcode_validation_error".to_string(),
                    fallback_reason: Some("typed_opcode_validation_error".to_string()),
                    matrix_key: "unknown|fallback|unknown",
                    dispatch_latency_ms: 0,
                }),
            );
        }
    };
    let lane = apply_hostcall_lane_kill_switch(ctx, call, lane);
    let fallback_reason = (lane.lane == HostcallDispatchLane::Compat).then_some(lane.reason);

    tracing::debug!(
        event = "host_call.lane_decision",
        call_id = %call.call_id,
        extension_id = ?ctx.extension_id,
        method = %call.method,
        lane = lane.lane.as_str(),
        decision_reason = lane.reason,
        lane_matrix_key = lane.matrix_key,
        lane_matrix_method = lane.opcode.map_or("fallback", CommonHostcallOpcode::method),
        capability_class = lane.capability_class,
        opcode = lane.opcode.map(CommonHostcallOpcode::code),
        fallback_reason,
        opcode_schema = HOSTCALL_OPCODE_SCHEMA_VERSION,
        opcode_version = HOSTCALL_OPCODE_VERSION,
        "Selected hostcall dispatch lane"
    );

    let dispatch_started_at = Instant::now();
    let outcome = match lane.lane {
        HostcallDispatchLane::Fast => {
            let Some(opcode) = lane.opcode else {
                tracing::warn!(
                    event = "host_call.lane_invalid_state",
                    call_id = %call.call_id,
                    extension_id = ?ctx.extension_id,
                    method = %call.method,
                    "Fast lane selected without opcode; rejecting call"
                );
                return (
                    HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "Invalid hostcall lane state: fast lane requires opcode"
                            .to_string(),
                    },
                    Some(HostcallLaneExecution {
                        lane: HostcallDispatchLane::Fast,
                        decision_reason: "invalid_lane_state".to_string(),
                        fallback_reason: None,
                        matrix_key: lane.matrix_key,
                        dispatch_latency_ms: 0,
                    }),
                );
            };
            // Record reactor mesh routing for shard telemetry (bd-3ar8v.4.20).
            // The reactor mesh assigns a shard for this opcode and uses completions
            // to keep queue-depth and dispatch-latency telemetry tied to real work.
            let mut reactor_completion: Option<(usize, u64)> = None;
            if let Some(ref manager) = ctx.manager {
                match manager.reactor_submit(
                    call.call_id.clone(),
                    opcode,
                    params_without_key(&call.params, "op"),
                ) {
                    Some(Ok(reactor_req)) => {
                        tracing::trace!(
                            event = "host_call.reactor_routed",
                            call_id = %call.call_id,
                            shard_id = reactor_req.shard_id,
                            global_seq = reactor_req.global_seq,
                            shard_seq = reactor_req.shard_seq,
                            opcode = opcode.code(),
                            "Hostcall routed through reactor mesh"
                        );
                        reactor_completion = Some((reactor_req.shard_id, reactor_req.global_seq));
                    }
                    Some(Err(backpressure)) => {
                        tracing::warn!(
                            event = "host_call.reactor_backpressure",
                            call_id = %call.call_id,
                            shard_id = backpressure.shard_id,
                            queue_depth = backpressure.depth,
                            queue_capacity = backpressure.capacity,
                            opcode = opcode.code(),
                            stall_reason = "lane_overflow",
                            "Hostcall reactor lane saturated; dispatching through conservative compat lane"
                        );
                        manager.record_budget_overload_signal(
                            ctx.extension_id,
                            "reactor_lane_overflow",
                            Some(backpressure.depth),
                            Some(backpressure.capacity),
                        );
                        let outcome = dispatch_shared_allowed_legacy(ctx, call).await;
                        let dispatch_latency_ms =
                            u64::try_from(dispatch_started_at.elapsed().as_millis())
                                .unwrap_or(u64::MAX);
                        return (
                            outcome,
                            Some(HostcallLaneExecution {
                                lane: HostcallDispatchLane::Compat,
                                decision_reason: "reactor_lane_overflow".to_string(),
                                fallback_reason: Some("reactor_lane_overflow".to_string()),
                                matrix_key: lane.matrix_key,
                                dispatch_latency_ms,
                            }),
                        );
                    }
                    None => {}
                }
            }
            let outcome = dispatch_shared_allowed_fast(ctx, call, opcode).await;
            if let (Some(manager), Some((shard_id, global_seq))) =
                (ctx.manager.as_ref(), reactor_completion.as_ref())
            {
                manager.reactor_record_completion(*shard_id, *global_seq);
            }
            outcome
        }
        HostcallDispatchLane::Compat => dispatch_shared_allowed_legacy(ctx, call).await,
    };

    let dispatch_latency_ms =
        u64::try_from(dispatch_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    (
        outcome,
        Some(HostcallLaneExecution {
            lane: lane.lane,
            decision_reason: lane.reason.to_string(),
            fallback_reason: fallback_reason.map(ToString::to_string),
            matrix_key: lane.matrix_key,
            dispatch_latency_ms,
        }),
    )
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn dispatch_hostcall_session_fast_ref(
    manager: &ExtensionManager,
    op: &str,
    params: &Value,
) -> HostcallOutcome {
    let Some(session) = manager.session_handle() else {
        return HostcallOutcome::Error {
            code: "denied".to_string(),
            message: "No session configured".to_string(),
        };
    };

    let result = match parse_session_opcode_atom(op) {
        Some(CommonHostcallOpcode::SessionGetState) => Ok(session.get_state().await),
        Some(CommonHostcallOpcode::SessionGetMessages) => {
            serde_json::to_value(session.get_messages().await)
                .map_err(|err| Error::extension(format!("Serialize messages: {err}")))
        }
        Some(CommonHostcallOpcode::SessionGetEntries) => {
            serde_json::to_value(session.get_entries().await)
                .map_err(|err| Error::extension(format!("Serialize entries: {err}")))
        }
        Some(CommonHostcallOpcode::SessionGetBranch) => {
            serde_json::to_value(session.get_branch().await)
                .map_err(|err| Error::extension(format!("Serialize branch: {err}")))
        }
        Some(CommonHostcallOpcode::SessionGetFile) => {
            let state = session.get_state().await;
            let file = state
                .get("sessionFile")
                .or_else(|| state.get("session_file"))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(file)
        }
        Some(CommonHostcallOpcode::SessionGetName) => {
            let state = session.get_state().await;
            let name = state
                .get("sessionName")
                .or_else(|| state.get("session_name"))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(name)
        }
        Some(CommonHostcallOpcode::SessionSetName) => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            session.set_name(name).await.map(|()| Value::Null)
        }
        Some(CommonHostcallOpcode::SessionSetModel) => {
            let provider = params
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let model_id = params
                .get("modelId")
                .and_then(Value::as_str)
                .or_else(|| params.get("model_id").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            if provider.is_empty() || model_id.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "setModel: provider and modelId are required".to_string(),
                };
            }
            session
                .set_model(provider, model_id)
                .await
                .map(|()| Value::Bool(true))
        }
        Some(CommonHostcallOpcode::SessionGetModel) => {
            let (provider, model_id) = session.get_model().await;
            Ok(serde_json::json!({
                "provider": provider,
                "modelId": model_id,
            }))
        }
        Some(CommonHostcallOpcode::SessionSetThinkingLevel) => {
            let level = params
                .get("level")
                .and_then(Value::as_str)
                .or_else(|| params.get("thinkingLevel").and_then(Value::as_str))
                .or_else(|| params.get("thinking_level").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            if level.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "setThinkingLevel: level is required".to_string(),
                };
            }
            session
                .set_thinking_level(level)
                .await
                .map(|()| Value::Null)
        }
        Some(CommonHostcallOpcode::SessionGetThinkingLevel) => {
            let level = session.get_thinking_level().await;
            Ok(level.map_or(Value::Null, Value::String))
        }
        Some(CommonHostcallOpcode::SessionSetLabel) => {
            let target_id = params
                .get("targetId")
                .and_then(Value::as_str)
                .or_else(|| params.get("target_id").and_then(Value::as_str))
                .or_else(|| params.get("entryId").and_then(Value::as_str))
                .or_else(|| params.get("entry_id").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            if target_id.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "setLabel: targetId is required".to_string(),
                };
            }
            let label = params
                .get("label")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            session
                .set_label(target_id, label)
                .await
                .map(|()| Value::Null)
        }
        Some(_) | None => Err(Error::validation(format!("Unknown session op: {op}"))),
    };

    match result {
        Ok(value) => HostcallOutcome::Success(value),
        Err(err) => HostcallOutcome::Error {
            code: err.hostcall_error_code().to_string(),
            message: err.to_string(),
        },
    }
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn dispatch_shared_allowed_fast(
    ctx: &HostCallContext<'_>,
    call: &HostCallPayload,
    opcode: CommonHostcallOpcode,
) -> HostcallOutcome {
    match opcode {
        CommonHostcallOpcode::ToolRead => {
            let input = call.params.get("input").cloned().unwrap_or(Value::Null);
            dispatch_hostcall_tool(ctx.tools, &call.call_id, "read", input).await
        }
        CommonHostcallOpcode::ToolWrite => {
            let input = call.params.get("input").cloned().unwrap_or(Value::Null);
            dispatch_hostcall_tool(ctx.tools, &call.call_id, "write", input).await
        }
        CommonHostcallOpcode::ToolEdit => {
            let input = call.params.get("input").cloned().unwrap_or(Value::Null);
            dispatch_hostcall_tool(ctx.tools, &call.call_id, "edit", input).await
        }
        CommonHostcallOpcode::ToolBash => {
            let input = call.params.get("input").cloned().unwrap_or(Value::Null);
            dispatch_hostcall_tool(ctx.tools, &call.call_id, "bash", input).await
        }
        CommonHostcallOpcode::SessionGetName => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "get_name", &call.params).await
        }
        CommonHostcallOpcode::SessionSetName => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "set_name", &call.params).await
        }
        CommonHostcallOpcode::SessionGetModel => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "get_model", &call.params).await
        }
        CommonHostcallOpcode::SessionSetModel => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "set_model", &call.params).await
        }
        CommonHostcallOpcode::SessionGetThinkingLevel => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "get_thinking_level", &call.params).await
        }
        CommonHostcallOpcode::SessionSetThinkingLevel => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "set_thinking_level", &call.params).await
        }
        CommonHostcallOpcode::SessionSetLabel => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "set_label", &call.params).await
        }
        CommonHostcallOpcode::EventsGetActiveTools => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "get_active_tools",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsGetAllTools => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "get_all_tools",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsSetActiveTools => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "set_active_tools",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsEmit => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "emit",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsList => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "list",
                &call.params,
            )
            .await
        }
        // --- New fast-lane session getters (bd-3ar8v.4.12) ---
        CommonHostcallOpcode::SessionGetState => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "get_state", &call.params).await
        }
        CommonHostcallOpcode::SessionGetMessages => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "get_messages", &call.params).await
        }
        CommonHostcallOpcode::SessionGetEntries => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "get_entries", &call.params).await
        }
        CommonHostcallOpcode::SessionGetBranch => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "get_branch", &call.params).await
        }
        CommonHostcallOpcode::SessionGetFile => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_fast_ref(manager, "get_file", &call.params).await
        }
        // --- New fast-lane events operations (bd-3ar8v.4.12) ---
        CommonHostcallOpcode::EventsGetModel => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "get_model",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsSetModel => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "set_model",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsGetThinkingLevel => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "get_thinking_level",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsSetThinkingLevel => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "set_thinking_level",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsGetFlag => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "get_flag",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsListFlags => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "list_flags",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsAppendEntry => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "append_entry",
                &call.params,
            )
            .await
        }
        CommonHostcallOpcode::EventsRegisterCommand => {
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                "register_command",
                &call.params,
            )
            .await
        }
    }
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn dispatch_shared_allowed_legacy(
    ctx: &HostCallContext<'_>,
    call: &HostCallPayload,
) -> HostcallOutcome {
    match call.method.as_str() {
        "tool" => {
            let name = call
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let input = call.params.get("input").cloned().unwrap_or(Value::Null);
            dispatch_hostcall_tool(ctx.tools, &call.call_id, name, input).await
        }
        "exec" => {
            let cmd = call
                .params
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Extract args for mediation classification.
            // IMPORTANT: Must use the same normalization as dispatch_hostcall_exec
            // (which converts non-string values via to_string) to prevent bypass
            // by passing dangerous arguments as non-string JSON types.
            let args: Vec<String> = call
                .params
                .get("args")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|v| {
                            v.as_str()
                                .map_or_else(|| v.to_string(), ToString::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();

            // SEC-4.3: Exec mediation — classify and gate dangerous commands.
            let mediation = evaluate_exec_mediation(&ctx.policy.exec_mediation, cmd, &args);

            // Record mediation decision in the SEC-4.3 ledger.
            let (decision_label, class_label, tier_label) = match &mediation {
                ExecMediationResult::Allow => ("allow", None, None),
                ExecMediationResult::AllowWithAudit { class, .. } => (
                    "allow_with_audit",
                    Some(class.label()),
                    Some(class.risk_tier().label()),
                ),
                ExecMediationResult::Deny { class, .. } => (
                    "deny",
                    class.map(DangerousCommandClass::label),
                    class.map(|c| c.risk_tier().label()),
                ),
            };
            let reason_text = match &mediation {
                ExecMediationResult::Allow => String::new(),
                ExecMediationResult::AllowWithAudit { reason, .. }
                | ExecMediationResult::Deny { reason, .. } => reason.clone(),
            };
            if let Some(ref manager) = ctx.manager {
                let redacted = redact_command_for_logging(&ctx.policy.secret_broker, cmd);
                manager.record_exec_mediation(ExecMediationLedgerEntry {
                    ts_ms: runtime_risk_now_ms(),
                    extension_id: ctx.extension_id.map(ToString::to_string),
                    command_hash: sha256_hex_standalone(&redacted),
                    command_class: class_label.map(ToString::to_string),
                    risk_tier: tier_label.map(ToString::to_string),
                    decision: decision_label.to_string(),
                    reason: reason_text,
                });
            }

            match &mediation {
                ExecMediationResult::Deny { class, reason } => {
                    tracing::warn!(
                        event = "exec.mediation.deny",
                        extension_id = ?ctx.extension_id,
                        command_class = ?class.map(DangerousCommandClass::label),
                        reason = %reason,
                        "Exec command denied by mediation policy"
                    );
                    // SEC-5.1: Emit security alert for exec mediation denial.
                    if let Some(ref manager) = ctx.manager {
                        let redacted = redact_command_for_logging(&ctx.policy.secret_broker, cmd);
                        manager.record_security_alert(SecurityAlert {
                            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
                            ts_ms: runtime_risk_now_ms(),
                            sequence_id: 0, // filled by record_security_alert
                            extension_id: ctx.extension_id.unwrap_or("<unknown>").to_string(),
                            category: SecurityAlertCategory::ExecMediation,
                            severity: SecurityAlertSeverity::Error,
                            capability: "exec".to_string(),
                            method: "spawn".to_string(),
                            reason_codes: class
                                .map(|c| vec![c.label().to_string()])
                                .unwrap_or_default(),
                            summary: format!("Exec denied: {reason}"),
                            policy_source: "exec_mediation".to_string(),
                            action: SecurityAlertAction::Deny,
                            remediation:
                                "Review the command and adjust exec mediation policy if intended."
                                    .to_string(),
                            risk_score: 0.0,
                            risk_state: None,
                            context_hash: sha256_hex_standalone(&redacted),
                        });
                    }
                    return HostcallOutcome::Error {
                        code: "denied".to_string(),
                        message: format!("Exec denied by mediation policy: {reason}"),
                    };
                }
                ExecMediationResult::AllowWithAudit { class, reason } => {
                    tracing::info!(
                        event = "exec.mediation.audit",
                        extension_id = ?ctx.extension_id,
                        command_class = class.label(),
                        reason = %reason,
                        "Exec command allowed with audit"
                    );
                    // SEC-5.1: Emit informational alert for audited exec.
                    if let Some(ref manager) = ctx.manager {
                        let redacted = redact_command_for_logging(&ctx.policy.secret_broker, cmd);
                        manager.record_security_alert(SecurityAlert {
                            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
                            ts_ms: runtime_risk_now_ms(),
                            sequence_id: 0,
                            extension_id: ctx.extension_id.unwrap_or("<unknown>").to_string(),
                            category: SecurityAlertCategory::ExecMediation,
                            severity: SecurityAlertSeverity::Info,
                            capability: "exec".to_string(),
                            method: "spawn".to_string(),
                            reason_codes: vec![class.label().to_string()],
                            summary: format!("Exec audited: {reason}"),
                            policy_source: "exec_mediation".to_string(),
                            action: SecurityAlertAction::Harden,
                            remediation: String::new(),
                            risk_score: 0.0,
                            risk_state: None,
                            context_hash: sha256_hex_standalone(&redacted),
                        });
                    }
                }
                ExecMediationResult::Allow => {}
            }

            dispatch_hostcall_exec_ref(ctx.js_runtime, &call.call_id, cmd, &call.params).await
        }
        "http" => dispatch_hostcall_http(&call.call_id, ctx.http, call.params.clone()).await,
        "session" => {
            let op = call
                .params
                .get("op")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let Some(op) = op else {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "host_call session requires non-empty params.op".to_string(),
                };
            };
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_session_ref(&call.call_id, manager, op, &call.params).await
        }
        "ui" => {
            let op = call
                .params
                .get("op")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let Some(op) = op else {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "host_call ui requires non-empty params.op".to_string(),
                };
            };
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_ui_ref(&call.call_id, manager, op, &call.params, ctx.extension_id)
                .await
        }
        "events" => {
            let op = call
                .params
                .get("op")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let Some(op) = op else {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "host_call events requires non-empty params.op".to_string(),
                };
            };
            let Some(ref manager) = ctx.manager else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Extension manager is shutting down".to_string(),
                };
            };
            dispatch_hostcall_events_ref(
                &call.call_id,
                manager,
                ctx.tools,
                ctx.extension_id,
                op,
                &call.params,
            )
            .await
        }
        "log" => dispatch_hostcall_log(&call.call_id, ctx.extension_id, call.params.clone()).await,
        "env" => dispatch_hostcall_env(ctx, call.params.clone()).await,
        _ => HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: format!("Unsupported hostcall method: {}", call.method),
        },
    }
}

#[allow(clippy::future_not_send)]
#[allow(clippy::unused_async)]
async fn dispatch_hostcall_env(ctx: &HostCallContext<'_>, params: Value) -> HostcallOutcome {
    let mut names = Vec::new();

    if let Some(name) = params.get("name").and_then(Value::as_str) {
        let name = name.trim();
        if !name.is_empty() {
            names.push(name.to_string());
        }
    } else if let Some(items) = params.get("names").and_then(Value::as_array) {
        for item in items {
            if let Some(name) = item.as_str() {
                let name = name.trim();
                if !name.is_empty() {
                    names.push(name.to_string());
                }
            }
        }
    }

    if names.is_empty() {
        return HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: "Missing env var name(s)".to_string(),
        };
    }

    // In shared dispatcher, we don't have a per-extension env allowlist yet.
    // We rely on the "env" capability grant (already checked by policy before this function)
    // and the SecretBrokerPolicy.

    let mut values = serde_json::Map::new();
    let broker = &ctx.policy.secret_broker;

    for name in names {
        match std::env::var_os(&name) {
            None => {
                values.insert(name, Value::Null);
            }
            Some(value) => match value.into_string() {
                Ok(val_str) => {
                    // SEC-4.3: Apply secret broker redaction.
                    let final_value = broker.maybe_redact(&name, &val_str);
                    if final_value != val_str {
                        tracing::info!(
                            event = "secret_broker.redact",
                            name = %name,
                            "Secret broker redacted env var value"
                        );
                    }
                    values.insert(name, Value::String(final_value.to_string()));
                }
                Err(_) => {
                    return HostcallOutcome::Error {
                        code: "io".to_string(),
                        message: "Env var value is not valid UTF-8".to_string(),
                    };
                }
            },
        }
    }

    HostcallOutcome::Success(json!({ "values": Value::Object(values) }))
}

#[allow(clippy::future_not_send)]
#[allow(dead_code)]
async fn dispatch_hostcall(host: &JsRuntimeHost, request: HostcallRequest) -> HostcallOutcome {
    dispatch_hostcall_with_runtime(None, host, request).await
}

/// Dispatch a JS hostcall through the shared ABI surface (bd-1uy.1.3).
///
/// All JS-origin hostcalls now route through [`dispatch_host_call_shared`],
/// which enforces the canonical [`HostCallPayload`] representation,
/// taxonomy-only error codes, and deterministic params hashing.
///
/// The test interceptor is checked *before* entering the shared path since
/// it operates on the JS-specific [`HostcallRequest`] type.
#[allow(clippy::future_not_send)]
async fn dispatch_hostcall_with_runtime(
    runtime: Option<&PiJsRuntime>,
    host: &JsRuntimeHost,
    request: HostcallRequest,
) -> HostcallOutcome {
    // Test interceptor check (short-circuits before the shared ABI path).
    if let Some(ref interceptor) = host.interceptor
        && let Some(outcome) = interceptor.intercept(&request)
    {
        return outcome;
    }

    // Convert JS request to canonical payload.
    let canonical = hostcall_request_to_payload(&request);

    // Build the shared dispatch context from the JsRuntimeHost.
    let ctx = HostCallContext {
        runtime_name: "js",
        extension_id: request.extension_id.as_deref(),
        tools: &host.tools,
        http: &host.http,
        manager: host.manager(),
        policy: &host.policy,
        js_runtime: runtime,
        interceptor: None, // already checked above
    };

    // Dispatch through the shared ABI and convert back to JS outcome.
    let result = dispatch_host_call_shared(&ctx, canonical).await;
    host_result_to_outcome(result)
}

#[allow(clippy::future_not_send)]
async fn dispatch_hostcall_tool(
    tools: &ToolRegistry,
    call_id: &str,
    name: &str,
    payload: Value,
) -> HostcallOutcome {
    let Some(tool) = tools.get(name) else {
        return HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: format!("Unknown tool: {name}"),
        };
    };

    match tool.execute(call_id, payload, None).await {
        Ok(output) => match serde_json::to_value(output) {
            Ok(value) => HostcallOutcome::Success(value),
            Err(err) => HostcallOutcome::Error {
                code: "internal".to_string(),
                message: format!("Serialize tool output: {err}"),
            },
        },
        Err(err) => HostcallOutcome::Error {
            code: "io".to_string(),
            message: err.to_string(),
        },
    }
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
#[allow(dead_code)]
async fn dispatch_hostcall_exec(
    runtime: Option<&PiJsRuntime>,
    call_id: &str,
    cmd: &str,
    payload: Value,
) -> HostcallOutcome {
    dispatch_hostcall_exec_ref(runtime, call_id, cmd, &payload).await
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn dispatch_hostcall_exec_ref(
    runtime: Option<&PiJsRuntime>,
    call_id: &str,
    cmd: &str,
    payload: &Value,
) -> HostcallOutcome {
    dispatch_hostcall_exec_ref_with_limit(
        runtime,
        call_id,
        cmd,
        payload,
        crate::tools::READ_TOOL_MAX_BYTES,
    )
    .await
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn dispatch_hostcall_exec_ref_with_limit(
    runtime: Option<&PiJsRuntime>,
    call_id: &str,
    cmd: &str,
    payload: &Value,
    max_capture_bytes: u64,
) -> HostcallOutcome {
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::mpsc;

    enum ExecStreamFrame {
        Stdout(String),
        Stderr(String),
        Final { code: i32, killed: bool },
        Error(String),
    }

    fn pump_stream<R: std::io::Read>(
        mut reader: R,
        tx: &std::sync::mpsc::SyncSender<ExecStreamFrame>,
        stdout: bool,
    ) -> std::result::Result<(), String> {
        let mut buf = [0u8; 4096];
        let mut partial = Vec::new();

        loop {
            let read = match reader.read(&mut buf) {
                Ok(0) => 0,
                Ok(n) => n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err.to_string()),
            };
            if read == 0 {
                // EOF. Flush partial if any (lossy).
                if !partial.is_empty() {
                    let text = String::from_utf8_lossy(&partial).to_string();
                    let frame = if stdout {
                        ExecStreamFrame::Stdout(text)
                    } else {
                        ExecStreamFrame::Stderr(text)
                    };
                    let _ = tx.send(frame);
                }
                break;
            }

            let chunk = &buf[..read];

            if partial.is_empty() {
                let mut processed = 0;
                loop {
                    match std::str::from_utf8(&chunk[processed..]) {
                        Ok(s) => {
                            if !s.is_empty() {
                                let frame = if stdout {
                                    ExecStreamFrame::Stdout(s.to_string())
                                } else {
                                    ExecStreamFrame::Stderr(s.to_string())
                                };
                                if tx.send(frame).is_err() {
                                    return Ok(());
                                }
                            }
                            break;
                        }
                        Err(e) => {
                            let valid_len = e.valid_up_to();
                            if valid_len > 0 {
                                let s =
                                    std::str::from_utf8(&chunk[processed..processed + valid_len])
                                        .expect("valid utf8 prefix");
                                let frame = if stdout {
                                    ExecStreamFrame::Stdout(s.to_string())
                                } else {
                                    ExecStreamFrame::Stderr(s.to_string())
                                };
                                if tx.send(frame).is_err() {
                                    return Ok(());
                                }
                                processed += valid_len;
                            }

                            if let Some(len) = e.error_len() {
                                let frame = if stdout {
                                    ExecStreamFrame::Stdout("\u{FFFD}".to_string())
                                } else {
                                    ExecStreamFrame::Stderr("\u{FFFD}".to_string())
                                };
                                if tx.send(frame).is_err() {
                                    return Ok(());
                                }
                                processed += len;
                            } else {
                                partial.extend_from_slice(&chunk[processed..]);
                                break;
                            }
                        }
                    }
                }
            } else {
                partial.extend_from_slice(chunk);
                let mut processed = 0;
                loop {
                    match std::str::from_utf8(&partial[processed..]) {
                        Ok(s) => {
                            if !s.is_empty() {
                                let frame = if stdout {
                                    ExecStreamFrame::Stdout(s.to_string())
                                } else {
                                    ExecStreamFrame::Stderr(s.to_string())
                                };
                                if tx.send(frame).is_err() {
                                    return Ok(());
                                }
                            }
                            partial.clear();
                            break;
                        }
                        Err(e) => {
                            let valid_len = e.valid_up_to();
                            if valid_len > 0 {
                                let s =
                                    std::str::from_utf8(&partial[processed..processed + valid_len])
                                        .expect("valid utf8 prefix");
                                let frame = if stdout {
                                    ExecStreamFrame::Stdout(s.to_string())
                                } else {
                                    ExecStreamFrame::Stderr(s.to_string())
                                };
                                if tx.send(frame).is_err() {
                                    return Ok(());
                                }
                                processed += valid_len;
                            }

                            if let Some(len) = e.error_len() {
                                let frame = if stdout {
                                    ExecStreamFrame::Stdout("\u{FFFD}".to_string())
                                } else {
                                    ExecStreamFrame::Stderr("\u{FFFD}".to_string())
                                };
                                if tx.send(frame).is_err() {
                                    return Ok(());
                                }
                                processed += len;
                            } else {
                                let remaining = partial.len() - processed;
                                partial.copy_within(processed.., 0);
                                partial.truncate(remaining);
                                break;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::unnecessary_lazy_evaluations)] // lazy eval needed on unix for signal()
    fn exit_status_code(status: std::process::ExitStatus) -> i32 {
        status.code().unwrap_or_else(|| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt as _;
                status.signal().map_or(-1, |signal| -signal)
            }
            #[cfg(not(unix))]
            {
                -1
            }
        })
    }

    let args_value = payload.get("args").cloned().unwrap_or(Value::Null);
    let args_array = match args_value {
        Value::Null => Vec::new(),
        Value::Array(items) => items,
        _ => {
            return HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: "exec args must be an array".to_string(),
            };
        }
    };

    let args = args_array
        .iter()
        .map(|v| {
            v.as_str()
                .map_or_else(|| v.to_string(), ToString::to_string)
        })
        .collect::<Vec<_>>();

    let options = payload.get("options").cloned().unwrap_or_else(|| json!({}));
    let cwd = options
        .get("cwd")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let timeout_ms = options
        .get("timeout")
        .and_then(Value::as_u64)
        .or_else(|| options.get("timeoutMs").and_then(Value::as_u64))
        .or_else(|| options.get("timeout_ms").and_then(Value::as_u64))
        .filter(|ms| *ms > 0);
    let stream = options
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if stream && let Some(runtime) = runtime {
        if !runtime.is_hostcall_active(call_id) {
            return HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: Value::Null,
                is_final: false,
            };
        }

        let cmd = cmd.to_string();
        // Keep the pump threads draining pipes even if the runtime is
        // temporarily behind on chunk delivery. Bounded channels can
        // recreate the same shell/pipe deadlock seen in the main bash tool.
        let (tx, rx) = mpsc::sync_channel::<ExecStreamFrame>(1024);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = Arc::clone(&cancel);
        let call_id_for_error = call_id.to_string();

        thread::spawn(move || {
            let result = (|| -> std::result::Result<(), String> {
                let mut command = Command::new(&cmd);
                command
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());

                if let Some(cwd) = cwd.as_ref() {
                    command.current_dir(cwd);
                }
                crate::tools::isolate_command_process_group(&mut command);

                let mut child = command.spawn().map_err(|err| err.to_string())?;
                let pid = child.id();

                let stdout = child.stdout.take().ok_or("Missing stdout pipe")?;
                let stderr = child.stderr.take().ok_or("Missing stderr pipe")?;

                let stdout_tx = tx.clone();
                let stderr_tx = tx.clone();
                let stdout_handle = thread::spawn(move || pump_stream(stdout, &stdout_tx, true));
                let stderr_handle = thread::spawn(move || pump_stream(stderr, &stderr_tx, false));

                let start = Instant::now();
                let mut killed = false;
                let status = loop {
                    if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
                        break status;
                    }

                    if !killed && cancel_worker.load(AtomicOrdering::SeqCst) {
                        killed = true;
                        crate::tools::kill_process_group_tree(Some(pid));
                        let _ = child.kill();
                        break child.wait().map_err(|err| err.to_string())?;
                    }

                    if let Some(timeout_ms) = timeout_ms
                        && !killed
                        && start.elapsed() >= Duration::from_millis(timeout_ms)
                    {
                        killed = true;
                        crate::tools::kill_process_group_tree(Some(pid));
                        let _ = child.kill();
                        break child.wait().map_err(|err| err.to_string())?;
                    }

                    thread::sleep(Duration::from_millis(10));
                };

                let drain_start = Instant::now();
                let drain_deadline = drain_start + Duration::from_secs(5);
                loop {
                    if stdout_handle.is_finished() && stderr_handle.is_finished() {
                        break;
                    }
                    if Instant::now() >= drain_deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }

                // Explicitly reap to avoid leaving a zombie behind after a
                // successful try_wait()-observed exit on isolated process groups.
                let _ = child.wait();

                let code = exit_status_code(status);
                let _ = tx.send(ExecStreamFrame::Final { code, killed });
                Ok(())
            })();

            if let Err(err) = result
                && tx.send(ExecStreamFrame::Error(err)).is_err()
            {
                tracing::trace!(
                    call_id = %call_id_for_error,
                    "Exec hostcall stream result dropped before completion"
                );
            }
        });

        let mut sequence = 0_u64;
        let mut processed_in_turn = 0_u32;
        let call_id_owned = call_id.to_string();
        loop {
            if !runtime.is_hostcall_active(call_id) {
                cancel.store(true, AtomicOrdering::SeqCst);
                return HostcallOutcome::StreamChunk {
                    sequence,
                    chunk: Value::Null,
                    is_final: false,
                };
            }

            match rx.try_recv() {
                Ok(ExecStreamFrame::Stdout(chunk)) => {
                    let mut m = serde_json::Map::with_capacity(1);
                    m.insert("stdout".into(), Value::String(chunk));
                    runtime.complete_hostcall(
                        call_id_owned.clone(),
                        HostcallOutcome::StreamChunk {
                            sequence,
                            chunk: Value::Object(m),
                            is_final: false,
                        },
                    );
                    sequence = sequence.saturating_add(1);
                    processed_in_turn += 1;
                }
                Ok(ExecStreamFrame::Stderr(chunk)) => {
                    let mut m = serde_json::Map::with_capacity(1);
                    m.insert("stderr".into(), Value::String(chunk));
                    runtime.complete_hostcall(
                        call_id_owned.clone(),
                        HostcallOutcome::StreamChunk {
                            sequence,
                            chunk: Value::Object(m),
                            is_final: false,
                        },
                    );
                    sequence = sequence.saturating_add(1);
                    processed_in_turn += 1;
                }
                Ok(ExecStreamFrame::Final { code, killed }) => {
                    return HostcallOutcome::StreamChunk {
                        sequence,
                        chunk: json!({
                            "code": code,
                            "killed": killed,
                        }),
                        is_final: true,
                    };
                }
                Ok(ExecStreamFrame::Error(message)) => {
                    return HostcallOutcome::Error {
                        code: "io".to_string(),
                        message,
                    };
                }
                Err(mpsc::TryRecvError::Empty) => {
                    processed_in_turn = 0;
                    extension_wait_sleep(Duration::from_millis(25)).await;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return HostcallOutcome::Error {
                        code: "internal".to_string(),
                        message: "exec stream channel closed".to_string(),
                    };
                }
            }

            if processed_in_turn >= 64 {
                processed_in_turn = 0;
                asupersync::runtime::yield_now().await;
            }
        }
    }

    let cmd = cmd.to_string();
    let (tx, rx) = std::sync::mpsc::sync_channel::<std::result::Result<Value, String>>(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = Arc::clone(&cancel);

    thread::spawn(move || {
        let result: std::result::Result<Value, String> = (|| {
            let mut command = Command::new(&cmd);
            command
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            if let Some(cwd) = cwd.as_ref() {
                command.current_dir(cwd);
            }
            crate::tools::isolate_command_process_group(&mut command);

            let mut child = command.spawn().map_err(|err| err.to_string())?;
            let pid = child.id();

            let stdout = child.stdout.take().ok_or("Missing stdout pipe")?;
            let stderr = child.stderr.take().ok_or("Missing stderr pipe")?;

            let (tx_stream, rx_stream) = mpsc::sync_channel::<ExecStreamFrame>(1024);
            let stdout_tx = tx_stream.clone();

            let _stdout_handle = thread::spawn(move || pump_stream(stdout, &stdout_tx, true));
            let _stderr_handle = thread::spawn(move || pump_stream(stderr, &tx_stream, false));

            let start = Instant::now();
            let mut killed = false;
            let mut stdout_acc = String::new();
            let mut stderr_acc = String::new();

            let mut ingest_frame = |frame: ExecStreamFrame| match frame {
                ExecStreamFrame::Stdout(s) if (stdout_acc.len() as u64) < max_capture_bytes => {
                    stdout_acc.push_str(&s);
                }
                ExecStreamFrame::Stderr(s) if (stderr_acc.len() as u64) < max_capture_bytes => {
                    stderr_acc.push_str(&s);
                }
                _ => {}
            };

            let status = loop {
                while let Ok(frame) = rx_stream.try_recv() {
                    ingest_frame(frame);
                }

                if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
                    break status;
                }

                if !killed && cancel_worker.load(AtomicOrdering::SeqCst) {
                    killed = true;
                    crate::tools::kill_process_group_tree(Some(pid));
                    let _ = child.kill();
                    break child.wait().map_err(|err| err.to_string())?;
                }

                if let Some(timeout_ms) = timeout_ms
                    && !killed
                    && start.elapsed() >= Duration::from_millis(timeout_ms)
                {
                    killed = true;
                    crate::tools::kill_process_group_tree(Some(pid));
                    let _ = child.kill();
                    break child.wait().map_err(|err| err.to_string())?;
                }

                if let Ok(frame) = rx_stream.recv_timeout(Duration::from_millis(10)) {
                    ingest_frame(frame);
                }
            };

            let drain_deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match rx_stream.try_recv() {
                    Ok(frame) => ingest_frame(frame),
                    Err(mpsc::TryRecvError::Empty) => {
                        if Instant::now() >= drain_deadline {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }

            drop(rx_stream); // Unblock pump threads if they are blocked on send
            let _ = child.wait(); // Explicitly reap

            if stdout_acc.len() as u64 >= max_capture_bytes {
                stdout_acc.truncate(usize::try_from(max_capture_bytes).unwrap_or(usize::MAX));
                stdout_acc.push_str("\n... [stdout truncated] ...");
            }
            if stderr_acc.len() as u64 >= max_capture_bytes {
                stderr_acc.truncate(usize::try_from(max_capture_bytes).unwrap_or(usize::MAX));
                stderr_acc.push_str("\n... [stderr truncated] ...");
            }
            let code = exit_status_code(status);

            Ok(json!({
                "stdout": stdout_acc,
                "stderr": stderr_acc,
                "code": code,
                "killed": killed,
            }))
        })();

        let _ = tx.send(result);
    });

    let _guard = CancelGuard(Arc::clone(&cancel));

    loop {
        if let Some(runtime) = runtime
            && !runtime.is_hostcall_active(call_id)
        {
            cancel.store(true, AtomicOrdering::SeqCst);
            return HostcallOutcome::Error {
                code: "internal".to_string(),
                message: "exec task cancelled".to_string(),
            };
        }

        match rx.try_recv() {
            Ok(Ok(value)) => return HostcallOutcome::Success(value),
            Ok(Err(err)) => {
                return HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: err,
                };
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                extension_wait_sleep(Duration::from_millis(25)).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return HostcallOutcome::Error {
                    code: "internal".to_string(),
                    message: "exec task cancelled".to_string(),
                };
            }
        }
    }
}

#[allow(clippy::future_not_send)]
async fn dispatch_hostcall_http(
    call_id: &str,
    connector: &HttpConnector,
    payload: Value,
) -> HostcallOutcome {
    let call = crate::connectors::HostCallPayload {
        call_id: call_id.to_string(),
        capability: "http".to_string(),
        method: "http".to_string(),
        params: payload,
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    match connector.dispatch(&call).await {
        Ok(result) => {
            if result.is_error {
                let message = result.error.as_ref().map_or_else(
                    || "HTTP connector error".to_string(),
                    |err| err.message.clone(),
                );
                let code = result
                    .error
                    .as_ref()
                    .map_or("internal", |err| hostcall_code_to_str(err.code));
                HostcallOutcome::Error {
                    code: code.to_string(),
                    message,
                }
            } else {
                HostcallOutcome::Success(result.output)
            }
        }
        Err(err) => HostcallOutcome::Error {
            code: "internal".to_string(),
            message: err.to_string(),
        },
    }
}

const fn hostcall_code_to_str(code: HostCallErrorCode) -> &'static str {
    match code {
        HostCallErrorCode::Timeout => "timeout",
        HostCallErrorCode::Denied => "denied",
        HostCallErrorCode::Io => "io",
        HostCallErrorCode::InvalidRequest => "invalid_request",
        HostCallErrorCode::Internal => "internal",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionHostcallOp {
    AppendMessage,
    AppendEntry,
    GetState,
    GetMessages,
    GetEntries,
    GetBranch,
    GetFile,
    GetName,
    SetName,
    SetModel,
    GetModel,
    SetThinkingLevel,
    GetThinkingLevel,
    SetLabel,
}

fn parse_session_hostcall_op(op: &str) -> Option<SessionHostcallOp> {
    with_folded_ascii_alnum_token(op, |folded| match folded {
        b"appendmessage" => Some(SessionHostcallOp::AppendMessage),
        b"appendentry" => Some(SessionHostcallOp::AppendEntry),
        b"getstate" => Some(SessionHostcallOp::GetState),
        b"getmessages" => Some(SessionHostcallOp::GetMessages),
        b"getentries" => Some(SessionHostcallOp::GetEntries),
        b"getbranch" => Some(SessionHostcallOp::GetBranch),
        b"getfile" => Some(SessionHostcallOp::GetFile),
        b"getname" => Some(SessionHostcallOp::GetName),
        b"setname" => Some(SessionHostcallOp::SetName),
        b"setmodel" => Some(SessionHostcallOp::SetModel),
        b"getmodel" => Some(SessionHostcallOp::GetModel),
        b"setthinkinglevel" => Some(SessionHostcallOp::SetThinkingLevel),
        b"getthinkinglevel" => Some(SessionHostcallOp::GetThinkingLevel),
        b"setlabel" => Some(SessionHostcallOp::SetLabel),
        _ => None,
    })
}

#[allow(clippy::future_not_send)]
#[allow(clippy::too_many_lines)]
#[allow(dead_code)]
async fn dispatch_hostcall_session(
    call_id: &str,
    manager: &ExtensionManager,
    op: &str,
    payload: Value,
) -> HostcallOutcome {
    dispatch_hostcall_session_ref(call_id, manager, op, &payload).await
}

#[allow(clippy::future_not_send)]
#[allow(clippy::too_many_lines)]
#[allow(clippy::option_if_let_else)]
async fn dispatch_hostcall_session_ref(
    call_id: &str,
    manager: &ExtensionManager,
    op: &str,
    payload: &Value,
) -> HostcallOutcome {
    let _ = call_id;
    let Some(session) = manager.session_handle() else {
        return HostcallOutcome::Error {
            code: "denied".to_string(),
            message: "No session configured".to_string(),
        };
    };
    let Some(op_kind) = parse_session_hostcall_op(op) else {
        return HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: format!("Unknown session op: {op}"),
        };
    };

    let invalidate_ctx_cache = matches!(
        op_kind,
        SessionHostcallOp::AppendMessage
            | SessionHostcallOp::AppendEntry
            | SessionHostcallOp::SetName
            | SessionHostcallOp::SetModel
            | SessionHostcallOp::SetThinkingLevel
            | SessionHostcallOp::SetLabel
    );

    let result = match op_kind {
        SessionHostcallOp::AppendMessage => {
            let parsed: std::result::Result<SessionMessage, _> =
                if let Some(message) = payload.get("message") {
                    SessionMessage::deserialize(message)
                } else {
                    match payload {
                        Value::Object(map) if map.contains_key("op") => {
                            let mut without_op = map.clone();
                            without_op.remove("op");
                            serde_json::from_value(Value::Object(without_op))
                        }
                        _ => SessionMessage::deserialize(payload),
                    }
                };
            match parsed {
                Ok(message) => session.append_message(message).await.map(|()| Value::Null),
                Err(err) => Err(Error::validation(format!("Parse message: {err}"))),
            }
        }
        SessionHostcallOp::AppendEntry => {
            let custom_type = payload
                .get("customType")
                .and_then(Value::as_str)
                .or_else(|| payload.get("custom_type").and_then(Value::as_str))
                .or_else(|| payload.get("customtype").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            let data = payload.get("data").cloned();
            session
                .append_custom_entry(custom_type, data)
                .await
                .map(|()| Value::Null)
        }
        SessionHostcallOp::GetState => Ok(session.get_state().await),
        SessionHostcallOp::GetMessages => serde_json::to_value(session.get_messages().await)
            .map_err(|err| Error::extension(format!("Serialize messages: {err}"))),
        SessionHostcallOp::GetEntries => serde_json::to_value(session.get_entries().await)
            .map_err(|err| Error::extension(format!("Serialize entries: {err}"))),
        SessionHostcallOp::GetBranch => serde_json::to_value(session.get_branch().await)
            .map_err(|err| Error::extension(format!("Serialize branch: {err}"))),
        SessionHostcallOp::GetFile => {
            let state = session.get_state().await;
            let file = state
                .get("sessionFile")
                .or_else(|| state.get("session_file"))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(file)
        }
        SessionHostcallOp::GetName => {
            let state = session.get_state().await;
            let name = state
                .get("sessionName")
                .or_else(|| state.get("session_name"))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(name)
        }
        SessionHostcallOp::SetName => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            session.set_name(name).await.map(|()| Value::Null)
        }
        SessionHostcallOp::SetModel => {
            let provider = payload
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let model_id = payload
                .get("modelId")
                .and_then(Value::as_str)
                .or_else(|| payload.get("model_id").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            if provider.is_empty() || model_id.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "setModel: provider and modelId are required".to_string(),
                };
            }
            session
                .set_model(provider, model_id)
                .await
                .map(|()| Value::Bool(true))
        }
        SessionHostcallOp::GetModel => {
            let (provider, model_id) = session.get_model().await;
            Ok(serde_json::json!({
                "provider": provider,
                "modelId": model_id,
            }))
        }
        SessionHostcallOp::SetThinkingLevel => {
            let level = payload
                .get("level")
                .and_then(Value::as_str)
                .or_else(|| payload.get("thinkingLevel").and_then(Value::as_str))
                .or_else(|| payload.get("thinking_level").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            if level.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "setThinkingLevel: level is required".to_string(),
                };
            }
            session
                .set_thinking_level(level)
                .await
                .map(|()| Value::Null)
        }
        SessionHostcallOp::GetThinkingLevel => {
            let level = session.get_thinking_level().await;
            Ok(level.map_or(Value::Null, Value::String))
        }
        SessionHostcallOp::SetLabel => {
            let target_id = payload
                .get("targetId")
                .and_then(Value::as_str)
                .or_else(|| payload.get("target_id").and_then(Value::as_str))
                .or_else(|| payload.get("entryId").and_then(Value::as_str))
                .or_else(|| payload.get("entry_id").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            if target_id.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "setLabel: targetId is required".to_string(),
                };
            }
            let label = payload
                .get("label")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            session
                .set_label(target_id, label)
                .await
                .map(|()| Value::Null)
        }
    };

    match result {
        Ok(value) => {
            if invalidate_ctx_cache {
                manager.invalidate_ctx_cache();
            }
            HostcallOutcome::Success(value)
        }
        Err(err) => {
            let code = err.hostcall_error_code().to_string();
            HostcallOutcome::Error {
                code,
                message: err.to_string(),
            }
        }
    }
}

#[allow(clippy::future_not_send)]
#[allow(dead_code)]
async fn dispatch_hostcall_ui(
    call_id: &str,
    manager: &ExtensionManager,
    op: &str,
    payload: Value,
    extension_id: Option<&str>,
) -> HostcallOutcome {
    dispatch_hostcall_ui_ref(call_id, manager, op, &payload, extension_id).await
}

#[allow(clippy::future_not_send)]
async fn dispatch_hostcall_ui_ref(
    call_id: &str,
    manager: &ExtensionManager,
    op: &str,
    payload: &Value,
    extension_id: Option<&str>,
) -> HostcallOutcome {
    let op = op.trim();
    if op.is_empty() {
        return HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: "host_call ui requires non-empty op".to_string(),
        };
    }

    let request = ExtensionUiRequest {
        id: call_id.to_string(),
        method: op.to_string(),
        payload: params_without_key(payload, "op"),
        timeout_ms: None,
        extension_id: extension_id.map(ToString::to_string),
    };

    match manager.request_ui(request).await {
        Ok(Some(response)) => HostcallOutcome::Success(ui_response_value_for_op(op, &response)),
        Ok(None) => HostcallOutcome::Success(Value::Null),
        Err(err) => HostcallOutcome::Error {
            code: classify_ui_hostcall_error(&err).to_string(),
            message: err.to_string(),
        },
    }
}

pub(crate) fn ui_response_value_for_op(op: &str, response: &ExtensionUiResponse) -> Value {
    if response.cancelled {
        return match op {
            // Deterministic defaults: confirm cancellation/timeout resolves false.
            "confirm" => Value::Bool(false),
            // Custom overlays need an explicit close payload; `null` is ignored by the JS poll loop.
            "custom" => json!({ "closed": true }),
            _ => Value::Null,
        };
    }
    response.value.clone().unwrap_or(Value::Null)
}

pub(crate) fn classify_ui_hostcall_error(err: &Error) -> &'static str {
    let msg = err.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") || lower.contains("cancel") {
        "timeout"
    } else if lower.contains("not configured")
        || lower.contains("channel closed")
        || lower.contains("response dropped")
    {
        "denied"
    } else {
        err.hostcall_error_code()
    }
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
#[allow(clippy::unused_async)]
async fn dispatch_hostcall_log(
    call_id: &str,
    extension_id: Option<&str>,
    payload: Value,
) -> HostcallOutcome {
    let Value::Object(mut entry) = payload else {
        return HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: "host_call log requires params object".to_string(),
        };
    };

    entry
        .entry("schema".to_string())
        .or_insert_with(|| Value::String(LOG_SCHEMA_VERSION.to_string()));
    entry.entry("ts".to_string()).or_insert_with(|| {
        Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    });

    let mut correlation = match entry.remove("correlation") {
        Some(Value::Object(map)) => map,
        Some(_) => {
            return HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: "host_call log correlation must be an object".to_string(),
            };
        }
        None => serde_json::Map::new(),
    };

    if !correlation.contains_key("extension_id") {
        let ext = extension_id.unwrap_or("<unknown>");
        correlation.insert("extension_id".to_string(), Value::String(ext.to_string()));
    }
    correlation
        .entry("scenario_id".to_string())
        .or_insert_with(|| Value::String("runtime".to_string()));
    correlation
        .entry("host_call_id".to_string())
        .or_insert_with(|| Value::String(call_id.to_string()));
    entry.insert("correlation".to_string(), Value::Object(correlation));

    let payload = Value::Object(entry);
    let log_entry: LogPayload = match serde_json::from_value(payload) {
        Ok(value) => value,
        Err(err) => {
            return HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: format!("host_call log payload is invalid: {err}"),
            };
        }
    };

    if let Err(err) = validate_log(&log_entry) {
        return HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: format!("host_call log payload validation failed: {err}"),
        };
    }

    let data = log_entry.data.clone().unwrap_or(Value::Null);
    match log_entry.level {
        LogLevel::Debug => tracing::debug!(
            target: "pijs.ext.log",
            event = %log_entry.event,
            extension_id = %log_entry.correlation.extension_id,
            scenario_id = %log_entry.correlation.scenario_id,
            host_call_id = ?log_entry.correlation.host_call_id,
            data = ?data,
            "{message}",
            message = log_entry.message
        ),
        LogLevel::Info => tracing::info!(
            target: "pijs.ext.log",
            event = %log_entry.event,
            extension_id = %log_entry.correlation.extension_id,
            scenario_id = %log_entry.correlation.scenario_id,
            host_call_id = ?log_entry.correlation.host_call_id,
            data = ?data,
            "{message}",
            message = log_entry.message
        ),
        LogLevel::Warn => tracing::warn!(
            target: "pijs.ext.log",
            event = %log_entry.event,
            extension_id = %log_entry.correlation.extension_id,
            scenario_id = %log_entry.correlation.scenario_id,
            host_call_id = ?log_entry.correlation.host_call_id,
            data = ?data,
            "{message}",
            message = log_entry.message
        ),
        LogLevel::Error => tracing::error!(
            target: "pijs.ext.log",
            event = %log_entry.event,
            extension_id = %log_entry.correlation.extension_id,
            scenario_id = %log_entry.correlation.scenario_id,
            host_call_id = ?log_entry.correlation.host_call_id,
            data = ?data,
            "{message}",
            message = log_entry.message
        ),
    }

    HostcallOutcome::Success(json!({
        "ok": true,
        "schema": log_entry.schema,
        "event": log_entry.event,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventsHostcallOp {
    GetActiveTools,
    GetAllTools,
    SetActiveTools,
    AppendEntry,
    SendMessage,
    SendUserMessage,
    RegisterCommand,
    RegisterProvider,
    GetModel,
    GetModels,
    SetModel,
    GetThinkingLevel,
    SetThinkingLevel,
    RegisterFlag,
    GetFlag,
    ListFlags,
    CompleteAi,
}

fn parse_events_hostcall_op(op: &str) -> Option<EventsHostcallOp> {
    with_folded_ascii_alnum_token(op, |folded| match folded {
        b"getactivetools" => Some(EventsHostcallOp::GetActiveTools),
        b"getalltools" => Some(EventsHostcallOp::GetAllTools),
        b"setactivetools" => Some(EventsHostcallOp::SetActiveTools),
        b"appendentry" => Some(EventsHostcallOp::AppendEntry),
        b"registercommand" => Some(EventsHostcallOp::RegisterCommand),
        b"getmodel" => Some(EventsHostcallOp::GetModel),
        b"getmodels" => Some(EventsHostcallOp::GetModels),
        b"setmodel" => Some(EventsHostcallOp::SetModel),
        b"getthinkinglevel" => Some(EventsHostcallOp::GetThinkingLevel),
        b"setthinkinglevel" => Some(EventsHostcallOp::SetThinkingLevel),
        b"getflag" => Some(EventsHostcallOp::GetFlag),
        b"listflags" => Some(EventsHostcallOp::ListFlags),
        b"sendmessage" => Some(EventsHostcallOp::SendMessage),
        b"sendusermessage" => Some(EventsHostcallOp::SendUserMessage),
        b"registerprovider" => Some(EventsHostcallOp::RegisterProvider),
        b"registerflag" => Some(EventsHostcallOp::RegisterFlag),
        b"completeai" => Some(EventsHostcallOp::CompleteAi),
        _ => None,
    })
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
#[allow(dead_code)]
async fn dispatch_hostcall_events(
    call_id: &str,
    manager: &ExtensionManager,
    tools: &ToolRegistry,
    op: &str,
    payload: Value,
) -> HostcallOutcome {
    dispatch_hostcall_events_ref(call_id, manager, tools, None, op, &payload).await
}

fn authoritative_events_extension_id(
    authoritative_extension_id: Option<&str>,
    payload: &Value,
    operation: &str,
) -> std::result::Result<Option<String>, HostcallOutcome> {
    let claimed_ids = ["extensionId", "extension_id"]
        .into_iter()
        .filter_map(|key| payload.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|claimed| !claimed.is_empty())
        .collect::<Vec<_>>();
    let Some(authoritative) = authoritative_extension_id else {
        return Ok(claimed_ids.first().map(|claimed| (*claimed).to_string()));
    };
    if let Some(claimed) = claimed_ids
        .iter()
        .find(|claimed| **claimed != authoritative)
    {
        return Err(HostcallOutcome::Error {
            code: "extension_identity_mismatch".to_string(),
            message: format!(
                "{operation}: runtime owner {authoritative} rejected payload claiming extension {claimed}"
            ),
        });
    }
    Ok(Some(authoritative.to_string()))
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn dispatch_hostcall_events_ref(
    call_id: &str,
    manager: &ExtensionManager,
    tools: &ToolRegistry,
    authoritative_extension_id: Option<&str>,
    op: &str,
    payload: &Value,
) -> HostcallOutcome {
    let _ = call_id;
    let Some(op_kind) = parse_events_hostcall_op(op) else {
        return HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: format!("Unknown events op: {}", op.trim()),
        };
    };

    match op_kind {
        EventsHostcallOp::GetActiveTools => {
            let active = manager
                .active_tools()
                .unwrap_or_else(|| tools.tools().iter().map(|t| t.name().to_string()).collect());
            HostcallOutcome::Success(json!({ "tools": active }))
        }
        EventsHostcallOp::GetAllTools => {
            let tool_defs = manager.extension_tool_defs();
            let builtins = tools.tools();
            let mut result = Vec::with_capacity(builtins.len() + tool_defs.len());
            for tool in builtins {
                result.push(json!({
                    "name": tool.name(),
                    "description": tool.description(),
                }));
            }
            for def in tool_defs {
                let name = def.get("name").and_then(Value::as_str).unwrap_or_default();
                let description = def
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                result.push(json!({
                    "name": name,
                    "description": description,
                }));
            }
            HostcallOutcome::Success(json!({ "tools": result }))
        }
        EventsHostcallOp::SetActiveTools => {
            let tools = payload
                .get("tools")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            manager.set_active_tools(tools);
            HostcallOutcome::Success(Value::Null)
        }
        EventsHostcallOp::AppendEntry => {
            let Some(session) = manager.session_handle() else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "No session configured".to_string(),
                };
            };
            let custom_type = payload
                .get("customType")
                .and_then(Value::as_str)
                .or_else(|| payload.get("custom_type").and_then(Value::as_str))
                .or_else(|| payload.get("customtype").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            let data = payload.get("data").cloned();
            match session.append_custom_entry(custom_type, data).await {
                Ok(()) => {
                    manager.invalidate_ctx_cache();
                    HostcallOutcome::Success(Value::Null)
                }
                Err(err) => HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: err.to_string(),
                },
            }
        }
        EventsHostcallOp::SendMessage => {
            let Some(actions) = manager.host_actions() else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "No host actions configured".to_string(),
                };
            };

            let extension_id = match authoritative_events_extension_id(
                authoritative_extension_id,
                payload,
                "sendMessage",
            ) {
                Ok(extension_id) => extension_id,
                Err(outcome) => return outcome,
            };

            let message = payload.get("message").and_then(Value::as_object);
            let options = payload.get("options").and_then(Value::as_object);

            let custom_type = message
                .and_then(|msg| msg.get("customType").and_then(Value::as_str))
                .or_else(|| message.and_then(|msg| msg.get("custom_type").and_then(Value::as_str)))
                .unwrap_or_default()
                .trim()
                .to_string();
            if custom_type.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "sendMessage: message.customType is required".to_string(),
                };
            }

            let display = message
                .and_then(|msg| msg.get("display"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let details = message.and_then(|msg| msg.get("details")).cloned();

            let content = match message.and_then(|msg| msg.get("content")) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => {
                    serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string())
                }
                None => String::new(),
            };

            let deliver_as = ExtensionDeliverAs::parse(
                options
                    .and_then(|opts| opts.get("deliverAs"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        options.and_then(|opts| opts.get("deliver_as").and_then(Value::as_str))
                    }),
            );
            let trigger_turn = options
                .and_then(|opts| opts.get("triggerTurn"))
                .and_then(Value::as_bool)
                .or_else(|| {
                    options.and_then(|opts| opts.get("trigger_turn").and_then(Value::as_bool))
                })
                .unwrap_or(false);

            let msg = ExtensionSendMessage {
                extension_id,
                custom_type,
                content,
                display,
                details,
                deliver_as,
                trigger_turn,
            };

            match actions.send_message(msg).await {
                Ok(()) => HostcallOutcome::Success(Value::Null),
                Err(err) => HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: err.to_string(),
                },
            }
        }
        EventsHostcallOp::SendUserMessage => {
            let Some(actions) = manager.host_actions() else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "No host actions configured".to_string(),
                };
            };

            let extension_id = match authoritative_events_extension_id(
                authoritative_extension_id,
                payload,
                "sendUserMessage",
            ) {
                Ok(extension_id) => extension_id,
                Err(outcome) => return outcome,
            };

            let text = payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if text.is_empty() {
                return HostcallOutcome::Success(Value::Null);
            }

            let options = payload.get("options").and_then(Value::as_object);
            let deliver_as = ExtensionDeliverAs::parse(
                options
                    .and_then(|opts| opts.get("deliverAs"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        options.and_then(|opts| opts.get("deliver_as").and_then(Value::as_str))
                    }),
            );

            let msg = ExtensionSendUserMessage {
                extension_id,
                text,
                deliver_as,
            };

            match actions.send_user_message(msg).await {
                Ok(()) => HostcallOutcome::Success(Value::Null),
                Err(err) => HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: err.to_string(),
                },
            }
        }
        EventsHostcallOp::CompleteAi => {
            let Some(actions) = manager.host_actions() else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "No provider/session host bridge configured".to_string(),
                };
            };

            let request = ExtensionAiCompletionRequest {
                model: payload.get("model").cloned().unwrap_or(Value::Null),
                context: payload.get("context").cloned().unwrap_or(Value::Null),
                options: payload.get("options").cloned().unwrap_or_else(|| json!({})),
                simple: payload
                    .get("simple")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };

            match actions.complete_ai(request).await {
                Ok(value) => HostcallOutcome::Success(value),
                Err(err) => HostcallOutcome::Error {
                    code: "provider".to_string(),
                    message: err.to_string(),
                },
            }
        }
        EventsHostcallOp::RegisterCommand => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "registerCommand: name is required".to_string(),
                };
            }
            let description = payload
                .get("description")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let result = authoritative_extension_id.map_or_else(
                || {
                    manager.register_command(&name, description.as_deref());
                    Ok(())
                },
                |extension_id| {
                    manager.register_command_for_extension(
                        extension_id,
                        &name,
                        description.as_deref(),
                    )
                },
            );
            match result {
                Ok(()) => HostcallOutcome::Success(Value::Null),
                Err(err) => HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: err.to_string(),
                },
            }
        }
        EventsHostcallOp::RegisterProvider => {
            let id = payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if id.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "registerProvider: id is required".to_string(),
                };
            }
            let api = payload
                .get("api")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if api.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "registerProvider: api is required".to_string(),
                };
            }
            // Validate api type.
            match api.as_str() {
                "anthropic-messages"
                | "openai-completions"
                | "openai-responses"
                | "google-generative-ai" => {}
                other => {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: format!(
                            "registerProvider: unsupported api type: {other}. \
                             Supported: anthropic-messages, openai-completions, \
                             openai-responses, google-generative-ai"
                        ),
                    };
                }
            }
            let provider = params_without_key(payload, "op");
            let result = if let Some(extension_id) = authoritative_extension_id {
                manager.register_provider_for_extension(extension_id, provider)
            } else {
                manager.register_provider(provider);
                Ok(())
            };
            match result {
                Ok(()) => HostcallOutcome::Success(Value::Null),
                Err(err) => HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: err.to_string(),
                },
            }
        }
        EventsHostcallOp::GetModel => {
            // Prefer session-authoritative state; fall back to in-memory cache.
            let (provider, model_id) = if let Some(session) = manager.session_handle() {
                session.get_model().await
            } else {
                manager.current_model()
            };
            HostcallOutcome::Success(json!({
                "provider": provider,
                "modelId": model_id,
            }))
        }
        EventsHostcallOp::GetModels => {
            let Some(actions) = manager.host_actions() else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "No provider/session host bridge configured".to_string(),
                };
            };

            match actions.list_ai_models().await {
                Ok(value) => HostcallOutcome::Success(value),
                Err(err) => HostcallOutcome::Error {
                    code: "provider".to_string(),
                    message: err.to_string(),
                },
            }
        }
        EventsHostcallOp::SetModel => {
            let provider = payload
                .get("provider")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let model_id = payload
                .get("modelId")
                .and_then(Value::as_str)
                .or_else(|| payload.get("model_id").and_then(Value::as_str))
                .map(ToString::to_string);

            // Update in-memory cache on manager.
            manager.set_current_model(provider.clone(), model_id.clone());

            // Persist via session (creates ModelChangeEntry + updates header).
            let p = provider.unwrap_or_default();
            let m = model_id.unwrap_or_default();
            if let Some(session) = manager.session_handle()
                && !p.is_empty()
                && !m.is_empty()
                && let Err(err) = session.set_model(p, m).await
            {
                return HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: format!("setModel: session update failed: {err}"),
                };
            }
            HostcallOutcome::Success(Value::Null)
        }
        EventsHostcallOp::GetThinkingLevel => {
            // Prefer session-authoritative state; fall back to in-memory cache.
            let level = if let Some(session) = manager.session_handle() {
                session.get_thinking_level().await
            } else {
                manager.current_thinking_level()
            };
            HostcallOutcome::Success(json!({ "thinkingLevel": level }))
        }
        EventsHostcallOp::SetThinkingLevel => {
            let level = payload
                .get("thinkingLevel")
                .and_then(Value::as_str)
                .or_else(|| payload.get("thinking_level").and_then(Value::as_str))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            // Update in-memory cache on manager.
            manager.set_current_thinking_level(level.clone());

            // Persist via session (creates ThinkingLevelChangeEntry + updates header).
            if let Some(session) = manager.session_handle()
                && let Some(ref lvl) = level
                && let Err(err) = session.set_thinking_level(lvl.clone()).await
            {
                return HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: format!("setThinkingLevel: session update failed: {err}"),
                };
            }
            HostcallOutcome::Success(Value::Null)
        }
        EventsHostcallOp::RegisterFlag => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "registerFlag: name is required".to_string(),
                };
            }
            let flag = params_without_key(payload, "op");
            let result = if let Some(extension_id) = authoritative_extension_id {
                manager.register_flag_for_extension(extension_id, flag)
            } else {
                manager.register_flag(flag);
                Ok(())
            };
            match result {
                Ok(()) => HostcallOutcome::Success(Value::Null),
                Err(err) => HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: err.to_string(),
                },
            }
        }
        EventsHostcallOp::GetFlag => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "getFlag: name is required".to_string(),
                };
            }
            let all_flags = manager.list_flags();
            let flag = all_flags
                .iter()
                .find(|f| f.get("name").and_then(Value::as_str).unwrap_or_default() == name);
            flag.map_or(HostcallOutcome::Success(Value::Null), |f| {
                HostcallOutcome::Success(f.clone())
            })
        }
        EventsHostcallOp::ListFlags => {
            let flags = manager.list_flags();
            HostcallOutcome::Success(json!(flags))
        }
    }
}

enum JsTaskTakeResult {
    Missing,
    Pending,
    Resolved(Value),
    Rejected {
        code: Option<String>,
        message: String,
        stack: Option<String>,
    },
    Snapshot(Value),
}

#[allow(clippy::future_not_send)]
async fn take_js_task_state(runtime: &PiJsRuntime, task_id: &str) -> Result<JsTaskTakeResult> {
    let bridge_secret = runtime.bridge_secret().to_string();
    runtime
        .with_ctx(|ctx| {
            let global = ctx.globals();
            let take_fn: rquickjs::Function<'_> = global.get("__pi_task_take")?;
            let value: rquickjs::Value<'_> = take_fn.call((bridge_secret.as_str(), task_id))?;
            if value.is_null() || value.is_undefined() {
                return Ok(JsTaskTakeResult::Missing);
            }
            if let Some(obj) = value.as_object()
                && let Ok(status) = obj.get::<_, String>("status")
            {
                match status.as_str() {
                    "pending" => return Ok(JsTaskTakeResult::Pending),
                    "resolved" => {
                        let resolved_js = obj.get::<_, rquickjs::Value<'_>>("value").ok();
                        let resolved_json = if let Some(value) = resolved_js {
                            js_to_json(&value)?
                        } else {
                            Value::Null
                        };
                        return Ok(JsTaskTakeResult::Resolved(resolved_json));
                    }
                    "rejected" => {
                        let (code, message, stack) = obj
                            .get::<_, rquickjs::Value<'_>>("error")
                            .ok()
                            .and_then(|error_value| error_value.as_object().cloned())
                            .map_or_else(
                                || (None, "Unknown JS task error".to_string(), None),
                                |error_obj| {
                                    (
                                        error_obj.get::<_, String>("code").ok(),
                                        error_obj.get::<_, String>("message").unwrap_or_else(
                                            |_| "Unknown JS task error".to_string(),
                                        ),
                                        error_obj.get::<_, String>("stack").ok(),
                                    )
                                },
                            );
                        return Ok(JsTaskTakeResult::Rejected {
                            code,
                            message,
                            stack,
                        });
                    }
                    _ => {}
                }
            }
            Ok(JsTaskTakeResult::Snapshot(js_to_json(&value)?))
        })
        .await
}

fn finish_js_task_take(task_take: JsTaskTakeResult) -> Result<Option<Value>> {
    match task_take {
        JsTaskTakeResult::Missing => Err(Error::extension("JS task state missing".to_string())),
        JsTaskTakeResult::Pending => Ok(None),
        JsTaskTakeResult::Resolved(value) => Ok(Some(value)),
        JsTaskTakeResult::Rejected {
            code,
            mut message,
            stack,
        } => {
            if let Some(code) = code {
                message = format!("{code}: {message}");
            }
            if let Some(stack) = stack
                && !stack.is_empty()
            {
                message.push('\n');
                message.push_str(&stack);
            }
            Err(Error::extension(message))
        }
        JsTaskTakeResult::Snapshot(state_json) => {
            let state: JsTaskState = serde_json::from_value(state_json)
                .map_err(|err| Error::extension(err.to_string()))?;
            match state.status.as_str() {
                "pending" => Ok(None),
                "resolved" => Ok(Some(state.value.unwrap_or(Value::Null))),
                "rejected" => {
                    let err = state.error.unwrap_or_else(|| JsTaskError {
                        code: None,
                        message: "Unknown JS task error".to_string(),
                        stack: None,
                    });
                    let mut message = err.message;
                    if let Some(code) = err.code {
                        message = format!("{code}: {message}");
                    }
                    if let Some(stack) = err.stack
                        && !stack.is_empty()
                    {
                        message.push('\n');
                        message.push_str(&stack);
                    }
                    Err(Error::extension(message))
                }
                other => Err(Error::extension(format!(
                    "Unexpected JS task status: {other}"
                ))),
            }
        }
    }
}

fn js_task_timed_out(
    start: asupersync::types::Time,
    started_at: Instant,
    timeout: Duration,
) -> bool {
    let now = extension_wait_now();
    Duration::from_nanos(now.duration_since(start)) > timeout || started_at.elapsed() > timeout
}

#[allow(clippy::future_not_send)]
async fn await_js_task(
    runtime: &PiJsRuntime,
    host: &JsRuntimeHost,
    expected_owner: Option<&str>,
    task_id: &str,
    timeout: Duration,
) -> Result<Value> {
    let start = extension_wait_now();
    let started_at = Instant::now();

    loop {
        if js_task_timed_out(start, started_at, timeout) {
            return Err(Error::extension(format!(
                "JS task timed out after {}ms",
                timeout.as_millis()
            )));
        }

        let _has_pending = pump_js_runtime_once_for_owner(runtime, host, expected_owner).await?;
        if let Some(value) = finish_js_task_take(take_js_task_state(runtime, task_id).await?)? {
            return Ok(value);
        }
        if !runtime.has_pending() {
            extension_wait_short_blocking_pause(Duration::from_millis(1));
        }
    }
}

#[allow(clippy::future_not_send)]
async fn await_js_task_in_shards(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    shard_index: usize,
    task_id: &str,
    timeout: Duration,
) -> Result<Value> {
    let start = extension_wait_now();
    let started_at = Instant::now();

    loop {
        if js_task_timed_out(start, started_at, timeout) {
            return Err(quarantine_runtime_shard(
                shards,
                shard_index,
                &format!(
                    "task {task_id} timed out after {}ms with unresolved JavaScript state",
                    timeout.as_millis()
                ),
            ));
        }

        let _has_pending =
            pump_js_runtime_shards_once_for_target(shards, host, Some(shard_index)).await?;
        let runtime = &shards
            .shards
            .get(shard_index)
            .ok_or_else(|| Error::extension("JS runtime shard disappeared"))?
            .runtime;
        if let Some(value) = finish_js_task_take(take_js_task_state(runtime, task_id).await?)? {
            return Ok(value);
        }
        if !shards.shards[shard_index].runtime.has_pending() {
            extension_wait_short_blocking_pause(Duration::from_millis(1));
        }
    }
}

#[allow(clippy::future_not_send)]
async fn await_js_task_in_shards_and_refresh(
    shards: &mut JsRuntimeShardSet,
    host: &JsRuntimeHost,
    shard_index: usize,
    task_id: &str,
    timeout: Duration,
) -> Result<Value> {
    let task_result = await_js_task_in_shards(shards, host, shard_index, task_id, timeout).await;
    let refresh_result = refresh_runtime_shard_snapshot(shards, shard_index).await;
    match task_result {
        Ok(value) => {
            refresh_result?;
            Ok(value)
        }
        Err(err) => {
            if let Err(refresh_err) = refresh_result {
                tracing::warn!(
                    event = "extension_runtime.shards.refresh_after_task_error_failed",
                    shard_index,
                    error = %refresh_err,
                    "Failed to refresh JS extension shard after a task error"
                );
            }
            Err(err)
        }
    }
}

/// Immutable snapshot of frequently-read extension registry metadata.
///
/// Published via RCU-style swap: writers hold the mutex, build a new snapshot,
/// then atomically replace the shared `Arc`. Readers grab the `Arc` without
/// any lock, paying only an atomic increment for the refcount.
#[derive(Clone, Default)]
pub(crate) struct RegistrySnapshot {
    /// Number of registered/loaded extensions.
    pub extension_count: usize,
    /// Pre-computed set of event names with at least one registered hook.
    pub hook_bitmap: HashSet<String>,
    /// Whether any event hooks are registered at all.
    pub has_any_hooks: bool,
    /// Current session handle (cheap `Arc` clone).
    pub session: Option<Arc<dyn ExtensionSession>>,
    /// Filtered tool list for event dispatch context.
    pub active_tools: Option<Vec<String>>,
    /// Registered provider specs.
    pub providers: Vec<Value>,
    /// Registered MCP server specs.
    pub mcp_servers: Vec<Value>,
    /// Registered flags.
    // Snapshot keeps the raw registry view for diagnostics while hot readers
    // use `all_flags`, which is already merged and deduplicated.
    #[allow(dead_code)]
    pub flags: Vec<Value>,
    /// Current working directory.
    pub cwd: Option<String>,
    /// Model registry key-value pairs.
    pub model_registry_values: HashMap<String, String>,
    /// Current provider identifier.
    pub current_provider: Option<String>,
    /// Current model identifier.
    pub current_model_id: Option<String>,
    /// Current thinking level.
    pub current_thinking_level: Option<String>,
    /// Global kill-switch for hostcall compatibility lane.
    // The live path reads kill switches from the guarded manager state; the
    // snapshot copies them for read-only diagnostics and future RCU consumers.
    #[allow(dead_code)]
    pub hostcall_compat_kill_switch_global: bool,
    /// Per-extension kill-switch set.
    #[allow(dead_code)]
    pub hostcall_compat_kill_switch_extensions: HashSet<String>,
    /// Monotonic version counter (seqlock-style) for cache invalidation.
    pub version: u64,
    // ── Pre-computed derived views (RCU read-hot) ────────────────────
    /// Pre-computed merged flag list (dynamic flags take priority, then
    /// extension-payload flags, deduplicated by name).
    pub all_flags: Vec<Value>,
    /// Pre-computed slash command list from all extensions.
    pub all_commands: Vec<Value>,
    /// Pre-computed shortcut list from all extensions.
    pub all_shortcuts: Vec<Value>,
    /// Pre-computed set of lowercase shortcut `key_id`s for O(1) lookup.
    pub shortcut_key_ids: HashSet<String>,
    /// Pre-computed sorted event hook names from all extensions.
    pub all_event_hooks: Vec<String>,
    /// Pre-computed tool definitions from all extensions (avoids mutex + clone cascade).
    pub all_tool_defs: Vec<Value>,
    /// Pre-computed set of normalized command names for O(1) `has_command()` lookup.
    pub command_names: HashSet<String>,
    /// Whether a UI sender is configured (stable after startup).
    pub has_ui: bool,
}

/// Extension manager for handling loaded extensions.
#[derive(Clone)]
pub struct ExtensionManager {
    inner: Arc<Mutex<ExtensionManagerInner>>,
    /// Lock-free read path: immutable snapshot swapped via RCU.
    /// Readers grab the `Arc` via `RwLock::read()` (uncontended fast path).
    /// Writers hold the mutex, build a new snapshot, then swap under a brief
    /// write-lock.  The old snapshot is reclaimed when its last reader drops.
    snapshot: Arc<RwLock<Arc<RegistrySnapshot>>>,
    /// Monotonic seqlock counter for cheap staleness checks.
    snapshot_version: Arc<AtomicU64>,
}

#[cfg(feature = "wasm-host")]
#[derive(Clone, Default)]
pub(crate) struct ExtensionManagerHandle {
    inner: Weak<Mutex<ExtensionManagerInner>>,
    snapshot: Option<Arc<RwLock<Arc<RegistrySnapshot>>>>,
    snapshot_version: Option<Arc<AtomicU64>>,
}

#[cfg(feature = "wasm-host")]
impl ExtensionManagerHandle {
    fn new(manager: &ExtensionManager) -> Self {
        Self {
            inner: Arc::downgrade(&manager.inner),
            snapshot: Some(Arc::clone(&manager.snapshot)),
            snapshot_version: Some(Arc::clone(&manager.snapshot_version)),
        }
    }

    fn upgrade(&self) -> Option<ExtensionManager> {
        self.inner.upgrade().map(|inner| ExtensionManager {
            inner,
            snapshot: self
                .snapshot
                .clone()
                .unwrap_or_else(|| Arc::new(RwLock::new(Arc::new(RegistrySnapshot::default())))),
            snapshot_version: self
                .snapshot_version
                .clone()
                .unwrap_or_else(|| Arc::new(AtomicU64::new(0))),
        })
    }
}

/// Cached context payload for event dispatch.
///
/// Avoids rebuilding the JSON context (session state, entries, branch, cwd,
/// model registry) on every event dispatch.  The cache is invalidated when
/// `ctx_generation` on `ExtensionManagerInner` advances past `generation`.
#[derive(Clone)]
struct CachedEventContext {
    /// The generation at which this cache was built.
    generation: u64,
    /// The pre-built context payload (Arc-wrapped for cheap cache hits).
    payload: Arc<Value>,
}

#[derive(Default)]
struct ExtensionManagerInner {
    extensions: Vec<RegisterPayload>,
    /// Runtime principal for each `extensions` entry at the same index.
    /// Display names are intentionally kept separate from security identity.
    extension_ids: Vec<String>,
    extension_roots: Vec<PathBuf>,
    extension_versions: HashMap<String, String>,
    runtime: Option<ExtensionRuntimeHandle>,
    #[cfg(feature = "wasm-host")]
    wasm_extensions: Vec<WasmExtensionHandle>,
    ui_sender: Option<mpsc::Sender<ExtensionUiRequest>>,
    pending_ui: HashMap<String, oneshot::Sender<ExtensionUiResponse>>,
    session: Option<Arc<dyn ExtensionSession>>,
    active_tools: Option<Vec<String>>,
    providers: Vec<Value>,
    mcp_servers: Vec<Value>,
    flags: Vec<Value>,
    cwd: Option<String>,
    model_registry_values: HashMap<String, String>,
    current_provider: Option<String>,
    current_model_id: Option<String>,
    current_thinking_level: Option<String>,
    host_actions: Option<Arc<dyn ExtensionHostActions>>,
    policy_prompt_cache: HashMap<String, HashMap<String, PersistedDecision>>,
    /// Persistent store for "Allow Always" / "Deny Always" decisions.
    permission_store: Option<PermissionStore>,
    /// Runtime risk controller configuration and mutable per-extension state.
    runtime_risk_config: RuntimeRiskConfig,
    runtime_risk_states: HashMap<String, RuntimeRiskState>,
    runtime_risk_ledger: VecDeque<RuntimeRiskLedgerEntry>,
    runtime_hostcall_telemetry: VecDeque<RuntimeHostcallTelemetryEvent>,
    hostcall_marshalling_fallback_counts: HashMap<String, u64>,
    runtime_risk_last_hash: Option<String>,
    /// Per-extension resource quota config and mutable counters (SEC-4.1).
    quota_config: ExtensionQuotaConfig,
    quota_states: HashMap<String, ExtensionQuotaState>,
    /// Quota breach telemetry events (SEC-4.1).
    quota_breach_events: VecDeque<QuotaBreachEvent>,
    /// Exec mediation decision ledger (SEC-4.3).
    exec_mediation_ledger: VecDeque<ExecMediationLedgerEntry>,
    /// Secret broker decision ledger (SEC-4.3).
    secret_broker_ledger: VecDeque<SecretBrokerLedgerEntry>,
    /// Security alert stream (SEC-5.1).
    security_alerts: VecDeque<SecurityAlert>,
    /// Monotonic counter for security alert sequence IDs.
    security_alert_seq: u64,
    /// Per-extension trust state (SEC-5.2).
    trust_states: HashMap<String, ExtensionTrustState>,
    /// Emergency global kill-switch forcing hostcalls into compatibility lane.
    hostcall_compat_kill_switch_global: bool,
    /// Emergency per-extension kill-switch forcing hostcalls into compatibility lane.
    hostcall_compat_kill_switch_extensions: HashSet<String>,
    /// Automatic budget controller for overload/anomaly fallback routing.
    budget_controller_config: ExtensionBudgetControllerConfig,
    /// Per-extension fallback state tracked by the budget controller.
    budget_fallback_states: HashMap<String, ExtensionBudgetFallbackState>,
    /// Kill-switch audit trail (SEC-5.2).
    kill_switch_audit: VecDeque<KillSwitchAuditEntry>,
    /// Trust onboarding decision log (SEC-5.2).
    trust_onboarding_log: VecDeque<TrustOnboardingDecision>,
    /// Graduated enforcement rollout tracker (SEC-7.2).
    rollout_tracker: RolloutTracker,
    /// Budget for extension operations (structured concurrency).
    extension_budget: Budget,
    /// Pre-computed set of event names that have at least one registered hook.
    /// Updated on `register()` to enable O(1) hook-presence checks instead of
    /// iterating all extensions on every event dispatch.
    hook_bitmap: HashSet<String>,
    /// Cached context payload for event dispatch.  Rebuilt lazily when the
    /// generation counter (`ctx_cache_generation`) is stale.
    ctx_cache: Option<CachedEventContext>,
    /// Monotonic counter incremented whenever session or context-affecting state
    /// changes (e.g. session set, cwd change, model registry update).
    ctx_generation: u64,
    /// Core-pinned SPSC reactor mesh for fast-lane hostcall traffic (bd-3ar8v.4.20).
    hostcall_reactor: Option<HostcallReactorMesh>,
    /// Replay trace configuration for extension hostcall forensics.
    replay_config: Option<crate::extension_replay::ReplayLaneConfig>,
    /// Completed replay trace bundles from recent dispatch cycles.
    replay_bundles: VecDeque<crate::extension_replay::ReplayTraceBundle>,
}

impl std::fmt::Debug for ExtensionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionManager").finish_non_exhaustive()
    }
}

impl Default for ExtensionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard for extension lifecycle with structured concurrency guarantees.
///
/// Wraps an [`ExtensionManager`] and ensures that the JS runtime thread is
/// shut down when the region exits.  Provides:
///
/// - **No orphaned tasks**: the runtime thread exits on region close.
/// - **Bounded cleanup**: shutdown is capped by a configurable budget.
/// - **Drop safety**: best-effort shutdown if `shutdown()` was not called.
pub struct ExtensionRegion {
    manager: ExtensionManager,
    cleanup_budget: Duration,
    shutdown_done: std::sync::atomic::AtomicBool,
}

impl ExtensionRegion {
    /// Create a new extension region with the default cleanup budget (5 s).
    pub const fn new(manager: ExtensionManager) -> Self {
        Self {
            manager,
            cleanup_budget: ExtensionManager::DEFAULT_CLEANUP_BUDGET,
            shutdown_done: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Create a region with a custom cleanup budget.
    pub const fn with_budget(manager: ExtensionManager, budget: Duration) -> Self {
        Self {
            manager,
            cleanup_budget: budget,
            shutdown_done: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Access the inner [`ExtensionManager`].
    pub const fn manager(&self) -> &ExtensionManager {
        &self.manager
    }

    /// Consume the region and return the inner manager (caller takes
    /// responsibility for shutdown).
    pub fn into_inner(mut self) -> ExtensionManager {
        self.shutdown_done
            .store(true, std::sync::atomic::Ordering::Release);
        std::mem::take(&mut self.manager)
    }

    /// Explicitly shut down extensions with the configured budget.
    ///
    /// Returns `true` if the runtime exited cleanly within the budget.
    /// Subsequent calls are no-ops and return `true`.
    pub async fn shutdown(&self) -> bool {
        if self
            .shutdown_done
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return true; // already done
        }
        if let Err(err) = self
            .manager
            .dispatch_event(ExtensionEventName::SessionShutdown, None)
            .await
        {
            tracing::warn!("session_shutdown extension hook failed (fail-open): {err}");
        }
        self.manager.shutdown(self.cleanup_budget).await
    }
}

impl Drop for ExtensionRegion {
    fn drop(&mut self) {
        if self.shutdown_done.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // Best-effort: the Weak reference in JsRuntimeHost will fail to
        // upgrade once the ExtensionManager's Arc refcount drops, causing
        // the runtime thread to observe channel closure and exit.
        tracing::debug!(
            event = "extension_region.drop_without_shutdown",
            "ExtensionRegion dropped without explicit shutdown; \
             runtime thread will exit on Arc release"
        );
    }
}

impl std::fmt::Debug for ExtensionRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionRegion")
            .field("manager", &self.manager)
            .field("cleanup_budget", &self.cleanup_budget)
            .field(
                "shutdown_done",
                &self
                    .shutdown_done
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .finish()
    }
}

fn normalize_semver_literal(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let input = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let suffix_offset = input.find(['-', '+']).unwrap_or(input.len());
    let (core, suffix) = input.split_at(suffix_offset);
    let mut parts = core.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let patch = parts.next().unwrap_or("0");
    if parts.next().is_some() {
        return None;
    }
    Some(format!("{major}.{minor}.{patch}{suffix}"))
}

fn normalize_version_requirement(range: &str) -> Option<String> {
    let mut tokens = Vec::new();
    let mut remaining = range.trim();
    if remaining.is_empty() {
        return None;
    }
    loop {
        remaining = remaining.trim_start();
        if remaining.is_empty() {
            break;
        }
        if remaining == "*" {
            tokens.push("*".to_string());
            break;
        }

        let (op, version_and_rest) = [
            (">=", remaining.strip_prefix(">=")),
            ("<=", remaining.strip_prefix("<=")),
            (">", remaining.strip_prefix('>')),
            ("<", remaining.strip_prefix('<')),
            ("^", remaining.strip_prefix('^')),
            ("~", remaining.strip_prefix('~')),
            ("=", remaining.strip_prefix('=')),
        ]
        .into_iter()
        .find_map(|(op, version)| version.map(|value| (op, value)))
        .unwrap_or(("=", remaining));

        let version_and_rest = version_and_rest.trim_start();
        if version_and_rest.is_empty() {
            return None;
        }
        let version_end = version_and_rest
            .find(|ch: char| ch.is_whitespace() || ch == ',')
            .unwrap_or(version_and_rest.len());
        let version = &version_and_rest[..version_end];
        let normalized_version = normalize_semver_literal(version)?;
        tokens.push(format!("{op}{normalized_version}"));
        remaining = &version_and_rest[version_end..];

        let mut saw_separator = false;
        let mut saw_comma = false;
        while let Some(ch) = remaining.chars().next() {
            if ch.is_whitespace() {
                saw_separator = true;
                remaining = &remaining[ch.len_utf8()..];
                continue;
            }
            if ch == ',' {
                saw_separator = true;
                saw_comma = true;
                remaining = &remaining[ch.len_utf8()..];
                continue;
            }
            break;
        }

        if remaining.is_empty() {
            if saw_comma {
                return None;
            }
            break;
        }
        if !saw_separator {
            return None;
        }
    }

    (!tokens.is_empty()).then(|| tokens.join(", "))
}

fn looks_like_plain_semver_literal(range: &str) -> bool {
    let trimmed = range.trim();
    !trimmed.is_empty()
        && !trimmed.chars().any(|ch| {
            ch.is_whitespace() || matches!(ch, ',' | '^' | '~' | '>' | '<' | '=' | '*' | '|')
        })
}

fn check_version_constraint(version: &str, range: &str) -> bool {
    let range = range.trim();
    if range == "*" || range.is_empty() {
        return true;
    }

    let Some(version) = normalize_semver_literal(version) else {
        return false;
    };
    let Ok(version) = Version::parse(&version) else {
        return false;
    };
    if let Some(range) = normalize_version_requirement(range) {
        return VersionReq::parse(&range).is_ok_and(|req| req.matches(&version));
    }
    if looks_like_plain_semver_literal(range) {
        return false;
    }

    VersionReq::parse(range).is_ok_and(|req| req.matches(&version))
}

/// Extract extension event information from an agent event.
pub fn extension_event_from_agent(
    event: &AgentEvent,
) -> Option<(ExtensionEventName, Option<Value>)> {
    let name = match event {
        AgentEvent::AgentStart { .. } => ExtensionEventName::AgentStart,
        AgentEvent::AgentEnd { .. } => ExtensionEventName::AgentEnd,
        AgentEvent::TurnStart { .. } => ExtensionEventName::TurnStart,
        AgentEvent::TurnEnd { .. } => ExtensionEventName::TurnEnd,
        AgentEvent::MessageStart { .. } => ExtensionEventName::MessageStart,
        AgentEvent::MessageUpdate { .. } => ExtensionEventName::MessageUpdate,
        AgentEvent::MessageEnd { .. } => ExtensionEventName::MessageEnd,
        AgentEvent::ToolExecutionStart { .. } => ExtensionEventName::ToolExecutionStart,
        AgentEvent::ToolExecutionUpdate { .. } => ExtensionEventName::ToolExecutionUpdate,
        AgentEvent::ToolExecutionEnd { .. } => ExtensionEventName::ToolExecutionEnd,
        // Session-level compaction/retry events are not dispatched to extensions.
        AgentEvent::AutoCompactionStart { .. }
        | AgentEvent::AutoCompactionEnd { .. }
        | AgentEvent::AutoRetryStart { .. }
        | AgentEvent::AutoRetryEnd { .. }
        | AgentEvent::ExtensionError { .. } => return None,
    };

    let payload = serde_json::to_value(event).ok();
    Some((name, payload))
}

/// Cheap extraction of just the extension event name from an agent event,
/// without serializing the payload.  Use this to check `has_hook_for()`
/// before paying the `serde_json::to_value()` cost.
pub const fn extension_event_name_from_agent(event: &AgentEvent) -> Option<ExtensionEventName> {
    match event {
        AgentEvent::AgentStart { .. } => Some(ExtensionEventName::AgentStart),
        AgentEvent::AgentEnd { .. } => Some(ExtensionEventName::AgentEnd),
        AgentEvent::TurnStart { .. } => Some(ExtensionEventName::TurnStart),
        AgentEvent::TurnEnd { .. } => Some(ExtensionEventName::TurnEnd),
        AgentEvent::MessageStart { .. } => Some(ExtensionEventName::MessageStart),
        AgentEvent::MessageUpdate { .. } => Some(ExtensionEventName::MessageUpdate),
        AgentEvent::MessageEnd { .. } => Some(ExtensionEventName::MessageEnd),
        AgentEvent::ToolExecutionStart { .. } => Some(ExtensionEventName::ToolExecutionStart),
        AgentEvent::ToolExecutionUpdate { .. } => Some(ExtensionEventName::ToolExecutionUpdate),
        AgentEvent::ToolExecutionEnd { .. } => Some(ExtensionEventName::ToolExecutionEnd),
        AgentEvent::AutoCompactionStart { .. }
        | AgentEvent::AutoCompactionEnd { .. }
        | AgentEvent::AutoRetryStart { .. }
        | AgentEvent::AutoRetryEnd { .. }
        | AgentEvent::ExtensionError { .. } => None,
    }
}

/// Returns `true` if the given event is fire-and-forget (response is discarded)
/// and can be safely coalesced — i.e. only the most recent version matters.
///
/// Events that can modify agent behavior (tool_call blocking, input
/// transformation) must never be coalesced.
pub const fn is_coalescable_event(event: &ExtensionEventName) -> bool {
    matches!(
        event,
        ExtensionEventName::MessageUpdate | ExtensionEventName::ToolExecutionUpdate
    )
}

/// Returns `true` for agent lifecycle events that are dispatched directly by
/// the agent loop via `AgentSession::dispatch_extension_lifecycle_event`.
///
/// These events must be **excluded** from the event-callback path to avoid
/// double dispatch — the agent loop already sends them individually.
pub const fn is_lifecycle_event(event: &ExtensionEventName) -> bool {
    matches!(
        event,
        ExtensionEventName::AgentStart
            | ExtensionEventName::AgentEnd
            | ExtensionEventName::TurnStart
            | ExtensionEventName::TurnEnd
    )
}

/// Payload wrapper that supports lazy serialization to avoid O(N^2) copying
/// of large message buffers during high-frequency streaming events.
enum CoalescedPayload {
    Lazy(Box<dyn FnOnce() -> Option<Value> + Send>),
}

impl CoalescedPayload {
    fn resolve(self) -> Option<Value> {
        match self {
            Self::Lazy(f) => f(),
        }
    }
}

type EventBatchBuffer = Arc<Mutex<Vec<(ExtensionEventName, CoalescedPayload)>>>;

/// A coalescing dispatcher for fire-and-forget extension events.
///
/// For high-frequency events like `MessageUpdate` (fired per token delta)
/// and `ToolExecutionUpdate`, the coalescer replaces older pending events
/// of the same type so that at most one dispatch per event type is
/// in-flight at any time. Non-coalescable events are never discarded; they
/// are buffered and drained in ordered batches to amortize bridge overhead.
pub struct EventCoalescer {
    manager: ExtensionManager,
    /// For coalescable events, stores the latest pending payload keyed by
    /// event name.  When a dispatch task completes and a newer payload is
    /// waiting, it dispatches the replacement instead of discarding it.
    pending: Arc<Mutex<HashMap<String, CoalescedPayload>>>,
    /// Tracks whether a dispatch task is currently in-flight for a given
    /// coalescable event type.
    in_flight: Arc<Mutex<HashSet<String>>>,
    /// Batch buffer for non-coalescable events.  Events accumulate here and
    /// are dispatched together in a single JS bridge call when the drain
    /// task fires.
    batch_buffer: EventBatchBuffer,
    /// Whether a batch drain task is already scheduled.
    batch_drain_scheduled: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Default, Clone)]
struct CompatRegistrationHints {
    command_names: BTreeSet<String>,
    tool_name_literals: BTreeSet<String>,
    has_tool_registration: bool,
}

impl CompatRegistrationHints {
    fn merge_from(&mut self, other: &Self) {
        self.command_names
            .extend(other.command_names.iter().cloned());
        self.tool_name_literals
            .extend(other.tool_name_literals.iter().cloned());
        self.has_tool_registration |= other.has_tool_registration;
    }

    fn is_empty(&self) -> bool {
        self.command_names.is_empty()
            && self.tool_name_literals.is_empty()
            && !self.has_tool_registration
    }
}

fn register_command_literal_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?m)(?:^|[^\w])(?:pi\.)?registerCommand\s*\(\s*["'`]([^"'`]+)["'`]"#)
            .expect("registerCommand regex")
    })
}

fn register_tool_literal_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?ms)(?:^|[^\w])(?:pi\.)?registerTool\s*\(\s*\{[^{}]*?\bname\s*:\s*["'`]([^"'`]+)["'`]"#,
        )
        .expect("registerTool literal regex")
    })
}

fn collect_compat_registration_hints(paths: &[PathBuf]) -> CompatRegistrationHints {
    let mut hints = CompatRegistrationHints::default();

    for path in paths {
        if !is_supported_js_extension_entry(path) || !path.is_file() {
            continue;
        }
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };

        if source.contains("registerTool(") || source.contains("pi.registerTool(") {
            hints.has_tool_registration = true;
        }

        for captures in register_command_literal_regex().captures_iter(&source) {
            let Some(name) = captures.get(1).map(|m| m.as_str().trim()) else {
                continue;
            };
            if !name.is_empty() {
                hints.command_names.insert(name.to_string());
            }
        }

        for captures in register_tool_literal_regex().captures_iter(&source) {
            let Some(name) = captures.get(1).map(|m| m.as_str().trim()) else {
                continue;
            };
            if !name.is_empty() {
                hints.tool_name_literals.insert(name.to_string());
            }
        }
    }

    hints
}

fn sanitize_tool_name_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_sep = false;

    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('_');
            last_was_sep = true;
        }
    }

    out.trim_matches('_').to_string()
}

fn infer_compat_tool_name(extension_id: &str, hints: &CompatRegistrationHints) -> String {
    if let Some(name) = hints
        .tool_name_literals
        .iter()
        .find(|name| !name.trim().is_empty())
    {
        return name.clone();
    }

    let base = sanitize_tool_name_segment(extension_id);
    if base.is_empty() {
        "extension_compat_tool".to_string()
    } else {
        format!("{base}_compat_tool")
    }
}

fn apply_compat_registration_hints(
    extension_id: &str,
    extension_name: &str,
    tools: &mut Vec<Value>,
    slash_commands: &mut Vec<Value>,
    hints: &CompatRegistrationHints,
) {
    if hints.is_empty() {
        return;
    }

    let mut known_commands = slash_commands
        .iter()
        .filter_map(extract_slash_command_name)
        .map(|name| normalize_command(&name))
        .collect::<BTreeSet<_>>();

    for name in &hints.command_names {
        let normalized = normalize_command(name);
        if !known_commands.insert(normalized) {
            continue;
        }
        slash_commands.push(json!({
            "name": name,
            "description": "Compat-inferred command metadata (static scan fallback)",
            "compatInferred": true,
            "callable": false,
        }));
        tracing::info!(
            event = "ext.compat.command.inferred",
            extension_id = %extension_id,
            command = %name,
            "Added compat inferred slash command metadata"
        );
    }

    if tools.is_empty() && hints.has_tool_registration {
        let inferred_tool_name = infer_compat_tool_name(extension_id, hints);
        tools.push(json!({
            "name": inferred_tool_name,
            "label": format!("{extension_name} (compat)"),
            "description": "Compat-inferred tool metadata (static scan fallback)",
            "compatInferred": true,
            "callable": false,
            "parameters": {
                "type": "object",
                "properties": {},
                "additionalProperties": true,
            }
        }));
        tracing::info!(
            event = "ext.compat.tool.inferred",
            extension_id = %extension_id,
            tool_name = %inferred_tool_name,
            "Added compat inferred tool metadata"
        );
    }
}

fn build_compat_registration_hints(
    specs: &[JsExtensionLoadSpec],
) -> HashMap<String, CompatRegistrationHints> {
    let mut out: HashMap<String, CompatRegistrationHints> = HashMap::new();
    for spec in specs {
        let entry_paths = match discover_related_extension_entries(&spec.entry_path) {
            Ok(entry_paths) => entry_paths,
            Err(err) => {
                tracing::warn!(
                    extension_id = %spec.extension_id,
                    path = %spec.entry_path.display(),
                    error = %err,
                    "Skipping compat hint inference for extension with invalid package manifest"
                );
                continue;
            }
        };
        if entry_paths.is_empty() {
            continue;
        }
        let hints = collect_compat_registration_hints(&entry_paths);
        if hints.is_empty() {
            continue;
        }
        out.entry(spec.extension_id.clone())
            .and_modify(|existing: &mut CompatRegistrationHints| existing.merge_from(&hints))
            .or_insert(hints);
    }
    out
}

fn extract_slash_command_name(value: &Value) -> Option<String> {
    value
        .get("name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

/// Match the bridge's command-name normalization exactly: surrounding
/// whitespace is ignored and at most one leading slash is syntactic sugar.
fn js_command_route_name(name: &str) -> &str {
    let trimmed = name.trim();
    trimmed.strip_prefix('/').unwrap_or(trimmed)
}

fn normalize_command(name: &str) -> String {
    name.trim_start_matches('/').trim().to_ascii_lowercase()
}

fn is_non_callable_compat_inferred(value: &Value) -> bool {
    value
        .get("compatInferred")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !value
            .get("callable")
            .and_then(Value::as_bool)
            .unwrap_or(true)
}

#[cfg(test)]
mod tests;
