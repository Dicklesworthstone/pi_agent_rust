#!/usr/bin/env python3
"""Generate machine-readable parity evidence from cargo test output.

Parses cargo test stdout for pass/fail counts across parity test suites
and emits a structured JSON artifact suitable for CI gate consumption.

Usage:
    python3 scripts/ci/generate_parity_evidence.py --output OUT --log LOG
    python3 scripts/ci/generate_parity_evidence.py --self-test
"""

import argparse
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

SCHEMA = "pi.ci.parity_evidence.v1"
COUNTING_TAXONOMY_SCHEMA = "pi.qa.counting_taxonomy.v1"
COUNTING_TAXONOMY_CONTRACT_REL_PATH = "docs/counting-taxonomy-contract.json"
PROVIDER_METADATA_REL_PATH = "src/provider_metadata.rs"
EXTENSION_CATALOG_REL_PATH = "docs/extension-master-catalog.json"

LOC_RAW_LABEL = "loc_raw_lines"
LOC_LOGICAL_LABEL = "loc_logical_lines"
PROVIDER_CANONICAL_LABEL = "provider_canonical_ids"
PROVIDER_ALIAS_LABEL = "provider_alias_ids"
PROVIDER_FAMILY_LABEL = "provider_families"
EXTENSION_OFFICIAL_LABEL = "extension_official_subset"
EXTENSION_COMMUNITY_LABEL = "extension_community_subset"
EXTENSION_FULL_LABEL = "extension_full_corpus"

PARITY_SUITES = [
    "json_mode_parity",
    "cross_surface_parity",
    "config_precedence",
    "vcr_parity_validation",
    "e2e_cross_provider_parity",
]

# Matches cargo test summary lines like:
#   test result: ok. 104 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.42s
RESULT_RE = re.compile(
    r"test result: (ok|FAILED)\.\s+"
    r"(\d+) passed;\s+"
    r"(\d+) failed;\s+"
    r"(\d+) ignored"
)

# Matches running line like:
#   Running tests/json_mode_parity.rs (target/debug/deps/json_mode_parity-abc123)
RUNNING_RE = re.compile(r"Running (?:tests/)?(\S+?)(?:\.rs)?\s")

# CI sets CARGO_TERM_COLOR=always, so the captured log carries SGR sequences
# (for example "Running\x1b[0m tests/json_mode_parity.rs"). Strip them before
# matching or the suite markers never line up.
ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def parse_log(log_text: str) -> dict:
    """Parse cargo test output and return per-suite results."""
    suites = {}
    current_suite = None

    for raw_line in log_text.splitlines():
        line = ANSI_RE.sub("", raw_line)
        running_match = RUNNING_RE.search(line)
        if running_match:
            raw = running_match.group(1)
            # Normalize: strip path prefixes, extract stem
            stem = raw.rsplit("/", 1)[-1]
            # cargo test output may include the hash suffix
            stem = stem.split("-")[0] if "-" in stem else stem
            if stem in PARITY_SUITES:
                current_suite = stem

        result_match = RESULT_RE.search(line)
        if result_match and current_suite:
            status = result_match.group(1)
            passed = int(result_match.group(2))
            failed = int(result_match.group(3))
            ignored = int(result_match.group(4))
            suites[current_suite] = {
                "status": "pass" if status == "ok" else "fail",
                "passed": passed,
                "failed": failed,
                "ignored": ignored,
                "total": passed + failed + ignored,
            }
            current_suite = None

    return suites


def project_root_from_script() -> Path:
    # scripts/ci/generate_parity_evidence.py -> repo root is parents[2]
    return Path(__file__).resolve().parents[2]


