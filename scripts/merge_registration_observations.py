#!/usr/bin/env python3
"""Merge runtime registration observations into VALIDATED_MANIFEST.json.

Reads the JSONL oracle artifact emitted by conformance_must_pass_gate with
PI_DUMP_REGISTRATION_OBSERVATIONS=1 (tests/ext_conformance_generated.rs::
maybe_dump_registration_observation) and updates each referenced manifest
entry's registrations.* identity sets plus the derived registers_* capability
booleans so the manifest describes what the executed entry point actually
registers (bd-sog97.29).

Dry-run by default; pass --apply to write. Never touches entries absent from
the observations file.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

SET_FIELDS = ("commands", "flags", "tools", "providers", "event_handlers")
CAPABILITY_FOR_FIELD = {
    "tools": "registers_tools",
    "commands": "registers_commands",
    "flags": "registers_flags",
    "providers": "registers_providers",
}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--observations",
        type=Path,
        default=Path("tests/ext_conformance/reports/registration_observations.jsonl"),
    )
    parser.add_argument(
        "--manifest",
        type=Path,
        default=Path("tests/ext_conformance/VALIDATED_MANIFEST.json"),
    )
    parser.add_argument(
        "--ids-from",
        type=Path,
        help="optional file of extension ids to include (one per line); "
        "apply only triaged true-drift entries while deferring "
        "environment-gap failures",
    )
    args = parser.parse_args()

    manifest_path = args.manifest
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    by_id = {entry["id"]: entry for entry in manifest["extensions"]}

    observations: dict[str, dict] = {}
    for line_number, line in enumerate(
        args.observations.read_text(encoding="utf-8").splitlines(), start=1
    ):
        if not line.strip():
            continue
        record = json.loads(line)
        observations[record["id"]] = record  # last wins

    changed = 0
    for ext_id, observed in sorted(observations.items()):
        entry = by_id.get(ext_id)
        if entry is None:
            print(f"SKIP (not in manifest): {ext_id}")
            continue
    if args.ids_from:
        allowed = {
            line.strip()
            for line in args.ids_from.read_text(encoding="utf-8").splitlines()
            if line.strip()
        }
        observations = {
            ext_id: record
            for ext_id, record in observations.items()
            if ext_id in allowed
        }
    changed = 0
        capabilities = entry.setdefault("capabilities", {})
        diffs: list[str] = []
        for field in SET_FIELDS:
            new_values = sorted(observed.get(field) or [])
            if field == "providers" and "providers" not in registrations:
                # Manifest schema for this tier stores provider surface via
                # capability only; skip list sync when no list field exists.
                old_values: list[str] | None = None
            else:
                old_values = [str(v) for v in registrations.get(field, [])]
            if old_values is not None and old_values != new_values:
                diffs.append(f"{field}: {old_values} -> {new_values}")
                registrations[field] = new_values
            capability = CAPABILITY_FOR_FIELD.get(field)
            if capability:
                desired = bool(new_values)
                if capabilities.get(capability) != desired:
                    diffs.append(f"{capability}: {capabilities.get(capability)} -> {desired}")
                    capabilities[capability] = desired
        if "subscribes_events" in capabilities:
            new_events = sorted(observed.get("event_handlers") or [])
            old_events = [str(v) for v in capabilities["subscribes_events"]]
            if old_events != new_events:
                diffs.append(f"subscribes_events: {old_events} -> {new_events}")
                capabilities["subscribes_events"] = new_events
        if diffs:
            changed += 1
            print(f"{ext_id}:")
            for diff in diffs:
                print(f"  {diff}")

    print(f"\n{changed} entrie(s) would change; {len(observations)} observation(s) read.")
    if args.apply:
        manifest_path.write_text(
            json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
        )
        print(f"Wrote {manifest_path}")
    elif changed:
        print("Dry run: re-run with --apply to write.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
