//! Shared performance-build metadata helpers for benchmark tooling.
//!
//! These helpers keep profile and allocator reporting consistent across
//! benchmark binaries, regression tests, and shell harnesses.

use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Environment variable that overrides benchmark build-profile metadata.
pub const BENCH_BUILD_PROFILE_ENV: &str = "PI_BENCH_BUILD_PROFILE";

/// Environment variable that requests an allocator label for benchmark runs.
pub const BENCH_ALLOCATOR_ENV: &str = "PI_BENCH_ALLOCATOR";

/// Release binary-size budget (MB) shared by perf regression and budget gates.
///
/// Raised 22.0 → 26.0 on 2026-08-16 with the FrankenSQLite cutover
/// (bd-oc1wu): the pure-Rust engine replaces libsqlite3-sys C code and costs
/// ~5.6 MiB of compiled core (parser/planner/VDBE/MVCC/pager) that LTO cannot
/// remove. Reclaim tracked in the follow-up size bead before tightening.
/// Raised 26.0 → 48.0 on 2026-08-21 for the v0.3.0 capability wave (BPE
/// token tables, LSP/DAP bridges, MCP client, eval kernels, web tools);
/// bd tracker holds the re-trim investigation.
pub const BINARY_SIZE_RELEASE_BUDGET_MB: f64 = 48.0;

/// Cargo profile family embedded by `build.rs` (`PROFILE`; custom release-derived
/// profiles are reported by Cargo as `release`).
pub const COMPILED_PROFILE_FAMILY: &str = env!("PI_BUILD_PROFILE_FAMILY");

/// Cargo optimization level embedded by `build.rs` (`OPT_LEVEL`).
pub const COMPILED_OPT_LEVEL: &str = env!("PI_BUILD_OPT_LEVEL");

/// Cargo debug-info switch embedded by `build.rs` (`DEBUG`).
pub const COMPILED_DEBUG: &str = env!("PI_BUILD_DEBUG");

/// Sorted, comma-separated package feature set embedded by `build.rs`.
pub const COMPILED_FEATURES_CSV: &str = env!("PI_BUILD_FEATURES");

/// Exact package feature set for the canonical shipping/system PiJS perf lane.
pub const CANONICAL_PIJS_PERF_FEATURES: &[&str] = &[
    "clipboard",
    "image",
    "image-resize",
    "sqlite-sessions",
    "wasm-host",
];

/// Versioned name for the authoritative Cargo build fingerprint contract.
pub const BUILD_FINGERPRINT_CONTRACT: &str = "cargo_build_fingerprint.v1";

/// Release-binary size measurement control emitted by the perf orchestrator.
pub const BINARY_SIZE_MEASUREMENT_SCHEMA: &str = "pi.perf.binary_size_measurement.v1";

/// Idle-process RSS measurement control consumed by the release budget gate.
pub const IDLE_RSS_MEASUREMENT_SCHEMA: &str = "pi.perf.idle_rss_measurement.v1";

/// Criterion cold-load measurement control emitted by the perf orchestrator.
pub const COLD_LOAD_MEASUREMENT_SCHEMA: &str = "pi.perf.cold_load_measurement.v1";

/// A measurement control can be absent, malformed, or valid-but-too-noisy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeasurementControlError {
    Missing(PathBuf),
    Invalid(String),
    Noisy { observed: u8, maximum: u8 },
}

impl fmt::Display for MeasurementControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(path) => write!(
                formatter,
                "measurement control is missing: {}",
                path.display()
            ),
            Self::Invalid(detail) => write!(formatter, "invalid measurement control: {detail}"),
            Self::Noisy { observed, maximum } => write!(
                formatter,
                "measurement control noise_score={observed} exceeds maximum={maximum}"
            ),
        }
    }
}

/// Verified release-binary size input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBinarySizeMeasurement {
    pub control_path: PathBuf,
    pub control_sha256: String,
    pub source_commit: String,
    pub correlation_id: String,
    pub binary_path: PathBuf,
    pub binary_sha256: String,
    pub size_bytes: u64,
}

/// Verified Criterion cold-load input and its environment fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedColdLoadMeasurement {
    pub control_path: PathBuf,
    pub control_sha256: String,
    pub source_commit: String,
    pub correlation_id: String,
    pub artifact_path: PathBuf,
    pub artifact_sha256: String,
    pub bench_env_sha256: String,
    pub governor: String,
    pub aslr: String,
    pub thp: String,
    pub noise_score: u8,
}

/// Verified idle-process RSS input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdleRssMeasurement {
    pub control_path: PathBuf,
    pub control_sha256: String,
    pub source_commit: String,
    pub correlation_id: String,
    pub pid: u32,
    pub process_name: String,
    pub allocator: String,
    pub binary_path: PathBuf,
    pub binary_sha256: String,
    pub rss_bytes: u64,
    pub sample_count: usize,
    pub rss_spread_bytes: u64,
    pub settle_ms: u64,
    pub bench_env_sha256: String,
    pub governor: String,
    pub noise_score: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BinarySizeMeasurementControl {
    schema: String,
    generated_at: String,
    run_id: String,
    correlation_id: String,
    source_commit: String,
    source_dirty: bool,
    binary_path: String,
    binary_sha256: String,
    size_bytes: u64,
    cargo_profile: String,
    compiled_profile_family: String,
    compiled_opt_level: String,
    strip: bool,
    profile_source: String,
    build_command: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColdLoadMeasurementControl {
    schema: String,
    generated_at: String,
    run_id: String,
    correlation_id: String,
    source_commit: String,
    source_dirty: bool,
    benchmark_exit_code: i32,
    max_noise_score: u8,
    bench_env_source: String,
    status: String,
    reason: Option<String>,
    bench_env: Option<BenchEnvMeasurementControl>,
    #[serde(default)]
    bench_env_sha256: Option<String>,
    measurements: BTreeMap<String, ColdLoadArtifactControl>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct BenchEnvMeasurementControl {
    os: String,
    arch: String,
    cpu_brand: String,
    cpu_cores: usize,
    mem_total_mb: u64,
    governor: String,
    turbo_boost: String,
    aslr: String,
    thp: String,
    noise_score: u8,
    config_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColdLoadArtifactControl {
    #[serde(rename = "artifact_path")]
    path: String,
    #[serde(rename = "artifact_sha256")]
    sha256: String,
    #[serde(rename = "artifact_size_bytes")]
    size_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdleRssMeasurementControl {
    schema: String,
    generated_at: String,
    run_id: String,
    correlation_id: String,
    source_commit: String,
    source_dirty: bool,
    pid: u32,
    process_name: String,
    allocator: String,
    binary_path: String,
    binary_sha256: String,
    rss_bytes: u64,
    idle_state: String,
    cargo_profile: String,
    build_command: String,
    sample_count: usize,
    samples: Vec<IdleRssSampleControl>,
    rss_spread_bytes: u64,
    settle_ms: u64,
    bench_env_source: String,
    bench_env: BenchEnvMeasurementControl,
    bench_env_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdleRssSampleControl {
    pid: u32,
    process_name: String,
    rss_bytes: u64,
}

/// Independent build assertions carried by benchmark provenance.
#[derive(Debug, Clone, Copy)]
pub struct BenchmarkBuildVerification {
    pub executable_profile: bool,
    pub build_fingerprint: bool,
    pub build_profile: bool,
}

/// Inputs covered by the benchmark provenance configuration hash.
#[derive(Debug, Clone, Copy)]
pub struct BenchmarkProvenance<'a> {
    pub source_commit: &'a str,
    pub source_dirty: bool,
    pub build_profile: &'a str,
    pub executable_build_profile: &'a str,
    pub verification: BenchmarkBuildVerification,
    pub build_fingerprint_contract: &'a str,
    pub compiled_profile_family: &'a str,
    pub compiled_opt_level: &'a str,
    pub compiled_debug: &'a str,
    pub compiled_features: &'a [&'a str],
    pub binary_path: &'a str,
    pub binary_sha256: &'a str,
    pub debug_assertions: bool,
}

/// Effective allocator compiled into the current binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocatorKind {
    /// The platform/system allocator.
    System,
    /// `tikv-jemallocator` via the `jemalloc` Cargo feature.
    Jemalloc,
}

impl AllocatorKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Jemalloc => "jemalloc",
        }
    }
}

