//! Unit tests for the node:url shim (bd-1av0.5).
//!
//! Tests the enhanced URL and `URLSearchParams` implementations with proper
//! parsing, base URL resolution, and searchParams support.
#![allow(clippy::needless_raw_string_hashes)]

use std::sync::Arc;

use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::DeterministicClock;

fn default_config() -> PiJsRuntimeConfig {
    PiJsRuntimeConfig {
        cwd: "/test".to_string(),
        ..Default::default()
    }
}

#[test]
fn url_parses_full_url() {
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
                import('node:url').then((urlMod) => {
                    const u = new urlMod.URL('https://user:pass@example.com:8080/path?q=1#frag');
                    globalThis.urlResult = {
                        protocol: u.protocol,
                        hostname: u.hostname,
                        port: u.port,
                        pathname: u.pathname,
                        search: u.search,
                        hash: u.hash,
                        username: u.username,
                        password: u.password,
                        host: u.host,
                        origin: u.origin,
                        href: u.href,
                    };
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let val: serde_json::Value = runtime.read_global_json("urlResult").await.unwrap();
        assert_eq!(val["protocol"], "https:");
        assert_eq!(val["hostname"], "example.com");
        assert_eq!(val["port"], "8080");
        assert_eq!(val["pathname"], "/path");
        assert_eq!(val["search"], "?q=1");
        assert_eq!(val["hash"], "#frag");
        assert_eq!(val["username"], "user");
        assert_eq!(val["password"], "pass");
        assert_eq!(val["host"], "example.com:8080");
    });
}

