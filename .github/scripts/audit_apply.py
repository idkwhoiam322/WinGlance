from pathlib import Path
import subprocess


def replace_required(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"{label}: expected text not found in {path}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")

quality = Path("scripts/check_quality_metrics.py")
quality.parent.mkdir(parents=True, exist_ok=True)
quality.write_text('''#!/usr/bin/env python3
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
''', encoding="utf-8")

Path("docs/quality.md").write_text('''# Quality gates

WinGlance treats release checks as executable policy rather than a prose target.

## Enforced on normal CI

- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`
- `cargo build --release --locked`
- `cargo audit`
- `cargo deny check`
- `cargo machete` 0.9.2: no unused manifest dependencies
- `rust-code-analysis-cli` 0.0.25, checked per function/closure by `scripts/check_quality_metrics.py`:
  - cyclomatic complexity **< 22**
  - cognitive complexity **< 22**
  - Halstead difficulty **< 80**

The metrics job is separate from the Windows build job so a static-analysis-tool failure cannot be mistaken for a compiler/test result.

## Enforced before a GitHub Release

A release dispatch must also pass `cargo-mutants` 27.1.0 with deterministic ordering. The command exits non-zero when any viable mutant survives, so the release job cannot publish with a non-zero surviving-mutant count.

## Audit targets that are not fabricated

The external audit also named **CRAP < 25** and zero `any`/`unknown` types. Those are not reported as fake green checks:

- `any` / `unknown` are TypeScript type-system escape hatches and have no Rust analogue. Rust's corresponding hygiene is enforced by the compiler and Clippy warnings-as-errors.
- Function-level CRAP requires trustworthy function-level coverage mapped to the same function identities used by the complexity analyzer. The current Windows Rust CI stack does not provide a stable mapping that would justify presenting a CRAP number as a release gate. Complexity is therefore gated directly and behavior is independently pressure-tested by the zero-surviving-mutants release gate.

If a future coverage tool supplies stable function identities, add CRAP as an actual gate; do not derive a decorative number from file-level coverage.
''', encoding="utf-8")

replace_required(
    "docs/configuration.md",
    "A file that *parses* but holds a single bad value inside one section — e.g. `layout = \"bogus\"` or `duration_ms = \"fast\"` under `[overlay]` — only resets that section to its defaults: sibling sections (`[behavior]`, `[appearance]`) and top-level unknown keys survive, and the file remains persistable. The bad section is `warn!`ed per section (`config [overlay] invalid …; using defaults for [overlay]`) and is corrected to the canonical defaults on the next successful save.",
    "A file that *parses* but holds a single bad value inside one section — e.g. `layout = \"bogus\"` or `duration_ms = \"fast\"` under `[overlay]` — resets that section to defaults in memory while valid sibling sections (`[behavior]`, `[appearance]`) and top-level unknown keys survive. Because this build cannot faithfully round-trip the invalid section, persistence is disabled for the run: the original file stays byte-identical and Settings shows the persistence warning banner. The bad section is `warn!`ed per section (`config [overlay] invalid …; using defaults for [overlay]`).",
    "typed-invalid config persistence",
)
replace_required("docs/development.md", "│       └── ci.yml          fmt / clippy / test / build / cargo-deny on push/PR", "│       └── ci.yml          fmt / clippy / test / build / audit / metrics on push/PR", "development CI tree")
replace_required("docs/development.md", "Launching from the Start menu (or at logon) surfaces only the tray icon and\nthe always-visible pill — no window, no console, no dialogs.", "On a genuine first-ever launch, WinGlance opens the tracking window once so the user can review Settings. Later Start-menu launches and logon starts surface only the tray icon and always-visible pill — no console or dialogs — unless the user explicitly opens the window from the tray.", "development startup behavior")
replace_required("docs/development.md", "the overlay's cap-3 track cache (indefinite retention)", "the overlay's cap-8 track cache (indefinite retention, LRU-bounded)", "development track cache bound")
replace_required(
    "README.md",
    "`ci.yml` runs format/lint/test/release-build/cargo-deny on every push and PR, and\npublishes a GitHub Release (with the built exe and example config) only when manually\ndispatched from the Actions tab.",
    "`ci.yml` runs the Windows format/lint/test/release-build/audit/deny gate plus a pinned Rust static-metrics and unused-dependency gate on pushes to `main`/`checkpoint` and on pull requests. Cyclomatic and cognitive complexity must each stay below 22 per function/closure, and Halstead difficulty below 80. A manually dispatched release additionally runs pinned `cargo-mutants` and refuses to publish if any mutant survives. See `docs/quality.md` for what is enforced and which cross-language metrics are not meaningful for this Rust codebase.",
    "README CI wording",
)

a = Path("Analysis.md")
analysis = a.read_text(encoding="utf-8")
analysis = analysis.replace("I found **one Critical, three High, ten Medium, and three Low** findings.", "I found **one Critical, three High, ten Medium, and two Low** findings.", 1)
if "Post-audit implementation note (checkpoint)" not in analysis:
    marker = "## Executive verdict\n"
    note = "\n> **Post-audit implementation note (checkpoint):** The findings below describe the immutable audited head and are retained as historical evidence. The production-readiness implementation subsequently closed START-001, OVERLAY-001, DEDUP-001, A11Y-001/002/003, HIST-001, TOOLTIP-001, MON-001, HOVER-001, and CI-001. DATA-001 is an explicitly accepted developer-tool-only `-FreshInstall` behavior and was not changed at the maintainer's direction. SINGLE-001 remains the documented fail-closed same-user availability tradeoff. DOC-001/002/003 were reconciled with the implemented behavior. The corrected finding total is **1 Critical, 3 High, 10 Medium, 2 Low = 16**.\n\n"
    analysis = analysis.replace(marker, note + marker, 1)
a.write_text(analysis, encoding="utf-8")

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "add", "Analysis.md", "README.md", "docs/configuration.md", "docs/development.md", "docs/quality.md", "scripts/check_quality_metrics.py"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "commit", "-m", "docs: reconcile audit behavior and quality policy"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