/// Benchmark allocator selection metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatorSelection {
    /// Requested allocator token (normalized).
    pub requested: String,
    /// Source of `requested` (`env` or `default`).
    pub requested_source: &'static str,
    /// Effective allocator compiled into this binary.
    pub effective: AllocatorKind,
    /// Optional explanation when request/effective do not match.
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestedAllocator {
    Auto,
    System,
    Jemalloc,
    Unknown,
}

/// Returns the allocator compiled into the current binary.
#[must_use]
pub const fn compiled_allocator() -> AllocatorKind {
    if cfg!(all(
        feature = "jemalloc",
        any(target_os = "linux", target_os = "macos")
    )) {
        AllocatorKind::Jemalloc
    } else {
        AllocatorKind::System
    }
}

/// Resolves benchmark allocator metadata from [`BENCH_ALLOCATOR_ENV`].
#[must_use]
pub fn resolve_bench_allocator() -> AllocatorSelection {
    let raw_value = std::env::var(BENCH_ALLOCATOR_ENV).ok();
    resolve_bench_allocator_from(raw_value.as_deref())
}

/// Resolves benchmark allocator metadata from an optional raw token.
#[must_use]
pub fn resolve_bench_allocator_from(raw_value: Option<&str>) -> AllocatorSelection {
    let requested_raw = raw_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(|| "auto".to_string(), str::to_ascii_lowercase);
    let requested_source = if raw_value
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        "env"
    } else {
        "default"
    };

    let requested_kind = match requested_raw.as_str() {
        "auto" | "default" => RequestedAllocator::Auto,
        "system" | "native" => RequestedAllocator::System,
        "jemalloc" | "je" => RequestedAllocator::Jemalloc,
        _ => RequestedAllocator::Unknown,
    };

    let effective = compiled_allocator();
    let fallback_reason = match requested_kind {
        RequestedAllocator::System if effective == AllocatorKind::Jemalloc => {
            Some("system requested but binary was built with --features jemalloc".to_string())
        }
        RequestedAllocator::Jemalloc if effective != AllocatorKind::Jemalloc => {
            Some("jemalloc requested but this target/build uses the system allocator".to_string())
        }
        RequestedAllocator::Unknown => Some(format!(
            "unknown allocator '{requested_raw}'; using compiled allocator '{}'",
            effective.as_str()
        )),
        RequestedAllocator::Auto | RequestedAllocator::System | RequestedAllocator::Jemalloc => {
            None
        }
    };

    let requested = match requested_kind {
        RequestedAllocator::System => "system".to_string(),
        RequestedAllocator::Jemalloc => "jemalloc".to_string(),
        RequestedAllocator::Auto => "auto".to_string(),
        RequestedAllocator::Unknown => requested_raw,
    };

    AllocatorSelection {
        requested,
        requested_source,
        effective,
        fallback_reason,
    }
}

/// Detects the benchmark build profile for reporting.
#[must_use]
pub fn detect_build_profile() -> String {
    let env_profile = std::env::var(BENCH_BUILD_PROFILE_ENV).ok();
    let current_exe = std::env::current_exe().ok();
    detect_build_profile_from(
        env_profile.as_deref(),
        current_exe.as_deref(),
        cfg!(debug_assertions),
    )
}

/// Detects build profile with injectable dependencies for tests.
#[must_use]
pub fn detect_build_profile_from(
    env_profile: Option<&str>,
    current_exe: Option<&Path>,
    debug_assertions: bool,
) -> String {
    if let Some(value) = env_profile.map(str::trim).filter(|value| !value.is_empty()) {
        return value.to_string();
    }

    if let Some(profile) = current_exe.and_then(profile_from_target_path) {
        return profile;
    }

    if debug_assertions {
        "debug".to_string()
    } else {
        "release".to_string()
    }
}

/// Returns the sorted package features compiled into this binary.
#[must_use]
pub fn compiled_feature_set() -> Vec<&'static str> {
    if COMPILED_FEATURES_CSV.is_empty() {
        Vec::new()
    } else {
        COMPILED_FEATURES_CSV.split(',').collect()
    }
}

/// Returns whether Cargo's authoritative build settings match the custom
/// `perf` profile fingerprint.
///
/// The profile's directory name is deliberately not trusted because Cargo
/// reports release-inheriting custom profiles through `PROFILE=release`.
#[must_use]
pub fn has_canonical_perf_build_fingerprint() -> bool {
    matches_canonical_perf_build_fingerprint(
        COMPILED_PROFILE_FAMILY,
        COMPILED_OPT_LEVEL,
        COMPILED_DEBUG,
    )
}

/// Checks an injected Cargo build fingerprint against the canonical `perf`
/// settings. This is public so evidence consumers and tests use one contract.
#[must_use]
pub fn matches_canonical_perf_build_fingerprint(
    profile_family: &str,
    opt_level: &str,
    debug: &str,
) -> bool {
    profile_family == "release" && opt_level == "3" && debug == "true"
}

/// Returns whether this binary has the exact package features used by the
/// canonical shipping/system PiJS performance lane.
#[must_use]
pub fn has_canonical_pijs_perf_features() -> bool {
    matches_canonical_pijs_perf_features(&compiled_feature_set())
}

/// Checks an injected sorted feature set against the canonical shipping/system
/// PiJS lane.
///
/// `image-resize` also enables the package's implicit `image` feature, so both
/// are intentionally present.
#[must_use]
pub fn matches_canonical_pijs_perf_features(features: &[&str]) -> bool {
    features == CANONICAL_PIJS_PERF_FEATURES
}

/// Computes the lowercase SHA-256 digest of a file without loading it all into
/// memory.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(crate::package_manager::hex_encode(&hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    crate::package_manager::hex_encode(&hasher.finalize())
}

