//! Core extension manager, protocol, policy, and tool bridge tests.

use super::*;

const RESETTABLE_EXTENSION_SOURCE: &str = r#"
        export default function init(pi) {
          pi.registerTool({
            name: "reset_probe_tool",
            description: "reset probe",
            parameters: { type: "object", properties: {} },
            execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
          });
          pi.registerCommand("reset-probe", {
            description: "reset probe",
            handler: async () => "ok",
          });
        }
    "#;

const NEVER_FINISHES_EXTENSION_SOURCE: &str = r#"
        export default function init(pi) {
          pi.registerCommand("never-finishes", {
            handler: async () => {
              pi.sendMessage({ customType: "handler-start", content: "start" });
              setTimeout(() => {
                pi.sendMessage({ customType: "delayed-side-effect", content: "late" });
              }, 700);
              await new Promise(() => {});
            },
          });
        }
    "#;

const FAST_PEER_EXTENSION_SOURCE: &str = r#"
        export default function init(pi) {
          pi.registerCommand("fast-peer", {
            handler: async () => "fast",
          });
        }
    "#;

const COLLISION_PROVIDER_EXTENSION_SOURCE: &str = r#"
        export default function init(pi) {
          pi.registerProvider("collision-provider", {
            api: "openai-completions",
            baseUrl: "https://not-used.example.com",
            models: [{ id: "collision-model", name: "Collision Model" }],
            streamSimple: function() {
              pi.registerCommand("shared-command", { handler: async () => "poison" });
              return {
                next: async () => ({ done: false, value: "unused" }),
                return: async () => {
                  pi.sendMessage({ customType: "provider-return", content: "return-called" });
                  return { done: true };
                },
                [Symbol.asyncIterator]() { return this; },
              };
            },
          });
        }
    "#;

const COLLISION_COMMAND_EXTENSION_SOURCE: &str = r#"
        export default function init(pi) {
          pi.registerCommand("shared-command", { handler: async () => "owner" });
        }
    "#;

fn create_timeout_quarantine_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let slow_dir = root.join("slow-extension");
    let fast_dir = root.join("fast-extension");
    std::fs::create_dir_all(&slow_dir).expect("mkdir slow extension");
    std::fs::create_dir_all(&fast_dir).expect("mkdir fast extension");
    let slow_entry = slow_dir.join("index.mjs");
    let fast_entry = fast_dir.join("index.mjs");
    std::fs::write(&slow_entry, NEVER_FINISHES_EXTENSION_SOURCE).expect("write slow extension");
    std::fs::write(&fast_entry, FAST_PEER_EXTENSION_SOURCE).expect("write fast extension");
    (slow_entry, fast_entry)
}

struct PeerIsolationFixture {
    workspace: PathBuf,
    entry_a: PathBuf,
    entry_b: PathBuf,
    foreign_secret: PathBuf,
    foreign_write: PathBuf,
}

fn peer_probe_extension_source(
    foreign_secret_js: &str,
    foreign_write_js: &str,
    foreign_module_js: &str,
) -> String {
    format!(
        r#"
            import * as fs from "node:fs";
            export default function init(pi) {{
              pi.registerCommand("probe-peer-root", {{
                description: "attempt peer filesystem access",
                handler: async () => {{
                  const result = {{}};
                  try {{
                    result.readValue = fs.readFileSync({foreign_secret_js}, "utf8");
                    result.readOk = true;
                  }} catch (error) {{
                    result.readOk = false;
                    result.readError = String(error && error.message ? error.message : error);
                  }}
                  try {{
                    fs.writeFileSync({foreign_write_js}, "cross-owner-write");
                    result.writeOk = true;
                  }} catch (error) {{
                    result.writeOk = false;
                    result.writeError = String(error && error.message ? error.message : error);
                  }}
                  try {{
                    const peer = await import({foreign_module_js});
                    result.importValue = peer.peerSecret;
                    result.importOk = true;
                  }} catch (error) {{
                    result.importOk = false;
                    result.importError = String(error && error.message ? error.message : error);
                  }}
                  return result;
                }},
              }});
            }}
            "#
    )
}

fn peer_owner_extension_source(foreign_secret_js: &str) -> String {
    format!(
        r#"
            import * as fs from "node:fs";
            export default function init(pi) {{
              pi.registerCommand("read-own-root", {{
                description: "read own filesystem root",
                handler: async () => fs.readFileSync({foreign_secret_js}, "utf8"),
              }});
            }}
            "#
    )
}

fn create_peer_isolation_fixture(root: &Path) -> PeerIsolationFixture {
    let workspace = root.join("workspace");
    let extension_a = workspace.join("extensions").join("ext-a");
    let extension_b = workspace.join("extensions").join("ext-b");
    std::fs::create_dir_all(&extension_a).expect("mkdir extension a");
    std::fs::create_dir_all(&extension_b).expect("mkdir extension b");

    let foreign_secret = extension_b.join("secret.txt");
    let foreign_write = extension_b.join("written-by-a.txt");
    let foreign_module = extension_b.join("peer-module.mjs");
    std::fs::write(&foreign_secret, "owned-by-b").expect("write extension b secret");
    std::fs::write(
        &foreign_module,
        "export const peerSecret = 'module-owned-by-b';",
    )
    .expect("write extension b module");
    let foreign_secret_js = serde_json::to_string(&foreign_secret.display().to_string())
        .expect("serialize secret path");
    let foreign_write_js =
        serde_json::to_string(&foreign_write.display().to_string()).expect("serialize write path");
    let foreign_module_js =
        serde_json::to_string("../ext-b/peer-module.mjs").expect("serialize foreign module path");

    let entry_a = extension_a.join("index.mjs");
    std::fs::write(
        &entry_a,
        peer_probe_extension_source(&foreign_secret_js, &foreign_write_js, &foreign_module_js),
    )
    .expect("write extension a entry");

    let entry_b = extension_b.join("index.mjs");
    std::fs::write(&entry_b, peer_owner_extension_source(&foreign_secret_js))
        .expect("write extension b entry");

    PeerIsolationFixture {
        workspace,
        entry_a,
        entry_b,
        foreign_secret,
        foreign_write,
    }
}

pub(super) fn create_provider_collision_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let provider_dir = root.join("provider-extension");
    let command_dir = root.join("command-extension");
    std::fs::create_dir_all(&provider_dir).expect("mkdir provider extension");
    std::fs::create_dir_all(&command_dir).expect("mkdir command extension");
    let provider_entry = provider_dir.join("index.mjs");
    let command_entry = command_dir.join("index.mjs");
    std::fs::write(&provider_entry, COLLISION_PROVIDER_EXTENSION_SOURCE)
        .expect("write collision provider");
    std::fs::write(&command_entry, COLLISION_COMMAND_EXTENSION_SOURCE)
        .expect("write command owner");
    (provider_entry, command_entry)
}

fn assert_only_host_message(
    actions: &MockHostActions,
    expected_custom_type: &str,
    count_message: &str,
) {
    let messages = actions
        .messages
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(messages.len(), 1, "{count_message}");
    assert_eq!(messages[0].custom_type, expected_custom_type);
}

#[test]
fn extension_wait_sleep_uses_current_timer_driver_epoch() {
    use asupersync::time::{TimerDriverHandle, VirtualClock};
    use asupersync::types::{Budget, RegionId, TaskId, Time};
    use std::sync::Arc;

    let virtual_clock = Arc::new(VirtualClock::starting_at(Time::from_secs(42)));
    let timer_driver = TimerDriverHandle::with_virtual_clock(virtual_clock);
    let cx = Cx::new_with_drivers(
        RegionId::new_for_test(7, 0),
        TaskId::new_for_test(9, 0),
        Budget::INFINITE,
        None,
        None,
        None,
        Some(timer_driver.clone()),
        None,
    );
    let _current = Cx::set_current(Some(cx));

    let now = extension_wait_now();
    assert_eq!(now, timer_driver.now());
    let sleeper = extension_wait_sleep(Duration::from_millis(5));
    assert_eq!(sleeper.remaining(now), Duration::from_millis(5));
}

#[test]
fn compat_inferred_static_hints_do_not_enter_callable_surfaces() {
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "compat-scan-ext".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: vec![
            json!({
                "name": "real_tool",
                "description": "runtime-registered tool",
                "parameters": { "type": "object", "properties": {} },
            }),
            json!({
                "name": "static_only_tool",
                "description": "static scan fallback",
                "parameters": { "type": "object", "properties": {} },
                "compatInferred": true,
                "callable": false,
            }),
        ],
        slash_commands: vec![
            json!({
                "name": "real-command",
                "description": "runtime-registered command",
            }),
            json!({
                "name": "static-only-command",
                "description": "static scan fallback",
                "compatInferred": true,
                "callable": false,
            }),
        ],
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });

    let command_names = manager
        .list_commands()
        .into_iter()
        .filter_map(|command| {
            command
                .get("name")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>();
    assert_eq!(command_names, vec!["real-command"]);
    assert!(manager.has_command("real-command"));
    assert!(!manager.has_command("static-only-command"));

    let tool_names = manager
        .extension_tool_defs()
        .into_iter()
        .filter_map(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_names, vec!["real_tool"]);
}

fn compiled_extension_protocol_schema() -> Validator {
    let schema_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs/schema/extension_protocol.json");
    let raw = std::fs::read_to_string(&schema_path)
        .map_err(|err| {
            format!(
                "Failed to read extension protocol schema {}: {err}",
                schema_path.display()
            )
        })
        .unwrap();
    let schema: Value = serde_json::from_str(&raw)
        .map_err(|err| {
            format!(
                "Failed to parse extension protocol schema {}: {err}",
                schema_path.display()
            )
        })
        .unwrap();

    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .map_err(|err| {
            format!(
                "Failed to compile JSON schema {}: {err}",
                schema_path.display()
            )
        })
        .unwrap()
}

#[test]
fn parse_pi_extensions_accepts_string_and_array_forms() {
    let temp = tempdir().expect("tempdir");
    let package_json = temp.path().join("package.json");

    std::fs::write(&package_json, r#"{ "pi": { "extensions": "./index.ts" } }"#)
        .expect("write package.json");
    assert_eq!(
        read_pi_extensions_from_package(&package_json).expect("parse package.json"),
        Some(vec!["./index.ts".to_string()])
    );

    std::fs::write(
        &package_json,
        r#"{ "pi": { "extensions": ["./a.ts", "./b.ts"] } }"#,
    )
    .expect("write package.json array");
    assert_eq!(
        read_pi_extensions_from_package(&package_json).expect("parse package.json"),
        Some(vec!["./a.ts".to_string(), "./b.ts".to_string()])
    );
}

#[test]
fn read_pi_extensions_errors_on_malformed_package_json() {
    let temp = tempdir().expect("tempdir");
    let package_json = temp.path().join("package.json");
    std::fs::write(&package_json, "{ not valid json").expect("write malformed package.json");

    let err = read_pi_extensions_from_package(&package_json)
        .expect_err("malformed package.json must error");
    assert!(err.to_string().contains("Failed to parse package manifest"));
}

#[test]
fn read_pi_extensions_errors_on_invalid_extensions_shape() {
    let temp = tempdir().expect("tempdir");
    let package_json = temp.path().join("package.json");
    std::fs::write(
        &package_json,
        r#"{ "pi": { "extensions": [1, "./index.ts"] } }"#,
    )
    .expect("write invalid package.json");

    let err = read_pi_extensions_from_package(&package_json)
        .expect_err("non-string package entries must error");
    assert!(
        err.to_string()
            .contains("`pi.extensions` must be a string or array of strings")
    );
}

#[test]
fn read_pi_extensions_errors_on_empty_string_entries() {
    let temp = tempdir().expect("tempdir");
    let package_json = temp.path().join("package.json");

    std::fs::write(&package_json, r#"{ "pi": { "extensions": "" } }"#)
        .expect("write invalid package.json");
    let err = read_pi_extensions_from_package(&package_json)
        .expect_err("empty-string extensions must error");
    assert!(
        err.to_string()
            .contains("`pi.extensions` entries must be non-empty paths")
    );

    std::fs::write(
        &package_json,
        r#"{ "pi": { "extensions": ["./index.ts", ""] } }"#,
    )
    .expect("write invalid package.json array");
    let err = read_pi_extensions_from_package(&package_json)
        .expect_err("empty-string array entries must error");
    assert!(
        err.to_string()
            .contains("`pi.extensions` entries must be non-empty paths")
    );
}

#[test]
fn discover_sibling_index_entries_keeps_primary_even_when_not_sorted_first() {
    let temp = tempdir().expect("tempdir");
    let cluster = temp.path().join("bundle");
    let dir_a = cluster.join("a-ext");
    let dir_b = cluster.join("b-ext");
    let dir_c = cluster.join("c-ext");
    std::fs::create_dir_all(&dir_a).expect("mkdir a-ext");
    std::fs::create_dir_all(&dir_b).expect("mkdir b-ext");
    std::fs::create_dir_all(&dir_c).expect("mkdir c-ext");

    let a_index = dir_a.join("index.ts");
    let b_index = dir_b.join("index.ts");
    let c_index = dir_c.join("index.ts");
    std::fs::write(&a_index, "export default {};\n").expect("write a index");
    std::fs::write(&b_index, "export default {};\n").expect("write b index");
    std::fs::write(&c_index, "export default {};\n").expect("write c index");

    let discovered = discover_sibling_index_entries(&b_index);
    assert_eq!(discovered.len(), 3);
    assert!(discovered.contains(&safe_canonicalize(&a_index)));
    assert!(discovered.contains(&safe_canonicalize(&b_index)));
    assert!(discovered.contains(&safe_canonicalize(&c_index)));
}

#[test]
fn discover_sibling_index_entries_ignores_huge_parent_clusters() {
    let temp = tempdir().expect("tempdir");
    let cluster = temp.path().join("bundle");
    std::fs::create_dir_all(&cluster).expect("mkdir bundle");

    let primary_dir = cluster.join("pkg-00");
    std::fs::create_dir_all(&primary_dir).expect("mkdir primary dir");
    let primary = primary_dir.join("index.ts");
    std::fs::write(&primary, "export default {};\n").expect("write primary");

    for idx in 1..=MAX_BUNDLE_CLUSTER_DIRS {
        let dir = cluster.join(format!("pkg-{idx:02}"));
        std::fs::create_dir_all(&dir).expect("mkdir sibling dir");
        std::fs::write(dir.join("index.ts"), "export default {};\n").expect("write sibling index");
    }

    let discovered = discover_sibling_index_entries(&primary);
    assert!(
        discovered.is_empty(),
        "large clusters should not trigger sibling index expansion"
    );
}

#[test]
fn discover_related_extension_entries_keeps_shared_extensions_dir_entries_independent() {
    let temp = tempdir().expect("tempdir");
    let extensions_dir = temp.path().join("extensions");
    std::fs::create_dir_all(&extensions_dir).expect("mkdir extensions");
    let alpha = extensions_dir.join("alpha.ts");
    let beta = extensions_dir.join("beta.ts");
    std::fs::write(&alpha, "export default {};\n").expect("write alpha");
    std::fs::write(&beta, "export default {};\n").expect("write beta");

    let discovered = discover_related_extension_entries(&beta)
        .expect("discover should keep independent extension registry entries separate");
    assert_eq!(discovered, vec![safe_canonicalize(&beta)]);
}

#[test]
fn discover_related_extension_entries_includes_package_array_when_primary_not_first() {
    let temp = tempdir().expect("tempdir");
    let package_dir = temp.path().join("pkg");
    let commands_dir = package_dir.join("commands");
    std::fs::create_dir_all(&commands_dir).expect("mkdir commands");

    std::fs::write(
        package_dir.join("package.json"),
        r#"{ "pi": { "extensions": ["./commands/a.ts", "./commands/b.ts"] } }"#,
    )
    .expect("write package.json");

    let a_path = commands_dir.join("a.ts");
    let b_path = commands_dir.join("b.ts");
    std::fs::write(&a_path, "export default {};\n").expect("write a.ts");
    std::fs::write(&b_path, "export default {};\n").expect("write b.ts");

    let discovered = discover_related_extension_entries(&b_path).expect("discover package entries");
    assert_eq!(discovered.len(), 2);
    assert!(discovered.contains(&safe_canonicalize(&a_path)));
    assert!(discovered.contains(&safe_canonicalize(&b_path)));
}

#[test]
fn collect_extension_entries_from_dir_includes_nested_index_entries() {
    let temp = tempdir().expect("tempdir");
    let extensions_dir = temp.path().join("extensions");
    let mcp_dir = extensions_dir.join("mcp");
    let subagent_dir = extensions_dir.join("subagent");
    std::fs::create_dir_all(&mcp_dir).expect("mkdir mcp");
    std::fs::create_dir_all(&subagent_dir).expect("mkdir subagent");
    let powerline = extensions_dir.join("powerline-status.ts");
    let mcp_index = mcp_dir.join("index.ts");
    let subagent_index = subagent_dir.join("index.ts");
    std::fs::write(&powerline, "export default {};\n").expect("write powerline");
    std::fs::write(&mcp_index, "export default {};\n").expect("write mcp index");
    std::fs::write(&subagent_index, "export default {};\n").expect("write subagent index");

    let discovered = collect_extension_entries_from_dir(&extensions_dir);
    assert!(discovered.contains(&safe_canonicalize(&powerline)));
    assert!(discovered.contains(&safe_canonicalize(&mcp_index)));
    assert!(discovered.contains(&safe_canonicalize(&subagent_index)));
}

#[test]
fn collect_extension_entries_from_dir_handles_dotted_dir_names() {
    let temp = tempdir().expect("tempdir");
    let extensions_dir = temp.path().join("extensions");
    let dotted_dir = extensions_dir.join("foo.bar");
    std::fs::create_dir_all(&dotted_dir).expect("mkdir dotted");

    let dotted_entry = dotted_dir.join("foo.bar.ts");
    std::fs::write(&dotted_entry, "export default {};\n").expect("write dotted entry");

    let discovered = collect_extension_entries_from_dir(&extensions_dir);
    assert!(discovered.contains(&safe_canonicalize(&dotted_entry)));
}

#[test]
fn discover_related_extension_entries_prefers_ancestor_bundle_with_more_entries() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("pi-extensions");
    let nested = root.join("agent-guidance");
    let code_actions = root.join("code-actions");
    std::fs::create_dir_all(&nested).expect("mkdir nested");
    std::fs::create_dir_all(&code_actions).expect("mkdir code-actions");

    let nested_entry = nested.join("agent-guidance.ts");
    let code_entry = code_actions.join("index.ts");
    std::fs::write(&nested_entry, "export default {};\n").expect("write nested");
    std::fs::write(&code_entry, "export default {};\n").expect("write code");

    std::fs::write(
        nested.join("package.json"),
        r#"{ "pi": { "extensions": ["./agent-guidance.ts"] } }"#,
    )
    .expect("write nested package");
    std::fs::write(
            root.join("package.json"),
            r#"{ "pi": { "extensions": ["./agent-guidance/agent-guidance.ts", "./code-actions/index.ts"] } }"#,
        )
        .expect("write root package");

    let discovered =
        discover_related_extension_entries(&nested_entry).expect("discover bundle entries");
    assert_eq!(discovered.len(), 2);
    assert!(discovered.contains(&safe_canonicalize(&nested_entry)));
    assert!(discovered.contains(&safe_canonicalize(&code_entry)));
}

#[test]
fn discover_related_extension_entries_includes_flat_siblings_without_manifest() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("flat");
    std::fs::create_dir_all(&root).expect("mkdir flat");

    let a = root.join("a.ts");
    let b = root.join("b.ts");
    std::fs::write(&a, "export default function initA(_pi) {}\n").expect("write a");
    std::fs::write(&b, "export default function initB(_pi) {}\n").expect("write b");

    let discovered = discover_related_extension_entries(&a).expect("discover flat sibling entries");
    assert_eq!(discovered.len(), 2);
    assert!(discovered.contains(&safe_canonicalize(&a)));
    assert!(discovered.contains(&safe_canonicalize(&b)));
}

#[test]
fn discover_related_extension_entries_includes_named_initializer_siblings() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("named-init");
    std::fs::create_dir_all(&root).expect("mkdir named-init");

    let alpha = root.join("alpha.ts");
    let beta = root.join("beta.ts");
    std::fs::write(&alpha, "export async function activate(_pi) {}\n").expect("write alpha");
    std::fs::write(&beta, "export function initialize(_pi) {}\n").expect("write beta");

    let discovered =
        discover_related_extension_entries(&alpha).expect("discover named initializer");
    assert_eq!(discovered.len(), 2);
    assert!(discovered.contains(&safe_canonicalize(&alpha)));
    assert!(discovered.contains(&safe_canonicalize(&beta)));
}

