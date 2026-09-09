#!/usr/bin/env python3
"""Validate WinGlance's deterministic cargo-mutants core by function metadata.

cargo-mutants 27.1.0 can over-include struct-literal field-deletion mutants even
when --re filters are supplied. This checker therefore treats the engine's
function metadata as the source of truth for scope and verdicts.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

ALLOWED_FILES = {
    "src/main.rs",
    "src/events.rs",
    "src/overlay/mod.rs",
    "src/overlay/fullscreen.rs",
}

EXACT_FUNCTIONS = {
    "enforce_startup_policy",
    "OverlayState::idle_status_title",
    "OverlayState::idle_event",
    "OverlayState::content_playing",
    "hover_capped_deadline",
    "hover_restored_deadline",
    "resolve_target_persisted",
    "refresh_identity_slot",
}

GOOD_SUMMARIES = {"CaughtMutant", "Unviable"}
BAD_SUMMARIES = {"MissedMutant", "Timeout"}


def is_core_function(name: str) -> bool:
    return (
        name in EXACT_FUNCTIONS
        or name == "TrackInfo::same_media"
        or name.startswith("TrackInfo::same_media::")
    )


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, json.JSONDecodeError) as exc:
        raise SystemExit(f"{path}: could not read valid JSON: {exc}") from exc


def mutant_function(mutant: dict[str, Any]) -> str:
    function = mutant.get("function")
    if not isinstance(function, dict):
        return ""
    name = function.get("function_name")
    return name if isinstance(name, str) else ""


def selected_mutants(raw: Any) -> list[dict[str, Any]]:
    if not isinstance(raw, list):
        raise SystemExit("mutant listing must be a JSON array")
    selected: list[dict[str, Any]] = []
    for item in raw:
        if not isinstance(item, dict):
            continue
        if is_core_function(mutant_function(item)):
            selected.append(item)
    return selected


def validate_selection(selected: list[dict[str, Any]]) -> list[str]:
    if not selected:
        raise SystemExit("deterministic-core selector matched zero mutants")
    names: list[str] = []
    for mutant in selected:
        name = mutant.get("name")
        file_name = mutant.get("file")
        if not isinstance(name, str) or not name:
            raise SystemExit("selected mutant is missing a stable name")
        if file_name not in ALLOWED_FILES:
            raise SystemExit(
                f"deterministic-core selector escaped the file allowlist: {file_name!r}: {name}"
            )
        names.append(name)
    if len(names) != len(set(names)):
        raise SystemExit("deterministic-core mutant names are not unique")
    return names


def cmd_list(listing_path: Path, output_path: Path) -> int:
    selected = selected_mutants(load_json(listing_path))
    names = validate_selection(selected)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text("\n".join(names) + "\n", encoding="utf-8")
    function_names = sorted({mutant_function(m) for m in selected})
    print(f"Focused mutation scope contains {len(names)} mutants")
    print("Focused function metadata:")
    for name in function_names:
        print(f"  - {name}")
    return 0


def cmd_outcomes(listing_path: Path, outcomes_path: Path) -> int:
    selected = selected_mutants(load_json(listing_path))
    expected_names = validate_selection(selected)
    expected = set(expected_names)

    raw_outcomes = load_json(outcomes_path)
    if not isinstance(raw_outcomes, dict) or not isinstance(raw_outcomes.get("outcomes"), list):
        raise SystemExit("outcomes JSON must contain an 'outcomes' array")

    selected_outcomes: dict[str, str] = {}
    for entry in raw_outcomes["outcomes"]:
        if not isinstance(entry, dict):
            continue
        scenario = entry.get("scenario")
        if not isinstance(scenario, dict):
            continue
        mutant = scenario.get("Mutant")
        if not isinstance(mutant, dict):
            continue
        if not is_core_function(mutant_function(mutant)):
            continue
        name = mutant.get("name")
        summary = entry.get("summary")
        if not isinstance(name, str) or not isinstance(summary, str):
            raise SystemExit("selected outcome is missing mutant name or summary")
        selected_outcomes[name] = summary

    observed = set(selected_outcomes)
    missing = sorted(expected - observed)
    unexpected = sorted(observed - expected)
    if missing or unexpected:
        if missing:
            print("Missing deterministic-core outcomes:", file=sys.stderr)
            for name in missing:
                print(f"  - {name}", file=sys.stderr)
        if unexpected:
            print("Unexpected deterministic-core outcomes:", file=sys.stderr)
            for name in unexpected:
                print(f"  - {name}", file=sys.stderr)
        return 1

    counts: dict[str, int] = {}
    bad: list[tuple[str, str]] = []
    unknown: list[tuple[str, str]] = []
    for name in expected_names:
        summary = selected_outcomes[name]
        counts[summary] = counts.get(summary, 0) + 1
        if summary in BAD_SUMMARIES:
            bad.append((summary, name))
        elif summary not in GOOD_SUMMARIES:
            unknown.append((summary, name))

    print(
        "Deterministic-core mutation verdict: "
        + ", ".join(f"{key}={counts[key]}" for key in sorted(counts))
    )

    if bad:
        print("Surviving deterministic-core mutants:", file=sys.stderr)
        for summary, name in bad:
            print(f"  - {summary}: {name}", file=sys.stderr)
    if unknown:
        print("Unrecognized deterministic-core outcomes:", file=sys.stderr)
        for summary, name in unknown:
            print(f"  - {summary}: {name}", file=sys.stderr)

    if bad or unknown:
        return 1

    print(f"Zero surviving viable deterministic-core mutants ({len(expected_names)} evaluated).")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)

    list_parser = sub.add_parser("list")
    list_parser.add_argument("listing", type=Path)
    list_parser.add_argument("output", type=Path)

    outcomes_parser = sub.add_parser("outcomes")
    outcomes_parser.add_argument("listing", type=Path)
    outcomes_parser.add_argument("outcomes", type=Path)

    args = parser.parse_args()
    if args.command == "list":
        return cmd_list(args.listing, args.output)
    return cmd_outcomes(args.listing, args.outcomes)


if __name__ == "__main__":
    raise SystemExit(main())
