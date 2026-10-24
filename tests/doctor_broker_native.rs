//! Exercise the CLI producer and broker consumer together, without swarm tools.
#![cfg(unix)]
#![recursion_limit = "256"]
#![forbid(unsafe_code)]

use pi::validation_broker::{ValidationSourceProvenance, normalize_doctor_json};
use serde_json::Value;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn run_native_doctor(
    selection: Option<&str>,
    corrupt_ledger: bool,
) -> TestResult<(Value, i32, ValidationSourceProvenance)> {
    let root = tempfile::tempdir()?;
    let agent = root.path().join("agent");
    let cwd = root.path().join("project");
    let bin = root.path().join("bin");
    for path in [&agent, &cwd, &bin] {
        fs::create_dir_all(path)?;
    }
    for name in ["sessions", "packages"] {
        fs::create_dir_all(agent.join(name))?;
    }
    fs::write(agent.join("settings.json"), "{}")?;
    if corrupt_ledger {
        fs::create_dir_all(cwd.join(".beads"))?;
        fs::write(
            cwd.join(".beads/issues.jsonl"),
            "{ broken coordination state\n",
        )?;
    }
    // Only real ordinary shell executables are discoverable, never br/am/rch.
    for name in ["sh", "bash"] {
        std::os::unix::fs::symlink(Path::new("/bin/sh"), bin.join(name))?;
    }
    let mut args = vec!["doctor", "--format", "json"];
    if let Some(selection) = selection {
        args.extend(["--only", selection]);
    }
    let output = Command::new(env!("CARGO_BIN_EXE_pi"))
        .args(&args)
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", root.path())
        .env("PI_CODING_AGENT_DIR", &agent)
        .env("PATH", &bin)
        .stdin(Stdio::null())
        .output()?;
    let code = output.status.code().ok_or("doctor terminated by signal")?;
    let report = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "doctor did not produce JSON: {error}; exit={code}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })?;
    let mut command = vec!["pi".to_string()];
    command.extend(args.into_iter().map(str::to_owned));
    let provenance = ValidationSourceProvenance::new(
        "doctor",
        command,
        cwd.display().to_string(),
        chrono::Utc::now().to_rfc3339(),
        None,
    )?;
    Ok((report, code, provenance))
}

#[test]
fn real_consumer_report_is_healthy_but_not_swarm_admission_evidence() -> TestResult {
    for selection in [None, Some("config")] {
        let (report, exit, provenance) = run_native_doctor(selection, false)?;
        assert_eq!(
            exit, 0,
            "consumer installation must remain healthy: {report}"
        );
        assert_eq!(report["summary"]["fail"], 0);
        let findings = report["findings"].as_array().ok_or("findings array")?;
        assert!(!findings.is_empty());
        assert!(
            findings
                .iter()
                .all(|finding| finding["category"] != "swarm")
        );
        let input = normalize_doctor_json(provenance, &report)?;
        assert!(
            input.health.is_degraded(),
            "consumer health is not admission evidence"
        );
        assert!(
            !input.has_failures,
            "the broker must not invent consumer failures"
        );
        assert_eq!(input.checks.len(), findings.len());
    }
    Ok(())
}

#[test]
fn real_swarm_report_retains_storage_and_coordination_failures() -> TestResult {
    for selection in ["swarm", "config,swarm"] {
        let (report, exit, provenance) = run_native_doctor(Some(selection), true)?;
        assert_eq!(
            exit, 1,
            "explicit swarm failures must affect exit: {report}"
        );
        let findings = report["findings"].as_array().ok_or("findings array")?;
        assert!(findings.iter().any(|finding| {
            finding["category"] == "swarm"
                && finding["severity"] == "fail"
                && finding["title"] == "Beads ledger has malformed JSONL rows"
        }));
        let resource = findings
            .iter()
            .find(|finding| finding["data"]["schema"] == "pi.doctor.swarm_resource_preflight.v1")
            .ok_or("native resource preflight")?;
        assert_eq!(resource["severity"], "fail");
        let paths = resource["data"]["tmpfs_headroom"]["paths"]
            .as_array()
            .ok_or("native storage evidence")?;
        for name in ["CARGO_TARGET_DIR", "TMPDIR"] {
            let path = paths
                .iter()
                .find(|path| path["env_name"] == name)
                .ok_or("required native scratch row")?;
            assert_eq!(path["ready"], false);
        }
        let input = normalize_doctor_json(provenance, &report)?;
        assert!(input.has_failures);
        assert!(input.health.is_degraded());
        assert_eq!(input.checks.len(), findings.len());
        assert!(input.checks.iter().any(|check| {
            check.name.contains("Beads ledger has malformed JSONL rows") && check.status == "fail"
        }));
    }
    Ok(())
}
