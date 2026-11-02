//! Unit tests for the process global shim (bd-1av0.9).
//!
//! Tests the enhanced `globalThis.process` object: blocklist-based env filtering,
//! stdout/stderr routing, exit signaling, event emitter API, hrtime, and more.
#![allow(clippy::needless_raw_string_hashes)]

use std::collections::HashMap;
use std::sync::Arc;

use pi::extensions_js::{HostcallKind, PiJsRuntime, PiJsRuntimeConfig, is_env_var_allowed};
use pi::scheduler::DeterministicClock;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn config_with_env(env: Vec<(&str, &str)>) -> PiJsRuntimeConfig {
    let mut env: HashMap<String, String> = env
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    env.insert("PI_EXT_COMPAT_SCAN".to_string(), "0".to_string());
    PiJsRuntimeConfig {
        cwd: "/test".to_string(),
        env,
        deny_env: false,
        ..Default::default()
    }
}

fn default_config() -> PiJsRuntimeConfig {
    let env: HashMap<String, String> =
        HashMap::from([("PI_EXT_COMPAT_SCAN".to_string(), "0".to_string())]);
    PiJsRuntimeConfig {
        cwd: "/test".to_string(),
        env,
        ..Default::default()
    }
}

// ─── is_env_var_allowed tests ───────────────────────────────────────────────

#[test]
fn is_env_var_allowed_permits_common_vars() {
    for key in &[
        "PATH", "HOME", "USER", "SHELL", "TERM", "LANG", "EDITOR", "NODE_ENV",
    ] {
        assert!(is_env_var_allowed(key), "{key} should be allowed");
    }
}

#[test]
fn is_env_var_allowed_permits_pi_prefix() {
    for key in &[
        "PI_CONFIG",
        "PI_IMAGE_SAVE_MODE",
        "PI_PLATFORM",
        "PI_TARGET_ARCH",
    ] {
        assert!(is_env_var_allowed(key), "{key} should be allowed");
    }
}

#[test]
fn is_env_var_allowed_blocks_case_variants_of_sensitive_keys() {
    for key in &[
        "openai_api_key",
        "OpenAI_API_Key",
        "anthropic_api_key",
        "gItHuB_tOkEn",
    ] {
        assert!(!is_env_var_allowed(key), "{key} should be blocked");
    }
}

#[test]
fn is_env_var_allowed_blocks_pi_prefixed_secret_like_keys() {
    for key in &[
        "PI_OPENAI_API_KEY",
        "PI_DEPLOY_SECRET",
        "PI_AWS_SECRET_ACCESS_KEY",
    ] {
        assert!(!is_env_var_allowed(key), "{key} should be blocked");
    }
}

#[test]
fn is_env_var_allowed_blocks_api_keys() {
    for key in &[
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GOOGLE_API_KEY",
        "AZURE_OPENAI_API_KEY",
        "GROQ_API_KEY",
        "DEEPSEEK_API_KEY",
        "XAI_API_KEY",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "NPM_TOKEN",
        "DATABASE_URL",
    ] {
        assert!(!is_env_var_allowed(key), "{key} should be blocked");
    }
}

#[test]
fn is_env_var_allowed_blocks_secret_suffixes() {
    for key in &[
        "MY_APP_SECRET",
        "DB_PASSWORD",
        "SSH_PRIVATE_KEY",
        "SMTP_PASSWD",
        "SERVICE_CREDENTIAL",
        "AWS_ACCESS_KEY",
    ] {
        assert!(
            !is_env_var_allowed(key),
            "{key} should be blocked by suffix"
        );
    }
}

#[test]
fn is_env_var_allowed_blocks_aws_session() {
    assert!(!is_env_var_allowed("AWS_SESSION_TOKEN"));
    assert!(!is_env_var_allowed("AWS_SECRET_ACCESS_KEY"));
    // AWS_REGION should be allowed (not a secret)
    assert!(is_env_var_allowed("AWS_REGION"));
    assert!(is_env_var_allowed("AWS_DEFAULT_REGION"));
}

// ─── Process env integration tests ─────────────────────────────────────────

#[test]
fn pijs_process_env_path_now_accessible() {
    futures::executor::block_on(async {
        let config = config_with_env(vec![
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/home/test"),
            ("USER", "testuser"),
            ("SHELL", "/bin/bash"),
            ("TERM", "xterm"),
            ("EDITOR", "vim"),
            ("ANTHROPIC_API_KEY", "sk-secret-123"),
        ]);
        let runtime =
            PiJsRuntime::with_clock_and_config(Arc::new(DeterministicClock::new(0)), config)
                .await
                .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.results = {
                    path: process.env.PATH,
                    home: process.env.HOME,
                    user: process.env.USER,
                    shell: process.env.SHELL,
                    term: process.env.TERM,
                    editor: process.env.EDITOR,
                    secret: process.env.ANTHROPIC_API_KEY,
                };
                "#,
            )
            .await
            .expect("eval");

        let val: serde_json::Value = runtime.read_global_json("results").await.unwrap();
        assert_eq!(val["path"], "/usr/bin:/bin");
        assert_eq!(val["home"], "/home/test");
        assert_eq!(val["user"], "testuser");
        assert_eq!(val["shell"], "/bin/bash");
        assert_eq!(val["term"], "xterm");
        assert_eq!(val["editor"], "vim");
        assert!(
            val["secret"].is_null(),
            "ANTHROPIC_API_KEY should be blocked"
        );
    });
}

