//! Conformance test matrix for Pi extensions.
//!
//! Maps `ExtensionCategory × HostCapability → ExpectedBehavior` to produce
//! a concrete test plan.  Each cell in the matrix is a `ConformanceCell`
//! that specifies what the runtime MUST validate for that combination.
//!
//! The matrix is populated from the inclusion list, the API matrix, and the
//! validated manifest so that every extension shape and capability requirement
//! has an explicit test target.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::extension_inclusion::{ExtensionCategory, InclusionEntry, InclusionList};

// ────────────────────────────────────────────────────────────────────────────
// Host capabilities (canonical)
// ────────────────────────────────────────────────────────────────────────────

/// Host capabilities that extensions may require.
///
/// These map 1:1 to the capability taxonomy in EXTENSIONS.md §3.2A.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCapability {
    Read,
    Write,
    Exec,
    Http,
    Session,
    Ui,
    Log,
    Env,
    Tool,
}

impl HostCapability {
    /// Parse a capability string (case-insensitive).
    #[must_use]
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "exec" => Some(Self::Exec),
            "http" => Some(Self::Http),
            "session" => Some(Self::Session),
            "ui" => Some(Self::Ui),
            "log" => Some(Self::Log),
            "env" => Some(Self::Env),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }

    /// All defined capabilities (sorted).
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Read,
            Self::Write,
            Self::Exec,
            Self::Http,
            Self::Session,
            Self::Ui,
            Self::Log,
            Self::Env,
            Self::Tool,
        ]
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Expected behaviors
// ────────────────────────────────────────────────────────────────────────────

/// What the conformance harness MUST verify for a given cell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedBehavior {
    /// Short description of what is being tested.
    pub description: String,
    /// The specific protocol message or hostcall being validated.
    pub protocol_surface: String,
    /// Pass/fail criteria (human-readable).
    pub pass_criteria: String,
    /// Fail criteria (what constitutes a failure).
    pub fail_criteria: String,
}

/// A single cell in the conformance matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceCell {
    /// Extension category (row).
    pub category: ExtensionCategory,
    /// Host capability (column).
    pub capability: HostCapability,
    /// Whether this combination is required (must test) vs optional.
    pub required: bool,
    /// Expected behaviors to validate.
    pub behaviors: Vec<ExpectedBehavior>,
    /// Extensions from the inclusion list that exercise this cell.
    pub exemplar_extensions: Vec<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// Test plan
// ────────────────────────────────────────────────────────────────────────────

/// A fixture assignment linking a conformance cell to concrete test fixtures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureAssignment {
    /// Cell key: `"{category}:{capability}"`.
    pub cell_key: String,
    /// Extension IDs that serve as test fixtures for this cell.
    pub fixture_extensions: Vec<String>,
    /// Minimum number of fixtures required for adequate coverage.
    pub min_fixtures: usize,
    /// Whether the minimum is met.
    pub coverage_met: bool,
}

/// Pass/fail criteria for an extension category.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryCriteria {
    pub category: ExtensionCategory,
    /// What MUST happen for any extension of this category to pass.
    pub must_pass: Vec<String>,
    /// What constitutes a failure.
    pub failure_conditions: Vec<String>,
    /// What is not tested (out of scope).
    pub out_of_scope: Vec<String>,
}

/// The complete test plan document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceTestPlan {
    pub schema: String,
    pub generated_at: String,
    pub task: String,
    /// The matrix: category × capability → cell.
    pub matrix: Vec<ConformanceCell>,
    /// Fixture assignments: which extensions validate which cells.
    pub fixture_assignments: Vec<FixtureAssignment>,
    /// Per-category pass/fail criteria.
    pub category_criteria: Vec<CategoryCriteria>,
    /// Coverage summary.
    pub coverage: CoverageSummary,
}