#[test]
fn url_search_params_basic_ops() {
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
                import('node:url').then((urlMod) => {
                    const params = new urlMod.URLSearchParams('foo=bar&baz=qux&foo=extra');
                    globalThis.spResult = {
                        getFoo: params.get('foo'),
                        getBaz: params.get('baz'),
                        getMissing: params.get('missing'),
                        hasFoo: params.has('foo'),
                        hasMissing: params.has('missing'),
                        allFoo: params.getAll('foo'),
                    };
                    params.set('newKey', 'newVal');
                    params.delete('baz');
                    globalThis.spResult.afterSetDelete = params.toString();
                    globalThis.spResult.size = params.size;
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let val: serde_json::Value = runtime.read_global_json("spResult").await.unwrap();
        assert_eq!(val["getFoo"], "bar");
        assert_eq!(val["getBaz"], "qux");
        assert!(val["getMissing"].is_null());
        assert_eq!(val["hasFoo"], true);
        assert_eq!(val["hasMissing"], false);
        let all_foo: Vec<&str> = val["allFoo"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(all_foo, vec!["bar", "extra"]);
    });
}

#[test]
fn url_file_url_to_path() {
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
                import('node:url').then((urlMod) => {
                    globalThis.filePath = urlMod.fileURLToPath('file:///home/user/file.txt');
                    globalThis.roundTrip = urlMod.fileURLToPath(
                        urlMod.pathToFileURL('/home/user/file.txt').href
                    );
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let path: serde_json::Value = runtime.read_global_json("filePath").await.unwrap();
        assert_eq!(path, "/home/user/file.txt");
    });
}

#[test]
fn url_parse_and_resolve() {
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
                import('node:url').then((urlMod) => {
                    const parsed = urlMod.parse('https://example.com/path');
                    globalThis.parseResult = parsed ? parsed.hostname : null;
                    globalThis.formatResult = urlMod.format({ href: 'https://test.com/foo' });
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let hostname: serde_json::Value = runtime.read_global_json("parseResult").await.unwrap();
        assert_eq!(hostname, "example.com");

        let formatted: serde_json::Value = runtime.read_global_json("formatResult").await.unwrap();
        assert_eq!(formatted, "https://test.com/foo");
    });
}

#[test]
fn url_to_json_returns_href() {
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
                import('node:url').then((urlMod) => {
                    const u = new urlMod.URL('https://example.com/test');
                    globalThis.jsonStr = JSON.stringify(u);
                    globalThis.toStr = u.toString();
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let to_str: serde_json::Value = runtime.read_global_json("toStr").await.unwrap();
        assert_eq!(to_str, "https://example.com/test");
    });
}

#[test]
fn url_simple_no_port() {
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
                import('node:url').then((urlMod) => {
                    const u = new urlMod.URL('http://localhost/api/v1');
                    globalThis.simpleResult = {
                        protocol: u.protocol,
                        hostname: u.hostname,
                        port: u.port,
                        pathname: u.pathname,
                    };
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let val: serde_json::Value = runtime.read_global_json("simpleResult").await.unwrap();
        assert_eq!(val["protocol"], "http:");
        assert_eq!(val["hostname"], "localhost");
        assert_eq!(val["port"], "");
        assert_eq!(val["pathname"], "/api/v1");
    });
}

#[test]
fn url_resolves_relative_references_and_windows_file_paths() {
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
                import('node:url').then(({ URL: NodeURL }) => {
                    const cases = [
                        ['/health', 'http://127.0.0.1', 'http://127.0.0.1/health'],
                        ['child', 'https://example.com/dir/page?old#frag', 'https://example.com/dir/child'],
                        ['../next', 'https://example.com/dir/page', 'https://example.com/next'],
                        ['//other.example:443/a', 'https://example.com/base', 'https://other.example/a'],
                        ['https://other.example/a', 'http://localhost/base', 'https://other.example/a'],
                        ['?new=2', 'https://example.com/dir/page?old#frag', 'https://example.com/dir/page?new=2'],
                        ['#new', 'https://example.com/dir/page?old#frag', 'https://example.com/dir/page?old#new'],
                        ['', 'https://example.com/dir/page?old#frag', 'https://example.com/dir/page?old'],
                        ['%2Fencoded?x=%2F#frag', 'https://example.com/dir/page', 'https://example.com/dir/%2Fencoded?x=%2F#frag'],
                        ['%2e%2e/next', 'https://example.com/dir/page', 'https://example.com/next'],
                        ['../relay.ts', 'file:///C:/Users/reporter/ext/src/index.ts', 'file:///C:/Users/reporter/ext/relay.ts'],
                        ['/health', 'http://[::1]:28180/v1/', 'http://[::1]:28180/health'],
                        ['?', 'https://example.com/p?old#frag', 'https://example.com/p?'],
                        ['#', 'https://example.com/p?old#frag', 'https://example.com/p?old#'],
                    ];
                    globalThis.resolutionFailures = [];
                    for (const Constructor of [URL, NodeURL]) {
                        for (const [input, base, expected] of cases) {
                            const parsed = new Constructor(input, base);
                            if (parsed.href !== expected || parsed.toString() !== expected || parsed.toJSON() !== expected) {
                                resolutionFailures.push({ input, base, expected, actual: parsed.href, serialized: parsed.toString() });
                            }
                        }
                    }
                    const ipv6 = new URL('http://[::1]:28180/health');
                    globalThis.loopbackParts = { hostname: ipv6.hostname, host: ipv6.host, origin: ipv6.origin, pathname: ipv6.pathname };
                    globalThis.encodedPath = new URL('%2Fencoded', 'https://example.com/dir/').pathname;
                    globalThis.fileOrigin = new NodeURL('file:///C:/Users/reporter/relay.ts').origin;
                });
                "#,
            )
            .await
            .expect("eval URL resolution cases");
        runtime.drain_microtasks().await.expect("drain");
        let failures: serde_json::Value = runtime
            .read_global_json("resolutionFailures")
            .await
            .expect("resolution failures");
        assert_eq!(failures, serde_json::json!([]));
        let loopback: serde_json::Value = runtime
            .read_global_json("loopbackParts")
            .await
            .expect("loopback parts");
        assert_eq!(
            loopback,
            serde_json::json!({
                "hostname": "[::1]", "host": "[::1]:28180",
                "origin": "http://[::1]:28180", "pathname": "/health",
            })
        );
        let encoded: serde_json::Value = runtime
            .read_global_json("encodedPath")
            .await
            .expect("encoded path");
        assert_eq!(encoded, "/dir/%2Fencoded");
        let file_origin: serde_json::Value = runtime
            .read_global_json("fileOrigin")
            .await
            .expect("file origin");
        assert_eq!(file_origin, "null");
    });
}

#[test]
fn url_invalid_input_or_base_throws_type_error() {
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
                import('node:url').then(({ URL: NodeURL }) => {
                    globalThis.invalidUrlResults = [URL, NodeURL].flatMap(Constructor =>
                        [['relative'], ['http://[::1'], ['http://example.com:65536'], ['/health', 'invalid-base']].map(args => {
                            try { new Constructor(...args); return 'accepted'; }
                            catch (error) { return error.name; }
                        })
                    );
                });
                "#,
            )
            .await
            .expect("eval invalid URLs");
        runtime.drain_microtasks().await.expect("drain");
        let results: serde_json::Value = runtime
            .read_global_json("invalidUrlResults")
            .await
            .expect("invalid URL results");
        assert_eq!(
            results,
            serde_json::json!([
                "TypeError",
                "TypeError",
                "TypeError",
                "TypeError",
                "TypeError",
                "TypeError",
                "TypeError",
                "TypeError"
            ])
        );
    });
}