#[test]
fn pijs_process_env_write_does_not_mutate() {
    futures::executor::block_on(async {
        let config = config_with_env(vec![("HOME", "/home/test")]);
        let runtime =
            PiJsRuntime::with_clock_and_config(Arc::new(DeterministicClock::new(0)), config)
                .await
                .expect("create runtime");

        runtime
            .eval(
                r#"
                process.env.HOME = '/tmp/hacked';
                delete process.env.HOME;
                globalThis.homeAfter = process.env.HOME;
                "#,
            )
            .await
            .expect("eval");

        let val: serde_json::Value = runtime.read_global_json("homeAfter").await.unwrap();
        assert_eq!(val, "/home/test", "writes/deletes should not affect env");
    });
}

#[test]
fn pijs_process_stdout_write_routes_to_console() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.writeResult = process.stdout.write("hello");
                globalThis.isTTY = process.stdout.isTTY;
                "#,
            )
            .await
            .expect("eval");

        let result: serde_json::Value = runtime.read_global_json("writeResult").await.unwrap();
        assert_eq!(result, true, "stdout.write should return true");

        let is_tty: serde_json::Value = runtime.read_global_json("isTTY").await.unwrap();
        assert_eq!(is_tty, false, "stdout.isTTY should be false");
    });
}

#[test]
fn pijs_process_exit_signals_shutdown() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.exitError = null;
                try {
                    process.exit(42);
                } catch (e) {
                    globalThis.exitError = { code: e.code, exitCode: e.exitCode, msg: e.message };
                }
                "#,
            )
            .await
            .expect("eval");

        let val: serde_json::Value = runtime.read_global_json("exitError").await.unwrap();
        assert_eq!(val["code"], "ERR_PROCESS_EXIT");
        assert_eq!(val["exitCode"], 42);
        assert!(val["msg"].as_str().unwrap().contains("42"));

        // Check that a hostcall was enqueued for exit
        let requests = runtime.drain_hostcall_requests();
        let exit_req = requests
            .iter()
            .find(|r| matches!(&r.kind, HostcallKind::Events { op } if op == "exit"));
        assert!(exit_req.is_some(), "exit should enqueue a hostcall");
        assert_eq!(exit_req.unwrap().payload["code"], 42);
    });
}

#[test]
fn pijs_process_exit_fires_listeners() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.exitCodes = [];
                process.on('exit', (code) => { globalThis.exitCodes.push(code); });
                process.on('exit', (code) => { globalThis.exitCodes.push(code * 2); });
                try { process.exit(7); } catch (_) {}
                "#,
            )
            .await
            .expect("eval");

        let val: serde_json::Value = runtime.read_global_json("exitCodes").await.unwrap();
        let codes: Vec<i64> = val
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(codes, vec![7, 14], "both exit listeners should fire");
    });
}

#[test]
fn pijs_process_event_emitter_api() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.results = {};
                const calls = [];
                const fn1 = (x) => calls.push('fn1:' + x);
                const fn2 = (x) => calls.push('fn2:' + x);

                // on/emit
                process.on('test', fn1);
                process.on('test', fn2);
                process.emit('test', 'a');
                results.afterEmit = calls.slice();

                // off
                process.off('test', fn1);
                process.emit('test', 'b');
                results.afterOff = calls.slice();

                // once
                const onceCalls = [];
                process.once('single', (x) => onceCalls.push(x));
                process.emit('single', 'first');
                process.emit('single', 'second');
                results.onceCalls = onceCalls;

                // removeAllListeners
                process.removeAllListeners('test');
                results.listenersAfterRemoveAll = process.listeners('test').length;

                // listeners
                process.on('check', () => {});
                process.on('check', () => {});
                results.checkListenerCount = process.listeners('check').length;

                results.calls = calls;
                "#,
            )
            .await
            .expect("eval");

        let val: serde_json::Value = runtime.read_global_json("results").await.unwrap();
        let after_emit: Vec<&str> = val["afterEmit"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(after_emit, vec!["fn1:a", "fn2:a"]);

        let after_off: Vec<&str> = val["afterOff"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(after_off, vec!["fn1:a", "fn2:a", "fn2:b"]);

        let once_calls: Vec<&str> = val["onceCalls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(once_calls, vec!["first"], "once should fire only once");

        assert_eq!(val["listenersAfterRemoveAll"], 0);
        assert_eq!(val["checkListenerCount"], 2);
    });
}