fn read_measurement_control<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<(PathBuf, String, T), MeasurementControlError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(MeasurementControlError::Missing(path.to_path_buf()));
        }
        Err(error) => {
            return Err(MeasurementControlError::Invalid(format!(
                "cannot inspect {}: {error}",
                path.display()
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(MeasurementControlError::Invalid(format!(
            "{} must be a regular non-symlink file",
            path.display()
        )));
    }
    let canonical_path = std::fs::canonicalize(path).map_err(|error| {
        MeasurementControlError::Invalid(format!("cannot canonicalize {}: {error}", path.display()))
    })?;
    let bytes = std::fs::read(&canonical_path).map_err(|error| {
        MeasurementControlError::Invalid(format!(
            "cannot read {}: {error}",
            canonical_path.display()
        ))
    })?;
    let control = serde_json::from_slice(&bytes).map_err(|error| {
        MeasurementControlError::Invalid(format!(
            "{} is not the exact expected JSON document: {error}",
            canonical_path.display()
        ))
    })?;
    Ok((canonical_path, sha256_bytes(&bytes), control))
}

fn validate_control_lineage(
    generated_at: &str,
    run_id: &str,
    correlation_id: &str,
    source_commit: &str,
    source_dirty: bool,
) -> Result<(), MeasurementControlError> {
    if chrono::DateTime::parse_from_rfc3339(generated_at).is_err() {
        return Err(MeasurementControlError::Invalid(
            "generated_at must be an RFC3339 timestamp".to_string(),
        ));
    }
    if run_id.is_empty() || run_id != correlation_id {
        return Err(MeasurementControlError::Invalid(
            "run_id and correlation_id must be the same non-empty value".to_string(),
        ));
    }
    if source_commit.len() != 40
        || !source_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || source_commit.bytes().all(|byte| byte == b'0')
    {
        return Err(MeasurementControlError::Invalid(
            "source_commit must be a full lowercase nonzero Git SHA-1".to_string(),
        ));
    }
    if source_dirty {
        return Err(MeasurementControlError::Invalid(
            "source_dirty must be false for release-budget evidence".to_string(),
        ));
    }
    Ok(())
}

fn canonical_regular_path(
    path: &Path,
    field: &str,
) -> Result<PathBuf, MeasurementControlError> {
    if !path.is_absolute() {
        return Err(MeasurementControlError::Invalid(format!(
            "{field} must be absolute"
        )));
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        MeasurementControlError::Invalid(format!("cannot inspect {field}: {error}"))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(MeasurementControlError::Invalid(format!(
            "{field} must identify a regular non-symlink file"
        )));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        MeasurementControlError::Invalid(format!("cannot canonicalize {field}: {error}"))
    })?;
    if canonical != path {
        return Err(MeasurementControlError::Invalid(format!(
            "{field} must already be canonical"
        )));
    }
    Ok(canonical)
}

fn measurement_artifact_path(
    claimed_path: &str,
    relocated_path: Option<&Path>,
    field: &str,
) -> Result<PathBuf, MeasurementControlError> {
    let claimed_path = PathBuf::from(claimed_path);
    if !claimed_path.is_absolute()
        || claimed_path
            .components()
            .any(|component| matches!(component, std::path::Component::CurDir | std::path::Component::ParentDir))
    {
        return Err(MeasurementControlError::Invalid(format!(
            "{field} must claim an absolute normalized producer path"
        )));
    }
    relocated_path.map_or_else(
        || canonical_regular_path(&claimed_path, field),
        |relocated_path| canonical_regular_path(relocated_path, field),
    )
}

fn validate_sha256(value: &str, field: &str) -> Result<(), MeasurementControlError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Ok(());
    }
    Err(MeasurementControlError::Invalid(format!(
        "{field} must be 64 lowercase hexadecimal characters"
    )))
}

/// Verify that the size input names and hashes the exact release binary and
/// proves the shipping profile's size-oriented Cargo settings.
pub fn verify_binary_size_measurement_control(
    control_path: &Path,
) -> Result<VerifiedBinarySizeMeasurement, MeasurementControlError> {
    verify_binary_size_measurement_control_with_relocated_artifact(control_path, None)
}

/// Verify a release-binary control while reading the measured bytes from a
/// digest-identical relocated evidence package. The control's producer path is
/// preserved as provenance and must remain an absolute normalized path.
pub fn verify_binary_size_measurement_control_with_relocated_artifact(
    control_path: &Path,
    relocated_binary_path: Option<&Path>,
) -> Result<VerifiedBinarySizeMeasurement, MeasurementControlError> {
    let (control_path, control_sha256, control): (_, _, BinarySizeMeasurementControl) =
        read_measurement_control(control_path)?;
    if control.schema != BINARY_SIZE_MEASUREMENT_SCHEMA {
        return Err(MeasurementControlError::Invalid(format!(
            "schema must equal {BINARY_SIZE_MEASUREMENT_SCHEMA}"
        )));
    }
    validate_control_lineage(
        &control.generated_at,
        &control.run_id,
        &control.correlation_id,
        &control.source_commit,
        control.source_dirty,
    )?;
    if control.cargo_profile != "release"
        || control.compiled_profile_family != "release"
        || control.compiled_opt_level != "z"
        || !control.strip
        || control.profile_source != "Cargo.toml#profile.release"
        || control.build_command != "cargo build --bin pi --release"
    {
        return Err(MeasurementControlError::Invalid(
            "release binary must prove profile=release, opt-level=z, strip=true, and the canonical build command"
                .to_string(),
        ));
    }
    validate_sha256(&control.binary_sha256, "binary_sha256")?;
    let binary_path = measurement_artifact_path(
        &control.binary_path,
        relocated_binary_path,
        "binary_path",
    )?;
    let metadata = std::fs::metadata(&binary_path).map_err(|error| {
        MeasurementControlError::Invalid(format!("cannot inspect binary_path: {error}"))
    })?;
    if control.size_bytes == 0 || metadata.len() != control.size_bytes {
        return Err(MeasurementControlError::Invalid(format!(
            "size_bytes does not match binary_path (claimed={}, observed={})",
            control.size_bytes,
            metadata.len()
        )));
    }
    let observed_sha256 = sha256_file(&binary_path).map_err(|error| {
        MeasurementControlError::Invalid(format!("cannot hash binary_path: {error}"))
    })?;
    if observed_sha256 != control.binary_sha256 {
        return Err(MeasurementControlError::Invalid(format!(
            "binary_sha256 does not match binary_path (claimed={}, observed={observed_sha256})",
            control.binary_sha256
        )));
    }
    Ok(VerifiedBinarySizeMeasurement {
        control_path,
        control_sha256,
        source_commit: control.source_commit,
        correlation_id: control.correlation_id,
        binary_path,
        binary_sha256: control.binary_sha256,
        size_bytes: control.size_bytes,
    })
}

/// Verify one extension's Criterion cold-load estimate against the exact
/// `benches/bench_env.rs` fingerprint captured by the same orchestrator run.
pub fn verify_cold_load_measurement_control(
    control_path: &Path,
    extension: &str,
) -> Result<VerifiedColdLoadMeasurement, MeasurementControlError> {
    verify_cold_load_measurement_control_with_relocated_artifact(control_path, extension, None)
}