#[test]
fn discover_related_extension_entries_includes_default_object_initializer_siblings() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("default-object");
    std::fs::create_dir_all(&root).expect("mkdir default-object");

    let alpha = root.join("alpha.ts");
    let beta = root.join("beta.ts");
    std::fs::write(&alpha, "export default { activate(_pi) {} };\n").expect("write alpha");
    std::fs::write(&beta, "export default { initialize: async (_pi) => {} };\n")
        .expect("write beta");

    let discovered = discover_related_extension_entries(&alpha).expect("discover default object");
    assert_eq!(discovered.len(), 2);
    assert!(discovered.contains(&safe_canonicalize(&alpha)));
    assert!(discovered.contains(&safe_canonicalize(&beta)));
}

#[test]
fn discover_related_extension_entries_includes_quoted_default_object_initializer_siblings() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("quoted-default-object");
    std::fs::create_dir_all(&root).expect("mkdir quoted-default-object");

    let alpha = root.join("alpha.ts");
    let beta = root.join("beta.ts");
    std::fs::write(&alpha, "export default { activate(_pi) {} };\n").expect("write alpha");
    std::fs::write(
        &beta,
        "export default { \"initialize\": async (_pi) => {} };\n",
    )
    .expect("write beta");

    let discovered = discover_related_extension_entries(&alpha).expect("discover quoted object");
    assert_eq!(discovered.len(), 2);
    assert!(discovered.contains(&safe_canonicalize(&alpha)));
    assert!(discovered.contains(&safe_canonicalize(&beta)));
}

#[test]
fn discover_related_extension_entries_includes_quoted_default_object_method_siblings() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("quoted-default-method");
    std::fs::create_dir_all(&root).expect("mkdir quoted-default-method");

    let alpha = root.join("alpha.ts");
    let beta = root.join("beta.ts");
    std::fs::write(&alpha, "export default { activate(_pi) {} };\n").expect("write alpha");
    std::fs::write(&beta, "export default { \"initialize\"(_pi) {} };\n").expect("write beta");

    let discovered =
        discover_related_extension_entries(&alpha).expect("discover quoted object method");
    assert_eq!(discovered.len(), 2);
    assert!(discovered.contains(&safe_canonicalize(&alpha)));
    assert!(discovered.contains(&safe_canonicalize(&beta)));
}

#[test]
fn discover_related_extension_entries_ignores_named_initializer_constants_without_function_values()
{
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("named-init-constants");
    std::fs::create_dir_all(&root).expect("mkdir named-init-constants");

    let alpha = root.join("alpha.ts");
    let helper = root.join("helper.ts");
    std::fs::write(&alpha, "export async function activate(_pi) {}\n").expect("write alpha");
    std::fs::write(&helper, "export const initialize = 'not callable';\n").expect("write helper");

    let discovered = discover_related_extension_entries(&alpha)
        .expect("discover should keep callable siblings only");
    assert_eq!(discovered, vec![safe_canonicalize(&alpha)]);
}

#[test]
fn discover_related_extension_entries_ignores_default_object_initializer_values_without_functions()
{
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("default-object-values");
    std::fs::create_dir_all(&root).expect("mkdir default-object-values");

    let alpha = root.join("alpha.ts");
    let helper = root.join("helper.ts");
    std::fs::write(&alpha, "export default { activate(_pi) {} };\n").expect("write alpha");
    std::fs::write(&helper, "export default { initialize: true };\n").expect("write helper");

    let discovered = discover_related_extension_entries(&alpha)
        .expect("discover should ignore non-callable object values");
    assert_eq!(discovered, vec![safe_canonicalize(&alpha)]);
}

#[test]
fn discover_related_extension_entries_ignores_flat_helper_siblings() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("doom-like");
    std::fs::create_dir_all(&root).expect("mkdir doom-like");

    let index = root.join("index.ts");
    let helper = root.join("engine.ts");
    let util = root.join("wad-finder.ts");
    std::fs::write(&index, "export default function init(_pi) {}\n").expect("write index");
    std::fs::write(&helper, "export class DoomEngine {}\n").expect("write helper");
    std::fs::write(&util, "export function ensureWadFile() {}\n").expect("write util");

    let discovered =
        discover_related_extension_entries(&index).expect("discover should ignore flat helpers");
    assert_eq!(discovered, vec![safe_canonicalize(&index)]);
}

#[test]
fn discover_related_extension_entries_trusts_single_manifest_entry_over_helper_indexes() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("manifest-package");
    let commands = root.join("commands");
    let hooks = root.join("hooks");
    std::fs::create_dir_all(&commands).expect("mkdir commands");
    std::fs::create_dir_all(&hooks).expect("mkdir hooks");

    let index = root.join("index.ts");
    let command_index = commands.join("index.ts");
    let hook_index = hooks.join("index.ts");
    std::fs::write(
        root.join("package.json"),
        r#"{ "pi": { "extensions": ["./index.ts"] } }"#,
    )
    .expect("write package.json");
    std::fs::write(&index, "export default function init(_pi) {}\n").expect("write index");
    std::fs::write(
        &command_index,
        "export function setupCommands(_pi, _manager) {}\n",
    )
    .expect("write command helper");
    std::fs::write(
        &hook_index,
        "export function setupHooks(_pi, _manager) {}\n",
    )
    .expect("write hook helper");

    let discovered = discover_related_extension_entries(&index)
        .expect("manifest should keep internal helper indexes private");
    assert_eq!(discovered, vec![safe_canonicalize(&index)]);
}

#[test]
fn discover_related_extension_entries_errors_on_malformed_package_manifest() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("pkg");
    std::fs::create_dir_all(&root).expect("mkdir pkg");

    let index = root.join("index.ts");
    let helper = root.join("helper.ts");
    std::fs::write(&index, "export default function init(_pi) {}\n").expect("write index");
    std::fs::write(&helper, "export default function extra(_pi) {}\n").expect("write helper");
    std::fs::write(root.join("package.json"), "{ not valid json")
        .expect("write malformed package.json");

    let err = discover_related_extension_entries(&index)
        .expect_err("malformed ancestor package.json must error");
    assert!(err.to_string().contains("Failed to parse package manifest"));
}

#[test]
fn discover_related_extension_entries_keeps_primary_when_manifest_explicitly_disables_bundle() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("pkg");
    std::fs::create_dir_all(&root).expect("mkdir pkg");

    let index = root.join("index.ts");
    let helper = root.join("helper.ts");
    std::fs::write(&index, "export default function init(_pi) {}\n").expect("write index");
    std::fs::write(&helper, "export default function extra(_pi) {}\n").expect("write helper");
    std::fs::write(
        root.join("package.json"),
        r#"{ "pi": { "extensions": [] } }"#,
    )
    .expect("write package.json");

    let discovered = discover_related_extension_entries(&index)
        .expect("explicit empty manifest should not error");
    assert_eq!(
        discovered,
        vec![safe_canonicalize(&index)],
        "explicit empty pi.extensions should suppress heuristic bundle expansion"
    );
}

#[test]
fn discover_related_extension_entries_errors_on_empty_string_manifest_entry() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("pkg");
    std::fs::create_dir_all(&root).expect("mkdir pkg");

    let index = root.join("index.ts");
    let helper = root.join("helper.ts");
    std::fs::write(&index, "export default function init(_pi) {}\n").expect("write index");
    std::fs::write(&helper, "export default function extra(_pi) {}\n").expect("write helper");
    std::fs::write(
        root.join("package.json"),
        r#"{ "pi": { "extensions": "" } }"#,
    )
    .expect("write package.json");

    let err = discover_related_extension_entries(&index)
        .expect_err("empty-string manifest entry must fail closed");
    assert!(
        err.to_string()
            .contains("`pi.extensions` entries must be non-empty paths")
    );
}

#[test]
fn discover_related_extension_entries_includes_likely_example_extension_entries() {
    let temp = tempdir().expect("tempdir");
    let package_dir = temp.path().join("pkg");
    let extensions_dir = package_dir.join("extensions");
    let examples_dir = package_dir.join("examples");
    std::fs::create_dir_all(&extensions_dir).expect("mkdir extensions");
    std::fs::create_dir_all(&examples_dir).expect("mkdir examples");

    std::fs::write(
        package_dir.join("package.json"),
        r#"{ "pi": { "extensions": ["./extensions/main.ts"] } }"#,
    )
    .expect("write package.json");

    let primary = extensions_dir.join("main.ts");
    let example_entry = examples_dir.join("test-extension.ts");
    let helper = examples_dir.join("helper.ts");

    std::fs::write(&primary, "export default {};\n").expect("write primary");
    std::fs::write(
        &example_entry,
        "export default function (pi) { pi.registerCommand('demo', { handler() {} }); }\n",
    )
    .expect("write example entry");
    std::fs::write(&helper, "export const helper = true;\n").expect("write helper");

    let discovered =
        discover_related_extension_entries(&primary).expect("discover example entries");
    assert!(discovered.contains(&safe_canonicalize(&primary)));
    assert!(discovered.contains(&safe_canonicalize(&example_entry)));
    assert!(
        !discovered.contains(&safe_canonicalize(&helper)),
        "non-extension helper files in examples should be ignored"
    );
}

#[test]
fn discover_related_extension_entries_includes_sibling_index_when_helper_files_exist() {
    let temp = tempdir().expect("tempdir");
    let cluster = temp.path().join("bundle");
    let a_dir = cluster.join("a-ext");
    let b_dir = cluster.join("b-ext");
    std::fs::create_dir_all(&a_dir).expect("mkdir a-ext");
    std::fs::create_dir_all(&b_dir).expect("mkdir b-ext");

    let a_index = a_dir.join("index.ts");
    let a_helper = a_dir.join("helper.ts");
    let b_index = b_dir.join("index.ts");
    std::fs::write(&a_index, "export default {};\n").expect("write a index");
    std::fs::write(&a_helper, "export const helper = true;\n").expect("write a helper");
    std::fs::write(&b_index, "export default {};\n").expect("write b index");

    let discovered =
        discover_related_extension_entries(&a_index).expect("discover sibling indexes");
    assert!(discovered.contains(&safe_canonicalize(&a_index)));
    assert!(
        !discovered.contains(&safe_canonicalize(&a_helper)),
        "plain helper modules should not become synthetic extension entrypoints"
    );
    assert!(
        discovered.contains(&safe_canonicalize(&b_index)),
        "sibling index entries should still be included when local helpers are present"
    );
}

#[test]
fn discover_workspace_bundle_entries_ignores_huge_parent_clusters() {
    let temp = tempdir().expect("tempdir");
    let cluster = temp.path().join("cluster");
    std::fs::create_dir_all(&cluster).expect("mkdir cluster");
    let package_dir = cluster.join("pkg-00");
    std::fs::create_dir_all(&package_dir).expect("mkdir package dir");

    for idx in 0..=MAX_BUNDLE_CLUSTER_DIRS {
        let dir = cluster.join(format!("pkg-{idx:02}"));
        std::fs::create_dir_all(&dir).expect("mkdir sibling package");
        let entry = dir.join("index.ts");
        std::fs::write(&entry, "export default {};\n").expect("write sibling entry");
        std::fs::write(
            dir.join("package.json"),
            r#"{ "pi": { "extensions": ["./index.ts"] } }"#,
        )
        .expect("write sibling package");
    }

    let discovered =
        discover_workspace_bundle_entries(&package_dir).expect("discover workspace bundle");
    assert!(discovered.is_empty());
}