#[test]
fn pijs_process_hrtime_returns_real_values() {
    futures::executor::block_on(async {
        let clock = Arc::new(DeterministicClock::new(5000));
        let runtime = PiJsRuntime::with_clock_and_config(Arc::clone(&clock), default_config())
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.ht = process.hrtime();
                globalThis.htBigint = typeof process.hrtime.bigint() === 'bigint';
                "#,
            )
            .await
            .expect("eval");

        let ht: serde_json::Value = runtime.read_global_json("ht").await.unwrap();
        let arr = ht.as_array().unwrap();
        assert_eq!(arr[0].as_i64().unwrap(), 5, "5000ms = 5 seconds");
        assert_eq!(arr[1].as_i64().unwrap(), 0, "0 nanos remainder");

        let bigint_ok: serde_json::Value = runtime.read_global_json("htBigint").await.unwrap();
        assert_eq!(bigint_ok, true, "hrtime.bigint should return a bigint");
    });
}

#[test]
fn pijs_process_hrtime_diff() {
    futures::executor::block_on(async {
        let clock = Arc::new(DeterministicClock::new(3500));
        let runtime = PiJsRuntime::with_clock_and_config(Arc::clone(&clock), default_config())
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
                const prev = [2, 500000000]; // 2.5 seconds
                globalThis.diff = process.hrtime(prev);
                "#,
            )
            .await
            .expect("eval");

        let diff: serde_json::Value = runtime.read_global_json("diff").await.unwrap();
        let arr = diff.as_array().unwrap();
        // 3500ms = [3, 500000000], prev = [2, 500000000], diff = [1, 0]
        assert_eq!(arr[0].as_i64().unwrap(), 1);
        assert_eq!(arr[1].as_i64().unwrap(), 0);
    });
}

#[test]
fn pijs_process_arch_and_platform() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.archType = typeof process.arch;
                globalThis.archValue = process.arch;
                globalThis.platformType = typeof process.platform;
                "#,
            )
            .await
            .expect("eval");

        let arch_type: serde_json::Value = runtime.read_global_json("archType").await.unwrap();
        assert_eq!(arch_type, "string");

        let arch_val: serde_json::Value = runtime.read_global_json("archValue").await.unwrap();
        let arch = arch_val.as_str().unwrap();
        assert!(
            arch == "x64" || arch == "arm64",
            "arch should be x64 or arm64, got {arch}"
        );

        let plat_type: serde_json::Value = runtime.read_global_json("platformType").await.unwrap();
        assert_eq!(plat_type, "string");
    });
}

#[test]
fn pijs_process_execpath_and_title() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.execPathType = typeof process.execPath;
                globalThis.titleValue = process.title;
                globalThis.execArgvIsArray = Array.isArray(process.execArgv);
                "#,
            )
            .await
            .expect("eval");

        let ept: serde_json::Value = runtime.read_global_json("execPathType").await.unwrap();
        assert_eq!(ept, "string");

        let title: serde_json::Value = runtime.read_global_json("titleValue").await.unwrap();
        assert_eq!(title, "pi");

        let ea: serde_json::Value = runtime.read_global_json("execArgvIsArray").await.unwrap();
        assert_eq!(ea, true);
    });
}

#[test]
fn pijs_process_uptime_and_memory() {
    futures::executor::block_on(async {
        let clock = Arc::new(DeterministicClock::new(10_000));
        let runtime = PiJsRuntime::with_clock_and_config(Arc::clone(&clock), default_config())
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.uptimeVal = process.uptime();
                const mem = process.memoryUsage();
                globalThis.hasRss = 'rss' in mem;
                globalThis.hasHeapTotal = 'heapTotal' in mem;
                const cpu = process.cpuUsage();
                globalThis.hasUser = 'user' in cpu;
                globalThis.hasSystem = 'system' in cpu;
                "#,
            )
            .await
            .expect("eval");

        let uptime: serde_json::Value = runtime.read_global_json("uptimeVal").await.unwrap();
        // Clock is deterministic — startMs = 10000, nowMs = 10000, uptime = 0
        // But uptime should be (nowMs - startMs) / 1000 which is 0 since both are the same
        assert_eq!(uptime.as_i64().unwrap(), 0);

        let has_rss: serde_json::Value = runtime.read_global_json("hasRss").await.unwrap();
        assert_eq!(has_rss, true);
        let has_heap: serde_json::Value = runtime.read_global_json("hasHeapTotal").await.unwrap();
        assert_eq!(has_heap, true);
        let has_user: serde_json::Value = runtime.read_global_json("hasUser").await.unwrap();
        assert_eq!(has_user, true);
        let has_system: serde_json::Value = runtime.read_global_json("hasSystem").await.unwrap();
        assert_eq!(has_system, true);
    });
}

#[test]
fn pijs_process_chdir_throws() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.chdirError = null;
                try {
                    process.chdir('/tmp');
                } catch (e) {
                    globalThis.chdirError = e.code;
                }
                "#,
            )
            .await
            .expect("eval");

        let val: serde_json::Value = runtime.read_global_json("chdirError").await.unwrap();
        assert_eq!(val, "ENOSYS");
    });
}

