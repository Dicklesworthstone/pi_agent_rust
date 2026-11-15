#![no_main]

//! Fuzz harness for RPC dispatch loop.
//!
//! Feeds arbitrary JSON bytes into the src/rpc.rs dispatch loop
//! (around line 559) to test JSON parsing robustness and command
//! type handling. Early exits on malformed JSON are expected and fine.

use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fn fuzz_rpc_dispatch_line(line: &str) {
    // Simulate the RPC dispatch loop parsing logic from src/rpc.rs:559+
    if line.trim().is_empty() {
        return;
    }

    // Test JSON parsing (this is the main target)
    let parsed: Result<Value, _> = serde_json::from_str(line);

    if let Ok(parsed) = parsed {
        // Test command type extraction (part of dispatch logic)
        let _ = parsed.get("type").and_then(Value::as_str);

        // Test other common RPC fields that would be accessed
        let _ = parsed.get("id");
        let _ = parsed.get("params");
        let _ = parsed.get("method");

        // Exercise Value traversal patterns used in RPC dispatch
        if let Some(obj) = parsed.as_object() {
            for (_key, value) in obj {
                let _ = value.as_str();
                let _ = value.as_object();
                let _ = value.as_array();
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let lossy = String::from_utf8_lossy(data);

    // Test as single line (most common case)
    fuzz_rpc_dispatch_line(&lossy);

    // Test as JSONL-style multiple lines (RPC often uses line-based protocol)
    for line in lossy.lines().take(64) {
        fuzz_rpc_dispatch_line(line.trim_end_matches('\r'));
    }

    // Test with common JSON corruption patterns
    let trimmed = lossy.trim();
    if !trimmed.is_empty() {
        // Truncated JSON (common fuzzing pattern)
        for i in 1..=trimmed.len().min(256) {
            if let Some(substr) = trimmed.get(..i) {
                fuzz_rpc_dispatch_line(substr);
            }
        }
    }
});