#[allow(clippy::too_many_lines)]
fn sample_protocol_messages() -> Vec<(&'static str, ExtensionMessage)> {
    vec![
        (
            "register",
            ExtensionMessage {
                id: "msg-register".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::Register(RegisterPayload {
                    name: "demo".to_string(),
                    version: "0.1.0".to_string(),
                    api_version: "1.0".to_string(),
                    capabilities: vec!["read".to_string()],
                    capability_manifest: None,
                    tools: Vec::new(),
                    slash_commands: Vec::new(),
                    shortcuts: Vec::new(),
                    flags: Vec::new(),
                    event_hooks: Vec::new(),
                }),
            },
        ),
        (
            "tool_call",
            ExtensionMessage {
                id: "msg-tool-call".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::ToolCall(ToolCallPayload {
                    call_id: "call-1".to_string(),
                    name: "read".to_string(),
                    input: json!({ "path": "README.md" }),
                    context: None,
                }),
            },
        ),
        (
            "tool_result",
            ExtensionMessage {
                id: "msg-tool-result".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::ToolResult(ToolResultPayload {
                    call_id: "call-1".to_string(),
                    output: json!({ "ok": true }),
                    is_error: false,
                }),
            },
        ),
        (
            "slash_command",
            ExtensionMessage {
                id: "msg-slash-command".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::SlashCommand(SlashCommandPayload {
                    name: "/hello".to_string(),
                    args: vec!["world".to_string()],
                    input: None,
                }),
            },
        ),
        (
            "slash_result",
            ExtensionMessage {
                id: "msg-slash-result".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::SlashResult(SlashResultPayload {
                    output: json!({ "text": "ok" }),
                    is_error: false,
                }),
            },
        ),
        (
            "event_hook",
            ExtensionMessage {
                id: "msg-event-hook".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::EventHook(EventHookPayload {
                    event: "agent_start".to_string(),
                    data: Some(json!({ "note": "hello" })),
                }),
            },
        ),
        (
            "host_call",
            ExtensionMessage {
                id: "msg-host-call".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "host-1".to_string(),
                    capability: "read".to_string(),
                    method: "tool".to_string(),
                    params: json!({ "name": "read", "input": { "path": "README.md" } }),
                    timeout_ms: Some(2500),
                    cancel_token: None,
                    context: None,
                }),
            },
        ),
        (
            "host_call_cancel",
            ExtensionMessage {
                id: "msg-host-call-cancel".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "host-2".to_string(),
                    capability: "http".to_string(),
                    method: "http".to_string(),
                    params: json!({ "url": "https://example.com", "method": "GET" }),
                    timeout_ms: Some(1500),
                    cancel_token: Some("cancel-1".to_string()),
                    context: Some(json!({ "trace_id": "trace-1" })),
                }),
            },
        ),
        (
            "host_result",
            ExtensionMessage {
                id: "msg-host-result".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostResult(HostResultPayload {
                    call_id: "host-1".to_string(),
                    output: json!({ "content": [] }),
                    is_error: false,
                    error: None,
                    chunk: None,
                }),
            },
        ),
        (
            "host_result_timeout",
            ExtensionMessage {
                id: "msg-host-result-timeout".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostResult(HostResultPayload {
                    call_id: "host-2".to_string(),
                    output: json!({}),
                    is_error: true,
                    error: Some(HostCallError {
                        code: HostCallErrorCode::Timeout,
                        message: "Timed out".to_string(),
                        details: None,
                        retryable: Some(true),
                    }),
                    chunk: None,
                }),
            },
        ),
        (
            "host_result_denied",
            ExtensionMessage {
                id: "msg-host-result-denied".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostResult(HostResultPayload {
                    call_id: "host-3".to_string(),
                    output: json!({}),
                    is_error: true,
                    error: Some(HostCallError {
                        code: HostCallErrorCode::Denied,
                        message: "Denied".to_string(),
                        details: Some(json!({ "capability": "exec" })),
                        retryable: None,
                    }),
                    chunk: None,
                }),
            },
        ),
        (
            "log",
            ExtensionMessage {
                id: "msg-log".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::Log(Box::new(LogPayload {
                    schema: LOG_SCHEMA_VERSION.to_string(),
                    ts: "2026-02-03T03:01:02.123Z".to_string(),
                    level: LogLevel::Info,
                    event: "tool_call.start".to_string(),
                    message: "tool call dispatched".to_string(),
                    correlation: LogCorrelation {
                        extension_id: "ext.demo".to_string(),
                        scenario_id: "scn-001".to_string(),
                        session_id: None,
                        run_id: None,
                        artifact_id: None,
                        tool_call_id: None,
                        slash_command_id: None,
                        event_id: None,
                        host_call_id: None,
                        rpc_id: None,
                        trace_id: None,
                        span_id: None,
                    },
                    source: None,
                    data: None,
                })),
            },
        ),
        (
            "error",
            ExtensionMessage {
                id: "msg-error".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::Error(ErrorPayload {
                    code: "E_DEMO".to_string(),
                    message: "Something went wrong".to_string(),
                    details: Some(json!({ "hint": "check config" })),
                }),
            },
        ),
    ]
}

#[test]
fn parse_register_message() {
    let json = r#"
        {
          "id": "msg-1",
          "version": "1.0",
          "type": "register",
          "payload": {
            "name": "demo",
            "version": "0.1.0",
            "api_version": "1.0",
            "capabilities": ["read"]
          }
        }
        "#;
    let msg = ExtensionMessage::parse_and_validate(json).unwrap();
    assert!(matches!(msg.body, ExtensionBody::Register(_)));
}

#[test]
fn parse_register_message_with_capability_manifest_v2() {
    let json = r#"
        {
          "id": "msg-v2",
          "version": "1.0",
          "type": "register",
          "payload": {
            "name": "demo",
            "version": "0.1.0",
            "api_version": "1.0",
            "capability_manifest": {
              "schema": "pi.ext.cap.v2",
              "capabilities": [
                {
                  "capability": "read",
                  "intents": ["file_read"],
                  "connector_classes": ["fs"],
                  "hostcall_classes": ["fs.read"],
                  "scope": { "paths": ["."] },
                  "provenance": {
                    "source": "local",
                    "integrity": {
                      "algorithm": "sha256",
                      "digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    },
                    "publisher": {
                      "id": "dev@example",
                      "verification": "self_attested"
                    }
                  }
                }
              ]
            }
          }
        }
        "#;
    let msg = ExtensionMessage::parse_and_validate(json).expect("v2 register should parse");
    let ExtensionBody::Register(payload) = msg.body else {
        panic!();
    };
    let schema = payload
        .capability_manifest
        .expect("capability manifest")
        .schema;
    assert_eq!(schema, CAPABILITY_MANIFEST_SCHEMA_V2);
}

#[test]
fn parse_register_message_rejects_v2_manifest_without_provenance() {
    let json = r#"
        {
          "id": "msg-v2-invalid",
          "version": "1.0",
          "type": "register",
          "payload": {
            "name": "demo",
            "version": "0.1.0",
            "api_version": "1.0",
            "capability_manifest": {
              "schema": "pi.ext.cap.v2",
              "capabilities": [
                {
                  "capability": "read",
                  "intents": ["file_read"],
                  "connector_classes": ["fs"],
                  "hostcall_classes": ["fs.read"]
                }
              ]
            }
          }
        }
        "#;
    let err = ExtensionMessage::parse_and_validate(json).expect_err("must fail closed");
    let msg = format!("{err}");
    assert!(msg.contains("missing provenance"), "{msg}");
}

#[test]
fn parse_register_message_rejects_v2_manifest_unknown_requirement_field() {
    let json = r#"
        {
          "id": "msg-v2-unknown-field",
          "version": "1.0",
          "type": "register",
          "payload": {
            "name": "demo",
            "version": "0.1.0",
            "api_version": "1.0",
            "capability_manifest": {
              "schema": "pi.ext.cap.v2",
              "capabilities": [
                {
                  "capability": "read",
                  "intents": ["file_read"],
                  "connector_classes": ["fs"],
                  "hostcall_classes": ["fs.read"],
                  "provenance": {
                    "source": "local",
                    "integrity": {
                      "algorithm": "sha256",
                      "digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    },
                    "publisher": {
                      "id": "dev@example",
                      "verification": "self_attested"
                    }
                  },
                  "unexpected_critical": true
                }
              ]
            }
          }
        }
        "#;
    let err = ExtensionMessage::parse_and_validate(json).expect_err("must fail closed");
    let msg = format!("{err}");
    assert!(msg.contains("unknown field"), "{msg}");
    assert!(msg.contains("unexpected_critical"), "{msg}");
}

#[test]
fn parse_register_message_rejects_v2_manifest_unknown_provenance_field() {
    let json = r#"
        {
          "id": "msg-v2-provenance-unknown-field",
          "version": "1.0",
          "type": "register",
          "payload": {
            "name": "demo",
            "version": "0.1.0",
            "api_version": "1.0",
            "capability_manifest": {
              "schema": "pi.ext.cap.v2",
              "capabilities": [
                {
                  "capability": "read",
                  "intents": ["file_read"],
                  "connector_classes": ["fs"],
                  "hostcall_classes": ["fs.read"],
                  "provenance": {
                    "source": "local",
                    "integrity": {
                      "algorithm": "sha256",
                      "digest": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    },
                    "publisher": {
                      "id": "dev@example",
                      "verification": "self_attested"
                    },
                    "sigstore_bundle": "opaque"
                  }
                }
              ]
            }
          }
        }
        "#;
    let err = ExtensionMessage::parse_and_validate(json).expect_err("must fail closed");
    let msg = format!("{err}");
    assert!(msg.contains("unknown field"), "{msg}");
    assert!(msg.contains("sigstore_bundle"), "{msg}");
}

#[test]
fn reject_invalid_version() {
    let json = r#"
        {
          "id": "msg-2",
          "version": "2.0",
          "type": "log",
          "payload": {
            "schema": "pi.ext.log.v1",
            "ts": "2026-02-03T03:01:02.123Z",
            "level": "info",
            "event": "tool_call.start",
            "message": "hi",
            "correlation": {
              "extension_id": "ext.demo",
              "scenario_id": "scn-001"
            }
          }
        }
        "#;
    let err = ExtensionMessage::parse_and_validate(json).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Unsupported extension protocol version"));
}

#[test]
fn extension_manifest_rejects_v1_with_v2_only_fields() {
    let manifest = ExtensionManifest {
        schema: "pi.ext.manifest.v1".to_string(),
        extension_id: "ext.test".to_string(),
        name: "ext".to_string(),
        version: "0.1.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        runtime: ExtensionRuntime::NativeRust,
        entrypoint: "index.native.json".to_string(),
        capabilities: vec!["read".to_string()],
        capability_manifest: Some(CapabilityManifest {
            schema: CAPABILITY_MANIFEST_SCHEMA_V1.to_string(),
            capabilities: vec![CapabilityRequirement {
                capability: "read".to_string(),
                methods: vec!["fs".to_string()],
                intents: vec!["file_read".to_string()],
                connector_classes: Vec::new(),
                hostcall_classes: Vec::new(),
                risk_tier: None,
                scope: Some(CapabilityScope {
                    paths: Some(vec![".".to_string()]),
                    hosts: None,
                    env: None,
                    allowed_tools: None,
                }),
                provenance: None,
            }],
        }),
        description: None,
    };

    let err = validate_extension_manifest(&manifest).expect_err("v1 must reject v2 fields");
    let msg = format!("{err}");
    assert!(msg.contains("v2-only fields"), "{msg}");
}

#[test]
fn extension_manifest_accepts_capability_manifest_v2() {
    let manifest = ExtensionManifest {
        schema: "pi.ext.manifest.v1".to_string(),
        extension_id: "ext.test".to_string(),
        name: "ext".to_string(),
        version: "0.1.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        runtime: ExtensionRuntime::NativeRust,
        entrypoint: "index.native.json".to_string(),
        capabilities: Vec::new(),
        capability_manifest: Some(CapabilityManifest {
            schema: CAPABILITY_MANIFEST_SCHEMA_V2.to_string(),
            capabilities: vec![CapabilityRequirement {
                capability: "read".to_string(),
                methods: Vec::new(),
                intents: vec!["file_read".to_string()],
                connector_classes: vec!["fs".to_string()],
                hostcall_classes: vec!["fs.read".to_string()],
                risk_tier: Some("low".to_string()),
                scope: Some(CapabilityScope {
                    paths: Some(vec![".".to_string()]),
                    hosts: None,
                    env: None,
                    allowed_tools: Some(vec!["read".to_string()]),
                }),
                provenance: Some(CapabilityProvenance {
                    source: "local".to_string(),
                    integrity: CapabilityIntegrityAttestation {
                        algorithm: "sha256".to_string(),
                        digest: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_string(),
                    },
                    publisher: CapabilityPublisherAttestation {
                        id: "dev@example".to_string(),
                        verification: "self_attested".to_string(),
                    },
                }),
            }],
        }),
        description: None,
    };

    validate_extension_manifest(&manifest).expect("v2 manifest should validate");
}

#[must_use]
fn permission_drift_requirement(capability: &str, digest: &str) -> CapabilityRequirement {
    let (intent, connector_class, hostcall_class, risk_tier) = match capability {
        "exec" => ("process_exec", "exec", "exec", "critical"),
        "env" => ("environment_access", "env", "env", "high"),
        "http" => ("network_egress", "http", "http", "medium"),
        "write" => ("file_write", "fs", "fs.write", "medium"),
        "log" => ("telemetry_logging", "log", "log", "low"),
        _ => ("file_read", "fs", "fs.read", "low"),
    };
    CapabilityRequirement {
        capability: capability.to_string(),
        methods: Vec::new(),
        intents: vec![intent.to_string()],
        connector_classes: vec![connector_class.to_string()],
        hostcall_classes: vec![hostcall_class.to_string()],
        risk_tier: Some(risk_tier.to_string()),
        scope: None,
        provenance: Some(CapabilityProvenance {
            source: "registry".to_string(),
            integrity: CapabilityIntegrityAttestation {
                algorithm: "sha256".to_string(),
                digest: digest.to_string(),
            },
            publisher: CapabilityPublisherAttestation {
                id: "publisher@example".to_string(),
                verification: "registry_attested".to_string(),
            },
        }),
    }
}

#[must_use]
fn permission_drift_manifest(capabilities: &[(&str, &str)]) -> CapabilityManifest {
    CapabilityManifest {
        schema: CAPABILITY_MANIFEST_SCHEMA_V2.to_string(),
        capabilities: capabilities
            .iter()
            .map(|(capability, digest)| permission_drift_requirement(capability, digest))
            .collect(),
    }
}

#[test]
fn permission_drift_fails_closed_for_expansion_without_provenance_or_policy() {
    let previous = ExtensionPermissionSnapshot {
        extension_id: "ext.secure".to_string(),
        capabilities: vec!["read".to_string()],
        policy_profile: Some(PolicyProfile::Standard),
        catalog_policy_profile: Some(PolicyProfile::Standard),
        ..Default::default()
    };
    let current = ExtensionPermissionSnapshot {
        extension_id: "ext.secure".to_string(),
        capabilities: vec!["read".to_string(), "exec".to_string()],
        catalog_policy_profile: Some(PolicyProfile::Standard),
        ..Default::default()
    };

    let report = detect_extension_permission_drift(&previous, &current);
    assert_eq!(
        report.drift_class,
        ExtensionPermissionDriftClass::MissingProvenance
    );
    assert!(
        report
            .drift_classes
            .contains(&ExtensionPermissionDriftClass::AddedDangerousCapabilities)
    );
    assert!(
        report
            .drift_classes
            .contains(&ExtensionPermissionDriftClass::PolicyProfileMismatch)
    );
    assert_eq!(report.verdict, ExtensionPermissionDriftVerdict::FailClosed);
    assert_eq!(report.risk_level, ExtensionPermissionRiskLevel::Critical);
    assert_eq!(
        report.provenance_status,
        ExtensionPermissionProvenanceStatus::Missing
    );
    assert_eq!(report.recommended_action, "block_launch_refresh_provenance");

    let evidence =
        detect_extension_permission_drift_json(&previous, &current).expect("evidence json");
    assert_eq!(evidence["extension_id"], json!("ext.secure"));
    assert_eq!(evidence["previous_capabilities"], json!(["read"]));
    assert_eq!(evidence["current_capabilities"], json!(["exec", "read"]));
}

#[test]
fn permission_drift_records_explicitly_trusted_provenanced_expansion() {
    let read_digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let exec_digest = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let previous = ExtensionPermissionSnapshot {
        extension_id: "ext.secure".to_string(),
        capability_manifest: Some(permission_drift_manifest(&[("read", read_digest)])),
        policy_profile: Some(PolicyProfile::Standard),
        catalog_policy_profile: Some(PolicyProfile::Standard),
        ..Default::default()
    };
    let current = ExtensionPermissionSnapshot {
        extension_id: "ext.secure".to_string(),
        capability_manifest: Some(permission_drift_manifest(&[
            ("read", read_digest),
            ("exec", exec_digest),
        ])),
        policy_profile: Some(PolicyProfile::Standard),
        catalog_policy_profile: Some(PolicyProfile::Standard),
        trust: ExtensionPermissionTrust::ExplicitlyTrusted,
        ..Default::default()
    };

    let report = detect_extension_permission_drift(&previous, &current);
    assert_eq!(
        report.drift_class,
        ExtensionPermissionDriftClass::AddedDangerousCapabilities
    );
    assert!(
        report
            .drift_classes
            .contains(&ExtensionPermissionDriftClass::ExplicitlyTrustedChange)
    );
    assert_eq!(
        report.provenance_status,
        ExtensionPermissionProvenanceStatus::Trusted
    );
    assert_eq!(
        report.verdict,
        ExtensionPermissionDriftVerdict::AllowWithAudit
    );
    assert_eq!(report.risk_level, ExtensionPermissionRiskLevel::Medium);
    assert_eq!(
        report.recommended_action,
        "launch_extension_and_record_audit"
    );
}

#[test]
fn permission_drift_flags_removed_capabilities_as_auditable() {
    let previous = ExtensionPermissionSnapshot {
        extension_id: "ext.secure".to_string(),
        capabilities: vec!["read".to_string(), "write".to_string()],
        policy_profile: Some(PolicyProfile::Safe),
        catalog_policy_profile: Some(PolicyProfile::Safe),
        ..Default::default()
    };
    let current = ExtensionPermissionSnapshot {
        extension_id: "ext.secure".to_string(),
        capabilities: vec!["read".to_string()],
        policy_profile: Some(PolicyProfile::Safe),
        catalog_policy_profile: Some(PolicyProfile::Safe),
        ..Default::default()
    };

    let report = detect_extension_permission_drift(&previous, &current);
    assert_eq!(
        report.drift_class,
        ExtensionPermissionDriftClass::RemovedCapabilities
    );
    assert_eq!(
        report.verdict,
        ExtensionPermissionDriftVerdict::AllowWithAudit
    );
    assert_eq!(report.risk_level, ExtensionPermissionRiskLevel::Low);
    assert_eq!(
        report.removed_capabilities,
        BTreeSet::from(["write".to_string()])
    );
}

#[test]
fn permission_drift_flags_stale_manifest_and_policy_profile_mismatch() {
    let previous = ExtensionPermissionSnapshot {
        extension_id: "ext.secure".to_string(),
        capabilities: vec!["read".to_string()],
        policy_profile: Some(PolicyProfile::Safe),
        catalog_policy_profile: Some(PolicyProfile::Standard),
        ..Default::default()
    };
    let current = ExtensionPermissionSnapshot {
        extension_id: "ext.secure".to_string(),
        capabilities: vec!["read".to_string()],
        catalog_capabilities: vec!["read".to_string(), "write".to_string()],
        policy_profile: Some(PolicyProfile::Safe),
        catalog_policy_profile: Some(PolicyProfile::Standard),
        manifest_checksum: Some("observed".to_string()),
        catalog_manifest_checksum: Some("catalog".to_string()),
        ..Default::default()
    };

    let report = detect_extension_permission_drift(&previous, &current);
    assert_eq!(
        report.drift_class,
        ExtensionPermissionDriftClass::StaleManifest
    );
    assert!(
        report
            .drift_classes
            .contains(&ExtensionPermissionDriftClass::PolicyProfileMismatch)
    );
    assert_eq!(
        report.provenance_status,
        ExtensionPermissionProvenanceStatus::Stale
    );
    assert_eq!(report.verdict, ExtensionPermissionDriftVerdict::FailClosed);
    assert_eq!(report.recommended_action, "block_launch_refresh_manifest");
}

#[test]
fn permission_drift_flags_provenance_snapshot_mismatch() {
    let previous_digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let current_digest = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let previous = ExtensionPermissionSnapshot {
        extension_id: "ext.secure".to_string(),
        capability_manifest: Some(permission_drift_manifest(&[("read", previous_digest)])),
        policy_profile: Some(PolicyProfile::Standard),
        catalog_policy_profile: Some(PolicyProfile::Standard),
        provenance_snapshot_checksum: Some("previous-provenance".to_string()),
        catalog_provenance_checksum: Some("previous-provenance".to_string()),
        ..Default::default()
    };
    let current = ExtensionPermissionSnapshot {
        extension_id: "ext.secure".to_string(),
        capability_manifest: Some(permission_drift_manifest(&[("read", current_digest)])),
        policy_profile: Some(PolicyProfile::Standard),
        catalog_policy_profile: Some(PolicyProfile::Standard),
        provenance_snapshot_checksum: Some("current-provenance".to_string()),
        catalog_provenance_checksum: Some("catalog-provenance".to_string()),
        ..Default::default()
    };

    let report = detect_extension_permission_drift(&previous, &current);
    assert_eq!(
        report.drift_class,
        ExtensionPermissionDriftClass::ProvenanceMismatch
    );
    assert_eq!(
        report.provenance_status,
        ExtensionPermissionProvenanceStatus::Mismatch
    );
    assert_eq!(report.verdict, ExtensionPermissionDriftVerdict::FailClosed);
    assert_eq!(
        report.recommended_action,
        "block_launch_reconcile_provenance"
    );
}

#[test]
fn parse_host_call_message() {
    let json = r#"
        {
          "id": "msg-3",
          "version": "1.0",
          "type": "host_call",
          "payload": {
            "call_id": "call-1",
            "capability": "read",
            "method": "tool",
            "params": { "name": "read", "input": { "path": "README.md" } },
            "timeout_ms": 1000
          }
        }
        "#;
    let msg = ExtensionMessage::parse_and_validate(json).unwrap();
    assert!(matches!(msg.body, ExtensionBody::HostCall(_)));
}

#[test]
fn parse_log_message() {
    let json = r#"
        {
          "id": "msg-4",
          "version": "1.0",
          "type": "log",
          "payload": {
            "schema": "pi.ext.log.v1",
            "ts": "2026-02-03T03:01:02.123Z",
            "level": "info",
            "event": "tool_call.start",
            "message": "tool call dispatched",
            "correlation": {
              "extension_id": "ext.demo",
              "scenario_id": "scn-001"
            }
          }
        }
        "#;
    let msg = ExtensionMessage::parse_and_validate(json).unwrap();
    assert!(matches!(msg.body, ExtensionBody::Log(_)));
}

#[test]
fn extension_ui_rpc_event_format() {
    let request = ExtensionUiRequest::new(
        "req-1",
        "notify",
        json!({ "title": "Hello", "message": "World" }),
    );
    let event = request.to_rpc_event();
    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "req-1");
    assert_eq!(event["method"], "notify");
    assert_eq!(event["title"], "Hello");
    assert_eq!(event["message"], "World");
}

#[test]
fn extension_ui_custom_expects_response() {
    let request = ExtensionUiRequest::new("req-1", "custom", json!({}));
    assert!(
        request.expects_response(),
        "custom UI hostcalls must be response-bearing"
    );
}

#[test]
fn extension_ui_request_roundtrip() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    let handle = runtime.handle();

    runtime.block_on(async move {
        let (ui_tx, mut ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);

        let responder = manager.clone();
        handle.spawn(async move {
            let cx = Cx::for_request();
            if let Ok(req) = ui_rx.recv(&cx).await {
                responder.respond_ui(ExtensionUiResponse {
                    id: req.id,
                    value: Some(json!(true)),
                    cancelled: false,
                });
            }
        });

        let request = ExtensionUiRequest::new("", "confirm", json!({ "title": "Confirm" }));
        let response = manager.request_ui(request).await.unwrap();
        assert_eq!(response.unwrap().value, Some(json!(true)));
    });
}