#[test]
fn pijs_process_kill_throws_enosys() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.killError = null;
                try {
                    process.kill(999);
                } catch (e) {
                    globalThis.killError = e.code;
                }
                "#,
            )
            .await
            .expect("eval");

        let val: serde_json::Value = runtime.read_global_json("killError").await.unwrap();
        assert_eq!(val, "ENOSYS");
    });
}

#[test]
fn pijs_process_emit_warning() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        // Should not throw
        runtime
            .eval(r#"process.emitWarning("test warning");"#)
            .await
            .expect("emitWarning should not throw");
    });
}

#[test]
fn pijs_process_env_has_operator() {
    futures::executor::block_on(async {
        let config = config_with_env(vec![("HOME", "/home/test")]);
        let runtime =
            PiJsRuntime::with_clock_and_config(Arc::new(DeterministicClock::new(0)), config)
                .await
                .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.hasHome = 'HOME' in process.env;
                globalThis.hasMissing = 'NONEXISTENT_VAR_XYZ' in process.env;
                globalThis.ownKeys = Object.keys(process.env);
                "#,
            )
            .await
            .expect("eval");

        let has_home: serde_json::Value = runtime.read_global_json("hasHome").await.unwrap();
        assert_eq!(has_home, true, "'HOME' in process.env should be true");

        let has_missing: serde_json::Value = runtime.read_global_json("hasMissing").await.unwrap();
        assert_eq!(has_missing, false, "missing var should not be in env");

        let own_keys: serde_json::Value = runtime.read_global_json("ownKeys").await.unwrap();
        assert!(
            own_keys
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("HOME")),
            "permitted snapshot keys are enumerable"
        );
    });
}

#[test]
fn pijs_process_env_spread_filters_secrets_and_preserves_proxy_invariants() {
    futures::executor::block_on(async {
        let config = config_with_env(vec![
            ("HOME", "/home/test"),
            ("PATH", "/configured/bin"),
            ("PI_CONFIG_DIR", "/configured/pi"),
            ("OPENAI_API_KEY", "must-not-disclose"),
            ("PRIVATE_TOKEN", "also-private"),
            ("NODE_OPTIONS", "--require=untrusted"),
            ("LD_PRELOAD", "/untrusted.so"),
            ("PYTHONPATH", "/untrusted/python"),
            ("=D:", "D:\\project"),
        ]);
        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("runtime");
        runtime
            .eval(
                r#"
            const copied = {...process.env};
            const keys = Object.keys(process.env);
            const marker = Symbol('immutable');
            Object.defineProperty(process.env, marker, {value: 19});
            Object.defineProperty(process.env, 'getterless', {get: undefined});
            Object.freeze(process.env);
            globalThis.envSnapshot = {
                home: copied.HOME, path: copied.PATH, config: copied.PI_CONFIG_DIR,
                secretPresent: 'OPENAI_API_KEY' in copied || 'PRIVATE_TOKEN' in copied,
                unsafeCopied: ['NODE_OPTIONS', 'LD_PRELOAD', 'PYTHONPATH', '=D:'].some(key => key in copied),
                individualRead: process.env.NODE_OPTIONS,
                driveRead: process.env['=D:'],
                sorted: JSON.stringify(keys) === JSON.stringify([...keys].sort()),
                frozen: Object.isFrozen(process.env),
                immutable: process.env[marker],
                getterless: process.env.getterless === undefined,
                keysAfterFreeze: Reflect.ownKeys(process.env).includes(marker),
                overwrite: Reflect.set(process.env, 'HOME', '/changed'),
                deleteHome: Reflect.deleteProperty(process.env, 'HOME'),
                stable: {...process.env}.HOME,
            };
        "#,
            )
            .await
            .expect("enumeration, spread and freeze");
        assert_eq!(
            runtime.read_global_json("envSnapshot").await.unwrap(),
            serde_json::json!({
                "home": "/home/test", "path": "/configured/bin", "config": "/configured/pi",
                "secretPresent": false, "sorted": true, "frozen": true,
                "unsafeCopied": false, "individualRead": "--require=untrusted", "driveRead": "D:\\project",
                "immutable": 19, "getterless": true, "keysAfterFreeze": true,
                "overwrite": false, "deleteHome": false, "stable": "/home/test",
            })
        );
    });
}

#[test]
fn pijs_process_env_denial_applies_to_enumeration_and_config_metadata() {
    futures::executor::block_on(async {
        let mut config = config_with_env(vec![("HOME", "/home/test"), ("PATH", "/bin")]);
        config.deny_env = true;
        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("runtime");
        runtime
            .eval(
                r#"
            globalThis.deniedEnv = {
                keys: Object.keys(process.env), spread: {...process.env},
                configHidden: process.env.PI_CONFIG_DIR === undefined,
                workspaceHidden: process.env.PI_WORKSPACE_DIR === undefined,
                pwdHidden: process.env.PWD === undefined,
            };
        "#,
            )
            .await
            .expect("denied enumeration");
        assert_eq!(
            runtime.read_global_json("deniedEnv").await.unwrap(),
            serde_json::json!({
                "keys": [], "spread": {}, "configHidden": true,
                "workspaceHidden": true, "pwdHidden": true,
            })
        );
    });
}