/// Coverage summary for the test plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageSummary {
    pub total_cells: usize,
    pub required_cells: usize,
    pub covered_cells: usize,
    pub uncovered_required_cells: usize,
    pub total_exemplar_extensions: usize,
    pub categories_covered: usize,
    pub capabilities_covered: usize,
}

// ────────────────────────────────────────────────────────────────────────────
// Matrix builder
// ────────────────────────────────────────────────────────────────────────────

/// API matrix entry from `docs/extension-api-matrix.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiMatrixEntry {
    pub registration_types: Vec<String>,
    pub hostcalls: Vec<String>,
    pub capabilities_required: Vec<String>,
    pub events_listened: Vec<String>,
    pub node_apis: Vec<String>,
    pub third_party_deps: Vec<String>,
}

/// The top-level API matrix document.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiMatrix {
    pub schema: String,
    pub extensions: HashMap<String, ApiMatrixEntry>,
}

/// Build the canonical expected behaviors for a category × capability pair.
#[must_use]
#[allow(clippy::too_many_lines)]
fn build_behaviors(
    category: &ExtensionCategory,
    capability: HostCapability,
) -> Vec<ExpectedBehavior> {
    let mut behaviors = Vec::new();

    // Registration behaviors (universal for all categories)
    if matches!(capability, HostCapability::Log) {
        behaviors.push(ExpectedBehavior {
            description: "Extension load emits structured log".into(),
            protocol_surface: "pi.ext.log.v1".into(),
            pass_criteria: "Load event logged with correct extension_id and schema".into(),
            fail_criteria: "Missing load log or wrong extension_id".into(),
        });
        return behaviors;
    }

    match category {
        ExtensionCategory::Tool => match capability {
            HostCapability::Read => behaviors.push(ExpectedBehavior {
                description: "Tool reads files via pi.tool(read/grep/find/ls)".into(),
                protocol_surface: "host_call(method=tool, name∈{read,grep,find,ls})".into(),
                pass_criteria:
                    "Hostcall completes with correct file content; capability derived as read"
                        .into(),
                fail_criteria: "Hostcall denied, wrong capability derivation, or incorrect content"
                    .into(),
            }),
            HostCapability::Write => behaviors.push(ExpectedBehavior {
                description: "Tool writes/edits files via pi.tool(write/edit)".into(),
                protocol_surface: "host_call(method=tool, name∈{write,edit})".into(),
                pass_criteria: "Hostcall completes; file mutation applied correctly".into(),
                fail_criteria: "Hostcall denied or file not mutated".into(),
            }),
            HostCapability::Exec => behaviors.push(ExpectedBehavior {
                description: "Tool executes commands via pi.exec() or pi.tool(bash)".into(),
                protocol_surface: "host_call(method=exec) or host_call(method=tool, name=bash)"
                    .into(),
                pass_criteria: "Command runs, stdout/stderr/exitCode returned".into(),
                fail_criteria: "Execution denied, timeout without error, or wrong exit code".into(),
            }),
            HostCapability::Http => behaviors.push(ExpectedBehavior {
                description: "Tool makes HTTP requests via pi.http()".into(),
                protocol_surface: "host_call(method=http)".into(),
                pass_criteria: "Request sent, response returned with status/body".into(),
                fail_criteria: "HTTP denied or malformed response".into(),
            }),
            _ => {}
        },
        ExtensionCategory::Command => match capability {
            HostCapability::Ui => behaviors.push(ExpectedBehavior {
                description: "Slash command prompts user via pi.ui.*".into(),
                protocol_surface: "host_call(method=ui, op∈{select,input,confirm})".into(),
                pass_criteria: "UI prompt dispatched and response routed back to handler".into(),
                fail_criteria: "UI call denied in interactive mode or response lost".into(),
            }),
            HostCapability::Session => behaviors.push(ExpectedBehavior {
                description: "Command accesses session state via pi.session.*".into(),
                protocol_surface: "host_call(method=session)".into(),
                pass_criteria: "Session data read/written correctly".into(),
                fail_criteria: "Session call denied or data corrupted".into(),
            }),
            HostCapability::Exec => behaviors.push(ExpectedBehavior {
                description: "Command executes shell commands".into(),
                protocol_surface: "host_call(method=exec)".into(),
                pass_criteria: "Execution succeeds with correct output".into(),
                fail_criteria: "Execution denied or wrong output".into(),
            }),
            _ => {}
        },
        ExtensionCategory::Provider => match capability {
            HostCapability::Http => behaviors.push(ExpectedBehavior {
                description: "Provider streams LLM responses via pi.http()".into(),
                protocol_surface: "host_call(method=http) + streamSimple streaming".into(),
                pass_criteria: "HTTP request to LLM API succeeds; streaming chunks delivered"
                    .into(),
                fail_criteria: "HTTP denied, stream broken, or chunks lost".into(),
            }),
            HostCapability::Read => behaviors.push(ExpectedBehavior {
                description: "Provider reads local config files".into(),
                protocol_surface: "host_call(method=tool, name=read) or pi.fs.read".into(),
                pass_criteria: "Config file read succeeds".into(),
                fail_criteria: "Read denied or file not found".into(),
            }),
            HostCapability::Env => behaviors.push(ExpectedBehavior {
                description: "Provider accesses API keys via process.env".into(),
                protocol_surface: "process.env access (capability=env)".into(),
                pass_criteria: "Environment variable accessible when env capability granted".into(),
                fail_criteria: "Env access denied when capability should be granted".into(),
            }),
            _ => {}
        },
        ExtensionCategory::EventHook => match capability {
            HostCapability::Session => behaviors.push(ExpectedBehavior {
                description: "Event hook reads/modifies session on lifecycle events".into(),
                protocol_surface: "event_hook dispatch + host_call(method=session)".into(),
                pass_criteria: "Hook fires on correct event; session mutations applied".into(),
                fail_criteria: "Hook not fired, wrong event, or session mutation lost".into(),
            }),
            HostCapability::Ui => behaviors.push(ExpectedBehavior {
                description: "Event hook renders UI elements".into(),
                protocol_surface: "event_hook dispatch + host_call(method=ui)".into(),
                pass_criteria: "UI elements rendered after hook fires".into(),
                fail_criteria: "UI call fails or hook not dispatched".into(),
            }),
            HostCapability::Exec => behaviors.push(ExpectedBehavior {
                description: "Event hook executes commands on events".into(),
                protocol_surface: "event_hook dispatch + host_call(method=exec)".into(),
                pass_criteria: "Command execution triggered by event".into(),
                fail_criteria: "Execution denied or event not dispatched".into(),
            }),
            HostCapability::Http => behaviors.push(ExpectedBehavior {
                description: "Event hook makes HTTP requests on events".into(),
                protocol_surface: "event_hook dispatch + host_call(method=http)".into(),
                pass_criteria: "HTTP request sent when event fires".into(),
                fail_criteria: "HTTP denied or event not dispatched".into(),
            }),
            _ => {}
        },
        ExtensionCategory::UiComponent => {
            if matches!(capability, HostCapability::Ui) {
                behaviors.push(ExpectedBehavior {
                    description: "UI component registers message renderer".into(),
                    protocol_surface: "registerMessageRenderer in register payload".into(),
                    pass_criteria: "Renderer registered and callable".into(),
                    fail_criteria: "Renderer not found in registration snapshot".into(),
                });
            }
        }
        ExtensionCategory::Configuration => match capability {
            HostCapability::Ui => behaviors.push(ExpectedBehavior {
                description: "Flag/shortcut activation triggers UI".into(),
                protocol_surface: "register(flags/shortcuts) + host_call(method=ui)".into(),
                pass_criteria: "Flag/shortcut registered; activation dispatches correctly".into(),
                fail_criteria: "Registration missing or activation fails".into(),
            }),
            HostCapability::Session => behaviors.push(ExpectedBehavior {
                description: "Flag modifies session configuration".into(),
                protocol_surface: "register(flags) + host_call(method=session)".into(),
                pass_criteria: "Flag value reflected in session state".into(),
                fail_criteria: "Session state not updated after flag set".into(),
            }),
            _ => {}
        },
        ExtensionCategory::Multi => {
            // Multi-category extensions: behaviors are the union of their constituent types.
            // We add a cross-cutting behavior.
            behaviors.push(ExpectedBehavior {
                description: format!(
                    "Multi-type extension uses {capability:?} across registrations"
                ),
                protocol_surface: format!(
                    "Multiple register types + host_call using {capability:?}"
                ),
                pass_criteria: "All registration types load; capability dispatched correctly"
                    .into(),
                fail_criteria: "Any registration type fails or capability mismatch".into(),
            });
        }
        ExtensionCategory::General => {
            if matches!(capability, HostCapability::Session | HostCapability::Ui) {
                behaviors.push(ExpectedBehavior {
                    description: format!(
                        "General extension uses {capability:?} via export default"
                    ),
                    protocol_surface: format!("export default + host_call(method={capability:?})"),
                    pass_criteria: "Extension loads; hostcall dispatched and returns".into(),
                    fail_criteria: "Load failure or hostcall error".into(),
                });
            }
        }
    }

    // Universal registration behavior for all categories
    if matches!(capability, HostCapability::Tool) && !matches!(category, ExtensionCategory::Tool) {
        // Non-tool extensions that still call tools
        behaviors.push(ExpectedBehavior {
            description: "Extension calls non-core tool via pi.tool()".into(),
            protocol_surface: "host_call(method=tool, name=<non-core>)".into(),
            pass_criteria: "Tool capability check applied; prompt/deny in strict mode".into(),
            fail_criteria: "Tool call bypasses capability check".into(),
        });
    }

    behaviors
}

