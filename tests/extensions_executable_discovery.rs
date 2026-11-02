//! PATH discovery used during npm extension initialization must not execute
//! a shell, consult an ambient PATH, or bypass the exec capability.

use std::collections::HashMap;
use std::path::Path;

use pi::extensions::ExtensionPolicy;
use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::DeterministicClock;
use serde_json::{Value, json};

async fn discovery_runtime(
    cwd: &Path,
    env: HashMap<String, String>,
    policy: Option<ExtensionPolicy>,
) -> PiJsRuntime<DeterministicClock> {
    PiJsRuntime::with_clock_and_config_with_policy_for_extension(
        DeterministicClock::new(0),
        PiJsRuntimeConfig {
            cwd: cwd.to_string_lossy().into_owned(),
            env,
            allow_unsafe_sync_exec: false,
            ..Default::default()
        },
        policy,
        "discovery-fixture".to_string(),
    )
    .await
    .expect("runtime")
}

async fn evaluate(runtime: &PiJsRuntime<DeterministicClock>, body: &str) -> Value {
    runtime
        .eval(&format!(
            r"
            globalThis.discoveryResult = null;
            globalThis.discoveryError = null;
            import('node:child_process').then(({{ execSync }}) => {{
                const command = process.platform === 'win32'
                    ? 'where discovery-fixture' : 'command -v discovery-fixture';
                {body}
            }}).catch(error => {{ globalThis.discoveryError = String(error); }});
            "
        ))
        .await
        .expect("evaluate");
    runtime.drain_microtasks().await.expect("drain");
    assert_eq!(
        runtime
            .read_global_json("discoveryError")
            .await
            .expect("error"),
        Value::Null,
    );
    runtime
        .read_global_json("discoveryResult")
        .await
        .expect("result")
}

fn metadata_fixture(directory: &Path) -> String {
    std::fs::create_dir_all(directory).expect("directory");
    let path = directory.join(if cfg!(windows) {
        "discovery-fixture.EXE"
    } else {
        "discovery-fixture"
    });
    // Invalid executable contents make accidental execution observable.
    std::fs::write(&path, "This file is metadata, not an executable program.\n").expect("fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("mode");
    }
    path.canonicalize()
        .expect("canonical path")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn conventional_probes_and_bun_which_resolve_configured_paths_without_execution() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("first path with spaces");
        let second = temp.path().join("second");
        let first_file = metadata_fixture(&first);
        let second_file = metadata_fixture(&second);
        let env = HashMap::from([
            (
                "PATH".to_string(),
                std::env::join_paths([&first, &second])
                    .expect("PATH")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("PATHEXT".to_string(), ".EXE".to_string()),
        ]);
        let runtime = discovery_runtime(temp.path(), env, None).await;
        let result = evaluate(&runtime, r"
            const paths = execSync(command, {encoding: 'utf8', stdio: 'pipe'}).trim().split(/\r?\n/);
            const blocked = [];
            for (const unsafe of [command + '; echo injected', command + ' && echo injected', 'echo blocked']) {
                try { execSync(unsafe); blocked.push(false); }
                catch (error) { blocked.push(String(error).includes('disabled by default')); }
            }
            let missingStatus = null;
            try { execSync(command.replace('discovery-fixture', 'definitely-missing-fixture')); }
            catch (error) { missingStatus = error.status; }
            globalThis.discoveryResult = {
                paths, which: Bun.which('discovery-fixture'), blocked, missingStatus,
                badName: Bun.which('../discovery-fixture'),
            };
        ").await;
        let paths = if cfg!(windows) {
            vec![first_file.clone(), second_file]
        } else {
            vec![first_file.clone()]
        };
        assert_eq!(
            result,
            json!({
                "paths": paths, "which": first_file, "blocked": [true, true, true],
                "missingStatus": 1, "badName": null,
            })
        );
    });
}

#[test]
fn discovery_requires_exec_capability_and_never_uses_an_ambient_path() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("bin");
        metadata_fixture(&path);
        let env = HashMap::from([
            ("PATH".to_string(), path.to_string_lossy().into_owned()),
            ("PATHEXT".to_string(), ".EXE".to_string()),
        ]);
        let denied = discovery_runtime(temp.path(), env, Some(ExtensionPolicy::default())).await;
        let result = evaluate(
            &denied,
            r"
            let denied = false;
            try { execSync(command); }
            catch (error) { denied = String(error).includes('exec capability'); }
            globalThis.discoveryResult = {denied, which: Bun.which('discovery-fixture')};
        ",
        )
        .await;
        assert_eq!(result, json!({"denied": true, "which": null}));

        let empty = discovery_runtime(temp.path(), HashMap::new(), None).await;
        let result = evaluate(
            &empty,
            r"
            globalThis.discoveryResult = {
                fixture: Bun.which('discovery-fixture'), shell: Bun.which('sh'),
                command: Bun.which('cmd.exe'),
            };
        ",
        )
        .await;
        assert_eq!(
            result,
            json!({"fixture": null, "shell": null, "command": null})
        );
    });
}

#[test]
fn discovery_uses_the_canonical_exec_policy_precedence() {
    use pi::extensions::{ExtensionOverride, ExtensionPolicyMode};

    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let directory = temp.path().join("bin");
        metadata_fixture(&directory);
        let env = HashMap::from([
            ("PATH".to_string(), directory.to_string_lossy().into_owned()),
            ("PATHEXT".to_string(), ".EXE".to_string()),
        ]);
        // A global denial defeats an owner's allow, including differently
        // cased capability spellings. A strict owner mode still honors a
        // global default capability when neither layer denies it.
        for (global_deny, visible) in [(true, false), (false, true)] {
            let mut policy = ExtensionPolicy {
                mode: ExtensionPolicyMode::Prompt,
                default_caps: vec!["EXEC".to_string()],
                deny_caps: if global_deny {
                    vec!["eXeC".to_string()]
                } else {
                    Vec::new()
                },
                ..ExtensionPolicy::default()
            };
            policy.per_extension.insert(
                "discovery-fixture".to_string(),
                ExtensionOverride {
                    mode: Some(ExtensionPolicyMode::Strict),
                    allow: if global_deny {
                        vec!["exec".to_string()]
                    } else {
                        Vec::new()
                    },
                    ..ExtensionOverride::default()
                },
            );
            let runtime = discovery_runtime(temp.path(), env.clone(), Some(policy)).await;
            let result = evaluate(&runtime, r"
                let resolved = false;
                try { resolved = execSync(command).includes('discovery-fixture'); } catch (_) {}
                globalThis.discoveryResult = {resolved, which: Bun.which('discovery-fixture') !== null};
            ").await;
            assert_eq!(
                result,
                json!({"resolved": visible, "which": visible}),
                "global deny={global_deny}"
            );
        }
    });
}