#[test]
fn pijs_env_disclosure_allowlist_does_not_implicitly_forward_secrets_to_children() {
    use pi::extensions::{ExtensionPolicy, ExtensionPolicyMode};

    futures::executor::block_on(async {
        let mut policy = ExtensionPolicy {
            mode: ExtensionPolicyMode::Permissive,
            deny_caps: Vec::new(),
            ..ExtensionPolicy::default()
        };
        policy
            .secret_broker
            .disclosure_allowlist
            .push("OPENAI_API_KEY".to_string());
        let runtime = PiJsRuntime::with_clock_and_config_with_policy_for_extension(
            DeterministicClock::new(0),
            config_with_env(vec![
                ("OPENAI_API_KEY", "disclosure-fixture"),
                ("PI_FIXTURE", "safe"),
            ]),
            Some(policy),
            "disclosure-owner".to_string(),
        )
        .await
        .expect("owned runtime");
        runtime
            .eval(
                r#"
            const copied = {...process.env};
            globalThis.disclosureSnapshot = {
                direct: process.env.OPENAI_API_KEY,
                copiedSecret: 'OPENAI_API_KEY' in copied,
                enumeratedSecret: __pi_env_keys_native().includes('OPENAI_API_KEY'),
                copiedSafe: copied.PI_FIXTURE,
            };
        "#,
            )
            .await
            .expect("disclosure versus child transfer");
        assert_eq!(
            runtime
                .read_global_json("disclosureSnapshot")
                .await
                .unwrap(),
            serde_json::json!({
                "direct": "disclosure-fixture", "copiedSecret": false,
                "enumeratedSecret": false, "copiedSafe": "safe",
            })
        );
    });
}

#[test]
fn pijs_process_config_metadata_uses_the_supplied_home_without_creating_directories() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("home fixture");
        let home = temp.path().to_string_lossy().into_owned();
        let expected = temp.path().join(".pi/agent").to_string_lossy().into_owned();
        let runtime = PiJsRuntime::with_clock_and_config(
            DeterministicClock::new(0),
            config_with_env(vec![("HOME", &home)]),
        )
        .await
        .expect("runtime");
        runtime
            .eval("globalThis.piConfigMetadata = {...process.env}.PI_CONFIG_DIR;")
            .await
            .expect("config metadata");
        assert_eq!(
            runtime.read_global_json("piConfigMetadata").await.unwrap(),
            expected
        );
        assert!(
            !temp.path().join(".pi").exists(),
            "metadata must not create directories"
        );
    });
}

#[test]
fn pijs_workspace_identity_uses_the_selected_cwd_instead_of_the_launcher_environment() {
    use sha2::Digest as _;

    futures::executor::block_on(async {
        for (platform, cwd, deterministic_cwd) in [
            ("linux", "/workspaces/resumed", None),
            ("win32", r"C:\Workspaces\Resumed", None),
            ("linux", "/host/physical", Some("/deterministic/project")),
        ] {
            let expected = deterministic_cwd.unwrap_or(cwd);
            let mut config = config_with_env(vec![
                ("PI_PLATFORM", platform),
                ("PWD", "/stale/launcher"),
                ("PI_WORKSPACE_DIR", "/stale/previous-session"),
                ("PI_PROJECT_DIR", "/explicit/legacy-project"),
                ("PI_FIXTURE", "preserved"),
            ]);
            config.cwd = cwd.to_string();
            config.disk_cache_dir = None;
            if platform == "win32" {
                config.env.extend([
                    ("pWd".to_string(), r"D:\Stale\Launcher".to_string()),
                    (
                        "Pi_Workspace_Dir".to_string(),
                        r"D:\Stale\Session".to_string(),
                    ),
                ]);
            }
            if let Some(deterministic_cwd) = deterministic_cwd {
                config.env.insert(
                    "PI_DETERMINISTIC_CWD".to_string(),
                    deterministic_cwd.to_string(),
                );
            }
            let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
                .await
                .expect("workspace runtime");
            runtime
                .eval(
                    r#"
                    import('node:crypto').then(({createHash}) => {
                        // context-mode chooses these variables before cwd and
                        // hashes the result to select its persistent database.
                        const workspace = process.env.PI_WORKSPACE_DIR
                            || process.env.PI_PROJECT_DIR || process.env.PWD || process.cwd();
                        const normalized = workspace.replace(/\\/g, '/');
                        const canonical = process.platform === 'win32'
                            ? normalized.toLowerCase() : normalized;
                        const copied = {...process.env};
                        globalThis.workspaceIdentity = {
                            cwd: process.cwd(), workspace, pwd: copied.PWD,
                            copiedWorkspace: copied.PI_WORKSPACE_DIR,
                            legacyProject: copied.PI_PROJECT_DIR,
                            fixture: copied.PI_FIXTURE,
                            workspaceKeys: Object.keys(copied).filter(key => key.toUpperCase() === 'PI_WORKSPACE_DIR'),
                            pwdKeys: Object.keys(copied).filter(key => key.toUpperCase() === 'PWD'),
                            databaseKey: createHash('sha256').update(canonical).digest('hex').slice(0, 16),
                        };
                    });
                    "#,
                )
                .await
                .expect("resolve extension project identity");
            runtime.drain_microtasks().await.expect("load node:crypto");
            let normalized = expected.replace('\\', "/");
            let canonical = if platform == "win32" {
                normalized.to_lowercase()
            } else {
                normalized
            };
            let database_key =
                pi::package_manager::hex_encode(&sha2::Sha256::digest(canonical.as_bytes()));
            assert_eq!(
                runtime.read_global_json("workspaceIdentity").await.unwrap(),
                serde_json::json!({
                    "cwd": expected, "workspace": expected, "pwd": expected,
                    "copiedWorkspace": expected, "legacyProject": "/explicit/legacy-project",
                    "fixture": "preserved", "workspaceKeys": ["PI_WORKSPACE_DIR"],
                    "pwdKeys": ["PWD"], "databaseKey": &database_key[..16],
                }),
                "selected {platform} workspace must win over stale launcher state"
            );
        }
    });
}