def strip_rust_comments(line: str, in_block_comment: bool) -> tuple[str, bool]:
    """Strip Rust line/block comments while preserving non-comment code."""
    i = 0
    out = []
    while i < len(line):
        if in_block_comment:
            end = line.find("*/", i)
            if end == -1:
                return ("".join(out), True)
            i = end + 2
            in_block_comment = False
            continue

        if line.startswith("//", i):
            break
        if line.startswith("/*", i):
            in_block_comment = True
            i += 2
            continue

        out.append(line[i])
        i += 1

    return ("".join(out), in_block_comment)


def count_rust_loc(src_root: Path) -> tuple[int, int]:
    """Return (raw_lines, logical_lines) across src/**/*.rs."""
    raw_lines = 0
    logical_lines = 0
    in_block_comment = False

    for path in sorted(src_root.rglob("*.rs")):
        text = path.read_text(encoding="utf-8", errors="replace")
        for line in text.splitlines():
            raw_lines += 1
            cleaned, in_block_comment = strip_rust_comments(line, in_block_comment)
            if cleaned.strip():
                logical_lines += 1

    return (raw_lines, logical_lines)


def count_provider_dimensions(provider_metadata_path: Path) -> tuple[int, int, int]:
    """Return (canonical_ids, alias_ids, families)."""
    text = provider_metadata_path.read_text(encoding="utf-8", errors="replace")
    canonical_ids = re.findall(r'canonical_id:\s*"([^"]+)"', text)
    alias_blocks = re.findall(r"aliases:\s*&\[(.*?)\],", text)
    alias_ids = []
    for block in alias_blocks:
        alias_ids.extend(re.findall(r'"([^"]+)"', block))
    families = set(re.findall(r"onboarding:\s*ProviderOnboardingMode::([A-Za-z0-9_]+)", text))
    return (len(canonical_ids), len(alias_ids), len(families))


def count_extension_dimensions(extension_catalog_path: Path) -> tuple[int, int, int]:
    """Return (official_subset, community_subset, full_corpus)."""
    payload = json.loads(extension_catalog_path.read_text(encoding="utf-8"))
    entries = payload.get("extensions", [])
    official = 0
    community = 0
    for item in entries:
        tier = item.get("source_tier")
        if tier == "official-pi-mono":
            official += 1
        elif tier == "community":
            community += 1
    return (official, community, len(entries))


def metric_row(metric_key: str, granularity_label: str, value: int, unit: str, source: str, command_signature: str) -> dict:
    return {
        "metric_key": metric_key,
        "granularity_label": granularity_label,
        "value": value,
        "unit": unit,
        "tool_provenance": {
            "source": source,
            "command_signature": command_signature,
        },
    }


def load_counting_taxonomy_contract(project_root: Path) -> dict:
    contract_path = project_root / COUNTING_TAXONOMY_CONTRACT_REL_PATH
    if not contract_path.exists():
        raise ValueError(f"counting taxonomy contract not found: {contract_path}")
    return json.loads(contract_path.read_text(encoding="utf-8"))


