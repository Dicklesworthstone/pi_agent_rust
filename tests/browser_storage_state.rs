//! Public native admission checks and an opt-in real authentication round trip.
#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::{BrowserLaunchOptions, BrowserTool};
use pi::tools::Tool;
use serde_json::{Value, json};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build().unwrap().block_on(future)
}

#[test]
fn storage_actions_coexist_with_native_batch_schema() {
    let root = tempfile::tempdir().unwrap();
    let schema = BrowserTool::new(root.path()).parameters();
    let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
    for action in ["save_state", "restore_state", "batch", "snapshot"] {
        assert_eq!(actions.iter().filter(|value| **value == json!(action)).count(), 1);
    }
    assert!(schema["properties"].get("state_path").is_some());
    assert!(schema["properties"].get("steps").is_some());
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[test]
fn attached_profiles_are_refused_before_any_socket_or_file_access() {
    let root = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(root.path()).with_mock(false)
        .with_cdp_endpoint("http://127.0.0.1:9");
    run(async {
        for action in ["save_state", "restore_state"] {
            let failure = tool.execute("attached-state", json!({"action":action,
                "tab":"work", "state_path":"not-present.json"}), None).await.err().unwrap();
            assert!(failure.to_string().contains("BROWSER_STATE_OWNERSHIP"));
        }
    });
    assert!(!root.path().join("not-present.json").exists());
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[test]
fn storage_actions_never_launch_a_missing_managed_browser() {
    let root = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(root.path()).with_mock(false)
        .with_launch_options(BrowserLaunchOptions {
            executable_path: Some(root.path().join("must-not-be-launched")),
            ..Default::default()
        });
    run(async {
        for action in ["save_state", "restore_state"] {
            let failure = tool.execute("idle-state", json!({"action":action,
                "tab":"work", "state_path":"not-present.json"}), None).await.err().unwrap();
            assert!(failure.to_string().contains("BROWSER_STATE_NOT_RUNNING"));
        }
    });
    assert!(!root.path().join("not-present.json").exists());
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
mod live {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
    use std::time::Duration;

    struct Site {
        origin: String,
        stopped: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl Site {
        fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let stopped = Arc::new(AtomicBool::new(false));
            let quit = Arc::clone(&stopped);
            let worker = std::thread::spawn(move || {
                while !quit.load(Ordering::SeqCst) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(failure) if failure.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                    stream.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while request.len() < 16 * 1024 && !request.windows(4).any(|part| part == b"\r\n\r\n") {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(length) => request.extend_from_slice(&buffer[..length]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    let seed = request.starts_with("GET /seed ");
                    let authenticated = request.lines().any(|line| {
                        line.split_once(':').is_some_and(|(name, value)| {
                            name.eq_ignore_ascii_case("cookie") && value.split(';').any(|cookie| cookie.trim() == "auth=HTTPONLY-CANARY")
                        })
                    });
                    let body = format!("<!doctype html><html><head><title>State fixture</title></head><body data-authenticated=\"{authenticated}\">Fixture</body></html>");
                    let cookie = if seed { "Set-Cookie: auth=HTTPONLY-CANARY; Path=/; HttpOnly; SameSite=Lax\r\n" } else { "" };
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\n{cookie}Content-Length: {}\r\n\r\n{body}", body.len());
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            Self { origin, stopped, worker:Some(worker) }
        }
    }

    impl Drop for Site {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() { worker.join().unwrap(); }
        }
    }

    async fn call(tool: &BrowserTool, args: Value) -> pi::tools::ToolOutput {
        let output = tool.execute("state-round-trip", args, None).await.unwrap();
        assert!(!output.is_error);
        output
    }

    #[test]
    #[ignore = "requires an installed managed Chromium and explicit DSR live-browser lane"]
    fn managed_round_trip_restores_http_only_cookie_and_local_storage_without_clearing_other_keys() {
        let site = Site::start();
        let root = tempfile::tempdir().unwrap();
        let tool = BrowserTool::new(root.path()).with_mock(false)
            .with_launch_options(BrowserLaunchOptions::default());
        run(async {
            call(&tool, json!({"action":"open", "tab":"app", "url":format!("{}/seed", site.origin)})).await;
            call(&tool, json!({"action":"evaluate", "tab":"app", "script":"localStorage.setItem('token', 'STORAGE-CANARY'); true"})).await;
            let saved = call(&tool, json!({"action":"save_state", "tab":"app", "state_path":"private/auth.json"})).await;
            assert!(!serde_json::to_string(&saved).unwrap().contains("CANARY"));
            let state: Value = serde_json::from_slice(&std::fs::read(root.path().join("private/auth.json")).unwrap()).unwrap();
            let cookie = state["cookies"].as_array().unwrap().iter().find(|cookie| cookie["name"] == "auth").unwrap();
            assert_eq!(cookie["httpOnly"], true);
            assert_eq!(cookie["value"], "HTTPONLY-CANARY");
            assert!(cookie.get("expires").is_none());
            call(&tool, json!({"action":"stop"})).await;
            call(&tool, json!({"action":"open", "tab":"app", "url":format!("{}/blank", site.origin)})).await;
            let empty = call(&tool, json!({"action":"evaluate", "tab":"app", "script":"({token: localStorage.getItem('token'), authenticated: document.body.dataset.authenticated})"})).await;
            assert_eq!(empty.details.as_ref().unwrap()["result"], json!({"token":null, "authenticated":"false"}));
            call(&tool, json!({"action":"evaluate", "tab":"app", "script":"localStorage.setItem('keep', 'untouched'); document.cookie='keep=untouched; Path=/'; true"})).await;
            let restored = call(&tool, json!({"action":"restore_state", "tab":"app", "state_path":"private/auth.json"})).await;
            assert_eq!(restored.details.as_ref().unwrap()["storage_verified"], true);
            assert_eq!(restored.details.as_ref().unwrap()["authentication_verified"], false);
            assert!(!serde_json::to_string(&restored).unwrap().contains("CANARY"));
            call(&tool, json!({"action":"goto", "tab":"app", "url":format!("{}/check", site.origin)})).await;
            let result = call(&tool, json!({"action":"evaluate", "tab":"app", "script":"({token: localStorage.getItem('token'), keep: localStorage.getItem('keep'), authenticated: document.body.dataset.authenticated, visibleCookies: document.cookie})"})).await;
            let value = &result.details.as_ref().unwrap()["result"];
            assert_eq!(value["token"], "STORAGE-CANARY");
            assert_eq!(value["keep"], "untouched");
            assert_eq!(value["authenticated"], "true");
            assert!(value["visibleCookies"].as_str().unwrap().contains("keep=untouched"));
            assert!(!value["visibleCookies"].as_str().unwrap().contains("HTTPONLY-CANARY"));
            call(&tool, json!({"action":"stop"})).await;
        });
    }
}