#[test]
fn pijs_workspace_metadata_cannot_disclose_a_hidden_deterministic_cwd() {
    use pi::extensions::{ExtensionPolicy, ExtensionPolicyMode};

    futures::executor::block_on(async {
        for disclosed in [false, true] {
            let mut policy = ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                deny_caps: Vec::new(),
                ..ExtensionPolicy::default()
            };
            policy
                .secret_broker
                .secret_exact
                .push("PI_DETERMINISTIC_CWD".to_string());
            if disclosed {
                policy
                    .secret_broker
                    .disclosure_allowlist
                    .push("PI_DETERMINISTIC_CWD".to_string());
            }
            let mut config = config_with_env(vec![
                ("PI_DETERMINISTIC_CWD", "/private/deterministic-workspace"),
                ("PWD", "/stale/launcher"),
                ("PI_WORKSPACE_DIR", "/stale/session"),
            ]);
            config.cwd = "/selected/workspace".to_string();
            config.disk_cache_dir = None;
            let runtime = PiJsRuntime::with_clock_and_config_with_policy_for_extension(
                DeterministicClock::new(0),
                config,
                Some(policy),
                "workspace-owner".to_string(),
            )
            .await
            .expect("owned workspace runtime");
            runtime
                .eval(
                    r#"
                    globalThis.workspaceDisclosure = {
                        cwd: process.cwd(), workspace: process.env.PI_WORKSPACE_DIR,
                        copiedWorkspace: {...process.env}.PI_WORKSPACE_DIR,
                        pwd: process.env.PWD,
                        deterministic: process.env.PI_DETERMINISTIC_CWD ?? null,
                    };
                    "#,
                )
                .await
                .expect("workspace disclosure");
            let expected = if disclosed {
                "/private/deterministic-workspace"
            } else {
                "/selected/workspace"
            };
            assert_eq!(
                runtime
                    .read_global_json("workspaceDisclosure")
                    .await
                    .unwrap(),
                serde_json::json!({
                    "cwd": expected, "workspace": expected, "copiedWorkspace": expected,
                    "pwd": expected, "deterministic": disclosed.then_some(expected),
                }),
                "metadata must follow the same disclosure policy as process.cwd()"
            );
        }
    });
}

#[test]
fn pijs_environment_snapshot_is_bound_to_each_realms_immutable_policy_owner() {
    use pi::extensions::{ExtensionOverride, ExtensionPolicy, ExtensionPolicyMode};

    futures::executor::block_on(async {
        let mut policy = ExtensionPolicy {
            mode: ExtensionPolicyMode::Permissive,
            deny_caps: Vec::new(),
            ..ExtensionPolicy::default()
        };
        policy.per_extension.insert(
            "restricted".to_string(),
            ExtensionOverride {
                deny: vec!["EnV".to_string()],
                ..ExtensionOverride::default()
            },
        );
        for (owner, visible) in [("permitted", true), ("restricted", false)] {
            let runtime = PiJsRuntime::with_clock_and_config_with_policy_for_extension(
                DeterministicClock::new(0),
                config_with_env(vec![
                    ("PI_FIXTURE", "owner-scoped"),
                    ("PRIVATE_TOKEN", "hidden"),
                ]),
                Some(policy.clone()),
                owner.to_string(),
            )
            .await
            .expect("owned runtime");
            runtime
                .eval(
                    r#"
                const copied = {...process.env};
                globalThis.ownerEnvironment = {
                    read: process.env.PI_FIXTURE ?? null,
                    native: __pi_env_get_native('PI_FIXTURE'),
                    enumerated: Object.keys(process.env).includes('PI_FIXTURE'),
                    nativeEnumerated: __pi_env_keys_native().includes('PI_FIXTURE'),
                    copied: copied.PI_FIXTURE ?? null,
                    secretPresent: 'PRIVATE_TOKEN' in copied,
                };
            "#,
                )
                .await
                .expect("read owned snapshot");
            let value = if visible {
                serde_json::json!("owner-scoped")
            } else {
                serde_json::Value::Null
            };
            assert_eq!(
                runtime.read_global_json("ownerEnvironment").await.unwrap(),
                serde_json::json!({
                    "read": value, "native": value, "enumerated": visible,
                    "nativeEnumerated": visible, "copied": value, "secretPresent": false,
                }),
                "owner {owner}"
            );
        }
    });
}