/// Determine whether a category × capability cell is required.
#[must_use]
const fn is_required_cell(category: &ExtensionCategory, capability: HostCapability) -> bool {
    match category {
        ExtensionCategory::Tool => matches!(
            capability,
            HostCapability::Read
                | HostCapability::Write
                | HostCapability::Exec
                | HostCapability::Http
        ),
        ExtensionCategory::Command => {
            matches!(capability, HostCapability::Ui | HostCapability::Session)
        }
        ExtensionCategory::Provider => {
            matches!(capability, HostCapability::Http | HostCapability::Env)
        }
        ExtensionCategory::EventHook => matches!(
            capability,
            HostCapability::Session | HostCapability::Ui | HostCapability::Exec
        ),
        ExtensionCategory::UiComponent => matches!(capability, HostCapability::Ui),
        ExtensionCategory::Configuration => {
            matches!(capability, HostCapability::Ui | HostCapability::Session)
        }
        ExtensionCategory::Multi => true, // All cells required for multi-type
        ExtensionCategory::General => {
            matches!(capability, HostCapability::Session | HostCapability::Ui)
        }
    }
}

/// Build per-category pass/fail criteria.
#[must_use]
#[allow(clippy::too_many_lines)]
fn build_category_criteria() -> Vec<CategoryCriteria> {
    vec![
        CategoryCriteria {
            category: ExtensionCategory::Tool,
            must_pass: vec![
                "registerTool present in registration snapshot".into(),
                "Tool definition includes name, description, and JSON Schema parameters".into(),
                "tool_call dispatch reaches handler and returns tool_result".into(),
                "Hostcalls use correct capability derivation (read/write/exec per tool name)"
                    .into(),
            ],
            failure_conditions: vec![
                "registerTool missing from snapshot".into(),
                "Tool schema validation fails".into(),
                "tool_call dispatch error or timeout".into(),
                "Capability mismatch between declared and derived".into(),
            ],
            out_of_scope: vec![
                "Tool output correctness beyond protocol conformance".into(),
                "Performance benchmarks (covered by perf harness)".into(),
            ],
        },
        CategoryCriteria {
            category: ExtensionCategory::Command,
            must_pass: vec![
                "registerCommand/registerSlashCommand in registration snapshot".into(),
                "Command definition includes name and description".into(),
                "slash_command dispatch reaches handler and returns slash_result".into(),
                "UI hostcalls (select/input/confirm) dispatch correctly".into(),
            ],
            failure_conditions: vec![
                "Command missing from snapshot".into(),
                "slash_command dispatch fails".into(),
                "UI hostcall denied in interactive mode".into(),
            ],
            out_of_scope: vec!["Command business logic correctness".into()],
        },
        CategoryCriteria {
            category: ExtensionCategory::Provider,
            must_pass: vec![
                "registerProvider in registration snapshot with model entries".into(),
                "streamSimple callable and returns AsyncIterable<string>".into(),
                "HTTP hostcalls dispatched with correct capability".into(),
                "Stream cancellation propagates correctly".into(),
            ],
            failure_conditions: vec![
                "Provider missing from snapshot".into(),
                "streamSimple throws or hangs".into(),
                "HTTP capability not derived correctly".into(),
                "Cancellation does not terminate stream".into(),
            ],
            out_of_scope: vec![
                "LLM response quality".into(),
                "OAuth token refresh (separate test suite)".into(),
            ],
        },
        CategoryCriteria {
            category: ExtensionCategory::EventHook,
            must_pass: vec![
                "Event hooks registered for declared events".into(),
                "Hook fires when event dispatched".into(),
                "Hook can access session/UI/exec hostcalls as declared".into(),
                "Hook errors do not crash the host".into(),
            ],
            failure_conditions: vec![
                "Event hook not registered".into(),
                "Hook does not fire on matching event".into(),
                "Hostcall denied when capability is granted".into(),
                "Hook error propagates as host crash".into(),
            ],
            out_of_scope: vec!["Hook side-effect correctness".into()],
        },
        CategoryCriteria {
            category: ExtensionCategory::UiComponent,
            must_pass: vec![
                "registerMessageRenderer in registration snapshot".into(),
                "Renderer callable with message content".into(),
                "Rendered output is a valid string/markup".into(),
            ],
            failure_conditions: vec![
                "Renderer missing from snapshot".into(),
                "Renderer throws on valid input".into(),
            ],
            out_of_scope: vec!["Visual rendering correctness (requires UI testing)".into()],
        },
        CategoryCriteria {
            category: ExtensionCategory::Configuration,
            must_pass: vec![
                "registerFlag/registerShortcut in registration snapshot".into(),
                "Flag value readable after registration".into(),
                "Shortcut activation dispatches correctly".into(),
            ],
            failure_conditions: vec![
                "Flag/shortcut missing from snapshot".into(),
                "Flag value not persisted".into(),
                "Shortcut activation does not trigger handler".into(),
            ],
            out_of_scope: vec!["Configuration persistence across sessions".into()],
        },
        CategoryCriteria {
            category: ExtensionCategory::Multi,
            must_pass: vec![
                "All declared registration types present in snapshot".into(),
                "Each registration type independently functional".into(),
                "Capabilities correctly derived for each registration type".into(),
            ],
            failure_conditions: vec![
                "Any declared registration type missing".into(),
                "Cross-type interaction causes error".into(),
            ],
            out_of_scope: vec!["Interaction semantics between registration types".into()],
        },
        CategoryCriteria {
            category: ExtensionCategory::General,
            must_pass: vec![
                "Extension loads via export default without error".into(),
                "Hostcalls dispatched correctly when used".into(),
            ],
            failure_conditions: vec![
                "Load throws an error".into(),
                "Hostcall denied when capability is granted".into(),
            ],
            out_of_scope: vec![
                "Extensions with no hostcalls (load-only test is sufficient)".into(),
            ],
        },
    ]
}