def validate_counting_taxonomy(taxonomy: dict, contract: dict) -> list[str]:
    errors = []

    expected_schema = contract.get("taxonomy_schema")
    if taxonomy.get("schema") != expected_schema:
        errors.append(
            f"taxonomy schema mismatch: expected {expected_schema!r}, got {taxonomy.get('schema')!r}"
        )

    dimensions = taxonomy.get("dimensions")
    if not isinstance(dimensions, dict):
        return errors + ["taxonomy.dimensions must be an object"]

    required_dimensions = contract.get("required_dimensions", {})
    required_metric_fields = contract.get("required_metric_fields", [])
    required_provenance_fields = contract.get("required_tool_provenance_fields", [])

    for dim_name, dim_contract in required_dimensions.items():
        dim = dimensions.get(dim_name)
        if not isinstance(dim, dict):
            errors.append(f"missing taxonomy dimension: {dim_name}")
            continue

        metrics = dim.get("metrics")
        if not isinstance(metrics, list):
            errors.append(f"taxonomy dimension {dim_name} metrics must be an array")
            continue

        required_labels = set(dim_contract.get("required_granularity_labels", []))
        seen_labels = set()

        for idx, metric in enumerate(metrics):
            metric_path = f"dimensions.{dim_name}.metrics[{idx}]"
            if not isinstance(metric, dict):
                errors.append(f"{metric_path} must be an object")
                continue

            for field in required_metric_fields:
                if field not in metric:
                    errors.append(f"{metric_path} missing field {field!r}")

            label = metric.get("granularity_label")
            if isinstance(label, str) and label:
                seen_labels.add(label)
            else:
                errors.append(f"{metric_path}.granularity_label must be a non-empty string")

            value = metric.get("value")
            if not isinstance(value, (int, float)):
                errors.append(f"{metric_path}.value must be numeric")

            provenance = metric.get("tool_provenance")
            if not isinstance(provenance, dict):
                errors.append(f"{metric_path}.tool_provenance must be an object")
            else:
                for field in required_provenance_fields:
                    val = provenance.get(field)
                    if not isinstance(val, str) or not val.strip():
                        errors.append(f"{metric_path}.tool_provenance.{field} must be non-empty")

        missing_labels = sorted(required_labels - seen_labels)
        if missing_labels:
            errors.append(
                f"taxonomy dimension {dim_name} missing labels: {', '.join(missing_labels)}"
            )

    return errors


def build_counting_taxonomy(project_root: Path) -> dict:
    src_root = project_root / "src"
    provider_metadata_path = project_root / PROVIDER_METADATA_REL_PATH
    extension_catalog_path = project_root / EXTENSION_CATALOG_REL_PATH

    raw_loc, logical_loc = count_rust_loc(src_root)
    canonical_providers, alias_providers, provider_families = count_provider_dimensions(
        provider_metadata_path
    )
    official_extensions, community_extensions, full_corpus_extensions = count_extension_dimensions(
        extension_catalog_path
    )
    taxonomy = {
        "schema": COUNTING_TAXONOMY_SCHEMA,
        "contract_ref": COUNTING_TAXONOMY_CONTRACT_REL_PATH,
        "dimensions": {
            "loc": {
                "comparison_mode": "side_by_side",
                "metrics": [
                    metric_row(
                        "rust_loc",
                        LOC_RAW_LABEL,
                        raw_loc,
                        "lines",
                        "src/**/*.rs",
                        "python3 scripts/ci/generate_parity_evidence.py --count-loc",
                    ),
                    metric_row(
                        "rust_loc",
                        LOC_LOGICAL_LABEL,
                        logical_loc,
                        "lines",
                        "src/**/*.rs",
                        "python3 scripts/ci/generate_parity_evidence.py --count-loc",
                    ),
                ],
            },
            "providers": {
                "comparison_mode": "side_by_side",
                "metrics": [
                    metric_row(
                        "provider_breadth",
                        PROVIDER_CANONICAL_LABEL,
                        canonical_providers,
                        "count",
                        PROVIDER_METADATA_REL_PATH,
                        "python3 scripts/ci/generate_parity_evidence.py --count-providers",
                    ),
                    metric_row(
                        "provider_breadth",
                        PROVIDER_ALIAS_LABEL,
                        alias_providers,
                        "count",
                        PROVIDER_METADATA_REL_PATH,
                        "python3 scripts/ci/generate_parity_evidence.py --count-providers",
                    ),
                    metric_row(
                        "provider_breadth",
                        PROVIDER_FAMILY_LABEL,
                        provider_families,
                        "count",
                        PROVIDER_METADATA_REL_PATH,
                        "python3 scripts/ci/generate_parity_evidence.py --count-providers",
                    ),
                ],
            },
            "extensions": {
                "comparison_mode": "side_by_side",
                "metrics": [
                    metric_row(
                        "extension_corpus",
                        EXTENSION_OFFICIAL_LABEL,
                        official_extensions,
                        "count",
                        EXTENSION_CATALOG_REL_PATH,
                        "python3 scripts/ci/generate_parity_evidence.py --count-extensions",
                    ),
                    metric_row(
                        "extension_corpus",
                        EXTENSION_COMMUNITY_LABEL,
                        community_extensions,
                        "count",
                        EXTENSION_CATALOG_REL_PATH,
                        "python3 scripts/ci/generate_parity_evidence.py --count-extensions",
                    ),
                    metric_row(
                        "extension_corpus",
                        EXTENSION_FULL_LABEL,
                        full_corpus_extensions,
                        "count",
                        EXTENSION_CATALOG_REL_PATH,
                        "python3 scripts/ci/generate_parity_evidence.py --count-extensions",
                    ),
                ],
            },
        },
    }
    contract = load_counting_taxonomy_contract(project_root)
    validation_errors = validate_counting_taxonomy(taxonomy, contract)
    if validation_errors:
        raise ValueError(
            "counting taxonomy validation failed: "
            + "; ".join(validation_errors)
        )
    return taxonomy