/// Verify a cold-load control against an inventory-bound relocated Criterion
/// estimate while retaining the producer's absolute artifact path in the
/// control document.
pub fn verify_cold_load_measurement_control_with_relocated_artifact(
    control_path: &Path,
    extension: &str,
    relocated_artifact_path: Option<&Path>,
) -> Result<VerifiedColdLoadMeasurement, MeasurementControlError> {
    let (control_path, control_sha256, control): (_, _, ColdLoadMeasurementControl) =
        read_measurement_control(control_path)?;
    if control.schema != COLD_LOAD_MEASUREMENT_SCHEMA {
        return Err(MeasurementControlError::Invalid(format!(
            "schema must equal {COLD_LOAD_MEASUREMENT_SCHEMA}"
        )));
    }
    validate_control_lineage(
        &control.generated_at,
        &control.run_id,
        &control.correlation_id,
        &control.source_commit,
        control.source_dirty,
    )?;
    if control.benchmark_exit_code != 0
        || control.status != "verified"
        || control.reason.is_some()
        || control.bench_env_source != "benches/bench_env.rs"
        || control.max_noise_score != 0
    {
        return Err(MeasurementControlError::Invalid(
            "cold-load control must be a successful verified benches/bench_env.rs run with max_noise_score=0"
                .to_string(),
        ));
    }
    let bench_env = control
        .bench_env
        .ok_or_else(|| MeasurementControlError::Invalid("bench_env must be present".to_string()))?;
    let claimed_bench_env_sha256 = control.bench_env_sha256.ok_or_else(|| {
        MeasurementControlError::Invalid("bench_env_sha256 must be present".to_string())
    })?;
    validate_sha256(&claimed_bench_env_sha256, "bench_env_sha256")?;
    let bench_env = verify_bench_env_measurement(
        bench_env,
        &claimed_bench_env_sha256,
        control.max_noise_score,
    )?;
    let measurement = control.measurements.get(extension).ok_or_else(|| {
        MeasurementControlError::Invalid(format!(
            "measurements must contain extension {extension:?}"
        ))
    })?;
    validate_sha256(&measurement.sha256, "artifact_sha256")?;
    let artifact_path = measurement_artifact_path(
        &measurement.path,
        relocated_artifact_path,
        "artifact_path",
    )?;
    let artifact_metadata = std::fs::metadata(&artifact_path).map_err(|error| {
        MeasurementControlError::Invalid(format!("cannot inspect artifact_path: {error}"))
    })?;
    if measurement.size_bytes == 0 || artifact_metadata.len() != measurement.size_bytes {
        return Err(MeasurementControlError::Invalid(format!(
            "artifact_size_bytes does not match artifact_path (claimed={}, observed={})",
            measurement.size_bytes,
            artifact_metadata.len()
        )));
    }
    let observed_artifact_sha256 = sha256_file(&artifact_path).map_err(|error| {
        MeasurementControlError::Invalid(format!("cannot hash artifact_path: {error}"))
    })?;
    if observed_artifact_sha256 != measurement.sha256 {
        return Err(MeasurementControlError::Invalid(format!(
            "artifact_sha256 does not match artifact_path (claimed={}, observed={observed_artifact_sha256})",
            measurement.sha256
        )));
    }
    Ok(VerifiedColdLoadMeasurement {
        control_path,
        control_sha256,
        source_commit: control.source_commit,
        correlation_id: control.correlation_id,
        artifact_path,
        artifact_sha256: measurement.sha256.clone(),
        bench_env_sha256: claimed_bench_env_sha256,
        governor: bench_env.governor,
        aslr: bench_env.aslr,
        thp: bench_env.thp,
        noise_score: bench_env.noise_score,
    })
}

fn verify_bench_env_measurement(
    bench_env: BenchEnvMeasurementControl,
    claimed_sha256: &str,
    max_noise_score: u8,
) -> Result<BenchEnvMeasurementControl, MeasurementControlError> {
    if bench_env.os.is_empty()
        || bench_env.arch.is_empty()
        || bench_env.cpu_brand.is_empty()
        || bench_env.cpu_cores == 0
        || bench_env.mem_total_mb == 0
        || bench_env.governor.is_empty()
        || bench_env.turbo_boost.is_empty()
        || bench_env.aslr.is_empty()
        || bench_env.thp.is_empty()
    {
        return Err(MeasurementControlError::Invalid(
            "bench_env fields must be complete and non-empty".to_string(),
        ));
    }
    validate_sha256(&bench_env.config_hash, "bench_env.config_hash")?;
    let bench_env_value = serde_json::to_value(&bench_env).map_err(|error| {
        MeasurementControlError::Invalid(format!("cannot serialize bench_env: {error}"))
    })?;
    let bench_env_bytes = serde_json::to_vec(&bench_env_value).map_err(|error| {
        MeasurementControlError::Invalid(format!("cannot canonicalize bench_env: {error}"))
    })?;
    let observed_sha256 = sha256_bytes(&bench_env_bytes);
    if claimed_sha256 != observed_sha256 {
        return Err(MeasurementControlError::Invalid(format!(
            "bench_env_sha256 mismatch (claimed={claimed_sha256}, observed={observed_sha256})"
        )));
    }
    if bench_env.noise_score > max_noise_score {
        return Err(MeasurementControlError::Noisy {
            observed: bench_env.noise_score,
            maximum: max_noise_score,
        });
    }
    Ok(bench_env)
}

fn verify_idle_rss_samples(
    control: &IdleRssMeasurementControl,
) -> Result<(), MeasurementControlError> {
    if control.sample_count < 5 || control.sample_count != control.samples.len() {
        return Err(MeasurementControlError::Invalid(
            "idle RSS control requires sample_count >= 5 matching samples length".to_string(),
        ));
    }
    if !(100..=10_000).contains(&control.settle_ms) {
        return Err(MeasurementControlError::Invalid(
            "idle RSS settle_ms must be in 100..=10000".to_string(),
        ));
    }
    let mut unique_pids = HashSet::with_capacity(control.samples.len());
    let mut min_rss_bytes = u64::MAX;
    let mut max_rss_bytes = 0u64;
    for sample in &control.samples {
        if sample.pid == 0
            || sample.process_name != "pi"
            || sample.rss_bytes == 0
            || !unique_pids.insert(sample.pid)
        {
            return Err(MeasurementControlError::Invalid(
                "idle RSS samples require unique pid>0, process_name=pi, and rss_bytes>0"
                    .to_string(),
            ));
        }
        min_rss_bytes = min_rss_bytes.min(sample.rss_bytes);
        max_rss_bytes = max_rss_bytes.max(sample.rss_bytes);
    }
    if control.rss_bytes != max_rss_bytes
        || control.rss_spread_bytes != max_rss_bytes.saturating_sub(min_rss_bytes)
        || !control.samples.iter().any(|sample| {
            sample.pid == control.pid
                && sample.process_name == control.process_name
                && sample.rss_bytes == control.rss_bytes
        })
    {
        return Err(MeasurementControlError::Invalid(
            "idle RSS aggregate must identify the maximum sample and exact max-minus-min spread"
                .to_string(),
        ));
    }
    Ok(())
}

/// Verify that idle RSS was sampled from a real `pi` process and remains
/// bound to the exact measured executable and allocator.
pub fn verify_idle_rss_measurement_control(
    control_path: &Path,
) -> Result<VerifiedIdleRssMeasurement, MeasurementControlError> {
    verify_idle_rss_measurement_control_with_relocated_artifact(control_path, None)
}