/// Determine capabilities for an extension based on its API matrix entry.
#[must_use]
fn capabilities_from_api_entry(entry: &ApiMatrixEntry) -> BTreeSet<HostCapability> {
    let mut caps = BTreeSet::new();
    for cap_str in &entry.capabilities_required {
        if let Some(cap) = HostCapability::from_str_loose(cap_str) {
            caps.insert(cap);
        }
    }
    // Infer from hostcalls
    for hc in &entry.hostcalls {
        if hc.contains("http") {
            caps.insert(HostCapability::Http);
        }
        if hc.contains("exec") {
            caps.insert(HostCapability::Exec);
        }
        if hc.contains("session") {
            caps.insert(HostCapability::Session);
        }
        if hc.contains("ui") {
            caps.insert(HostCapability::Ui);
        }
        if hc.contains("events") {
            caps.insert(HostCapability::Session);
        }
    }
    // Infer from node APIs
    for api in &entry.node_apis {
        match api.as_str() {
            "fs" | "path" => {
                caps.insert(HostCapability::Read);
            }
            "child_process" | "process" => {
                caps.insert(HostCapability::Exec);
            }
            "os" => {
                caps.insert(HostCapability::Env);
            }
            // Pure computation or unknown — no capability needed
            _ => {}
        }
    }
    caps
}