#[test]
fn js_hostcall_prompt_mode_asks_once_per_capability() {
    let manager = extension_manager_no_persisted_permissions();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    let handle = runtime.handle();

    runtime.block_on(async move {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (ui_tx, mut ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);

        let prompt_count = Arc::new(AtomicUsize::new(0));
        let prompt_count_clone = Arc::clone(&prompt_count);

        let responder = manager.clone();
        handle.spawn(async move {
            let cx = Cx::for_request();
            while let Ok(req) = ui_rx.recv(&cx).await {
                prompt_count_clone.fetch_add(1, Ordering::SeqCst);
                responder.respond_ui(ExtensionUiResponse {
                    id: req.id,
                    value: Some(json!(true)),
                    cancelled: false,
                });
            }
        });

        let dir = tempdir().expect("tempdir");
        let host = JsRuntimeHost {
            tools: Arc::new(ToolRegistry::new(&[], dir.path(), None)),
            manager_ref: Arc::downgrade(&manager.inner),
            manager_snapshot: Arc::clone(&manager.snapshot),
            manager_snapshot_version: Arc::clone(&manager.snapshot_version),
            http: Arc::new(HttpConnector::with_defaults()),
            policy: ExtensionPolicy {
                mode: ExtensionPolicyMode::Prompt,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                ..Default::default()
            },
            interceptor: None,
        };

        let request = HostcallRequest {
            call_id: "call-1".to_string(),
            kind: HostcallKind::Tool {
                name: "nonexistent".to_string(),
            },
            payload: json!({}),
            trace_id: 1,
            extension_id: Some("ext.test".to_string()),
        };

        let _ = dispatch_hostcall(&host, request).await;

        let request = HostcallRequest {
            call_id: "call-2".to_string(),
            kind: HostcallKind::Tool {
                name: "nonexistent".to_string(),
            },
            payload: json!({}),
            trace_id: 2,
            extension_id: Some("ext.test".to_string()),
        };

        let _ = dispatch_hostcall(&host, request).await;

        assert_eq!(prompt_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn js_runtime_pump_once_advances_timers_and_hostcalls() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let entry_path = dir.path().join("ext.mjs");
        std::fs::write(
            &entry_path,
            r#"
                export default function init(pi) {
                  pi.on("agent_start", () => {
                    setTimeout(() => {
                      pi.tool("write", { path: "out.txt", content: "hi" });
                    }, 0);
                  });
                }
                "#,
        )
        .expect("write extension entry");

        let tools = Arc::new(ToolRegistry::new(&["write"], dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());

        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load extension");

        manager
            .dispatch_event_with_response(
                ExtensionEventName::AgentStart,
                None,
                EXTENSION_EVENT_TIMEOUT_MS,
            )
            .await
            .expect("dispatch agent_start");

        let out_path = dir.path().join("out.txt");
        let mut wrote = false;
        for _ in 0..20 {
            let _ = js_runtime.pump_once().await.expect("pump_once");
            if out_path.exists() {
                wrote = true;
                break;
            }
            sleep(wall_now(), Duration::from_millis(1)).await;
        }

        assert!(wrote, "expected out.txt to be created after pumping");
        let contents = std::fs::read_to_string(&out_path).expect("read out.txt");
        assert_eq!(contents, "hi");
    });
}

#[test]
fn isolated_runtime_reset_drops_registry_routes_and_requires_cold_reload() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let entry_path = dir.path().join("resettable.mjs");
        std::fs::write(&entry_path, RESETTABLE_EXTENSION_SOURCE).expect("write extension entry");

        let tools = Arc::new(ToolRegistry::new(&[], dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());

        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
        manager
            .load_js_extensions(vec![spec.clone()])
            .await
            .expect("load extension");
        assert_eq!(
            js_runtime
                .get_registered_tools()
                .await
                .expect("registered tools")
                .len(),
            1
        );
        js_runtime
            .execute_command(
                "reset-probe".to_string(),
                String::new(),
                Arc::new(json!({})),
                5_000,
            )
            .await
            .expect("command before reset");

        js_runtime
            .reset_transient_state()
            .await
            .expect("full transient reset");
        assert!(
            js_runtime
                .get_registered_tools()
                .await
                .expect("registered tools after reset")
                .is_empty(),
            "reset must discard every shard registry"
        );
        let stale_route = js_runtime
            .execute_command(
                "reset-probe".to_string(),
                String::new(),
                Arc::new(json!({})),
                5_000,
            )
            .await
            .expect_err("reset must discard command routes");
        assert!(
            stale_route
                .to_string()
                .contains("Unknown JS extension command"),
            "unexpected stale-route error: {stale_route}"
        );

        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("cold reload after reset");
        js_runtime
            .execute_command(
                "reset-probe".to_string(),
                String::new(),
                Arc::new(json!({})),
                5_000,
            )
            .await
            .expect("command after cold reload");

        assert!(manager.shutdown(Duration::from_secs(3)).await);
    });
}

#[test]
fn isolated_runtime_actor_skips_expired_or_abandoned_commands_before_side_effects() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let marker = dir.path().join("expired-command-ran.txt");
        let marker_js =
            serde_json::to_string(&marker.display().to_string()).expect("serialize marker path");
        let entry_path = dir.path().join("deadline.mjs");
        std::fs::write(
            &entry_path,
            format!(
                r#"
                    import * as fs from "node:fs";
                    export default function init(pi) {{
                      pi.registerCommand("expired-probe", {{
                        handler: async () => {{
                          fs.writeFileSync({marker_js}, "ran");
                          return "ran";
                        }},
                      }});
                    }}
                    "#
            ),
        )
        .expect("write deadline extension");

        let tools = Arc::new(ToolRegistry::new(&[], dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());
        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load deadline extension");

        let cx = Cx::for_request();
        let (expired_tx, mut expired_rx) = oneshot::channel();
        js_runtime
            .sender
            .send(
                &cx,
                JsRuntimeCommand::ExecuteCommand {
                    command_name: "expired-probe".to_string(),
                    args: String::new(),
                    ctx_payload: Arc::new(json!({})),
                    timeout_ms: 5_000,
                    deadline: Instant::now()
                        .checked_sub(Duration::from_millis(1))
                        .expect("past deadline"),
                    reply: expired_tx,
                },
            )
            .await
            .expect("enqueue expired command");
        let expired = expired_rx
            .recv(&cx)
            .await
            .expect("expired command reply")
            .expect_err("expired command must fail");
        assert!(
            expired
                .to_string()
                .contains("expired before actor execution")
        );
        assert!(!marker.exists(), "expired handler body must not run");

        let (abandoned_tx, abandoned_rx) = oneshot::channel();
        drop(abandoned_rx);
        js_runtime
            .sender
            .send(
                &cx,
                JsRuntimeCommand::ExecuteCommand {
                    command_name: "expired-probe".to_string(),
                    args: String::new(),
                    ctx_payload: Arc::new(json!({})),
                    timeout_ms: 5_000,
                    deadline: js_runtime_request_deadline(5_000),
                    reply: abandoned_tx,
                },
            )
            .await
            .expect("enqueue abandoned command");
        // A following query is an actor-ordering barrier.
        let _ = js_runtime
            .get_registered_tools()
            .await
            .expect("actor ordering barrier");
        assert!(!marker.exists(), "abandoned handler body must not run");

        assert!(manager.shutdown(Duration::from_secs(3)).await);
    });
}

#[test]
fn isolated_runtime_timeout_quarantines_and_peer_work_skips_failed_shard() {
    let manager = ExtensionManager::new();
    let actions = Arc::new(MockHostActions::new());
    manager.set_host_actions(actions.clone());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let (slow_entry, fast_entry) = create_timeout_quarantine_fixture(dir.path());

        let tools = Arc::new(ToolRegistry::new(&[], dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());
        let slow_spec =
            JsExtensionLoadSpec::from_entry_path(&slow_entry).expect("slow extension spec");
        let fast_spec =
            JsExtensionLoadSpec::from_entry_path(&fast_entry).expect("fast extension spec");
        manager
            .load_js_extensions(vec![slow_spec, fast_spec])
            .await
            .expect("load slow and fast shards");

        let first = js_runtime
            .execute_command(
                "never-finishes".to_string(),
                String::new(),
                Arc::new(json!({})),
                500,
            )
            .await;
        assert!(first.is_err(), "never-finishing handler must time out");
        assert_only_host_message(
            &actions,
            "handler-start",
            "the first handler must have started",
        );
        sleep(wall_now(), Duration::from_millis(400)).await;
        js_runtime
            .pump_once()
            .await
            .expect("untargeted pumping must skip an already-quarantined shard");

        let fast_started = Instant::now();
        let fast = js_runtime
            .execute_command(
                "fast-peer".to_string(),
                String::new(),
                Arc::new(json!({})),
                5_000,
            )
            .await
            .expect("healthy peer command");
        assert_eq!(fast, Value::String("fast".to_string()));
        assert!(
            fast_started.elapsed() < Duration::from_secs(1),
            "healthy peer latency must not depend on quarantined shard work"
        );
        {
            let messages = actions
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                messages.len(),
                1,
                "peer pumping must not revive a timed-out shard timer"
            );
        }

        let second = js_runtime
            .execute_command(
                "never-finishes".to_string(),
                String::new(),
                Arc::new(json!({})),
                5_000,
            )
            .await
            .expect_err("quarantined shard must reject before invocation");
        assert!(
            second.to_string().contains("quarantined"),
            "unexpected quarantine error: {second}"
        );
        {
            let messages = actions
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                messages.len(),
                1,
                "second direct call must fail before its handler body runs"
            );
        }

        assert!(manager.shutdown(Duration::from_secs(3)).await);
    });
}

