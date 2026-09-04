#!/usr/bin/env python3
import json
import pathlib
import sys

CC_LIMIT = 22.0
COGNITIVE_LIMIT = 22.0
HALSTEAD_DIFFICULTY_LIMIT = 80.0
FUNCTION_KINDS = {"function", "closure", "method"}


def iter_dicts(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from iter_dicts(child)
    elif isinstance(value, list):
        for child in value:
            yield from iter_dicts(child)


def metric_max(metrics, name):
    """Return RCA's maximum value for one function-space metric.

    rust-code-analysis stats are hierarchical. `sum` includes nested spaces
    and therefore over-counts a function that owns closures (and whole impl
    containers). The audit threshold is per function/closure, so `max` is the
    correct statistic for a function space.
    """
    value = metrics.get(name)
    if isinstance(value, dict):
        candidate = value.get("max")
        if isinstance(candidate, (int, float)):
            return float(candidate)
        candidate = value.get("average")
        if isinstance(candidate, (int, float)):
            return float(candidate)
    if isinstance(value, (int, float)):
        return float(value)
    return None


def halstead_difficulty(metrics):
    halstead = metrics.get("halstead")
    if not isinstance(halstead, dict):
        return None
    difficulty = halstead.get("difficulty")
    if isinstance(difficulty, (int, float)):
        return float(difficulty)
    if isinstance(difficulty, dict):
        candidate = difficulty.get("max")
        if isinstance(candidate, (int, float)):
            return float(candidate)
    return None


def main():
    root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "target/quality-metrics")
    files = sorted(root.rglob("*.json"))
    if not files:
        raise SystemExit(f"no rust-code-analysis JSON files found under {root}")

    checked = 0
    violations = []
    seen_kinds = set()
    for path in files:
        data = json.loads(path.read_text(encoding="utf-8"))
        for unit in iter_dicts(data):
            metrics = unit.get("metrics")
            if not isinstance(metrics, dict):
                continue
            kind = str(unit.get("kind", "")).lower()
            if kind:
                seen_kinds.add(kind)
            if kind not in FUNCTION_KINDS:
                continue
            cc = metric_max(metrics, "cyclomatic")
            cognitive = metric_max(metrics, "cognitive")
            difficulty = halstead_difficulty(metrics)
            if cc is None and cognitive is None and difficulty is None:
                continue
            checked += 1
            name = unit.get("name") or f"{path.name}:{unit.get('start_line', '?')}"
            location = f"{path.name}:{unit.get('start_line', '?')}"
            if cc is not None and cc >= CC_LIMIT:
                violations.append(f"{location} {name}: cyclomatic {cc:g} >= {CC_LIMIT:g}")
            if cognitive is not None and cognitive >= COGNITIVE_LIMIT:
                violations.append(f"{location} {name}: cognitive {cognitive:g} >= {COGNITIVE_LIMIT:g}")
            if difficulty is not None and difficulty >= HALSTEAD_DIFFICULTY_LIMIT:
                violations.append(
                    f"{location} {name}: Halstead difficulty {difficulty:g} >= {HALSTEAD_DIFFICULTY_LIMIT:g}"
                )

    if checked == 0:
        kinds = ", ".join(sorted(seen_kinds)) or "none"
        raise SystemExit(
            "rust-code-analysis output contained no function/closure metric spaces; "
            f"observed kinds: {kinds}"
        )
    print(f"quality metrics: checked {checked} function/closure spaces")
    if violations:
        print("quality metric violations:")
        for violation in violations:
            print(f"  - {violation}")
        raise SystemExit(1)
    print("quality metrics pass: cyclomatic <22, cognitive <22, Halstead difficulty <80")


if __name__ == "__main__":
    main()
