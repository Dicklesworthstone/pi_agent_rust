//! Every surface that drives the agent must route observation events to
//! extensions, and this test exists so the next one cannot quietly forget
//! (bd-82331).
//!
//! ## Why a source-inventory test rather than a behavioural one
//!
//! AgentEvents reach extensions by two routes. The four lifecycle events
//! (`agent_start`, `agent_end`, `turn_start`, `turn_end`) are dispatched from
//! inside the agent loop, so every surface gets them for free. Everything else
//! — `message_start`, `message_update`, `message_end`, `tool_execution_start`,
//! `tool_execution_end` — reaches extensions only if the surface routes its own
//! event stream through `EventCoalescer::dispatch_agent_event_lazy`.
//!
//! That is a per-surface obligation with no compiler support, and two of five
//! surfaces did not meet it: the SDK path, and therefore the default
//! FrankenTUI interactive stack, delivered lifecycle events and nothing else.
//! There was no error and no warning, because the events were never routed
//! rather than dropped in transit. An extension that observes looked installed
//! and did nothing, on the stack most users run.
//!
//! A behavioural parity test across five surfaces would be the stronger check
//! and is worth building, but it needs a real binary under tmux for two of
//! them. This inventory test is deterministic, runs in milliseconds, and
//! catches the specific failure that actually happened: a surface added or
//! rewritten without the routing call. The `reason` field is the point — it
//! converts "nobody noticed" into "someone decided", the same way the
//! module-reachability allowlist does.

use std::fs;
use std::path::{Path, PathBuf};

/// One agent-driving surface and what it does about observation events.
struct Surface {
    /// Human name, used in failure messages.
    name: &'static str,
    /// The file that owns this surface's agent-event callback.
    file: &'static str,
    /// Whether this file is expected to route observation events.
    routes: bool,
    /// Why. Required for both values, because "false" needs justifying and
    /// "true" needs to say where the obligation comes from.
    reason: &'static str,
}

/// The complete set of surfaces that build an agent-event callback.
///
/// Adding a surface means adding a row. A row with `routes: false` must carry a
/// reason that survives review.
const SURFACES: &[Surface] = &[
    Surface {
        name: "print mode",
        file: "src/main.rs",
        routes: true,
        reason: "builds its own coalescer in make_event_handler and dispatches per event",
    },
    Surface {
        name: "RPC / stdin",
        file: "src/rpc.rs",
        routes: true,
        reason: "builds a coalescer and dispatches from both its event paths",
    },
    Surface {
        name: "classic interactive",
        file: "src/interactive/agent.rs",
        routes: true,
        reason: "builds a coalescer at each of its three agent-driving sites",
    },
    Surface {
        name: "SDK handle (and the default ftui stack, which drives it)",
        file: "src/sdk.rs",
        routes: true,
        reason: "routes from make_combined_callback, the single fan-out point every SDK event \
                 travels through, so an SDK-based surface inherits it (bd-82331)",
    },
];

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// The routing call every surface owes its extensions.
const ROUTING_CALL: &str = "dispatch_agent_event_lazy";

#[test]
fn every_agent_driving_surface_routes_observation_events_to_extensions() {
    let root = project_root();
    let mut missing = Vec::new();

    for surface in SURFACES {
        let path = root.join(surface.file);
        let source = read(&path);
        let routes = source.contains(ROUTING_CALL);

        if routes != surface.routes {
            missing.push(format!(
                "  {} ({})\n    declared routes={}, observed routes={}\n    reason on record: {}",
                surface.name, surface.file, surface.routes, routes, surface.reason
            ));
        }
    }

    assert!(
        missing.is_empty(),
        "surface event-routing inventory is out of date. Each surface below either gained or lost \
         its `{ROUTING_CALL}` call without this table being updated.\n\n{}\n\nIf a surface \
         deliberately does not route observation events, set routes=false and write a reason. \
         Extensions on a surface that does not route see the four lifecycle events and nothing \
         else, with no error and no warning (bd-82331).",
        missing.join("\n\n")
    );
}

/// The inventory must be complete, not merely correct about what it lists.
///
/// A new surface that builds a coalescer but is not in the table would pass the
/// test above by not being checked. This catches that: any file under `src/`
/// that constructs an `EventCoalescer` must appear in `SURFACES`.
#[test]
fn the_surface_inventory_lists_every_file_that_builds_a_coalescer() {
    let root = project_root();
    let src = root.join("src");
    let mut builders = Vec::new();
    collect_coalescer_builders(&src, &root, &mut builders);
    builders.sort();

    // The coalescer's own implementation and its re-export are not surfaces.
    // Neither are test modules living under src/: they construct coalescers to
    // exercise them, which is the opposite of owning a user-facing event path.
    // The rule is a path rule rather than a list so a new test module does not
    // trip this and get "fixed" by being added to an allowlist it does not
    // belong in.
    let ignore = [
        "src/extensions.rs",
        "src/extensions/event_coalescer_impl.rs",
        "src/extensions/extension_manager_impl.rs",
    ];
    let is_test_module = |path: &str| path.contains("/tests/") || path.ends_with("_tests.rs");
    let listed: Vec<&str> = SURFACES.iter().map(|s| s.file).collect();

    let unlisted: Vec<String> = builders
        .into_iter()
        .filter(|f| {
            !ignore.contains(&f.as_str()) && !listed.contains(&f.as_str()) && !is_test_module(f)
        })
        .collect();

    assert!(
        unlisted.is_empty(),
        "these files construct an EventCoalescer but are not in the SURFACES table, so their \
         routing is unchecked: {unlisted:?}\n\nAdd a row for each with a reason, or add it to the \
         ignore list if it is not a surface (bd-82331)."
    );
}

fn collect_coalescer_builders(dir: &Path, root: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_coalescer_builders(&path, root, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let source = read(&path);
            if source.contains("EventCoalescer::new") {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.display().to_string());
                }
            }
        }
    }
}

/// A surface that routes must also be able to: routing needs a runtime to
/// dispatch onto, and the SDK path had none (bd-82331).
///
/// `EventCoalescer::dispatch_agent_event_lazy` spawns its batched dispatch onto
/// a runtime handle. Before this bead the SDK built its sessions without one,
/// so installing a coalescer there would have produced routing that could never
/// fire — the same silence in a different place. `SessionOptions` now carries
/// the handle and the ftui driver supplies its own.
#[test]
fn the_sdk_session_can_be_given_a_runtime_to_dispatch_events_on() {
    let root = project_root();
    let sdk = read(&root.join("src/sdk.rs"));
    assert!(
        sdk.contains("pub runtime_handle: Option<asupersync::runtime::RuntimeHandle>"),
        "SessionOptions must carry a runtime handle: without one the coalescer installed on this \
         path has nothing to dispatch onto and extension observation events are silently dropped \
         (bd-82331)"
    );
    assert!(
        sdk.contains("agent_session.with_runtime_handle(handle)"),
        "create_agent_session must install the supplied runtime handle on the session it builds; \
         carrying the option without applying it is the same bug one layer down (bd-82331)"
    );

    let ftui = read(&root.join("src/interactive_ftui.rs"));
    assert!(
        ftui.contains("session_options.runtime_handle = Some(runtime_handle.clone())"),
        "the ftui driver must hand its own runtime to the SDK session, or the default interactive \
         stack routes nothing (bd-82331)"
    );
}