/// Verify an idle-RSS control while resolving its measured executable from an
/// inventory-bound relocated evidence package.
pub fn verify_idle_rss_measurement_control_with_relocated_artifact(
    control_path: &Path,
    relocated_binary_path: Option<&Path>,
) -> Result<VerifiedIdleRssMeasurement, MeasurementControlError> {
    let (control_path, control_sha256, control): (_, _, IdleRssMeasurementControl) =
        read_measurement_control(control_path)?;
    if control.schema != IDLE_RSS_MEASUREMENT_SCHEMA {
        return Err(MeasurementControlError::Invalid(format!(
            "schema must equal {IDLE_RSS_MEASUREMENT_SCHEMA}"
        )));
    }
    validate_control_lineage(
        &control.generated_at,
        &control.run_id,
        &control.correlation_id,
        &control.source_commit,
        control.source_dirty,
    )?;
    if control.pid == 0
        || control.process_name != "pi"
        || !matches!(control.allocator.as_str(), "system" | "jemalloc")
        || control.rss_bytes == 0
        || control.idle_state != "startup_before_user_input"
        || control.cargo_profile != "release"
        || control.build_command != "cargo build --bin pi --release"
    {
        return Err(MeasurementControlError::Invalid(
            "idle RSS control requires a release-built pi process, a known allocator, rss_bytes>0, and the startup_before_user_input boundary"
                .to_string(),
        ));
    }
    verify_idle_rss_samples(&control)?;
    if control.bench_env_source != "benches/bench_env.rs" {
        return Err(MeasurementControlError::Invalid(
            "idle RSS bench_env_source must equal benches/bench_env.rs".to_string(),
        ));
    }
    validate_sha256(&control.bench_env_sha256, "bench_env_sha256")?;
    let bench_env = verify_bench_env_measurement(control.bench_env, &control.bench_env_sha256, 7)?;
    validate_sha256(&control.binary_sha256, "binary_sha256")?;
    let binary_path = measurement_artifact_path(
        &control.binary_path,
        relocated_binary_path,
        "binary_path",
    )?;
    if binary_path.file_name().and_then(|name| name.to_str()) != Some("pi") {
        return Err(MeasurementControlError::Invalid(
            "idle RSS binary_path must identify the pi executable".to_string(),
        ));
    }
    let observed_binary_sha256 = sha256_file(&binary_path).map_err(|error| {
        MeasurementControlError::Invalid(format!("cannot hash binary_path: {error}"))
    })?;
    if observed_binary_sha256 != control.binary_sha256 {
        return Err(MeasurementControlError::Invalid(format!(
            "binary_sha256 does not match binary_path (claimed={}, observed={observed_binary_sha256})",
            control.binary_sha256
        )));
    }
    Ok(VerifiedIdleRssMeasurement {
        control_path,
        control_sha256,
        source_commit: control.source_commit,
        correlation_id: control.correlation_id,
        pid: control.pid,
        process_name: control.process_name,
        allocator: control.allocator,
        binary_path,
        binary_sha256: control.binary_sha256,
        rss_bytes: control.rss_bytes,
        sample_count: control.sample_count,
        rss_spread_bytes: control.rss_spread_bytes,
        settle_ms: control.settle_ms,
        bench_env_sha256: control.bench_env_sha256,
        governor: bench_env.governor,
        noise_score: bench_env.noise_score,
    })
}

/// Hashes asserted build/source/binary provenance as compact canonical JSON.
///
/// Evidence producers and consumers share this helper so field omissions or
/// serialization-order drift fail closed.
#[must_use]
pub fn benchmark_provenance_config_hash(provenance: &BenchmarkProvenance<'_>) -> String {
    let canonical = serde_json::json!({
        "binary_path": provenance.binary_path,
        "binary_sha256": provenance.binary_sha256,
        "build_fingerprint_contract": provenance.build_fingerprint_contract,
        "build_fingerprint_verified": provenance.verification.build_fingerprint,
        "build_profile": provenance.build_profile,
        "build_profile_verified": provenance.verification.build_profile,
        "compiled_debug": provenance.compiled_debug,
        "compiled_features": provenance.compiled_features,
        "compiled_opt_level": provenance.compiled_opt_level,
        "compiled_profile_family": provenance.compiled_profile_family,
        "debug_assertions": provenance.debug_assertions,
        "executable_build_profile": provenance.executable_build_profile,
        "executable_profile_verified": provenance.verification.executable_profile,
        "source_commit": provenance.source_commit,
        "source_dirty": provenance.source_dirty,
    });
    let bytes = serde_json::to_vec(&canonical)
        .expect("benchmark provenance contains only JSON-serializable primitives");
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    crate::package_manager::hex_encode(&hasher.finalize())
}

