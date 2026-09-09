#!/usr/bin/env python3
import json
import pathlib
import sys
from collections import defaultdict

CC_LIMIT = 22.0
COGNITIVE_LIMIT = 22.0
HALSTEAD_DIFFICULTY_LIMIT = 80.0
FUNCTION_KINDS = {"function", "closure", "method"}
EPSILON = 1e-9


def iter_dicts(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from iter_dicts(child)
    elif isinstance(value, list):
        for child in value:
            yield from iter_dicts(child)


def metric_max(metrics, name):
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


def source_metric_path(path, root):
    """Return a scan-root-independent `src/...json` identity.

    rust-code-analysis preserves the scanned input path beneath its output
    directory. The current scan therefore emits `src/...`, while an archived
    baseline scanned from `target/quality-baseline/src` emits that longer
    prefix. Comparing the raw output paths would falsely classify unchanged
    legacy functions as new debt. Anchor both forms at the final `src/`
    component instead.
    """
    rel = path.relative_to(root).as_posix()
    if rel.startswith("src/"):
        return rel
    marker = "/src/"
    if marker in rel:
        return "src/" + rel.rsplit(marker, 1)[1]
    return rel


def collect(root):
    root = pathlib.Path(root)
    files = sorted(root.rglob("*.json"))
    if not files:
        raise SystemExit(f"no rust-code-analysis JSON files found under {root}")
    # A few Rust constructs produce multiple nested RCA spaces with the same
    # source name. Aggregate by file/name and retain the worst value; this is
    # stable under harmless line movement while remaining conservative.
    values = defaultdict(lambda: {"cyclomatic": 0.0, "cognitive": 0.0, "halstead": 0.0})
    checked = 0
    seen_kinds = set()
    for path in files:
        rel = source_metric_path(path, root)
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
            name = str(unit.get("name") or "<anonymous>")
            cc = metric_max(metrics, "cyclomatic") or 0.0
            cognitive = metric_max(metrics, "cognitive") or 0.0
            difficulty = halstead_difficulty(metrics) or 0.0
            checked += 1
            slot = values[(rel, name)]
            slot["cyclomatic"] = max(slot["cyclomatic"], cc)
            slot["cognitive"] = max(slot["cognitive"], cognitive)
            slot["halstead"] = max(slot["halstead"], difficulty)
    if checked == 0:
        kinds = ", ".join(sorted(seen_kinds)) or "none"
        raise SystemExit(
            "rust-code-analysis output contained no function/closure metric spaces; "
            f"observed kinds: {kinds}"
        )
    return values, checked


def check_metric(violations, key, label, current, baseline, limit):
    if current < limit:
        return
    if baseline is None or baseline < limit:
        violations.append(f"{key}: new {label} violation {current:g} >= {limit:g}")
        return
    if current > baseline + EPSILON:
        violations.append(
            f"{key}: {label} regressed {baseline:g} -> {current:g} (target < {limit:g})"
        )


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: check_quality_metrics.py CURRENT_DIR BASELINE_DIR")
    current, checked = collect(sys.argv[1])
    baseline, _ = collect(sys.argv[2])
    violations = []
    debt = 0
    for key, metrics in sorted(current.items()):
        old = baseline.get(key)
        check_metric(
            violations,
            key,
            "cyclomatic",
            metrics["cyclomatic"],
            None if old is None else old["cyclomatic"],
            CC_LIMIT,
        )
        check_metric(
            violations,
            key,
            "cognitive",
            metrics["cognitive"],
            None if old is None else old["cognitive"],
            COGNITIVE_LIMIT,
        )
        check_metric(
            violations,
            key,
            "Halstead difficulty",
            metrics["halstead"],
            None if old is None else old["halstead"],
            HALSTEAD_DIFFICULTY_LIMIT,
        )
        if (
            metrics["cyclomatic"] >= CC_LIMIT
            or metrics["cognitive"] >= COGNITIVE_LIMIT
            or metrics["halstead"] >= HALSTEAD_DIFFICULTY_LIMIT
        ):
            debt += 1

    print(f"quality metrics: checked {checked} function/closure spaces")
    print(f"quality metrics: {debt} grandfathered function-name groups remain above a target")
    if violations:
        print("quality metric regressions/new violations:")
        for violation in violations:
            print(f"  - {violation}")
        raise SystemExit(1)
    print(
        "quality metrics pass: no new or worsened complexity debt; "
        "targets remain cyclomatic <22, cognitive <22, Halstead difficulty <80"
    )


if __name__ == "__main__":
    main()