#[test]
fn pijs_environment_owner_grants_respect_global_and_explicit_host_denials() {
    use pi::extensions::{ExtensionOverride, ExtensionPolicy, ExtensionPolicyMode};

    futures::executor::block_on(async {
        for (global_deny, host_deny, visible) in [
            (false, false, true),
            (true, false, false),
            (false, true, false),
        ] {
            let mut policy = ExtensionPolicy {
                mode: ExtensionPolicyMode::Strict,
                default_caps: Vec::new(),
                deny_caps: if global_deny {
                    vec!["ENV".to_string()]
                } else {
                    Vec::new()
                },
                ..ExtensionPolicy::default()
            };
            policy.per_extension.insert(
                "owner".to_string(),
                ExtensionOverride {
                    allow: vec!["env".to_string()],
                    ..ExtensionOverride::default()
                },
            );
            let mut config = config_with_env(vec![("PI_FIXTURE", "allowed-by-owner")]);
            config.deny_env = host_deny;
            let runtime = PiJsRuntime::with_clock_and_config_with_policy_for_extension(
                DeterministicClock::new(0),
                config,
                Some(policy),
                "owner".to_string(),
            )
            .await
            .expect("owned runtime");
            runtime
                .eval("globalThis.envVisible = {...process.env}.PI_FIXTURE === 'allowed-by-owner';")
                .await
                .expect("owner-granted environment");
            assert_eq!(
                runtime.read_global_json("envVisible").await.unwrap(),
                visible,
                "global deny={global_deny}, explicit host deny={host_deny}"
            );
        }
    });
}

#[test]
fn pijs_process_release_and_config() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.releaseName = process.release.name;
                globalThis.hasConfig = typeof process.config === 'object';
                globalThis.hasFeatures = typeof process.features === 'object';
                "#,
            )
            .await
            .expect("eval");

        let name: serde_json::Value = runtime.read_global_json("releaseName").await.unwrap();
        assert_eq!(name, "node");

        let has_config: serde_json::Value = runtime.read_global_json("hasConfig").await.unwrap();
        assert_eq!(has_config, true);

        let has_features: serde_json::Value =
            runtime.read_global_json("hasFeatures").await.unwrap();
        assert_eq!(has_features, true);
    });
}

#[test]
fn pijs_node_process_module_exports_enhanced() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.moduleResults = {};
                import('node:process').then((proc) => {
                    const r = globalThis.moduleResults;
                    r.hasChdir = typeof proc.chdir === 'function';
                    r.hasExecPath = typeof proc.execPath === 'string';
                    r.hasTitle = typeof proc.title === 'string';
                    r.hasOnce = typeof proc.once === 'function';
                    r.hasAddListener = typeof proc.addListener === 'function';
                    r.hasRemoveListener = typeof proc.removeListener === 'function';
                    r.hasRemoveAllListeners = typeof proc.removeAllListeners === 'function';
                    r.hasListeners = typeof proc.listeners === 'function';
                    r.hasEmit = typeof proc.emit === 'function';
                    r.hasEmitWarning = typeof proc.emitWarning === 'function';
                    r.hasUptime = typeof proc.uptime === 'function';
                    r.hasMemoryUsage = typeof proc.memoryUsage === 'function';
                    r.hasCpuUsage = typeof proc.cpuUsage === 'function';
                    r.hasRelease = typeof proc.release === 'object';
                });
                "#,
            )
            .await
            .expect("eval");

        // Drain microtasks so import() promise resolves
        runtime.drain_microtasks().await.expect("drain microtasks");

        let val: serde_json::Value = runtime.read_global_json("moduleResults").await.unwrap();
        for key in [
            "hasChdir",
            "hasExecPath",
            "hasTitle",
            "hasOnce",
            "hasAddListener",
            "hasRemoveListener",
            "hasRemoveAllListeners",
            "hasListeners",
            "hasEmit",
            "hasEmitWarning",
            "hasUptime",
            "hasMemoryUsage",
            "hasCpuUsage",
            "hasRelease",
        ] {
            assert_eq!(val[key], true, "node:process module should export {key}");
        }
    });
}