def build_evidence(suites: dict, project_root: Path) -> dict:
    """Build the evidence payload."""
    total_passed = sum(s["passed"] for s in suites.values())
    total_failed = sum(s["failed"] for s in suites.values())
    total_tests = sum(s["total"] for s in suites.values())

    all_pass = all(s["status"] == "pass" for s in suites.values())
    missing = [name for name in PARITY_SUITES if name not in suites]

    return {
        "schema": SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "verdict": "pass" if (all_pass and not missing) else "fail",
        "summary": {
            "suites_expected": len(PARITY_SUITES),
            "suites_found": len(suites),
            "suites_missing": missing,
            "total_passed": total_passed,
            "total_failed": total_failed,
            "total_tests": total_tests,
            "pass_rate_pct": round(
                100.0 * total_passed / total_tests, 2
            ) if total_tests > 0 else 0.0,
        },
        "suites": suites,
        "counting_taxonomy": build_counting_taxonomy(project_root),
    }


def synthetic_parity_log(
    failed_suite: str | None = None,
    omit_suite: str | None = None,
    *,
    colorized: bool = False,
) -> str:
    lines = []
    for suite in PARITY_SUITES:
        if suite == omit_suite:
            continue
        status = "FAILED" if suite == failed_suite else "ok"
        failed = 1 if suite == failed_suite else 0
        passed = 2 if suite == failed_suite else 3
        if colorized:
            lines.append(
                f"\x1b[1m\x1b[92m     Running\x1b[0m tests/{suite}.rs "
                f"(target/debug/deps/{suite}-abcdef)"
            )
        else:
            lines.append(f"Running tests/{suite}.rs (target/debug/deps/{suite}-abcdef)")
        lines.append(
            f"test result: {status}. {passed} passed; {failed} failed; "
            "0 ignored; 0 measured; 0 filtered out; finished in 0.01s"
        )
    return "\n".join(lines)


