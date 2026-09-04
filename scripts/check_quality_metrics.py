#!/usr/bin/env python3
import json
import pathlib
import sys

CC_LIMIT = 22.0
COGNITIVE_LIMIT = 22.0
HALSTEAD_DIFFICULTY_LIMIT = 80.0


def iter_dicts(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from iter_dicts(child)
    elif isinstance(value, list):
        for child in value:
            yield from iter_dicts(child)


def metric_value(metrics, name):
    value = metrics.get(name)
    if isinstance(value, dict):
        for key in ("sum", "max", "average"):
            candidate = value.get(key)
            if isinstance(candidate, (int, float)):
                return float(candidate)
    if isinstance(value, (int, float)):
        return float(value)
    return None


def main():
    root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "target/quality-metrics")
    files = sorted(root.rglob("*.json"))
    if not files:
        raise SystemExit(f"no rust-code-analysis JSON files found under {root}")
    checked = 0
    violations = []
    for path in files:
        data = json.loads(path.read_text(encoding="utf-8"))
        for unit in iter_dicts(data):
            metrics = unit.get("metrics")
            if not isinstance(metrics, dict):
                continue
            kind = str(unit.get("kind", "")).lower()
            if kind in {"unit", "file", ""}:
                continue
            cc = metric_value(metrics, "cyclomatic")
            cognitive = metric_value(metrics, "cognitive")
            halstead = metrics.get("halstead")
            difficulty = None
            if isinstance(halstead, dict) and isinstance(halstead.get("difficulty"), (int, float)):
                difficulty = float(halstead["difficulty"])
            if cc is None and cognitive is None and difficulty is None:
                continue
            checked += 1
            name = unit.get("name") or f"{path.name}:{unit.get('start_line', '?')}"
            if cc is not None and cc >= CC_LIMIT:
                violations.append(f"{name}: cyclomatic {cc:g} >= {CC_LIMIT:g}")
            if cognitive is not None and cognitive >= COGNITIVE_LIMIT:
                violations.append(f"{name}: cognitive {cognitive:g} >= {COGNITIVE_LIMIT:g}")
            if difficulty is not None and difficulty >= HALSTEAD_DIFFICULTY_LIMIT:
                violations.append(f"{name}: Halstead difficulty {difficulty:g} >= {HALSTEAD_DIFFICULTY_LIMIT:g}")
    if checked == 0:
        raise SystemExit("rust-code-analysis output contained no function/closure metric spaces")
    print(f"quality metrics: checked {checked} function/closure spaces")
    if violations:
        print("quality metric violations:")
        for violation in violations:
            print(f"  - {violation}")
        raise SystemExit(1)
    print("quality metrics pass: cyclomatic <22, cognitive <22, Halstead difficulty <80")


if __name__ == "__main__":
    main()
