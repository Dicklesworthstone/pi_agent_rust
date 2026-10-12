//! Tool-to-wire coverage for streamed symbol and source-navigation reads.

use std::process::{Command, Stdio};

use asupersync::runtime::{Runtime, RuntimeBuilder};
use serde_json::{Value, json};

use super::LspTool;
use crate::config::Config;
use crate::error::Result;
use crate::tools::{Tool, ToolOutput};

const SERVER: &str = include_str!("streamed_reads/server.py");

fn payload(output: ToolOutput) -> Value {
    assert!(!output.is_error);
    let mut details = output.details.expect("structured tool result");
    // Workspace symbols retain the existing nested details envelope.
    if let Some(payload) = details.get_mut("payload") {
        return payload.take();
    }
    details
}

struct Peer {
    // Drop the tool and its child process before removing the fixture root.
    tool: LspTool,
    runtime: Runtime,
    root: tempfile::TempDir,
}

impl Peer {
    fn new(mode: &str) -> Option<Self> {
        let python = ["python3", "python"].into_iter().find(|program| {
            Command::new(program)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        });
        let Some(python) = python else {
            eprintln!("SKIP streamed semantic stdio tests: Python is unavailable");
            return None;
        };
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("anchor.pistream"), "fn anchor() {}\n").unwrap();
        let config: Config = serde_json::from_value(json!({"lsp": {
            "enabled":true, "servers":{"streamed-semantic-fixture": {
                "command":python,
                "args":["-u","-c",SERVER,mode,root.path().display().to_string()],
                "languages":["plaintext"], "extensions":[".pistream"], "rootMarkers":[]
            }}
        }}))
        .unwrap();
        let tool = LspTool::new(root.path(), Some(&config));
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        Some(Self {
            tool,
            runtime,
            root,
        })
    }

    fn run(&self, action: &str, extra: &Value) -> Result<ToolOutput> {
        let mut input = json!({"action":action,"file":"anchor.pistream","timeout":5});
        if action == "symbols" {
            input["query"] = json!("a");
        } else {
            input["position"] = json!({"line":0,"character":3});
        }
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.runtime
            .block_on(self.tool.execute("streamed-read-test", input, None))
    }

    fn frames(&self, method: &str) -> Vec<Value> {
        std::fs::read_to_string(self.root.path().join("streamed-read-frames.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|frame| frame["method"] == method)
            .collect()
    }

    fn assert_reusable(&self) {
        let output = self.run("hover", &json!({})).unwrap();
        assert!(!output.is_error);
        assert_eq!(payload(output)["hover"], "still usable");
        assert!(!self.root.path().join("metadata-target.rs").exists());
        assert!(self.frames("workspace/executeCommand").is_empty());
    }
}

#[test]
fn workspace_symbols_merge_streams_before_lazy_resolution() {
    for mode in ["stream", "terminal-empty", "legacy"] {
        let Some(peer) = Peer::new(mode) else {
            return;
        };
        let output = peer.run("symbols", &json!({"resolve":true})).unwrap();
        assert!(!output.is_error);
        let details = payload(output);
        assert_eq!(details["count"], 2);
        assert_eq!(details["total"], 2);
        assert_eq!(details["responseComplete"], true);
        assert_eq!(details["truncated"], false);
        assert_eq!(details["resolvedCount"], 1);
        assert_eq!(details["symbols"][1]["name"], "界");
        assert_eq!(
            details["symbols"][1]["location"]["uri"],
            "untitled:virtual-target"
        );
        let resolves = peer.frames("workspaceSymbol/resolve");
        assert_eq!(resolves.len(), 1);
        assert_eq!(
            resolves[0]["params"]["data"],
            json!({"ticket":7,"nested":[true,null,"$(not executed)"]})
        );
        assert!(resolves[0]["params"]["location"].get("range").is_none());
        assert_eq!(peer.frames("workspace/symbol").len(), 1);
        peer.assert_reusable();
    }
}

#[test]
fn every_array_navigation_action_uses_streams_and_preserves_location_links() {
    for (action, method) in [
        ("references", "textDocument/references"),
        ("definition", "textDocument/definition"),
        ("type_definition", "textDocument/typeDefinition"),
        ("implementation", "textDocument/implementation"),
    ] {
        for mode in ["stream", "terminal-empty", "legacy"] {
            let Some(peer) = Peer::new(mode) else {
                return;
            };
            let details = payload(peer.run(action, &json!({})).unwrap());
            assert_eq!(details["total"], 2);
            assert_eq!(details["count"], 2);
            assert_eq!(details["responseComplete"], true);
            assert_eq!(details["locations"][1]["uri"], "untitled:virtual-target");
            let frames = peer.frames(method);
            assert_eq!(
                frames.len(),
                1,
                "a populated stream must not trigger warmup replay"
            );
            assert!(frames[0]["params"]["partialResultToken"].is_string());
            if action == "references" {
                assert_eq!(frames[0]["params"]["context"]["includeDeclaration"], true);
            } else {
                assert!(details["locations"][0]["targetSelectionRange"].is_object());
                assert!(details["locations"][0]["originSelectionRange"].is_object());
            }
            peer.assert_reusable();
        }
    }
}

#[test]
fn nonstreamed_single_locations_and_null_results_remain_supported() {
    for action in ["definition", "type_definition", "implementation"] {
        let Some(peer) = Peer::new("single") else {
            return;
        };
        let details = payload(peer.run(action, &json!({})).unwrap());
        assert_eq!(details["total"], 1);
        assert_eq!(details["count"], 1);
        peer.assert_reusable();
    }
    for action in ["symbols", "references"] {
        let Some(peer) = Peer::new("legacy-null") else {
            return;
        };
        let details = payload(peer.run(action, &json!({})).unwrap());
        assert_eq!(details["total"], 0);
        assert_eq!(details["count"], 0);
        peer.assert_reusable();
    }
}

#[test]
fn display_limits_do_not_limit_validation_or_hide_stream_totals() {
    for action in ["symbols", "references"] {
        let Some(peer) = Peer::new("stream") else {
            return;
        };
        let details = payload(peer.run(action, &json!({"limit":1})).unwrap());
        assert_eq!(details["total"], 2);
        assert_eq!(details["count"], 1);
        assert_eq!(details["truncated"], true);
        let Some(peer) = Peer::new("bad-tail") else {
            return;
        };
        assert!(peer.run(action, &json!({"limit":1})).is_err());
        assert!(peer.frames("workspaceSymbol/resolve").is_empty());
        peer.assert_reusable();
    }
}

#[test]
fn failed_ambiguous_and_oversized_streams_cannot_become_partial_success() {
    for action in ["symbols", "references"] {
        for (mode, marker) in [
            ("malformed-fragment", "LSP_PARTIAL_RESULT"),
            ("nonempty-terminal", "LSP_PARTIAL_RESULT"),
            ("too-many", "combined item limit"),
            ("oversized", "LSP_PARTIAL_RESULT"),
            ("error", "search failed after progress"),
        ] {
            let Some(peer) = Peer::new(mode) else {
                return;
            };
            let error = peer.run(action, &json!({"limit":1})).unwrap_err();
            assert!(error.to_string().contains(marker), "{mode}: {error}");
            assert!(peer.frames("workspaceSymbol/resolve").is_empty());
            peer.assert_reusable();
        }
    }
}

#[test]
fn empty_navigation_streams_keep_the_existing_warmup_retry_policy() {
    let Some(peer) = Peer::new("warmup") else {
        return;
    };
    let details = payload(peer.run("references", &json!({})).unwrap());
    assert_eq!(details["total"], 2);
    let frames = peer.frames("textDocument/references");
    assert_eq!(frames.len(), 2);
    assert_ne!(
        frames[0]["params"]["partialResultToken"],
        frames[1]["params"]["partialResultToken"]
    );

    // Workspace symbols did not participate in empty-result warmup retries.
    let Some(peer) = Peer::new("warmup") else {
        return;
    };
    let details = payload(peer.run("symbols", &json!({})).unwrap());
    assert_eq!(details["total"], 0);
    assert_eq!(peer.frames("workspace/symbol").len(), 1);
}

#[test]
fn server_retry_discards_old_fragments_and_uses_a_fresh_token() {
    let Some(peer) = Peer::new("retry") else {
        return;
    };
    let details = payload(peer.run("references", &json!({})).unwrap());
    assert_eq!(details["total"], 2);
    let text = details.to_string();
    assert!(!text.contains("discarded-attempt"));
    assert!(!text.contains("late-old-attempt"));
    let frames = peer.frames("textDocument/references");
    assert_eq!(frames.len(), 2);
    assert_ne!(
        frames[0]["params"]["partialResultToken"],
        frames[1]["params"]["partialResultToken"]
    );
    peer.assert_reusable();
}

#[test]
fn progress_without_completion_times_out_and_releases_the_client_lane() {
    let Some(peer) = Peer::new("stall") else {
        return;
    };
    let error = peer.run("references", &json!({"timeout":1})).unwrap_err();
    assert!(error.to_string().contains("LSP_TIMEOUT"), "{error}");
    peer.assert_reusable();
    let requests = peer.frames("textDocument/references");
    let cancellations = peer.frames("$/cancelRequest");
    assert_eq!(requests.len(), 1);
    assert_eq!(cancellations.len(), 1);
    assert_eq!(cancellations[0]["params"]["id"], requests[0]["id"]);
}

#[test]
fn streamed_results_do_not_bypass_the_source_drift_fence() {
    for action in ["symbols", "references"] {
        let Some(peer) = Peer::new("source-change") else {
            return;
        };
        let error = peer.run(action, &json!({})).unwrap_err();
        assert!(error.to_string().contains("LSP_SEMANTIC_STALE"), "{error}");
        assert!(peer.frames("workspaceSymbol/resolve").is_empty());
        peer.assert_reusable();
    }
}