/// Attempts to derive the Cargo profile from an executable artifact layout.
///
/// This works with both the default `target/` directory and arbitrary
/// `CARGO_TARGET_DIR` values. Example/test artifacts live one level below the
/// profile in `examples/` or `deps/`; ordinary binaries live directly below it.
#[must_use]
pub fn profile_from_target_path(path: &Path) -> Option<String> {
    let artifact_parent = path.parent()?;
    let artifact_parent_name = artifact_parent.file_name()?.to_str()?;
    let profile_dir = if matches!(artifact_parent_name, "deps" | "examples") {
        artifact_parent.parent()?
    } else {
        artifact_parent
    };
    let candidate = profile_dir.file_name()?.to_str()?.trim();
    if candidate.is_empty() {
        return None;
    }

    Some(candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        AllocatorKind, BENCH_ALLOCATOR_ENV, BenchmarkBuildVerification, BenchmarkProvenance,
        MeasurementControlError, benchmark_provenance_config_hash, detect_build_profile_from,
        matches_canonical_perf_build_fingerprint, matches_canonical_pijs_perf_features,
        profile_from_target_path, resolve_bench_allocator_from,
        verify_binary_size_measurement_control,
        verify_binary_size_measurement_control_with_relocated_artifact,
        verify_cold_load_measurement_control,
        verify_cold_load_measurement_control_with_relocated_artifact,
        verify_idle_rss_measurement_control,
        verify_idle_rss_measurement_control_with_relocated_artifact,
    };
    use std::path::Path;

    const TEST_SOURCE_COMMIT: &str = "1234567890abcdef1234567890abcdef12345678";

    fn write_json(path: &Path, value: &serde_json::Value) {
        std::fs::write(
            path,
            serde_json::to_vec(value).expect("serialize test control"),
        )
        .expect("write test control");
    }

    #[test]
    fn binary_size_control_binds_exact_release_binary_and_profile() {
        let temp = tempfile::tempdir().expect("create test directory");
        let binary_path = temp.path().join("pi");
        std::fs::write(&binary_path, b"shipping release binary").expect("write release binary");
        let binary_path = std::fs::canonicalize(binary_path).expect("canonical binary path");
        let binary_sha256 = super::sha256_file(&binary_path).expect("hash release binary");
        let control_path = temp.path().join("binary-size.json");
        let control = serde_json::json!({
            "schema": super::BINARY_SIZE_MEASUREMENT_SCHEMA,
            "generated_at": "2026-08-24T00:00:00Z",
            "run_id": "test-run",
            "correlation_id": "test-run",
            "source_commit": TEST_SOURCE_COMMIT,
            "source_dirty": false,
            "binary_path": binary_path,
            "binary_sha256": binary_sha256,
            "size_bytes": 23,
            "cargo_profile": "release",
            "compiled_profile_family": "release",
            "compiled_opt_level": "z",
            "strip": true,
            "profile_source": "Cargo.toml#profile.release",
            "build_command": "cargo build --bin pi --release"
        });
        write_json(&control_path, &control);

        let verified = verify_binary_size_measurement_control(&control_path)
            .expect("valid release-binary control");
        assert_eq!(verified.size_bytes, 23);

        let relocated_binary_path = temp.path().join("relocated/pi");
        std::fs::create_dir_all(relocated_binary_path.parent().expect("relocated parent"))
            .expect("create relocated binary directory");
        std::fs::write(&relocated_binary_path, b"shipping release binary")
            .expect("write relocated release binary");
        let relocated_binary_path = std::fs::canonicalize(relocated_binary_path)
            .expect("canonicalize relocated release binary");
        let mut relocated_control = control.clone();
        relocated_control["binary_path"] =
            serde_json::json!("/unavailable/producer/target/release/pi");
        let relocated_control_path = temp.path().join("binary-size-relocated.json");
        write_json(&relocated_control_path, &relocated_control);
        assert!(verify_binary_size_measurement_control(&relocated_control_path).is_err());
        let relocated = verify_binary_size_measurement_control_with_relocated_artifact(
            &relocated_control_path,
            Some(&relocated_binary_path),
        )
        .expect("relocated binary bytes satisfy the producer control");
        assert_eq!(relocated.binary_path, relocated_binary_path);

        std::fs::write(&binary_path, b"tampered release binary").expect("tamper release binary");
        assert!(matches!(
            verify_binary_size_measurement_control(&control_path),
            Err(MeasurementControlError::Invalid(_))
        ));
    }

    #[test]
    fn cold_load_control_rejects_noisy_or_tampered_criterion_input() {
        let temp = tempfile::tempdir().expect("create test directory");
        let artifact_path = temp.path().join("estimates.json");
        std::fs::write(&artifact_path, br#"{"mean":{"point_estimate":1000000}}"#)
            .expect("write Criterion estimate");
        let artifact_path = std::fs::canonicalize(artifact_path).expect("canonical Criterion path");
        let artifact_sha256 = super::sha256_file(&artifact_path).expect("hash Criterion estimate");
        let bench_env = serde_json::json!({
            "os": "linux",
            "arch": "x86_64",
            "cpu_brand": "fixture cpu",
            "cpu_cores": 8,
            "mem_total_mb": 16384,
            "governor": "performance",
            "turbo_boost": "disabled",
            "aslr": "enabled",
            "thp": "never",
            "noise_score": 0,
            "config_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });
        let bench_env_sha256 = super::sha256_bytes(
            &serde_json::to_vec(&bench_env).expect("serialize benchmark environment"),
        );
        let control_path = temp.path().join("cold-load.json");
        let mut control = serde_json::json!({
            "schema": super::COLD_LOAD_MEASUREMENT_SCHEMA,
            "generated_at": "2026-08-24T00:00:00Z",
            "run_id": "test-run",
            "correlation_id": "test-run",
            "source_commit": TEST_SOURCE_COMMIT,
            "source_dirty": false,
            "benchmark_exit_code": 0,
            "max_noise_score": 0,
            "bench_env_source": "benches/bench_env.rs",
            "status": "verified",
            "reason": null,
            "bench_env": bench_env,
            "bench_env_sha256": bench_env_sha256,
            "measurements": {
                "hello": {
                    "artifact_path": artifact_path,
                    "artifact_sha256": artifact_sha256,
                    "artifact_size_bytes": 35
                }
            }
        });
        write_json(&control_path, &control);

        verify_cold_load_measurement_control(&control_path, "hello")
            .expect("quiet, bound Criterion control");

        let relocated_artifact_path = temp.path().join("relocated/estimates.json");
        std::fs::create_dir_all(relocated_artifact_path.parent().expect("relocated parent"))
            .expect("create relocated Criterion directory");
        std::fs::write(
            &relocated_artifact_path,
            br#"{"mean":{"point_estimate":1000000}}"#,
        )
        .expect("write relocated Criterion estimate");
        let relocated_artifact_path = std::fs::canonicalize(relocated_artifact_path)
            .expect("canonicalize relocated Criterion estimate");
        let mut relocated_control = control.clone();
        relocated_control["measurements"]["hello"]["artifact_path"] =
            serde_json::json!("/unavailable/producer/target/criterion/estimates.json");
        let relocated_control_path = temp.path().join("cold-load-relocated.json");
        write_json(&relocated_control_path, &relocated_control);
        assert!(
            verify_cold_load_measurement_control(&relocated_control_path, "hello").is_err()
        );
        let relocated = verify_cold_load_measurement_control_with_relocated_artifact(
            &relocated_control_path,
            "hello",
            Some(&relocated_artifact_path),
        )
        .expect("relocated Criterion bytes satisfy the producer control");
        assert_eq!(relocated.artifact_path, relocated_artifact_path);

        control["bench_env"]["noise_score"] = serde_json::json!(1);
        let noisy_env_sha256 = super::sha256_bytes(
            &serde_json::to_vec(&control["bench_env"]).expect("serialize noisy environment"),
        );
        control["bench_env_sha256"] = serde_json::json!(noisy_env_sha256);
        write_json(&control_path, &control);
        assert_eq!(
            verify_cold_load_measurement_control(&control_path, "hello"),
            Err(MeasurementControlError::Noisy {
                observed: 1,
                maximum: 0
            })
        );

        control["bench_env"]["noise_score"] = serde_json::json!(0);
        let quiet_env_sha256 = super::sha256_bytes(
            &serde_json::to_vec(&control["bench_env"]).expect("serialize quiet environment"),
        );
        control["bench_env_sha256"] = serde_json::json!(quiet_env_sha256);
        write_json(&control_path, &control);
        std::fs::write(&artifact_path, br#"{"mean":{"point_estimate":2000000}}"#)
            .expect("tamper Criterion estimate");
        assert!(matches!(
            verify_cold_load_measurement_control(&control_path, "hello"),
            Err(MeasurementControlError::Invalid(_))
        ));
    }

    #[test]
    fn idle_rss_control_binds_pi_process_allocator_and_executable() {
        let temp = tempfile::tempdir().expect("create test directory");
        let binary_path = temp.path().join("pi");
        std::fs::write(&binary_path, b"measured pi executable").expect("write Pi executable");
        let binary_path = std::fs::canonicalize(binary_path).expect("canonical Pi path");
        let binary_sha256 = super::sha256_file(&binary_path).expect("hash Pi executable");
        let control_path = temp.path().join("idle-rss.json");
        let bench_env = serde_json::json!({
            "os": "Linux",
            "arch": "x86_64",
            "cpu_brand": "fixture cpu",
            "cpu_cores": 8,
            "mem_total_mb": 16_384,
            "governor": "performance",
            "turbo_boost": "disabled",
            "aslr": "full",
            "thp": "never",
            "noise_score": 1,
            "config_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });
        let typed_bench_env: super::BenchEnvMeasurementControl =
            serde_json::from_value(bench_env.clone()).expect("parse benchmark environment");
        let bench_env_sha256 = super::sha256_bytes(
            &serde_json::to_vec(
                &serde_json::to_value(&typed_bench_env).expect("normalize benchmark environment"),
            )
            .expect("serialize benchmark environment"),
        );
        let mut control = serde_json::json!({
            "schema": super::IDLE_RSS_MEASUREMENT_SCHEMA,
            "generated_at": "2026-08-24T00:00:00Z",
            "run_id": "test-run",
            "correlation_id": "test-run",
            "source_commit": TEST_SOURCE_COMMIT,
            "source_dirty": false,
            "pid": 4242,
            "process_name": "pi",
            "allocator": "system",
            "binary_path": binary_path,
            "binary_sha256": binary_sha256,
            "rss_bytes": 1_572_864,
            "idle_state": "startup_before_user_input",
            "cargo_profile": "release",
            "build_command": "cargo build --bin pi --release",
            "sample_count": 5,
            "samples": [
                {"pid": 4240, "process_name": "pi", "rss_bytes": 1_048_576},
                {"pid": 4241, "process_name": "pi", "rss_bytes": 1_179_648},
                {"pid": 4242, "process_name": "pi", "rss_bytes": 1_572_864},
                {"pid": 4243, "process_name": "pi", "rss_bytes": 1_310_720},
                {"pid": 4244, "process_name": "pi", "rss_bytes": 1_441_792}
            ],
            "rss_spread_bytes": 524_288,
            "settle_ms": 1_000,
            "bench_env_source": "benches/bench_env.rs",
            "bench_env": bench_env,
            "bench_env_sha256": bench_env_sha256
        });
        write_json(&control_path, &control);

        let verified =
            verify_idle_rss_measurement_control(&control_path).expect("valid idle RSS control");
        assert_eq!(verified.pid, 4242);
        assert_eq!(verified.sample_count, 5);
        assert_eq!(verified.rss_spread_bytes, 524_288);

        let relocated_binary_path = temp.path().join("relocated/pi");
        std::fs::create_dir_all(relocated_binary_path.parent().expect("relocated parent"))
            .expect("create relocated idle-RSS binary directory");
        std::fs::write(&relocated_binary_path, b"measured pi executable")
            .expect("write relocated idle-RSS binary");
        let relocated_binary_path = std::fs::canonicalize(relocated_binary_path)
            .expect("canonicalize relocated idle-RSS binary");
        let mut relocated_control = control.clone();
        relocated_control["binary_path"] =
            serde_json::json!("/unavailable/producer/target/release/pi");
        let relocated_control_path = temp.path().join("idle-rss-relocated.json");
        write_json(&relocated_control_path, &relocated_control);
        assert!(verify_idle_rss_measurement_control(&relocated_control_path).is_err());
        let relocated = verify_idle_rss_measurement_control_with_relocated_artifact(
            &relocated_control_path,
            Some(&relocated_binary_path),
        )
        .expect("relocated idle-RSS binary bytes satisfy the producer control");
        assert_eq!(relocated.binary_path, relocated_binary_path);

        control["process_name"] = serde_json::json!("cargo-test");
        write_json(&control_path, &control);
        assert!(matches!(
            verify_idle_rss_measurement_control(&control_path),
            Err(MeasurementControlError::Invalid(_))
        ));

        control["process_name"] = serde_json::json!("pi");
        control["sample_count"] = serde_json::json!(4);
        control["samples"] = serde_json::json!([
            {"pid": 4240, "process_name": "pi", "rss_bytes": 1_048_576},
            {"pid": 4241, "process_name": "pi", "rss_bytes": 1_179_648},
            {"pid": 4242, "process_name": "pi", "rss_bytes": 1_572_864},
            {"pid": 4243, "process_name": "pi", "rss_bytes": 1_310_720}
        ]);
        write_json(&control_path, &control);
        assert!(matches!(
            verify_idle_rss_measurement_control(&control_path),
            Err(MeasurementControlError::Invalid(_))
        ));

        control["sample_count"] = serde_json::json!(5);
        control["samples"] = serde_json::json!([
            {"pid": 4240, "process_name": "pi", "rss_bytes": 1_048_576},
            {"pid": 4241, "process_name": "pi", "rss_bytes": 1_179_648},
            {"pid": 4242, "process_name": "pi", "rss_bytes": 1_572_864},
            {"pid": 4243, "process_name": "pi", "rss_bytes": 1_310_720},
            {"pid": 4244, "process_name": "pi", "rss_bytes": 1_441_792}
        ]);
        control["bench_env_sha256"] = serde_json::json!("0".repeat(64));
        write_json(&control_path, &control);
        assert!(matches!(
            verify_idle_rss_measurement_control(&control_path),
            Err(MeasurementControlError::Invalid(_))
        ));
    }

    #[test]
    fn detect_build_profile_prefers_env_override() {
        let profile = detect_build_profile_from(Some("perf"), None, true);
        assert_eq!(profile, "perf");
    }

    #[test]
    fn detect_build_profile_from_target_path_detects_profile() {
        let path = Path::new("/tmp/repo/target/perf/pijs_workload");
        let profile = detect_build_profile_from(None, Some(path), true);
        assert_eq!(profile, "perf");
    }

    #[test]
    fn detect_build_profile_falls_back_to_debug_or_release() {
        assert_eq!(detect_build_profile_from(None, None, true), "debug");
        assert_eq!(detect_build_profile_from(None, None, false), "release");
    }

    #[test]
    fn profile_from_target_path_detects_release_deps_binary() {
        let path = Path::new("/tmp/repo/target/release/deps/pijs_workload-abc123");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("release"));
    }

    #[test]
    fn profile_from_target_path_detects_perf_example_binary() {
        let path = Path::new("/tmp/repo/target/perf/examples/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("perf"));
    }

    #[test]
    fn profile_from_target_path_detects_cross_target_perf_example_binary() {
        let path =
            Path::new("/tmp/repo/target/x86_64-unknown-linux-gnu/perf/examples/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("perf"));
    }

    #[test]
    fn profile_from_target_path_does_not_misclassify_moved_binary_as_perf() {
        let path = Path::new("/tmp/repo/pijs_workload");
        let derived = profile_from_target_path(path);
        assert_eq!(derived.as_deref(), Some("repo"));
        assert_ne!(derived.as_deref(), Some("perf"));
    }

    #[test]
    fn profile_from_target_path_uses_direct_artifact_parent_as_profile_hint() {
        let path = Path::new("/tmp/repo/bin/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("bin"));
    }

    #[test]
    fn profile_from_target_path_supports_arbitrary_cargo_target_dir() {
        let path = Path::new("/tmp/pi-build/perf/examples/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("perf"));
    }

    #[test]
    fn canonical_perf_fingerprint_distinguishes_perf_from_release() {
        assert!(matches_canonical_perf_build_fingerprint(
            "release", "3", "true"
        ));
        assert!(!matches_canonical_perf_build_fingerprint(
            "release", "z", "false"
        ));
        assert!(!matches_canonical_perf_build_fingerprint(
            "release", "3", "false"
        ));
        assert!(!matches_canonical_perf_build_fingerprint(
            "perf", "3", "true"
        ));
    }

    #[test]
    fn canonical_pijs_features_include_transitively_enabled_image_feature() {
        let canonical = [
            "clipboard",
            "image",
            "image-resize",
            "sqlite-sessions",
            "wasm-host",
        ];
        assert!(matches_canonical_pijs_perf_features(&canonical));

        let missing_implicit_image = ["clipboard", "image-resize", "sqlite-sessions", "wasm-host"];
        assert!(!matches_canonical_pijs_perf_features(
            &missing_implicit_image
        ));
    }

    #[test]
    fn benchmark_provenance_hash_binds_every_asserted_field() {
        let features = ["clipboard", "image"];
        let canonical = BenchmarkProvenance {
            source_commit: "0123456789abcdef0123456789abcdef01234567",
            source_dirty: false,
            build_profile: "perf",
            executable_build_profile: "perf",
            verification: BenchmarkBuildVerification {
                executable_profile: true,
                build_fingerprint: true,
                build_profile: true,
            },
            build_fingerprint_contract: "cargo_build_fingerprint.v1",
            compiled_profile_family: "release",
            compiled_opt_level: "3",
            compiled_debug: "true",
            compiled_features: &features,
            binary_path: "/tmp/pi-build/perf/examples/pijs_workload",
            binary_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            debug_assertions: false,
        };
        let first = benchmark_provenance_config_hash(&canonical);
        assert_eq!(first, benchmark_provenance_config_hash(&canonical));

        let dirty = BenchmarkProvenance {
            source_dirty: true,
            ..canonical
        };
        assert_ne!(first, benchmark_provenance_config_hash(&dirty));
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn allocator_unknown_token_fails_closed_to_compiled_allocator() {
        let resolved = resolve_bench_allocator_from(Some("weird"));
        assert_eq!(resolved.requested, "weird");
        assert_eq!(resolved.requested_source, "env");
        assert_eq!(resolved.effective, super::compiled_allocator());
        assert!(resolved.fallback_reason.is_some());
    }

    #[test]
    fn allocator_auto_defaults_to_compiled_allocator() {
        let resolved = resolve_bench_allocator_from(None);
        assert_eq!(resolved.requested, "auto");
        assert_eq!(resolved.requested_source, "default");
        assert_eq!(resolved.effective, super::compiled_allocator());
        assert!(resolved.fallback_reason.is_none());
    }

    #[test]
    fn allocator_jemalloc_request_reports_compile_time_mismatch() {
        let resolved = resolve_bench_allocator_from(Some("jemalloc"));
        assert_eq!(resolved.requested, "jemalloc");
        if super::compiled_allocator() == AllocatorKind::Jemalloc {
            assert_eq!(resolved.effective, AllocatorKind::Jemalloc);
            assert!(resolved.fallback_reason.is_none());
        } else {
            assert_eq!(resolved.effective, AllocatorKind::System);
            assert!(
                resolved.fallback_reason.is_some(),
                "{BENCH_ALLOCATOR_ENV}=jemalloc should report fallback without compiled jemalloc"
            );
        }
    }

    #[test]
    fn allocator_system_request_reports_compile_time_mismatch() {
        let resolved = resolve_bench_allocator_from(Some("system"));
        assert_eq!(resolved.requested, "system");
        if super::compiled_allocator() == AllocatorKind::Jemalloc {
            assert_eq!(resolved.effective, AllocatorKind::Jemalloc);
            assert!(resolved.fallback_reason.is_some());
        } else {
            assert_eq!(resolved.effective, AllocatorKind::System);
            assert!(resolved.fallback_reason.is_none());
        }
    }

    // ── Property tests ──

    mod proptest_perf_build {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn resolve_allocator_effective_is_always_compiled(
                raw_value in prop::option::of("[a-z]{0,20}"),
            ) {
                let resolved = resolve_bench_allocator_from(raw_value.as_deref());
                assert!(
                    resolved.effective == super::super::compiled_allocator(),
                    "effective allocator must always be compiled allocator"
                );
            }

            #[test]
            fn resolve_allocator_known_tokens_have_no_unknown_fallback(
                token in prop::sample::select(vec![
                    "auto", "default", "system", "native", "jemalloc", "je",
                ]),
            ) {
                let resolved = resolve_bench_allocator_from(Some(token));
                // Known tokens never produce "unknown allocator" fallback
                if let Some(reason) = &resolved.fallback_reason {
                    assert!(
                        !reason.starts_with("unknown allocator"),
                        "known token '{token}' should not produce unknown fallback: {reason}"
                    );
                }
            }

            #[test]
            fn resolve_allocator_unknown_tokens_always_have_fallback(
                token in "[a-z]{3,10}".prop_filter(
                    "must not be known",
                    |t| !matches!(t.as_str(), "auto" | "default" | "system" | "native" | "jemalloc" | "je"),
                ),
            ) {
                let resolved = resolve_bench_allocator_from(Some(&token));
                assert!(
                    resolved.fallback_reason.is_some(),
                    "unknown token '{token}' must produce a fallback reason"
                );
                assert!(
                    resolved.requested == token,
                    "unknown token should be passed through as-is"
                );
            }

            #[test]
            fn resolve_allocator_empty_or_whitespace_defaults_to_auto(
                value in prop::sample::select(vec!["", " ", "  ", "\t"]),
            ) {
                let resolved = resolve_bench_allocator_from(Some(value));
                assert!(
                    resolved.requested == "auto",
                    "empty/whitespace should default to 'auto', got '{}'",
                    resolved.requested,
                );
                assert_eq!(resolved.requested_source, "default");
            }

            #[test]
            fn resolve_allocator_none_defaults_to_auto(_dummy in Just(())) {
                let resolved = resolve_bench_allocator_from(None);
                assert_eq!(resolved.requested, "auto");
                assert_eq!(resolved.requested_source, "default");
                assert!(resolved.fallback_reason.is_none());
            }

            #[test]
            fn profile_from_target_path_uses_artifact_parent_for_custom_target_dirs(
                dir in "[a-z]{1,10}",
                binary in "[a-z_]{1,10}",
            ) {
                let path_str = format!("/{dir}/{binary}");
                let path = Path::new(&path_str);
                assert!(
                    profile_from_target_path(path).as_deref() == Some(dir.as_str()),
                    "direct artifact should use its parent as profile: {path_str}"
                );
            }

            #[test]
            fn profile_from_target_path_extracts_profile(
                profile in "[a-z]{3,10}",
                binary in "[a-z_]{3,10}",
            ) {
                let path_str = format!("/repo/target/{profile}/{binary}");
                let path = Path::new(&path_str);
                let result = profile_from_target_path(path);
                assert!(
                    result == Some(profile.clone()),
                    "expected Some(\"{profile}\"), got {result:?} for path {path_str}"
                );
            }

            #[test]
            fn detect_build_profile_env_overrides_all(
                env_val in "[a-z]{1,15}",
            ) {
                let result = detect_build_profile_from(
                    Some(&env_val),
                    Some(Path::new("/target/release/bin")),
                    true,
                );
                assert!(
                    result == env_val,
                    "env override should take priority: expected '{env_val}', got '{result}'"
                );
            }

            #[test]
            fn allocator_kind_as_str_is_stable(
                kind in prop::sample::select(vec![
                    AllocatorKind::System,
                    AllocatorKind::Jemalloc,
                ]),
            ) {
                let s1 = kind.as_str();
                let s2 = kind.as_str();
                assert!(s1 == s2, "as_str must be deterministic");
                assert!(
                    s1 == "system" || s1 == "jemalloc",
                    "as_str must return known value: {s1}"
                );
            }
        }
    }
}