/// Map an extension from the inclusion list to its category.
///
/// Uses the registration types from the API matrix if available, otherwise
/// falls back to the inclusion list category.
#[must_use]
fn category_for_extension(
    entry: &InclusionEntry,
    api_entry: Option<&ApiMatrixEntry>,
) -> ExtensionCategory {
    if let Some(api) = api_entry {
        if !api.registration_types.is_empty() {
            return crate::extension_inclusion::classify_registrations(
                &api.registration_types
                    .iter()
                    .map(|r| format!("register{}", capitalize_first(r)))
                    .collect::<Vec<_>>(),
            );
        }
    }
    entry.category.clone()
}

fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| {
        f.to_uppercase().collect::<String>() + c.as_str()
    })
}

/// Build the full conformance test plan from inclusion list + API matrix.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build_test_plan(
    inclusion: &InclusionList,
    api_matrix: Option<&ApiMatrix>,
    task_id: &str,
) -> ConformanceTestPlan {
    // Collect all included extensions
    let all_entries: Vec<&InclusionEntry> = inclusion
        .tier0
        .iter()
        .chain(inclusion.tier1.iter())
        .chain(inclusion.tier2.iter())
        .collect();

    // Build extension → category + capabilities map
    let mut ext_map: BTreeMap<String, (ExtensionCategory, BTreeSet<HostCapability>)> =
        BTreeMap::new();

    for entry in &all_entries {
        let api_entry = api_matrix.and_then(|m| m.extensions.get(&entry.id));
        let cat = category_for_extension(entry, api_entry);
        let caps = api_entry.map_or_else(BTreeSet::new, capabilities_from_api_entry);
        ext_map.insert(entry.id.clone(), (cat, caps));
    }

    // Build the matrix: for each category × capability, collect exemplars
    let categories = [
        ExtensionCategory::Tool,
        ExtensionCategory::Command,
        ExtensionCategory::Provider,
        ExtensionCategory::EventHook,
        ExtensionCategory::UiComponent,
        ExtensionCategory::Configuration,
        ExtensionCategory::Multi,
        ExtensionCategory::General,
    ];

    let mut matrix = Vec::new();
    let mut fixture_assignments = Vec::new();

    for category in &categories {
        for capability in HostCapability::all() {
            let behaviors = build_behaviors(category, *capability);
            if behaviors.is_empty() {
                continue;
            }

            let required = is_required_cell(category, *capability);

            // Find exemplar extensions
            let exemplars: Vec<String> = ext_map
                .iter()
                .filter(|(_, (cat, caps))| cat == category && caps.contains(capability))
                .map(|(id, _)| id.clone())
                .collect();

            let cell_key = format!("{category:?}:{capability:?}");

            let min_fixtures = if required { 2 } else { 1 };
            let coverage_met = exemplars.len() >= min_fixtures;

            matrix.push(ConformanceCell {
                category: category.clone(),
                capability: *capability,
                required,
                behaviors,
                exemplar_extensions: exemplars.clone(),
            });

            fixture_assignments.push(FixtureAssignment {
                cell_key,
                fixture_extensions: exemplars,
                min_fixtures,
                coverage_met,
            });
        }
    }

    // Build coverage summary
    let total_cells = matrix.len();
    let required_cells = matrix.iter().filter(|c| c.required).count();
    let covered_cells = fixture_assignments
        .iter()
        .filter(|a| a.coverage_met)
        .count();
    let uncovered_required_cells = fixture_assignments
        .iter()
        .filter(|a| {
            !a.coverage_met
                && matrix.iter().any(|c| {
                    format!("{:?}:{:?}", c.category, c.capability) == a.cell_key && c.required
                })
        })
        .count();
    let total_exemplars: BTreeSet<&str> = ext_map.keys().map(String::as_str).collect();
    let categories_covered: std::collections::HashSet<String> = ext_map
        .values()
        .map(|(cat, _)| format!("{cat:?}"))
        .collect();
    let capabilities_covered: BTreeSet<&HostCapability> =
        ext_map.values().flat_map(|(_, caps)| caps.iter()).collect();

    let coverage = CoverageSummary {
        total_cells,
        required_cells,
        covered_cells,
        uncovered_required_cells,
        total_exemplar_extensions: total_exemplars.len(),
        categories_covered: categories_covered.len(),
        capabilities_covered: capabilities_covered.len(),
    };

    let category_criteria = build_category_criteria();

    ConformanceTestPlan {
        schema: "pi.ext.conformance-matrix.v1".to_string(),
        generated_at: crate::extension_validation::chrono_now_iso(),
        task: task_id.to_string(),
        matrix,
        fixture_assignments,
        category_criteria,
        coverage,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_capability_from_str_all_variants() {
        assert_eq!(
            HostCapability::from_str_loose("read"),
            Some(HostCapability::Read)
        );
        assert_eq!(
            HostCapability::from_str_loose("WRITE"),
            Some(HostCapability::Write)
        );
        assert_eq!(
            HostCapability::from_str_loose("Exec"),
            Some(HostCapability::Exec)
        );
        assert_eq!(
            HostCapability::from_str_loose("http"),
            Some(HostCapability::Http)
        );
        assert_eq!(
            HostCapability::from_str_loose("session"),
            Some(HostCapability::Session)
        );
        assert_eq!(
            HostCapability::from_str_loose("ui"),
            Some(HostCapability::Ui)
        );
        assert_eq!(HostCapability::from_str_loose("unknown"), None);
    }

    #[test]
    fn build_behaviors_tool_read() {
        let behaviors = build_behaviors(&ExtensionCategory::Tool, HostCapability::Read);
        assert_eq!(behaviors.len(), 1);
        assert!(behaviors[0].description.contains("reads files"));
    }

    #[test]
    fn build_behaviors_provider_http() {
        let behaviors = build_behaviors(&ExtensionCategory::Provider, HostCapability::Http);
        assert_eq!(behaviors.len(), 1);
        assert!(behaviors[0].description.contains("streams LLM"));
    }

    #[test]
    fn build_behaviors_empty_for_irrelevant_cell() {
        let behaviors = build_behaviors(&ExtensionCategory::UiComponent, HostCapability::Exec);
        assert!(behaviors.is_empty());
    }

    #[test]
    fn is_required_tool_read() {
        assert!(is_required_cell(
            &ExtensionCategory::Tool,
            HostCapability::Read
        ));
    }

    #[test]
    fn is_required_provider_http() {
        assert!(is_required_cell(
            &ExtensionCategory::Provider,
            HostCapability::Http
        ));
    }

    #[test]
    fn not_required_tool_session() {
        assert!(!is_required_cell(
            &ExtensionCategory::Tool,
            HostCapability::Session
        ));
    }

    #[test]
    fn capabilities_from_api_entry_basic() {
        let entry = ApiMatrixEntry {
            registration_types: vec!["tool".into()],
            hostcalls: vec!["pi.http()".into()],
            capabilities_required: vec!["read".into(), "write".into()],
            events_listened: vec![],
            node_apis: vec!["fs".into()],
            third_party_deps: vec![],
        };
        let caps = capabilities_from_api_entry(&entry);
        assert!(caps.contains(&HostCapability::Read));
        assert!(caps.contains(&HostCapability::Write));
        assert!(caps.contains(&HostCapability::Http));
    }

    #[test]
    fn category_criteria_all_categories_covered() {
        let criteria = build_category_criteria();
        assert_eq!(criteria.len(), 8); // All 8 categories
        let cats: Vec<_> = criteria.iter().map(|c| &c.category).collect();
        assert!(cats.contains(&&ExtensionCategory::Tool));
        assert!(cats.contains(&&ExtensionCategory::Provider));
        assert!(cats.contains(&&ExtensionCategory::General));
    }

    #[test]
    fn build_test_plan_empty_inclusion() {
        let inclusion = InclusionList {
            schema: "pi.ext.inclusion.v1".into(),
            generated_at: "2026-01-01T00:00:00Z".into(),
            task: "test".into(),
            stats: crate::extension_inclusion::InclusionStats {
                total_included: 0,
                tier0_count: 0,
                tier1_count: 0,
                tier2_count: 0,
                excluded_count: 0,
                pinned_npm: 0,
                pinned_git: 0,
                pinned_url: 0,
                pinned_checksum_only: 0,
            },
            tier0: vec![],
            tier1: vec![],
            tier2: vec![],
            exclusions: vec![],
            category_coverage: std::collections::HashMap::new(),
        };

        let plan = build_test_plan(&inclusion, None, "test-task");
        assert_eq!(plan.schema, "pi.ext.conformance-matrix.v1");
        assert!(!plan.matrix.is_empty()); // Cells defined even without extensions
        assert_eq!(plan.coverage.total_exemplar_extensions, 0);
    }

    #[test]
    fn capitalize_first_works() {
        assert_eq!(capitalize_first("tool"), "Tool");
        assert_eq!(capitalize_first(""), "");
        assert_eq!(capitalize_first("a"), "A");
    }

    #[test]
    fn host_capability_all_count() {
        assert_eq!(HostCapability::all().len(), 9);
    }

    #[test]
    fn serde_roundtrip_host_capability() {
        let cap = HostCapability::Http;
        let json = serde_json::to_string(&cap).unwrap();
        assert_eq!(json, "\"http\"");
        let back: HostCapability = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cap);
    }

    #[test]
    fn serde_roundtrip_conformance_cell() {
        let cell = ConformanceCell {
            category: ExtensionCategory::Tool,
            capability: HostCapability::Read,
            required: true,
            behaviors: vec![ExpectedBehavior {
                description: "test".into(),
                protocol_surface: "test".into(),
                pass_criteria: "test".into(),
                fail_criteria: "test".into(),
            }],
            exemplar_extensions: vec!["hello".into()],
        };
        let json = serde_json::to_string(&cell).unwrap();
        let back: ConformanceCell = serde_json::from_str(&json).unwrap();
        assert_eq!(back.category, ExtensionCategory::Tool);
        assert!(back.required);
    }
}
