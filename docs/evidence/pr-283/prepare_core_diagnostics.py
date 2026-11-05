#!/usr/bin/env python3
"""Recreate the explicitly limited PR #283 Rust module diagnostics.

This creates a separate crate and does not modify the source checkout. It is
not a replacement for `dsr quality --tool pi_agent_rust`. Source and fixture
checkouts must remain fixed while Cargo runs. Python 3.11+ is required.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import tomllib


VERSIONS = {
    "base64": "0.23.1", "chrono": "0.4.45", "dirs": "7.0.0",
    "fs4": "1.1.0", "rustix": "1.1.5", "serde": "1.0.229",
    "serde_json": "1.0.151", "sha2": "0.11.0", "tempfile": "3.27.0",
    "thiserror": "2.0.21", "tracing": "0.1.44", "uuid": "1.27.0",
}
FEATURES = {
    "chrono": ["serde"], "rustix": ["fs", "process"],
    "serde": ["derive", "rc"], "serde_json": ["raw_value"], "uuid": ["v4"],
}
SURFACES = {
    "undo": ["base64", "dirs", "fs4", "rustix", "serde", "serde_json",
             "sha2", "tempfile", "tracing", "uuid"],
    "validation": ["chrono", "rustix", "serde", "serde_json", "sha2",
                   "tempfile", "thiserror"],
}

# Exact relevant production helper bodies; the full Config type is excluded.
CONFIG_ADAPTER = r'''use std::path::PathBuf;
pub struct Config;
impl Config {
    pub fn global_dir() -> PathBuf { global_dir_from_env(env_lookup) }
}
fn env_lookup(var: &str) -> Option<String> { std::env::var(var).ok() }
fn global_dir_from_env<F>(get_env: F) -> PathBuf
where F: Fn(&str) -> Option<String>,
{
    get_env("PI_CODING_AGENT_DIR").map_or_else(
        || {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".pi")
                .join("agent")
        },
        PathBuf::from,
    )
}
'''

# Only the broker's required error variants; full runtime/provider errors absent.
ERROR_ADAPTER = r'''pub type Result<T> = std::result::Result<T, Error>;
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("IO error: {0}")]
    Io(#[from] Box<std::io::Error>),
    #[error("JSON error: {0}")]
    Json(#[from] Box<serde_json::Error>),
}
impl Error {
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }
}
impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self { Self::Io(Box::new(value)) }
}
impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self { Self::Json(Box::new(value)) }
}
'''


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def registry_packages(lock: Path) -> set[tuple[str, str]]:
    packages = tomllib.loads(lock.read_text())["package"]
    return {(p["name"], p["version"]) for p in packages
            if p.get("source", "").startswith("registry+")}


def prepare(source: Path, output: Path, surface: str, tests: list[str]) -> None:
    source, output = source.resolve(), output.resolve()
    if output.exists() or output.is_relative_to(source):
        raise SystemExit("Output must be a new directory outside the source checkout")
    lock = source / "Cargo.lock"
    versions = registry_packages(lock)
    required = {(name, VERSIONS[name]) for name in SURFACES[surface]}
    if missing := required - versions:
        raise SystemExit(f"Required historical dependency versions absent: {sorted(missing)}")
    modules = ["undo.rs", "file_identity.rs"] if surface == "undo" else ["validation_broker.rs"]
    child = "undo" if surface == "undo" else "validation_broker"
    source_files = [Path("src") / name for name in modules]
    source_files += [p.relative_to(source) for p in (source / "src" / child).rglob("*.rs")]
    if surface == "validation":
        tests = tests or ["validation_broker_store"]
        if any(re.fullmatch(r"[A-Za-z0-9_-]+", name) is None for name in tests):
            raise SystemExit("Test names must be simple Cargo integration-target names")
        source_files += [Path("tests") / f"{name}.rs" for name in tests]
    elif tests:
        raise SystemExit("--test is only supported for the validation surface")
    for relative in source_files:
        if not (source / relative).is_file():
            raise SystemExit(f"Source file missing: {relative}")
    output.mkdir(parents=True)
    inventory = {str(relative): digest(source / relative) for relative in source_files}
    for relative in source_files:
        destination = output / ("source" if relative.parts[0] == "src" else "diagnostic_tests") / Path(*relative.parts[1:])
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source / relative, destination)
    shutil.copyfile(lock, output / "Cargo.lock")
    shutil.copyfile(lock, output / "source-Cargo.lock")
    shutil.copyfile(source / "rust-toolchain.toml", output / "rust-toolchain.toml")
    header = ('[package]\nname = "pi_core_diagnostic"\nversion = "0.0.0"\n'
              'edition = "2024"\npublish = false\nautotests = false\n\n'
              '[lib]\nname = "pi"\npath = "source/lib.rs"\n')
    if surface == "validation":
        header += 'test = false\ndoctest = false\n'
        for name in tests:
            header += f'\n[[test]]\nname = {json.dumps(name)}\npath = "diagnostic_tests/{name}.rs"\n'
        for folder in ("tests", "docs"):
            (output / folder).symlink_to(source / folder, target_is_directory=True)
        library = "pub mod validation_broker;\npub mod error;\n"
        (output / "source/error.rs").write_text(ERROR_ADAPTER)
    else:
        library = "pub mod file_identity;\npub mod undo;\npub mod config;\n"
        (output / "source/config.rs").write_text(CONFIG_ADAPTER)
    (output / "source/lib.rs").write_text('#![forbid(unsafe_code)]\n' + library)
    manifest = header + "\n[dependencies]\n"
    for name in SURFACES[surface]:
        version = json.dumps("=" + VERSIONS[name])
        manifest += (f'{name} = {{ version = {version}, features = {json.dumps(FEATURES[name])} }}\n'
                     if name in FEATURES else f'{name} = {version}\n')
    manifest += '\n[profile.dev]\ndebug = 0\nincremental = false\n\n[profile.test]\ndebug = 0\nincremental = false\n'
    source_manifest = source / "Cargo.toml"
    if source_manifest.exists():
        sections = re.split(r"(?=^\[)", source_manifest.read_text(), flags=re.M)
        manifest += "\n" + "\n".join(part for part in sections if part.startswith("[lints."))
    if (source / "clippy.toml").exists():
        shutil.copyfile(source / "clippy.toml", output / "clippy.toml")
    (output / "Cargo.toml").write_text(manifest)
    (output / "source-inventory.json").write_text(json.dumps({
        "source_checkout": str(source), "surface": surface, "source_files_sha256": inventory,
        "source_lock_sha256": digest(lock), "toolchain_sha256": digest(source / "rust-toolchain.toml"),
        "fixture_note": "tests/docs are links to the fixed source checkout" if surface == "validation" else "No external fixtures",
        "scope": "Exact source module diagnostics with explicit Config/Error adapters; not full Pi or DSR qualification",
    }, indent=2) + "\n")
    print(f"Prepared {output}; run Cargo with the pinned toolchain, then run verify.")


def verify(output: Path) -> None:
    inventory = json.loads((output / "source-inventory.json").read_text())
    for name, key in (("source-Cargo.lock", "source_lock_sha256"),
                      ("rust-toolchain.toml", "toolchain_sha256")):
        if digest(output / name) != inventory[key]:
            raise SystemExit(f"Copied provenance anchor changed since preparation: {name}")
    for name, expected in inventory["source_files_sha256"].items():
        relative = Path(name)
        copied = output / ("source" if relative.parts[0] == "src" else "diagnostic_tests") / Path(*relative.parts[1:])
        if digest(copied) != expected:
            raise SystemExit(f"Copied source changed since preparation: {name}")
    original = registry_packages(output / "source-Cargo.lock")
    actual = registry_packages(output / "Cargo.lock")
    if extra := actual - original:
        raise SystemExit(f"Resolved registry versions absent from source lock: {sorted(extra)}")
    print(f"All {len(actual)} registry package versions in Cargo.lock occur in the source lock.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("prepare")
    create.add_argument("source", type=Path)
    create.add_argument("output", type=Path)
    create.add_argument("--surface", choices=SURFACES, required=True)
    create.add_argument("--test", action="append", default=[])
    check = commands.add_parser("verify")
    check.add_argument("output", type=Path)
    args = parser.parse_args()
    if args.command == "prepare":
        prepare(args.source, args.output, args.surface, args.test)
    else:
        verify(args.output)


if __name__ == "__main__":
    main()