#[test]
fn pijs_windows_environment_aliases_select_one_deterministic_snapshot_binding() {
    futures::executor::block_on(async {
        for reverse in [false, true] {
            for deny_env in [false, true] {
                let mut bindings = vec![
                    ("PI_PLATFORM", "win32"),
                    ("Path", r"C:\mixed"),
                    ("path", r"C:\lower"),
                    ("PATH", r"C:\selected"),
                    ("Api_Token", "hidden"),
                ];
                if reverse {
                    bindings.reverse();
                }
                let mut config = config_with_env(bindings);
                config.deny_env = deny_env;
                let runtime =
                    PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
                        .await
                        .expect("Windows snapshot runtime");
                runtime
                    .eval(
                        r#"
                        const copied = {...process.env};
                        const alias = Object.getOwnPropertyDescriptor(process.env, 'pAtH');
                        Object.freeze(process.env);
                        globalThis.windowsEnvironment = {
                            values: ['PATH', 'Path', 'path', 'pAtH'].map(key => process.env[key] ?? null),
                            native: __pi_env_get_native('pAtH'),
                            pi: pi.env.get('pAtH') ?? null,
                            descriptor: alias?.value ?? null,
                            present: 'pAtH' in process.env,
                            keys: Object.keys(copied).filter(key => key.toLowerCase() === 'path'),
                            copied: copied.PATH ?? null,
                            secret: process.env.API_TOKEN ?? null,
                            missing: process.env.PI_MISSING_CASE_TEST ?? null,
                            frozen: Object.isFrozen(process.env),
                        };
                        "#,
                    )
                    .await
                    .expect("read case aliases");
                let value = if deny_env {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(r"C:\selected")
                };
                let keys = if deny_env {
                    serde_json::json!([])
                } else {
                    serde_json::json!(["PATH"])
                };
                assert_eq!(
                    runtime
                        .read_global_json("windowsEnvironment")
                        .await
                        .unwrap(),
                    serde_json::json!({
                        "values": [value, value, value, value], "native": value,
                        "pi": value, "descriptor": value, "present": !deny_env,
                        "keys": keys, "copied": value, "secret": null,
                        "missing": null, "frozen": true,
                    }),
                    "reverse insertion={reverse}, env denied={deny_env}"
                );
            }
        }
    });
}

#[test]
fn pijs_non_windows_environment_preserves_distinct_case_bindings() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            DeterministicClock::new(0),
            config_with_env(vec![
                ("PI_PLATFORM", "linux"),
                ("PATH", "/upper"),
                ("Path", "/mixed"),
                ("path", "/lower"),
            ]),
        )
        .await
        .expect("case-sensitive snapshot runtime");
        runtime
            .eval(
                r#"
                globalThis.posixEnvironment = {
                    values: ['PATH', 'Path', 'path', 'pAtH'].map(key => process.env[key] ?? null),
                    native: __pi_env_get_native('pAtH'),
                    keys: Object.keys(process.env).filter(key => key.toLowerCase() === 'path').sort(),
                };
                "#,
            )
            .await
            .expect("read distinct bindings");
        assert_eq!(
            runtime.read_global_json("posixEnvironment").await.unwrap(),
            serde_json::json!({
                "values": ["/upper", "/mixed", "/lower", null],
                "native": null, "keys": ["PATH", "Path", "path"],
            })
        );
    });
}

#[test]
fn pijs_windows_configured_home_aliases_drive_os_and_config_metadata() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("home fixture");
        let selected = temp.path().join("selected").to_string_lossy().into_owned();
        let ignored = temp.path().join("ignored").to_string_lossy().into_owned();
        for (key, duplicate) in [("Home", "home"), ("UserProfile", "userprofile")] {
            let runtime = PiJsRuntime::with_clock_and_config(
                DeterministicClock::new(0),
                config_with_env(vec![
                    ("PI_PLATFORM", "win32"),
                    (key, &selected),
                    (duplicate, &ignored),
                ]),
            )
            .await
            .expect("configured home runtime");
            runtime
                .eval(
                    r#"
                    import('node:os').then(os => {
                        globalThis.homeAliases = {
                            home: os.homedir(),
                            config: process.env.PI_CONFIG_DIR,
                            alias: process.env.HOME || process.env.USERPROFILE,
                        };
                    });
                    "#,
                )
                .await
                .expect("read configured home metadata");
            runtime.drain_microtasks().await.expect("load node:os");
            assert_eq!(
                runtime.read_global_json("homeAliases").await.unwrap(),
                serde_json::json!({
                    "home": selected,
                    "config": temp.path().join("selected").join(".pi").join("agent"),
                    "alias": selected,
                }),
                "configured {key} and {duplicate} aliases"
            );
        }
        assert!(
            !temp.path().join("selected").exists(),
            "reading host metadata must not create directories"
        );
    });
}