#[test]
fn isolated_runtime_denies_peer_fs_and_module_access_inside_workspace() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let PeerIsolationFixture {
            workspace,
            entry_a,
            entry_b,
            foreign_secret,
            foreign_write,
        } = create_peer_isolation_fixture(dir.path());

        let tools = Arc::new(ToolRegistry::new(&[], &workspace, None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: workspace.display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());

        let spec_a = JsExtensionLoadSpec::from_entry_path(&entry_a).expect("extension a spec");
        let spec_b = JsExtensionLoadSpec::from_entry_path(&entry_b).expect("extension b spec");
        manager
            .load_js_extensions(vec![spec_a, spec_b])
            .await
            .expect("load isolated extension shards");

        let denied = js_runtime
            .execute_command(
                "probe-peer-root".to_string(),
                String::new(),
                Arc::new(json!({})),
                5_000,
            )
            .await
            .expect("peer-root probe command");
        assert_eq!(denied.get("readOk"), Some(&Value::Bool(false)));
        assert_eq!(denied.get("writeOk"), Some(&Value::Bool(false)));
        assert_eq!(denied.get("importOk"), Some(&Value::Bool(false)));
        assert!(
            denied
                .get("readError")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("host read denied")),
            "unexpected peer read result: {denied}"
        );
        assert!(
            denied
                .get("writeError")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("host write denied")),
            "unexpected peer write result: {denied}"
        );
        assert!(
            denied
                .get("importError")
                .and_then(Value::as_str)
                .is_some_and(|error| {
                    error.contains("Module path") && error.contains("extension root")
                }),
            "unexpected peer import result: {denied}"
        );
        assert_eq!(
            std::fs::read_to_string(&foreign_secret).expect("read unchanged secret"),
            "owned-by-b"
        );
        assert!(
            !foreign_write.exists(),
            "peer extension must not create a file inside a foreign root"
        );

        let own_read = js_runtime
            .execute_command(
                "read-own-root".to_string(),
                String::new(),
                Arc::new(json!({})),
                5_000,
            )
            .await
            .expect("owner read command");
        assert_eq!(own_read, Value::String("owned-by-b".to_string()));

        assert!(manager.shutdown(Duration::from_secs(3)).await);
    });
}

#[test]
fn isolated_runtime_rejects_distinct_owners_for_the_same_leaf_directory() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let entry_a = dir.path().join("first.mjs");
        let entry_b = dir.path().join("second.mjs");
        let source = "export default function init(_pi) {}";
        std::fs::write(&entry_a, source).expect("write first extension");
        std::fs::write(&entry_b, source).expect("write second extension");

        let tools = Arc::new(ToolRegistry::new(&[], dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime);

        let spec_a = JsExtensionLoadSpec::from_entry_path(&entry_a).expect("first spec");
        let spec_b = JsExtensionLoadSpec::from_entry_path(&entry_b).expect("second spec");
        let err = manager
            .load_js_extensions(vec![spec_a, spec_b])
            .await
            .expect_err("ambiguous leaf ownership must fail closed");
        assert!(
            err.to_string().contains("Ambiguous JS extension ownership"),
            "unexpected ownership error: {err}"
        );

        assert!(manager.shutdown(Duration::from_secs(3)).await);
    });
}

#[test]
#[cfg(feature = "ext-conformance")]
fn explicit_compat_scan_disable_prevents_static_registration_fallback() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let entry_path = dir.path().join("strict.mjs");
        std::fs::write(
            &entry_path,
            r#"
                export default function init(pi) {
                  if (false) {
                    pi.registerTool({
                      name: "static_ghost_tool",
                      description: "must not be inferred",
                      parameters: { type: "object", properties: {} },
                      execute: async () => ({ content: [] }),
                    });
                    pi.registerCommand("static-ghost-command", {
                      description: "must not be inferred",
                      handler: async () => "ghost",
                    });
                  }
                }
                "#,
        )
        .expect("write extension entry");

        let tools = Arc::new(ToolRegistry::new(&[], dir.path(), None));
        let mut config = PiJsRuntimeConfig {
            cwd: dir.path().display().to_string(),
            ..Default::default()
        };
        config
            .env
            .insert("PI_EXT_COMPAT_SCAN".to_string(), "0".to_string());
        let js_runtime =
            JsExtensionRuntimeHandle::start(config, Arc::clone(&tools), manager.clone())
                .await
                .expect("start strict js runtime");
        manager.set_js_runtime(js_runtime);

        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load strict extension");

        let guard = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let payload = guard
            .extensions
            .first()
            .expect("registered extension payload");
        assert!(
            payload.tools.is_empty(),
            "explicit compatibility disable must not infer tool metadata"
        );
        assert!(
            payload.slash_commands.is_empty(),
            "explicit compatibility disable must not infer command metadata"
        );
        drop(guard);

        assert!(manager.shutdown(Duration::from_secs(3)).await);
    });
}

#[test]
fn multi_entry_loader_fails_closed_on_failing_non_primary_entrypoints() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let bundle_root = dir.path().join("bundle");
        let primary_dir = bundle_root.join("a-primary");
        let failing_dir = bundle_root.join("b-failing");
        std::fs::create_dir_all(&primary_dir).expect("mkdir primary");
        std::fs::create_dir_all(&failing_dir).expect("mkdir failing");

        let primary_entry = primary_dir.join("index.ts");
        std::fs::write(
            &primary_entry,
            r#"
                export default function init(pi) {
                  pi.registerCommand("primary-ok", {
                    description: "ok",
                    handler: async () => "ok",
                  });
                }
                "#,
        )
        .expect("write primary entry");

        let failing_entry = failing_dir.join("index.ts");
        std::fs::write(
            &failing_entry,
            r#"
                export default function init(_pi) {
                  throw new Error("secondary entry failed");
                }
                "#,
        )
        .expect("write failing entry");

        let tools = Arc::new(ToolRegistry::new(&[], dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime);

        let spec = JsExtensionLoadSpec::from_entry_path(&primary_entry).expect("load spec");
        let err = manager
            .load_js_extensions(vec![spec])
            .await
            .expect_err("secondary entry failure should fail the whole extension load");

        assert!(
            err.to_string().contains("secondary entry failed"),
            "error should preserve the secondary failure context: {err}"
        );
        assert!(!manager.has_command("primary-ok"));
        assert!(!manager.has_command("secondary-should-not-exist"));
    });
}

#[test]
#[cfg(unix)]
fn js_runtime_pump_once_exec_streaming_callback_delivers_chunks_and_final_result() {
    futures::executor::block_on(async {
        let dir = tempdir().expect("tempdir");
        let manager = ExtensionManager::new();
        let host = JsRuntimeHost {
            tools: Arc::new(ToolRegistry::new(&[], dir.path(), None)),
            manager_ref: Arc::downgrade(&manager.inner),
            manager_snapshot: Arc::clone(&manager.snapshot),
            manager_snapshot_version: Arc::clone(&manager.snapshot_version),
            http: Arc::new(HttpConnector::with_defaults()),
            policy: ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                ..Default::default()
            },
            interceptor: None,
        };

        let runtime = PiJsRuntime::new().await.expect("runtime");
        runtime
                .eval(
                    r#"
                    globalThis.chunks = [];
                    globalThis.finalResult = null;
                    globalThis.finalErr = null;
                    pi.exec("sh", ["-c", "printf 'out-1\n'; printf 'err-1\n' 1>&2; printf 'out-2\n'"], {
                        stream: true,
                        onChunk: (chunk, isFinal) => {
                            globalThis.chunks.push({ chunk, isFinal });
                        },
                    })
                    .then((r) => { globalThis.finalResult = r; })
                    .catch((e) => { globalThis.finalErr = { code: e.code, message: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

        for _ in 0..256 {
            let has_pending = pump_js_runtime_once(&runtime, &host)
                .await
                .expect("pump_once");
            if !has_pending {
                break;
            }
        }
        assert!(
            !runtime.has_pending(),
            "runtime should have no pending tasks after streaming exec"
        );

        let chunks = runtime
            .read_global_json("chunks")
            .await
            .expect("read chunks");
        let entries = chunks.as_array().expect("chunks array");
        assert!(
            entries.len() >= 3,
            "expected stream chunks plus final chunk, got: {entries:?}"
        );
        assert!(
            entries.iter().any(|entry| {
                entry
                    .get("chunk")
                    .and_then(|chunk| chunk.get("stdout"))
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("out-1"))
            }),
            "missing stdout chunk: {entries:?}"
        );
        assert!(
            entries.iter().any(|entry| {
                entry
                    .get("chunk")
                    .and_then(|chunk| chunk.get("stderr"))
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("err-1"))
            }),
            "missing stderr chunk: {entries:?}"
        );
        assert_eq!(
            entries.last().and_then(|entry| entry.get("isFinal")),
            Some(&Value::Bool(true)),
            "expected final stream marker: {entries:?}"
        );

        let final_result = runtime
            .read_global_json("finalResult")
            .await
            .expect("read finalResult");
        assert_eq!(final_result.get("code"), Some(&json!(0)));
        assert_eq!(final_result.get("killed"), Some(&Value::Bool(false)));
        assert_eq!(
            runtime
                .read_global_json("finalErr")
                .await
                .expect("read finalErr"),
            Value::Null
        );
    });
}

#[test]
#[cfg(unix)]
fn js_runtime_pump_once_exec_streaming_signal_termination_reports_nonzero_code() {
    futures::executor::block_on(async {
        let dir = tempdir().expect("tempdir");
        let manager = ExtensionManager::new();
        let host = JsRuntimeHost {
            tools: Arc::new(ToolRegistry::new(&[], dir.path(), None)),
            manager_ref: Arc::downgrade(&manager.inner),
            manager_snapshot: Arc::clone(&manager.snapshot),
            manager_snapshot_version: Arc::clone(&manager.snapshot_version),
            http: Arc::new(HttpConnector::with_defaults()),
            policy: ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                ..Default::default()
            },
            interceptor: None,
        };

        let runtime = PiJsRuntime::new().await.expect("runtime");
        runtime
                .eval(
                    r#"
                    globalThis.sigChunks = [];
                    globalThis.sigDone = false;
                    globalThis.sigErr = null;
                    (async () => {
                        try {
                            const stream = pi.exec("/bin/sh", ["-c", "kill -KILL $$"], { stream: true });
                            for await (const chunk of stream) {
                                globalThis.sigChunks.push(chunk);
                            }
                            globalThis.sigDone = true;
                        } catch (e) {
                            globalThis.sigErr = e.message || String(e);
                        }
                    })();
                "#,
                )
                .await
                .expect("eval");

        for _ in 0..256 {
            let has_pending = pump_js_runtime_once(&runtime, &host)
                .await
                .expect("pump_once");
            if !has_pending {
                break;
            }
        }
        assert!(
            !runtime.has_pending(),
            "runtime should have no pending tasks after signal-terminated exec stream"
        );

        let signal_chunks = runtime
            .read_global_json("sigChunks")
            .await
            .expect("read sigChunks");
        let entries = signal_chunks.as_array().expect("sigChunks array");
        assert!(
            !entries.is_empty(),
            "expected a final chunk for signal termination"
        );
        let final_chunk = entries.last().expect("final chunk");
        let code = final_chunk
            .get("code")
            .and_then(Value::as_i64)
            .expect("numeric final exit code");
        assert_ne!(
            code, 0,
            "signal-terminated process must not report exit code 0"
        );
        assert_eq!(final_chunk.get("killed"), Some(&Value::Bool(false)));
        assert_eq!(
            runtime
                .read_global_json("sigDone")
                .await
                .expect("read sigDone"),
            Value::Bool(true)
        );
        assert_eq!(
            runtime
                .read_global_json("sigErr")
                .await
                .expect("read sigErr"),
            Value::Null
        );
    });
}

#[test]
#[cfg(unix)]
fn js_runtime_pump_once_exec_streaming_async_iterator_delivers_chunks_in_order() {
    futures::executor::block_on(async {
        let dir = tempdir().expect("tempdir");
        let manager = ExtensionManager::new();
        let host = JsRuntimeHost {
            tools: Arc::new(ToolRegistry::new(&[], dir.path(), None)),
            manager_ref: Arc::downgrade(&manager.inner),
            manager_snapshot: Arc::clone(&manager.snapshot),
            manager_snapshot_version: Arc::clone(&manager.snapshot_version),
            http: Arc::new(HttpConnector::with_defaults()),
            policy: ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                ..Default::default()
            },
            interceptor: None,
        };

        let runtime = PiJsRuntime::new().await.expect("runtime");
        runtime
                .eval(
                    r#"
                    globalThis.iterChunks = [];
                    globalThis.iterDone = false;
                    globalThis.iterErr = null;
                    (async () => {
                        try {
                            const stream = pi.exec("sh", ["-c", "printf 'a\n'; printf 'b\n'"], { stream: true });
                            for await (const chunk of stream) {
                                globalThis.iterChunks.push(chunk);
                            }
                            globalThis.iterDone = true;
                        } catch (e) {
                            globalThis.iterErr = e.message || String(e);
                        }
                    })();
                "#,
                )
                .await
                .expect("eval");

        for _ in 0..256 {
            let has_pending = pump_js_runtime_once(&runtime, &host)
                .await
                .expect("pump_once");
            if !has_pending {
                break;
            }
        }
        assert!(
            !runtime.has_pending(),
            "runtime should have no pending tasks after streaming exec"
        );

        let iter_chunks = runtime
            .read_global_json("iterChunks")
            .await
            .expect("read iterChunks");
        let entries = iter_chunks.as_array().expect("iterChunks array");
        assert!(
            entries.len() >= 2,
            "expected stdout+final chunks, got: {entries:?}"
        );
        let stdout_joined = entries
            .iter()
            .filter_map(|entry| entry.get("stdout").and_then(Value::as_str))
            .collect::<String>();
        assert_eq!(stdout_joined, "a\nb\n");
        let final_chunk = entries.last().expect("final chunk");
        assert_eq!(final_chunk.get("code"), Some(&json!(0)));
        assert_eq!(final_chunk.get("killed"), Some(&Value::Bool(false)));
        assert_eq!(
            runtime
                .read_global_json("iterDone")
                .await
                .expect("read iterDone"),
            Value::Bool(true)
        );
        assert_eq!(
            runtime
                .read_global_json("iterErr")
                .await
                .expect("read iterErr"),
            Value::Null
        );
    });
}

#[test]
#[cfg(unix)]
fn js_runtime_pump_once_exec_streaming_timeout_sets_killed_final_chunk() {
    futures::executor::block_on(async {
        let dir = tempdir().expect("tempdir");
        let manager = ExtensionManager::new();
        let host = JsRuntimeHost {
            tools: Arc::new(ToolRegistry::new(&[], dir.path(), None)),
            manager_ref: Arc::downgrade(&manager.inner),
            manager_snapshot: Arc::clone(&manager.snapshot),
            manager_snapshot_version: Arc::clone(&manager.snapshot_version),
            http: Arc::new(HttpConnector::with_defaults()),
            policy: ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                ..Default::default()
            },
            interceptor: None,
        };

        let runtime = PiJsRuntime::new().await.expect("runtime");
        runtime
                .eval(
                    r#"
                    globalThis.timeoutChunks = [];
                    globalThis.timeoutDone = false;
                    globalThis.timeoutErr = null;
                    (async () => {
                        try {
                            const stream = pi.exec("sh", ["-c", "sleep 10"], { stream: true, timeoutMs: 200 });
                            for await (const chunk of stream) {
                                globalThis.timeoutChunks.push(chunk);
                            }
                            globalThis.timeoutDone = true;
                        } catch (e) {
                            globalThis.timeoutErr = e.message || String(e);
                        }
                    })();
                "#,
                )
                .await
                .expect("eval");

        for _ in 0..256 {
            let has_pending = pump_js_runtime_once(&runtime, &host)
                .await
                .expect("pump_once");
            if !has_pending {
                break;
            }
        }
        assert!(
            !runtime.has_pending(),
            "runtime should have no pending tasks after timeout stream"
        );

        let timeout_chunks = runtime
            .read_global_json("timeoutChunks")
            .await
            .expect("read timeoutChunks");
        let entries = timeout_chunks.as_array().expect("timeoutChunks array");
        assert!(!entries.is_empty(), "expected at least one final chunk");
        let final_chunk = entries.last().expect("final chunk");
        assert_eq!(final_chunk.get("killed"), Some(&Value::Bool(true)));
        assert!(
            final_chunk.get("code").and_then(Value::as_i64).is_some(),
            "expected numeric exit code in final chunk: {final_chunk:?}"
        );
        assert_eq!(
            runtime
                .read_global_json("timeoutDone")
                .await
                .expect("read timeoutDone"),
            Value::Bool(true)
        );
        assert_eq!(
            runtime
                .read_global_json("timeoutErr")
                .await
                .expect("read timeoutErr"),
            Value::Null
        );
    });
}

