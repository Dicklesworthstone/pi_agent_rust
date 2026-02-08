//! Unit tests for the process global shim (bd-1av0.9).
//!
//! Tests the enhanced `globalThis.process` object: blocklist-based env filtering,
//! stdout/stderr routing, exit signaling, event emitter API, hrtime, and more.
#![allow(clippy::needless_raw_string_hashes)]

use std::collections::HashMap;
use std::sync::Arc;

use pi::extensions_js::{
    HostcallKind, PiJsRuntime, PiJsRuntimeConfig, PiJsRuntimeLimits, is_env_var_allowed,
};
use pi::scheduler::DeterministicClock;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn config_with_env(env: Vec<(&str, &str)>) -> PiJsRuntimeConfig {
    PiJsRuntimeConfig {
        cwd: "/test".to_string(),
        args: vec![],
        env: env
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        limits: PiJsRuntimeLimits::default(),
    }
}

fn default_config() -> PiJsRuntimeConfig {
    PiJsRuntimeConfig {
        cwd: "/test".to_string(),
        args: vec![],
        env: HashMap::new(),
        limits: PiJsRuntimeLimits::default(),
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
            own_keys.as_array().unwrap().is_empty(),
            "ownKeys returns []"
        );
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