def run_self_test() -> int:
    failures: list[str] = []

    def require(condition: bool, message: str) -> None:
        if not condition:
            failures.append(message)

    project_root = project_root_from_script()
    all_pass_suites = parse_log(synthetic_parity_log())
    require(set(all_pass_suites) == set(PARITY_SUITES), "parser missed parity suites")
    require(
        all(suite["status"] == "pass" for suite in all_pass_suites.values()),
        "all-pass synthetic log should parse as pass",
    )

    colorized_suites = parse_log(synthetic_parity_log(colorized=True))
    require(
        set(colorized_suites) == set(PARITY_SUITES),
        "parser missed parity suites in a colorized cargo log",
    )
    require(
        all(suite["status"] == "pass" for suite in colorized_suites.values()),
        "colorized synthetic log should parse as pass",
    )

    all_pass_evidence = build_evidence(all_pass_suites, project_root)
    require(all_pass_evidence["verdict"] == "pass", "all-pass evidence should pass")
    require(
        all_pass_evidence["summary"]["suites_found"] == len(PARITY_SUITES),
        "all-pass evidence should find every suite",
    )
    require(all_pass_evidence["summary"]["total_passed"] == 15, "unexpected pass total")
    require(all_pass_evidence["summary"]["total_failed"] == 0, "unexpected fail total")
    require(
        all_pass_evidence["summary"]["pass_rate_pct"] == 100.0,
        "all-pass evidence should report 100% pass rate",
    )
    require(
        all_pass_evidence["counting_taxonomy"]["schema"] == COUNTING_TAXONOMY_SCHEMA,
        "evidence should include counting taxonomy",
    )

    failed_suites = parse_log(synthetic_parity_log(failed_suite="config_precedence"))
    failed_evidence = build_evidence(failed_suites, project_root)
    require(failed_evidence["verdict"] == "fail", "failed suite should fail evidence")
    require(
        failed_evidence["summary"]["total_failed"] == 1,
        "failed evidence should count one failed test",
    )
    require(
        failed_evidence["suites"]["config_precedence"]["status"] == "fail",
        "failed suite status should be preserved",
    )

    missing_suites = parse_log(synthetic_parity_log(omit_suite="vcr_parity_validation"))
    missing_evidence = build_evidence(missing_suites, project_root)
    require(missing_evidence["verdict"] == "fail", "missing suite should fail evidence")
    require(
        missing_evidence["summary"]["suites_missing"] == ["vcr_parity_validation"],
        "missing suite should be named in the summary",
    )

    contract = load_counting_taxonomy_contract(project_root)
    invalid_taxonomy = json.loads(json.dumps(all_pass_evidence["counting_taxonomy"]))
    provider_metrics = invalid_taxonomy["dimensions"]["providers"]["metrics"]
    invalid_taxonomy["dimensions"]["providers"]["metrics"] = [
        metric
        for metric in provider_metrics
        if metric.get("granularity_label") != PROVIDER_ALIAS_LABEL
    ]
    taxonomy_errors = validate_counting_taxonomy(invalid_taxonomy, contract)
    require(
        any(PROVIDER_ALIAS_LABEL in error for error in taxonomy_errors),
        "invalid taxonomy should report missing provider alias label",
    )

    if failures:
        print("Parity evidence self-test failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print("Parity evidence self-test passed.")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run deterministic in-memory checks without writing evidence artifacts",
    )
    parser.add_argument(
        "--output", required=False, help="Path for output JSON"
    )
    parser.add_argument(
        "--log", required=False, help="Path to cargo test log"
    )
    args = parser.parse_args(argv)

    if args.self_test:
        return run_self_test()

    if not args.output:
        print("ERROR: --output is required unless --self-test is used", file=sys.stderr)
        return 2
    if not args.log:
        print("ERROR: --log is required unless --self-test is used", file=sys.stderr)
        return 2

    log_path = Path(args.log)
    if not log_path.exists():
        print(f"ERROR: log file not found: {log_path}", file=sys.stderr)
        return 1

    log_text = log_path.read_text(encoding="utf-8", errors="replace")
    suites = parse_log(log_text)
    evidence = build_evidence(suites, project_root_from_script())

    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(
        json.dumps(evidence, indent=2) + "\n", encoding="utf-8"
    )

    print(f"Parity evidence: {output_path}")
    print(f"  Verdict: {evidence['verdict']}")
    print(f"  Suites: {evidence['summary']['suites_found']}/{evidence['summary']['suites_expected']}")
    print(f"  Tests: {evidence['summary']['total_passed']}/{evidence['summary']['total_tests']} passed")

    if evidence["summary"]["suites_missing"]:
        print(f"  Missing: {', '.join(evidence['summary']['suites_missing'])}")

    return 0 if evidence["verdict"] == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