#[test]
#[cfg(unix)]
fn js_runtime_pump_once_exec_streaming_return_cancels_before_dispatch() {
    futures::executor::block_on(async {
        let dir = tempdir().expect("tempdir");
        let manager = ExtensionManager::new();
        let host = JsRuntimeHost {
            tools: Arc::new(ToolRegistry::new(&[], dir.path(), None)),
            manager_ref: Arc::downgrade(&manager.inner),
            manager_snapshot: Arc::clone(&manager.snapshot),
            manager_snapshot_version: Arc::clone(&manager.snapshot_version),
            http: Arc::new(HttpConnector::with_defaults()),
            policy: ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                ..Default::default()
            },
            interceptor: None,
        };

        let runtime = PiJsRuntime::new().await.expect("runtime");
        runtime
            .eval(
                r#"
                    globalThis.cancelDone = false;
                    (async () => {
                        const stream = pi.exec("sh", ["-c", "sleep 2"], { stream: true });
                        await stream.return();
                        globalThis.cancelDone = true;
                    })();
                "#,
            )
            .await
            .expect("eval");

        let start = Instant::now();
        for _ in 0..64 {
            let has_pending = pump_js_runtime_once(&runtime, &host)
                .await
                .expect("pump_once");
            if !has_pending {
                break;
            }
        }
        let elapsed = start.elapsed();

        assert!(
            !runtime.has_pending(),
            "runtime should not remain pending after stream.return() cancellation"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "stream cancellation should complete quickly, took {elapsed:?}",
        );
        assert_eq!(
            runtime
                .read_global_json("cancelDone")
                .await
                .expect("read cancelDone"),
            Value::Bool(true)
        );
    });
}

#[test]
fn extension_protocol_schema_accepts_all_variants() {
    let schema = compiled_extension_protocol_schema();
    for (label, message) in sample_protocol_messages() {
        let instance = serde_json::to_value(&message)
            .map_err(|err| format!("{label}: {err}"))
            .unwrap();

        let errors = schema
            .iter_errors(&instance)
            .map(|err| err.to_string())
            .collect::<Vec<_>>();
        assert!(
            errors.is_empty(),
            "{label}: schema validation failed:\n{}",
            errors.join("\n")
        );

        let json = serde_json::to_string(&message)
            .map_err(|err| format!("{label}: {err}"))
            .unwrap();
        let parsed = ExtensionMessage::parse_and_validate(&json)
            .map_err(|err| format!("{label}: parse_and_validate failed: {err}"))
            .unwrap();
        let parsed_json = serde_json::to_value(&parsed)
            .map_err(|err| format!("{label}: {err}"))
            .unwrap();
        assert_eq!(
            instance, parsed_json,
            "{label}: JSON changed after roundtrip"
        );
    }
}

#[test]
fn extension_protocol_schema_rejects_missing_required_fields() {
    let schema = compiled_extension_protocol_schema();

    let (_, message) = sample_protocol_messages()
        .into_iter()
        .find(|(label, _)| *label == "register")
        .expect("register sample");
    let mut instance = serde_json::to_value(&message).expect("serialize");

    // Missing "id"
    instance
        .as_object_mut()
        .expect("object")
        .remove("id")
        .expect("id present");
    assert!(
        schema.validate(&instance).is_err(),
        "schema should reject missing id"
    );
}

#[test]
fn parse_and_validate_rejects_unknown_type() {
    let json = r#"
        {
          "id": "msg-unknown",
          "version": "1.0",
          "type": "not_a_real_type",
          "payload": { "x": 1 }
        }
        "#;
    assert!(ExtensionMessage::parse_and_validate(json).is_err());
}

#[test]
fn parse_fs_host_call_message() {
    let json = r#"
        {
          "id": "msg-fs",
          "version": "1.0",
          "type": "host_call",
          "payload": {
            "call_id": "call-1",
            "capability": "read",
            "method": "fs",
            "params": { "op": "read", "path": "README.md" }
          }
        }
        "#;
    let msg = ExtensionMessage::parse_and_validate(json).unwrap();
    assert!(matches!(msg.body, ExtensionBody::HostCall(_)));
}

#[test]
fn required_capability_for_host_call_maps_tools_and_fs_ops() {
    let tool_read = HostCallPayload {
        call_id: "call-tool-read".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": { "path": "README.md" } }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    assert_eq!(
        required_capability_for_host_call(&tool_read).as_deref(),
        Some("read")
    );

    let tool_bash = HostCallPayload {
        call_id: "call-tool-bash".to_string(),
        capability: "exec".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "bash", "input": { "command": "echo hi" } }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    assert_eq!(
        required_capability_for_host_call(&tool_bash).as_deref(),
        Some("exec")
    );

    let fs_delete = HostCallPayload {
        call_id: "call-fs-delete".to_string(),
        capability: "write".to_string(),
        method: "fs".to_string(),
        params: json!({ "op": "delete", "path": "tmp.txt" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    assert_eq!(
        required_capability_for_host_call(&fs_delete).as_deref(),
        Some("write")
    );

    let env_get = HostCallPayload {
        call_id: "call-env-get".to_string(),
        capability: "env".to_string(),
        method: "env".to_string(),
        params: json!({ "name": "HOME" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    assert_eq!(
        required_capability_for_host_call(&env_get).as_deref(),
        Some("env")
    );

    let unknown = HostCallPayload {
        call_id: "call-unknown".to_string(),
        capability: "read".to_string(),
        method: "nope".to_string(),
        params: json!({}),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    assert!(required_capability_for_host_call(&unknown).is_none());
}

#[test]
fn fs_connector_denies_path_traversal_outside_cwd() {
    let dir = tempdir().expect("tempdir");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");

    let inside = project.join("inside.txt");
    std::fs::write(&inside, "hello").expect("write inside");

    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, "secret").expect("write outside");

    let policy = ExtensionPolicy::default();
    let scopes = FsScopes::for_cwd(&project).expect("scopes");
    let connector = FsConnector::new(project, policy, scopes).expect("connector");

    let ok_call = HostCallPayload {
        call_id: "call-ok".to_string(),
        capability: "read".to_string(),
        method: "fs".to_string(),
        params: json!({ "op": "read", "path": "inside.txt" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    let ok_result = connector.handle_host_call(&ok_call, None);
    assert!(!ok_result.is_error);

    let denied_call = HostCallPayload {
        call_id: "call-deny".to_string(),
        capability: "read".to_string(),
        method: "fs".to_string(),
        params: json!({ "op": "read", "path": "../outside.txt" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    let denied = connector.handle_host_call(&denied_call, None);
    assert!(denied.is_error);
    assert_eq!(
        denied.error.as_ref().expect("error").code,
        HostCallErrorCode::Denied
    );
}

#[test]
fn fs_connector_denies_write_escape_via_dotdot_segments() {
    let dir = tempdir().expect("tempdir");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");

    let policy = ExtensionPolicy::default();
    let scopes = FsScopes::for_cwd(&project).expect("scopes");
    let connector = FsConnector::new(&project, policy, scopes).expect("connector");

    let denied_call = HostCallPayload {
        call_id: "call-write-deny".to_string(),
        capability: "write".to_string(),
        method: "fs".to_string(),
        params: json!({
            "op": "write",
            "path": "subdir/../../outside.txt",
            "data": "secret",
        }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    let denied = connector.handle_host_call(&denied_call, None);
    assert!(denied.is_error);
    assert_eq!(
        denied.error.as_ref().expect("error").code,
        HostCallErrorCode::Denied
    );
}

#[cfg(unix)]
#[test]
fn fs_connector_denies_symlink_escape() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().expect("tempdir");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");

    let outside = dir.path().join("secret.txt");
    std::fs::write(&outside, "secret").expect("write outside");

    let link = project.join("link.txt");
    symlink(&outside, &link).expect("symlink");

    let policy = ExtensionPolicy::default();
    let scopes = FsScopes::for_cwd(&project).expect("scopes");
    let connector = FsConnector::new(project, policy, scopes).expect("connector");

    let call = HostCallPayload {
        call_id: "call-link".to_string(),
        capability: "read".to_string(),
        method: "fs".to_string(),
        params: json!({ "op": "read", "path": "link.txt" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    let result = connector.handle_host_call(&call, None);
    assert!(result.is_error);
    assert_eq!(
        result.error.as_ref().expect("error").code,
        HostCallErrorCode::Denied
    );
}

#[test]
fn fs_connector_denies_when_policy_denies_capability() {
    let dir = tempdir().expect("tempdir");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");

    let inside = project.join("inside.txt");
    std::fs::write(&inside, "hello").expect("write inside");

    let mut policy = ExtensionPolicy::default();
    policy.deny_caps.push("read".to_string());

    let scopes = FsScopes::for_cwd(&project).expect("scopes");
    let connector = FsConnector::new(&project, policy, scopes).expect("connector");

    let call = HostCallPayload {
        call_id: "call-policy-deny".to_string(),
        capability: "read".to_string(),
        method: "fs".to_string(),
        params: json!({ "op": "read", "path": "inside.txt" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let result = connector.handle_host_call(&call, None);
    assert!(result.is_error);
    assert_eq!(
        result.error.as_ref().expect("error").code,
        HostCallErrorCode::Denied
    );
}

#[test]
fn fs_connector_respects_per_extension_deny() {
    let dir = tempdir().expect("tempdir");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");

    let inside = project.join("inside.txt");
    std::fs::write(&inside, "hello").expect("write inside");

    let mut policy = ExtensionPolicy::default();
    policy.per_extension.insert(
        "ext-untrusted".to_string(),
        ExtensionOverride {
            deny: vec!["read".to_string()],
            ..ExtensionOverride::default()
        },
    );

    let scopes = FsScopes::for_cwd(&project).expect("scopes");
    let connector = FsConnector::new(&project, policy, scopes).expect("connector");

    let call = HostCallPayload {
        call_id: "call-ext-deny".to_string(),
        capability: "read".to_string(),
        method: "fs".to_string(),
        params: json!({ "op": "read", "path": "inside.txt" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let denied = connector.handle_host_call(&call, Some("ext-untrusted"));
    assert!(denied.is_error);
    assert_eq!(
        denied.error.as_ref().expect("error").code,
        HostCallErrorCode::Denied
    );

    let allowed = connector.handle_host_call(&call, Some("ext-trusted"));
    assert!(!allowed.is_error);
}

#[test]
fn fs_connector_denies_write_when_manifest_does_not_declare_write_scope() {
    let dir = tempdir().expect("tempdir");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");

    let inside = project.join("inside.txt");
    std::fs::write(&inside, "hello").expect("write inside");

    let manifest = CapabilityManifest {
        schema: "pi.ext.cap.v1".to_string(),
        capabilities: vec![CapabilityRequirement {
            capability: "read".to_string(),
            methods: vec!["fs".to_string()],
            intents: Vec::new(),
            connector_classes: Vec::new(),
            hostcall_classes: Vec::new(),
            risk_tier: None,
            scope: Some(CapabilityScope {
                paths: Some(vec![".".to_string()]),
                hosts: None,
                env: None,
                allowed_tools: None,
            }),
            provenance: None,
        }],
    };
    let scopes = FsScopes::from_manifest(Some(&manifest), &project).expect("scopes");
    let connector =
        FsConnector::new(&project, ExtensionPolicy::default(), scopes).expect("connector");

    let call = HostCallPayload {
        call_id: "call-scope-deny".to_string(),
        capability: "write".to_string(),
        method: "fs".to_string(),
        params: json!({ "op": "write", "path": "inside.txt" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let result = connector.handle_host_call(&call, None);
    assert!(result.is_error);
    assert_eq!(
        result.error.as_ref().expect("error").code,
        HostCallErrorCode::Denied
    );
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                if let Some(value) = map.get(&key) {
                    out.insert(key, canonicalize_json(value));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

fn sha256_hex(input: &str) -> String {
    use std::fmt::Write as _;

    let mut hasher = sha2::Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();

    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut out, "{byte:02x}").expect("write hex");
    }
    out
}

fn hostcall_params_hash(method: &str, params: &Value) -> String {
    let canonical = canonicalize_json(&json!({ "method": method, "params": params }));
    let json = serde_json::to_string(&canonical).expect("serialize canonical hostcall");
    sha256_hex(&json)
}

fn hostcall_ledger_start_data(call: &HostCallPayload) -> Value {
    let mut data = serde_json::Map::new();
    data.insert(
        "capability".to_string(),
        Value::String(call.capability.clone()),
    );
    data.insert("method".to_string(), Value::String(call.method.clone()));
    data.insert(
        "params_hash".to_string(),
        Value::String(hostcall_params_hash(&call.method, &call.params)),
    );
    if let Some(timeout_ms) = call.timeout_ms {
        data.insert("timeout_ms".to_string(), json!(timeout_ms));
    }
    Value::Object(data)
}

fn hostcall_ledger_end_data(
    call: &HostCallPayload,
    duration_ms: u64,
    result: &HostResultPayload,
) -> Value {
    let mut data = serde_json::Map::new();
    data.insert(
        "capability".to_string(),
        Value::String(call.capability.clone()),
    );
    data.insert("method".to_string(), Value::String(call.method.clone()));
    data.insert(
        "params_hash".to_string(),
        Value::String(hostcall_params_hash(&call.method, &call.params)),
    );
    if let Some(timeout_ms) = call.timeout_ms {
        data.insert("timeout_ms".to_string(), json!(timeout_ms));
    }
    data.insert("duration_ms".to_string(), json!(duration_ms));
    data.insert("is_error".to_string(), Value::Bool(result.is_error));
    if result.is_error {
        if let Some(error) = result.error.as_ref() {
            data.insert("error".to_string(), json!({ "code": error.code }));
        }
    }
    Value::Object(data)
}

#[test]
fn hostcall_params_hash_is_stable_for_key_ordering() {
    let mut first = serde_json::Map::new();
    first.insert("b".to_string(), json!(2));
    first.insert("a".to_string(), json!(1));
    let first = Value::Object(first);

    let mut second = serde_json::Map::new();
    second.insert("a".to_string(), json!(1));
    second.insert("b".to_string(), json!(2));
    let second = Value::Object(second);

    assert_eq!(
        hostcall_params_hash("http", &first),
        hostcall_params_hash("http", &second)
    );
    assert_ne!(
        hostcall_params_hash("http", &first),
        hostcall_params_hash("tool", &first)
    );
}

#[test]
fn hostcall_params_shape_hash_ignores_scalar_value_drift() {
    let first = json!({
        "url": "https://example.com/a",
        "headers": { "authorization": "Bearer abc" },
        "retries": 3,
        "flags": [true, false]
    });
    let second = json!({
        "url": "https://another.example/path",
        "headers": { "authorization": "Bearer xyz" },
        "retries": 99,
        "flags": [false, true]
    });
    assert_eq!(
        hostcall_params_shape_hash("http", &first),
        hostcall_params_shape_hash("http", &second),
        "shape hash should remain stable when only scalar values change"
    );
    assert_ne!(
        hostcall_params_shape_hash("http", &first),
        hostcall_params_shape_hash("tool", &first),
        "method remains part of the shape identity"
    );
}

#[test]
fn runtime_hostcall_resource_target_class_detects_common_targets() {
    assert_eq!(
        runtime_hostcall_resource_target_class(
            "http",
            &json!({"url":"http://127.0.0.1:8080/health"})
        ),
        "network.private"
    );
    assert_eq!(
        runtime_hostcall_resource_target_class(
            "http",
            &json!({"url":"https://api.example.com/v1/messages"})
        ),
        "network.public"
    );
    assert_eq!(
        runtime_hostcall_resource_target_class(
            "tool",
            &json!({"name":"read","input":{"path":"README.md"}})
        ),
        "filesystem.tool"
    );
    assert_eq!(
        runtime_hostcall_resource_target_class("tool", &json!({"name":"bash","input":{}})),
        "subprocess.tool"
    );
}

#[test]
fn hostcall_ledger_start_redacts_params_and_includes_hash() {
    let call = HostCallPayload {
        call_id: "host-ledger-1".to_string(),
        capability: "env".to_string(),
        method: "env".to_string(),
        params: json!({ "name": "ANTHROPIC_API_KEY", "value": "sk-ant-SECRET" }),
        timeout_ms: Some(1234),
        cancel_token: None,
        context: None,
    };

    let data = hostcall_ledger_start_data(&call);
    let obj = data.as_object().expect("object");
    assert!(obj.get("params_hash").is_some());
    assert!(obj.get("params").is_none());

    let encoded = serde_json::to_string(&data).expect("serialize data");
    assert!(!encoded.contains("sk-ant-SECRET"));
    assert!(!encoded.contains("ANTHROPIC_API_KEY"));
}

#[test]
fn hostcall_ledger_end_includes_error_code_when_is_error() {
    let call = HostCallPayload {
        call_id: "host-ledger-2".to_string(),
        capability: "exec".to_string(),
        method: "exec".to_string(),
        params: json!({ "cmd": "ls", "args": ["-la"] }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let result = HostResultPayload {
        call_id: call.call_id.clone(),
        output: json!({}),
        is_error: true,
        error: Some(HostCallError {
            code: HostCallErrorCode::Denied,
            message: "Denied".to_string(),
            details: None,
            retryable: None,
        }),
        chunk: None,
    };

    let data = hostcall_ledger_end_data(&call, 10, &result);
    let obj = data.as_object().expect("object");
    assert_eq!(obj.get("is_error").and_then(Value::as_bool), Some(true));

    let error = obj
        .get("error")
        .and_then(Value::as_object)
        .expect("error object");
    assert_eq!(error.get("code").and_then(Value::as_str), Some("denied"));
}

#[derive(Debug, Clone)]
pub(super) struct CapturedEvent {
    level: tracing::Level,
    pub(super) fields: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct CaptureLayer {
    events: std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>,
}

impl CaptureLayer {
    fn snapshot(&self) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

struct FieldVisitor<'a> {
    fields: &'a mut std::collections::BTreeMap<String, String>,
}

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = std::collections::BTreeMap::new();
        let mut visitor = FieldVisitor {
            fields: &mut fields,
        };
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("events mutex")
            .push(CapturedEvent {
                level: *event.metadata().level(),
                fields,
            });
    }
}

pub(super) fn capture_tracing_events<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedEvent>) {
    use tracing_subscriber::layer::SubscriberExt as _;

    let capture = CaptureLayer::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let result = tracing::subscriber::with_default(subscriber, f);
    (result, capture.snapshot())
}

pub(super) fn run_async<T, Fut>(future: Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");
    runtime.block_on(future)
}

pub(super) fn extension_manager_no_persisted_permissions() -> ExtensionManager {
    let manager = ExtensionManager::new();
    {
        let mut guard = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Unit tests should be deterministic and should never mutate the user's
        // global permissions file.
        guard.permission_store = None;
        guard.policy_prompt_cache.clear();
    }
    manager
}

#[test]
fn check_version_constraint_accepts_compound_range() {
    assert!(check_version_constraint("2.5.1", ">=2.0.0 <3.0.0"));
    assert!(check_version_constraint("2.5.1", ">=2.0.0, <3.0.0"));
    assert!(check_version_constraint("2.5.1", ">= 2.0.0, < 3.0.0"));
    assert!(!check_version_constraint("3.0.0", ">=2.0.0 <3.0.0"));
    assert!(check_version_constraint("1.2.4", ">1.2.3 <=2.0.0"));
    assert!(!check_version_constraint("1.2.3", ">1.2.3 <=2.0.0"));
    assert!(!check_version_constraint("2.5.1", ">=2.0.0,"));
}

#[test]
fn check_version_constraint_preserves_prerelease_semantics() {
    assert!(check_version_constraint("2.0.0-beta.1", "2.0.0-beta.1"));
    assert!(!check_version_constraint("2.0.0-beta.2", "2.0.0-beta.1"));
    assert!(!check_version_constraint("2.0.0-beta.1", "2.0.0"));
    assert!(!check_version_constraint("2.0.0", "2.0.0-beta.1"));
}

#[test]
fn check_version_constraint_treats_bare_literals_as_exact_matches() {
    assert!(check_version_constraint("2.0.0", "2.0.0"));
    assert!(!check_version_constraint("2.0.1", "2.0.0"));
    assert!(check_version_constraint("2.0.0", "2.0"));
    assert!(!check_version_constraint("2.0.1", "2.0"));
    assert!(!check_version_constraint("1.0.0", "1"));
}

#[test]
fn cached_policy_prompt_decision_honors_compound_version_range() {
    let manager = extension_manager_no_persisted_permissions();
    {
        let mut guard = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extensions.push(RegisterPayload {
            name: "ext-1".to_string(),
            version: "2.5.1".to_string(),
            api_version: "1.0.0".to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        });
        guard
            .extension_versions
            .insert("ext-1".to_string(), "2.5.1".to_string());
        guard.policy_prompt_cache.insert(
            "ext-1".to_string(),
            HashMap::from([(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2026-01-01T00:00:00Z".to_string(),
                    expires_at: None,
                    version_range: Some(">=2.0.0, <3.0.0".to_string()),
                },
            )]),
        );
    }

    assert_eq!(
        manager.cached_policy_prompt_decision("ext-1", "exec"),
        Some(true)
    );
}

#[test]
fn cached_policy_prompt_decision_uses_runtime_extension_id_for_named_extension() {
    let manager = extension_manager_no_persisted_permissions();
    {
        let mut guard = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extensions.push(RegisterPayload {
            name: "Friendly Extension".to_string(),
            version: "2.5.1".to_string(),
            api_version: "1.0.0".to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        });
        guard
            .extension_versions
            .insert("ext.named".to_string(), "2.5.1".to_string());
        guard.policy_prompt_cache.insert(
            "ext.named".to_string(),
            HashMap::from([(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2026-01-01T00:00:00Z".to_string(),
                    expires_at: None,
                    version_range: Some("^2.5.1".to_string()),
                },
            )]),
        );
    }

    assert_eq!(
        manager.cached_policy_prompt_decision("ext.named", "exec"),
        Some(true)
    );
}

#[test]
fn cache_policy_prompt_decision_uses_runtime_extension_id_for_named_extension() {
    let manager = extension_manager_no_persisted_permissions();
    {
        let mut guard = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extensions.push(RegisterPayload {
            name: "Friendly Extension".to_string(),
            version: "2.5.1".to_string(),
            api_version: "1.0.0".to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        });
        guard
            .extension_versions
            .insert("ext.named".to_string(), "2.5.1".to_string());
    }

    manager.cache_policy_prompt_decision("ext.named", "exec", true);

    let decision = manager
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .policy_prompt_cache
        .get("ext.named")
        .and_then(|by_cap| by_cap.get("exec"))
        .expect("cached decision")
        .clone();
    assert_eq!(decision.version_range.as_deref(), Some("^2.5.1"));
}

#[test]
fn invalid_permissions_file_still_allows_future_decisions_to_persist() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("extension-permissions.json");
    std::fs::write(&path, r#"{"version":999,"decisions":{}}"#)
        .expect("write invalid permissions file");

    let manager = extension_manager_no_persisted_permissions();
    {
        let mut guard = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ExtensionManager::load_persisted_permissions_from(&mut guard, &path);
        assert!(guard.permission_store.is_some());
        guard.extensions.push(RegisterPayload {
            name: "Friendly Extension".to_string(),
            version: "1.2.3".to_string(),
            api_version: "1.0.0".to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        });
        guard
            .extension_versions
            .insert("ext.persist".to_string(), "1.2.3".to_string());
    }

    manager.cache_policy_prompt_decision("ext.persist", "exec", true);

    let store = PermissionStore::open(&path).expect("reload permissions file");
    assert_eq!(store.lookup("ext.persist", "exec"), Some(true));

    let raw = std::fs::read_to_string(&path).expect("read repaired permissions file");
    assert!(raw.contains("\"version\": 1"));
    assert!(raw.contains("\"ext.persist\""));
}

#[test]
fn cached_policy_prompt_decision_rejects_out_of_range_version() {
    let manager = extension_manager_no_persisted_permissions();
    {
        let mut guard = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extensions.push(RegisterPayload {
            name: "ext-1".to_string(),
            version: "3.0.0".to_string(),
            api_version: "1.0.0".to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        });
        guard
            .extension_versions
            .insert("ext-1".to_string(), "3.0.0".to_string());
        guard.policy_prompt_cache.insert(
            "ext-1".to_string(),
            HashMap::from([(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2026-01-01T00:00:00Z".to_string(),
                    expires_at: None,
                    version_range: Some(">=2.0.0 <3.0.0".to_string()),
                },
            )]),
        );
    }

    assert_eq!(manager.cached_policy_prompt_decision("ext-1", "exec"), None);
}

#[test]
fn cached_policy_prompt_decision_rejects_prerelease_mismatch() {
    let manager = extension_manager_no_persisted_permissions();
    {
        let mut guard = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extensions.push(RegisterPayload {
            name: "ext-1".to_string(),
            version: "2.0.0-beta.1".to_string(),
            api_version: "1.0.0".to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        });
        guard
            .extension_versions
            .insert("ext-1".to_string(), "2.0.0-beta.1".to_string());
        guard.policy_prompt_cache.insert(
            "ext-1".to_string(),
            HashMap::from([(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2026-01-01T00:00:00Z".to_string(),
                    expires_at: None,
                    version_range: Some("2.0.0".to_string()),
                },
            )]),
        );
    }

    assert_eq!(manager.cached_policy_prompt_decision("ext-1", "exec"), None);
}

#[test]
fn cached_policy_prompt_decision_rejects_exact_bare_version_mismatch() {
    let manager = extension_manager_no_persisted_permissions();
    {
        let mut guard = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extensions.push(RegisterPayload {
            name: "ext-1".to_string(),
            version: "2.0.1".to_string(),
            api_version: "1.0.0".to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        });
        guard
            .extension_versions
            .insert("ext-1".to_string(), "2.0.1".to_string());
        guard.policy_prompt_cache.insert(
            "ext-1".to_string(),
            HashMap::from([(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2026-01-01T00:00:00Z".to_string(),
                    expires_at: None,
                    version_range: Some("2.0.0".to_string()),
                },
            )]),
        );
    }

    assert_eq!(manager.cached_policy_prompt_decision("ext-1", "exec"), None);
}

#[test]
#[allow(clippy::too_many_lines)]
fn js_hostcall_prompt_policy_caches_user_allow_and_never_logs_raw_params() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();

    let manager = extension_manager_no_persisted_permissions();

    let host = JsRuntimeHost {
        tools: Arc::new(crate::tools::ToolRegistry::new(&[], &cwd, None)),
        manager_ref: Arc::downgrade(&manager.inner),
        manager_snapshot: Arc::clone(&manager.snapshot),
        manager_snapshot_version: Arc::clone(&manager.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Prompt,
            max_memory_mb: 256,
            default_caps: Vec::new(),
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    let request = crate::extensions_js::HostcallRequest {
        call_id: "hostcall-1".to_string(),
        kind: crate::extensions_js::HostcallKind::Tool {
            name: "custom_tool".to_string(),
        },
        payload: serde_json::json!({
            "token": "supersecret",
            "nested": { "apiKey": "sk-ant-SECRET" }
        }),
        trace_id: 0,
        extension_id: Some("ext-1".to_string()),
    };

    let request_cached = crate::extensions_js::HostcallRequest {
        call_id: "hostcall-2".to_string(),
        kind: crate::extensions_js::HostcallKind::Tool {
            name: "custom_tool".to_string(),
        },
        payload: serde_json::json!({ "token": "supersecret" }),
        trace_id: 0,
        extension_id: Some("ext-1".to_string()),
    };

    let ((first, second), events) = capture_tracing_events(|| {
        run_async(async {
            use asupersync::time::{timeout, wall_now};
            use std::time::Duration;

            let cx = asupersync::Cx::for_request();
            let (ui_tx, mut ui_rx) = asupersync::channel::mpsc::channel(8);
            manager.set_ui_sender(ui_tx);

            let ui_task = async {
                let ui_request = timeout(wall_now(), Duration::from_secs(2), ui_rx.recv(&cx))
                    .await
                    .expect("timed out waiting for ui request")
                    .expect("ui request");
                assert_eq!(ui_request.method, "confirm");

                assert!(
                    manager.respond_ui(ExtensionUiResponse {
                        id: ui_request.id,
                        value: Some(serde_json::Value::Bool(true)),
                        cancelled: false,
                    }),
                    "respond_ui"
                );

                // Ensure the allow decision is cached (second hostcall should not prompt again).
                if let Ok(Ok(_)) =
                    timeout(wall_now(), Duration::from_millis(200), ui_rx.recv(&cx)).await
                {
                    panic!();
                }
            };

            let hostcalls = async {
                let first = super::dispatch_hostcall(&host, request).await;
                let second = super::dispatch_hostcall(&host, request_cached).await;
                (first, second)
            };

            let ((), (first, second)) = futures::join!(ui_task, hostcalls);
            (first, second)
        })
    });

    assert!(matches!(first, HostcallOutcome::Error { code, .. } if code == "invalid_request"));
    assert!(matches!(second, HostcallOutcome::Error { code, .. } if code == "invalid_request"));

    let decision_events = events
        .iter()
        .filter(|event| {
            event
                .fields
                .get("event")
                .is_some_and(|value| value.contains("policy.decision"))
        })
        .collect::<Vec<_>>();
    assert_eq!(decision_events.len(), 2);
    assert!(
        decision_events[0]
            .fields
            .get("reason")
            .is_some_and(|value| value.contains("prompt_user_allow")),
        "expected prompt_user_allow reason, got {:?}",
        decision_events[0].fields
    );
    assert!(
        decision_events[1]
            .fields
            .get("reason")
            .is_some_and(|value| value.contains("prompt_cache_allow")),
        "expected prompt_cache_allow reason, got {:?}",
        decision_events[1].fields
    );

    for event in &events {
        for value in event.fields.values() {
            assert!(
                !value.contains("supersecret"),
                "secret leaked into logs: {value}"
            );
            assert!(
                !value.contains("sk-ant-SECRET"),
                "api key leaked into logs: {value}"
            );
        }
    }

    let params_hash = decision_events[0]
        .fields
        .get("params_hash")
        .expect("params_hash");
    let params_hash = params_hash.trim_matches('"');
    assert_eq!(params_hash.len(), 64);
    assert!(params_hash.chars().all(|ch| ch.is_ascii_hexdigit()));
}

#[test]
fn js_hostcall_strict_policy_denies_and_logs_reason() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();

    let mgr = ExtensionManager::new();
    let host = JsRuntimeHost {
        tools: Arc::new(crate::tools::ToolRegistry::new(&[], &cwd, None)),
        manager_ref: Arc::downgrade(&mgr.inner),
        manager_snapshot: Arc::clone(&mgr.snapshot),
        manager_snapshot_version: Arc::clone(&mgr.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            max_memory_mb: 256,
            default_caps: vec!["read".to_string()],
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    let request = crate::extensions_js::HostcallRequest {
        call_id: "hostcall-strict-1".to_string(),
        kind: crate::extensions_js::HostcallKind::Exec {
            cmd: "does-not-run".to_string(),
        },
        payload: serde_json::json!({}),
        trace_id: 0,
        extension_id: Some("ext-1".to_string()),
    };

    let (outcome, events) = capture_tracing_events(|| {
        run_async(async { super::dispatch_hostcall(&host, request).await })
    });

    assert!(matches!(outcome, HostcallOutcome::Error { code, .. } if code == "denied"));

    let decision = events.iter().find(|event| {
        event
            .fields
            .get("event")
            .is_some_and(|value| value.contains("policy.decision"))
    });
    let decision = decision.expect("policy.decision event");
    assert_eq!(decision.level, tracing::Level::WARN);
    assert!(
        decision
            .fields
            .get("reason")
            .is_some_and(|value| value.contains("not_in_default_caps"))
    );
    assert!(
        decision
            .fields
            .get("call_id")
            .is_some_and(|value| value.contains("hostcall-strict-1"))
    );
}

#[test]
fn shared_dispatcher_logs_runtime_from_context() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let tools = Arc::new(crate::tools::ToolRegistry::new(&[], &cwd, None));
    let http = Arc::new(crate::connectors::http::HttpConnector::with_defaults());
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    };
    let call = HostCallPayload {
        call_id: "runtime-log-1".to_string(),
        capability: "ui".to_string(),
        method: "ui".to_string(),
        params: serde_json::json!({ "op": "confirm" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let (_result, events) = capture_tracing_events(|| {
        run_async(async {
            let ctx = HostCallContext {
                runtime_name: "protocol",
                extension_id: Some("ext-log"),
                tools: &tools,
                http: &http,
                manager: None,
                policy: &policy,
                js_runtime: None,
                interceptor: None,
            };
            dispatch_host_call_shared(&ctx, call).await
        })
    });

    let start = events.iter().find(|event| {
        event
            .fields
            .get("event")
            .is_some_and(|value| value.contains("host_call.start"))
    });
    let start = start.expect("expected host_call.start event in trace");
    assert_eq!(
        start.fields.get("runtime").map(std::string::String::as_str),
        Some("protocol")
    );
}

#[test]
fn js_hostcall_ui_missing_op_is_invalid_request() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let manager = extension_manager_no_persisted_permissions();

    let host = JsRuntimeHost {
        tools: Arc::new(crate::tools::ToolRegistry::new(&[], &cwd, None)),
        manager_ref: Arc::downgrade(&manager.inner),
        manager_snapshot: Arc::clone(&manager.snapshot),
        manager_snapshot_version: Arc::clone(&manager.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            max_memory_mb: 256,
            default_caps: vec!["ui".to_string()],
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    let request = crate::extensions_js::HostcallRequest {
        call_id: "hostcall-ui-missing-op".to_string(),
        kind: crate::extensions_js::HostcallKind::Ui { op: String::new() },
        payload: serde_json::json!({}),
        trace_id: 0,
        extension_id: Some("ext-ui".to_string()),
    };

    let outcome = run_async(async { super::dispatch_hostcall(&host, request).await });
    assert!(matches!(outcome, HostcallOutcome::Error { code, .. } if code == "invalid_request"));
}

#[test]
fn js_hostcall_ui_timeout_maps_to_timeout_taxonomy() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let manager = extension_manager_no_persisted_permissions();
    let (ui_tx, _ui_rx) = mpsc::channel(8);
    manager.set_ui_sender(ui_tx);

    let host = JsRuntimeHost {
        tools: Arc::new(crate::tools::ToolRegistry::new(&[], &cwd, None)),
        manager_ref: Arc::downgrade(&manager.inner),
        manager_snapshot: Arc::clone(&manager.snapshot),
        manager_snapshot_version: Arc::clone(&manager.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            max_memory_mb: 256,
            default_caps: vec!["ui".to_string()],
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    let request = crate::extensions_js::HostcallRequest {
        call_id: "hostcall-ui-timeout".to_string(),
        kind: crate::extensions_js::HostcallKind::Ui {
            op: "confirm".to_string(),
        },
        payload: serde_json::json!({ "timeout": 10 }),
        trace_id: 0,
        extension_id: Some("ext-ui".to_string()),
    };

    let outcome = run_async(async { super::dispatch_hostcall(&host, request).await });
    assert!(matches!(outcome, HostcallOutcome::Error { code, .. } if code == "timeout"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn js_hostcall_capability_denial_matrix_emits_deterministic_errors_and_logs() {
    use std::sync::Arc;

    #[derive(Clone)]
    struct DenyCase {
        call_id: &'static str,
        kind: crate::extensions_js::HostcallKind,
        payload: serde_json::Value,
        capability: &'static str,
        reason: &'static str,
    }

    fn to_request(case: &DenyCase) -> crate::extensions_js::HostcallRequest {
        crate::extensions_js::HostcallRequest {
            call_id: case.call_id.to_string(),
            kind: case.kind.clone(),
            payload: case.payload.clone(),
            trace_id: 0,
            extension_id: Some("ext.test".to_string()),
        }
    }

    fn assert_denied(outcome: &HostcallOutcome, capability: &str, reason: &str) {
        match outcome {
            HostcallOutcome::Error { code, message } => {
                assert_eq!(code, "denied");
                assert!(
                    message.contains(&format!(
                        "Capability '{capability}' denied by policy ({reason})"
                    )),
                    "unexpected denial message: {message}"
                );
            }
            other @ (HostcallOutcome::Success(_) | HostcallOutcome::StreamChunk { .. }) => {
                panic!();
            }
        }
    }

    fn assert_policy_decision_logged(
        events: &[CapturedEvent],
        call_id: &str,
        capability: &str,
        reason: &str,
    ) {
        let matching = events
            .iter()
            .filter(|event| {
                event
                    .fields
                    .get("event")
                    .is_some_and(|value| value.contains("policy.decision"))
                    && event
                        .fields
                        .get("call_id")
                        .is_some_and(|value| value.contains(call_id))
            })
            .collect::<Vec<_>>();

        assert!(
            !matching.is_empty(),
            "expected policy.decision log for call_id={call_id}; got events: {events:#?}"
        );

        assert!(
            matching.iter().any(|event| {
                event.level == tracing::Level::WARN
                    && event
                        .fields
                        .get("capability")
                        .is_some_and(|value| value.contains(capability))
                    && event
                        .fields
                        .get("reason")
                        .is_some_and(|value| value.contains(reason))
            }),
            "expected WARN policy.decision with capability={capability} reason={reason} for call_id={call_id}; got: {matching:#?}"
        );
    }

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let tools = Arc::new(crate::tools::ToolRegistry::new(
        &["read", "write", "bash"],
        &cwd,
        None,
    ));

    // Strict: deny anything not in default_caps.
    let mgr_strict = ExtensionManager::new();
    let host_strict = JsRuntimeHost {
        tools: Arc::clone(&tools),
        manager_ref: Arc::downgrade(&mgr_strict.inner),
        manager_snapshot: Arc::clone(&mgr_strict.snapshot),
        manager_snapshot_version: Arc::clone(&mgr_strict.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            max_memory_mb: 256,
            default_caps: vec!["read".to_string()],
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    let strict_cases = vec![
        DenyCase {
            call_id: "deny-strict-exec",
            kind: crate::extensions_js::HostcallKind::Exec {
                cmd: "does-not-run".to_string(),
            },
            payload: serde_json::json!({}),
            capability: "exec",
            reason: "not_in_default_caps",
        },
        DenyCase {
            call_id: "deny-strict-http",
            kind: crate::extensions_js::HostcallKind::Http,
            payload: serde_json::json!({ "url": "https://example.com", "method": "GET" }),
            capability: "http",
            reason: "not_in_default_caps",
        },
        DenyCase {
            call_id: "deny-strict-session",
            kind: crate::extensions_js::HostcallKind::Session {
                op: "get_name".to_string(),
            },
            payload: serde_json::json!({}),
            capability: "session",
            reason: "not_in_default_caps",
        },
        DenyCase {
            call_id: "deny-strict-ui",
            kind: crate::extensions_js::HostcallKind::Ui {
                op: "confirm".to_string(),
            },
            payload: serde_json::json!({ "title": "t", "message": "m" }),
            capability: "ui",
            reason: "not_in_default_caps",
        },
        DenyCase {
            call_id: "deny-strict-events",
            kind: crate::extensions_js::HostcallKind::Events {
                op: "getTools".to_string(),
            },
            payload: serde_json::json!({}),
            capability: "events",
            reason: "not_in_default_caps",
        },
        // Use a tool hostcall to cover filesystem-ish access (write capability).
        DenyCase {
            call_id: "deny-strict-write",
            kind: crate::extensions_js::HostcallKind::Tool {
                name: "write".to_string(),
            },
            payload: serde_json::json!({ "path": "note.txt", "content": "hi" }),
            capability: "write",
            reason: "not_in_default_caps",
        },
    ];

    let (strict_outcomes, strict_events) = capture_tracing_events(|| {
        run_async(async {
            let mut out = Vec::new();
            for case in &strict_cases {
                let outcome = super::dispatch_hostcall(&host_strict, to_request(case)).await;
                out.push((case.call_id, case.capability, case.reason, outcome));
            }
            out
        })
    });

    for (call_id, capability, reason, outcome) in &strict_outcomes {
        assert_denied(outcome, capability, reason);
        assert_policy_decision_logged(&strict_events, call_id, capability, reason);
    }

    // Prompt: non-default capabilities trigger UI, simulate user deny for each capability.
    let manager_prompt = extension_manager_no_persisted_permissions();

    let host_prompt = JsRuntimeHost {
        tools: Arc::clone(&tools),
        manager_ref: Arc::downgrade(&manager_prompt.inner),
        manager_snapshot: Arc::clone(&manager_prompt.snapshot),
        manager_snapshot_version: Arc::clone(&manager_prompt.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Prompt,
            max_memory_mb: 256,
            default_caps: vec!["read".to_string()],
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    let prompt_cases = strict_cases
        .iter()
        .map(|case| DenyCase {
            call_id: match case.call_id {
                "deny-strict-exec" => "deny-prompt-exec",
                "deny-strict-http" => "deny-prompt-http",
                "deny-strict-session" => "deny-prompt-session",
                "deny-strict-ui" => "deny-prompt-ui",
                "deny-strict-events" => "deny-prompt-events",
                "deny-strict-write" => "deny-prompt-write",
                _ => "deny-prompt-unknown",
            },
            kind: case.kind.clone(),
            payload: case.payload.clone(),
            capability: case.capability,
            reason: "prompt_user_deny",
        })
        .collect::<Vec<_>>();

    let (prompt_outcomes, prompt_events) = capture_tracing_events(|| {
        run_async(async {
            use asupersync::time::{timeout, wall_now};
            use std::time::Duration;

            let cx = asupersync::Cx::for_request();
            let (ui_tx, mut ui_rx) = asupersync::channel::mpsc::channel(16);
            manager_prompt.set_ui_sender(ui_tx);
            let prompt_count = prompt_cases.len();

            let ui_task = async {
                for _ in 0..prompt_count {
                    let ui_request = timeout(wall_now(), Duration::from_secs(2), ui_rx.recv(&cx))
                        .await
                        .expect("timed out waiting for ui request")
                        .expect("ui request");
                    assert_eq!(ui_request.method, "confirm");

                    assert!(
                        manager_prompt.respond_ui(ExtensionUiResponse {
                            id: ui_request.id,
                            value: Some(serde_json::Value::Bool(false)),
                            cancelled: false,
                        }),
                        "respond_ui"
                    );
                }

                // Ensure we don't leak an extra prompt that would hang on future runs.
                if let Ok(Ok(_)) =
                    timeout(wall_now(), Duration::from_millis(200), ui_rx.recv(&cx)).await
                {
                    panic!();
                }
            };

            let hostcalls = async {
                let mut out = Vec::new();
                for case in &prompt_cases {
                    let outcome = super::dispatch_hostcall(&host_prompt, to_request(case)).await;
                    out.push((case.call_id, case.capability, case.reason, outcome));
                }
                out
            };

            let ((), out) = futures::join!(ui_task, hostcalls);
            out
        })
    });

    for (call_id, capability, reason, outcome) in &prompt_outcomes {
        assert_denied(outcome, capability, reason);
        assert_policy_decision_logged(&prompt_events, call_id, capability, reason);
    }

    // Permissive: deny_caps still takes precedence and must produce deterministic denial.
    let mgr_perm = ExtensionManager::new();
    let host_perm = JsRuntimeHost {
        tools,
        manager_ref: Arc::downgrade(&mgr_perm.inner),
        manager_snapshot: Arc::clone(&mgr_perm.snapshot),
        manager_snapshot_version: Arc::clone(&mgr_perm.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Permissive,
            max_memory_mb: 256,
            default_caps: Vec::new(),
            deny_caps: vec!["http".to_string()],
            ..Default::default()
        },
        interceptor: None,
    };

    let perm_case = DenyCase {
        call_id: "deny-permissive-http",
        kind: crate::extensions_js::HostcallKind::Http,
        payload: serde_json::json!({ "url": "https://example.com" }),
        capability: "http",
        reason: "deny_caps",
    };

    let (perm_outcome, perm_events) = capture_tracing_events(|| {
        run_async(async { super::dispatch_hostcall(&host_perm, to_request(&perm_case)).await })
    });

    assert_denied(&perm_outcome, perm_case.capability, perm_case.reason);
    assert_policy_decision_logged(
        &perm_events,
        perm_case.call_id,
        perm_case.capability,
        perm_case.reason,
    );
}

#[test]
fn js_hostcall_routes_write_and_read_tools_when_allowed() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let cwd = dir.path().to_path_buf();

    let mgr2 = ExtensionManager::new();
    let host = JsRuntimeHost {
        tools: Arc::new(crate::tools::ToolRegistry::new(
            &["read", "write"],
            &cwd,
            None,
        )),
        manager_ref: Arc::downgrade(&mgr2.inner),
        manager_snapshot: Arc::clone(&mgr2.snapshot),
        manager_snapshot_version: Arc::clone(&mgr2.snapshot_version),
        http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
        policy: ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            max_memory_mb: 256,
            default_caps: vec!["read".to_string(), "write".to_string()],
            deny_caps: Vec::new(),
            ..Default::default()
        },
        interceptor: None,
    };

    let write_request = crate::extensions_js::HostcallRequest {
        call_id: "hostcall-write".to_string(),
        kind: crate::extensions_js::HostcallKind::Tool {
            name: "write".to_string(),
        },
        payload: serde_json::json!({
            "path": "note.txt",
            "content": "hello"
        }),
        trace_id: 0,
        extension_id: Some("ext-1".to_string()),
    };

    let read_request = crate::extensions_js::HostcallRequest {
        call_id: "hostcall-read".to_string(),
        kind: crate::extensions_js::HostcallKind::Tool {
            name: "read".to_string(),
        },
        payload: serde_json::json!({ "path": "note.txt" }),
        trace_id: 0,
        extension_id: Some("ext-1".to_string()),
    };

    let ((write_outcome, read_outcome), events) = capture_tracing_events(|| {
        run_async(async {
            let write_outcome = super::dispatch_hostcall(&host, write_request).await;
            let read_outcome = super::dispatch_hostcall(&host, read_request).await;
            (write_outcome, read_outcome)
        })
    });

    assert!(matches!(write_outcome, HostcallOutcome::Success(_)));
    assert_eq!(
        std::fs::read_to_string(cwd.join("note.txt")).expect("read note.txt"),
        "hello"
    );

    let value = match read_outcome {
        HostcallOutcome::Success(value) => value,
        HostcallOutcome::Error { code, message } => {
            assert!(
                code == "__expected_success__",
                "expected read success, got error {code}: {message}"
            );
            return;
        }
        HostcallOutcome::StreamChunk {
            sequence,
            chunk,
            is_final,
        } => {
            panic!();
        }
    };

    let encoded = serde_json::to_string(&value).expect("serialize read output");
    assert!(encoded.contains("hello"));

    let decisions = events
        .iter()
        .filter(|event| {
            event
                .fields
                .get("event")
                .is_some_and(|value| value.contains("policy.decision"))
        })
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 2);
    for decision in decisions {
        assert_eq!(decision.level, tracing::Level::INFO);
        assert!(
            decision
                .fields
                .get("reason")
                .is_some_and(|value| value.contains("default_caps"))
        );
    }
}

#[test]
fn events_get_active_tools_returns_all_when_none_set() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools =
            crate::tools::ToolRegistry::new(&["read", "bash", "edit"], Path::new("."), None);

        let outcome =
            dispatch_hostcall_events("call-1", &manager, &tools, "getActiveTools", json!({})).await;

        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        let tool_names: Vec<String> = value
            .get("tools")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        assert_eq!(tool_names, vec!["read", "bash", "edit"]);
    });
}

#[test]
fn events_get_active_tools_returns_filtered_list() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools =
            crate::tools::ToolRegistry::new(&["read", "bash", "edit"], Path::new("."), None);

        manager.set_active_tools(vec!["read".to_string(), "bash".to_string()]);

        let outcome =
            dispatch_hostcall_events("call-1", &manager, &tools, "get_active_tools", json!({}))
                .await;

        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        let tool_names: Vec<String> = value
            .get("tools")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        assert_eq!(tool_names, vec!["read", "bash"]);
    });
}

#[test]
fn events_get_all_tools_returns_builtin_tools() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read", "bash"], Path::new("."), None);

        let outcome =
            dispatch_hostcall_events("call-1", &manager, &tools, "getAllTools", json!({})).await;

        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        let tool_list = value.get("tools").and_then(Value::as_array).unwrap();
        assert_eq!(tool_list.len(), 2);

        let names: Vec<&str> = tool_list
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"bash"));

        // Each tool should have a description
        for tool in tool_list {
            assert!(tool.get("description").and_then(Value::as_str).is_some());
        }
    });
}

#[test]
fn events_get_all_tools_includes_extension_tools() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        // Register an extension with a custom tool
        manager.register(RegisterPayload {
            name: "test-ext".to_string(),
            version: "1.0.0".to_string(),
            api_version: PROTOCOL_VERSION.to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: vec![json!({
                "name": "custom_tool",
                "label": "Custom Tool",
                "description": "A custom extension tool",
                "parameters": { "type": "object" }
            })],
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        });

        let outcome =
            dispatch_hostcall_events("call-1", &manager, &tools, "get_all_tools", json!({})).await;

        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        let tool_list = value.get("tools").and_then(Value::as_array).unwrap();
        assert_eq!(tool_list.len(), 2); // 1 built-in + 1 extension

        let names: Vec<&str> = tool_list
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"custom_tool"));
    });
}

#[test]
fn events_set_active_tools_changes_get_active_tools_result() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools =
            crate::tools::ToolRegistry::new(&["read", "bash", "edit"], Path::new("."), None);

        // Set active tools via hostcall
        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "setActiveTools",
            json!({ "tools": ["edit"] }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        // Verify getActiveTools reflects the change
        let outcome =
            dispatch_hostcall_events("call-2", &manager, &tools, "getActiveTools", json!({})).await;

        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        let tool_names: Vec<String> = value
            .get("tools")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        assert_eq!(tool_names, vec!["edit"]);
    });
}
