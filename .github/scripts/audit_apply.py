from pathlib import Path
import subprocess


def replace_required(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"{label}: expected text not found in {path}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")

# Deterministic parser for rust-code-analysis JSON. The tool reports each
# function/closure as a nested space; only those units are gated so large
# modules are not mistaken for single functions.
quality = Path("scripts/check_quality_metrics.py")
quality.parent.mkdir(parents=True, exist_ok=True)
quality.write_text(r'''#!/usr/bin/env python3
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
            # RCA has used both explicit function/closure kinds and nested
            # function spaces across versions. Exclude the file/unit aggregate;
            # accept any named non-unit space with function-level metrics.
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

# Production CI: normal Windows correctness/safety gate plus an independent
# static-metrics/unused-dependency gate. Full mutation testing is intentionally
# release-only because it recompiles/tests many mutants, but a release cannot
# publish unless every generated mutant is killed.
Path(".github/workflows/ci.yml").write_text(r'''name: CI

on:
  push:
    branches: [main, checkpoint]
    tags: ["v*"]
  pull_request:
  workflow_dispatch:
    inputs:
      release:
        description: "Release tag to create after a successful build ('none' = plain CI run, 'auto' = next patch, or type a tag like v0.2.0)"
        required: false
        default: "none"

permissions:
  contents: read

jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v7
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - name: Format check
        run: cargo fmt --all --check
      - name: Clippy
        run: cargo clippy --all-targets --locked -- -D warnings
      - name: Tests
        run: cargo test --locked
      - name: Build (release)
        run: cargo build --release --locked
      - name: Upload release exe
        uses: actions/upload-artifact@v7
        with:
          name: WinGlance-windows
          path: target/release/WinGlance.exe
          if-no-files-found: error
      - uses: taiki-e/install-action@cargo-audit
      - name: cargo-audit (advisories)
        run: cargo audit
      - uses: taiki-e/install-action@cargo-deny
      - name: cargo-deny (licenses, bans, advisories, sources)
        run: cargo deny check

  metrics:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Install pinned static-analysis tools
        run: |
          cargo install rust-code-analysis-cli --version 0.0.25 --locked
          cargo install cargo-machete --version 0.9.2 --locked
      - name: Complexity and Halstead thresholds
        run: |
          mkdir -p target/quality-metrics
          rust-code-analysis-cli -m -p src -O json -o target/quality-metrics
          python scripts/check_quality_metrics.py target/quality-metrics
      - name: Unused dependency check
        run: cargo machete

  mutation:
    if: ${{ github.event_name == 'workflow_dispatch' && inputs.release != 'none' }}
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v7
        with:
          persist-credentials: false
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Install pinned mutation runner
        run: cargo install cargo-mutants --version 27.1.0 --locked
      - name: Require zero surviving mutants
        run: cargo mutants --no-shuffle --timeout-multiplier 3 --jobs 2

  release:
    if: ${{ github.event_name == 'workflow_dispatch' && inputs.release != 'none' }}
    needs: [check, metrics, mutation]
    runs-on: ubuntu-latest
    permissions:
      contents: write
      actions: read
    steps:
      - uses: actions/checkout@v7
        with:
          fetch-depth: 0
          persist-credentials: false
      - name: Compute release tag
        id: compute
        shell: pwsh
        env:
          RELEASE_INPUT: ${{ inputs.release }}
        run: |
          $release = $env:RELEASE_INPUT
          if ($release -eq "auto") {
            git fetch --tags --force
            $latest = git tag --sort=-v:refname |
              Where-Object { $_ -match '^v\d+\.\d+\.\d+$' } |
              Select-Object -First 1
            if ($latest) {
              $m = [regex]::Match($latest, '^v(\d+)\.(\d+)\.(\d+)$')
              $tag = "v$($m.Groups[1].Value).$($m.Groups[2].Value).$([int]$m.Groups[3].Value + 1)"
            } else {
              $tag = "v1.0.0"
            }
          } elseif ($release -match '^v\d+\.\d+\.\d+$') {
            $tag = $release
          } else {
            Write-Error "invalid release input '$release': expected 'auto' or a vN.N.N tag"
            exit 1
          }
          Add-Content -Path $env:GITHUB_OUTPUT -Value "tag=$tag"
      - name: Download CI artifact
        uses: actions/download-artifact@v8
        with:
          name: WinGlance-windows
          path: staging
      - name: Stage release assets
        id: stage
        shell: pwsh
        run: |
          $fullSha = "${{ github.sha }}"
          $shortSha = $fullSha.Substring(0, [Math]::Min(7, $fullSha.Length))
          $stamp = [DateTime]::UtcNow.ToString("yyyyMMdd-HHmmss")
          $assetName = "WinGlance-$stamp-$shortSha.exe"
          Copy-Item staging/WinGlance.exe "staging/$assetName"
          $hash = Get-FileHash -Algorithm SHA256 "staging/$assetName"
          Set-Content -Path "staging/$assetName.sha256" -Value ($hash.Hash.ToLowerInvariant() + "  " + $assetName) -Encoding ascii
          Add-Content -Path $env:GITHUB_OUTPUT -Value "asset=$assetName"
      - name: Publish GitHub Release
        uses: softprops/action-gh-release@v3
        with:
          tag_name: ${{ steps.compute.outputs.tag }}
          target_commitish: ${{ github.sha }}
          files: |
            staging/${{ steps.stage.outputs.asset }}
            staging/${{ steps.stage.outputs.asset }}.sha256
            config.example.toml
          generate_release_notes: true
''', encoding="utf-8")

# Documentation reconciliation.
replace_required(
    "docs/configuration.md",
    "A file that *parses* but holds a single bad value inside one section — e.g. `layout = \"bogus\"` or `duration_ms = \"fast\"` under `[overlay]` — only resets that section to its defaults: sibling sections (`[behavior]`, `[appearance]`) and top-level unknown keys survive, and the file remains persistable. The bad section is `warn!`ed per section (`config [overlay] invalid …; using defaults for [overlay]`) and is corrected to the canonical defaults on the next successful save.",
    "A file that *parses* but holds a single bad value inside one section — e.g. `layout = \"bogus\"` or `duration_ms = \"fast\"` under `[overlay]` — resets that section to defaults in memory while valid sibling sections (`[behavior]`, `[appearance]`) and top-level unknown keys survive. Because this build cannot faithfully round-trip the invalid section, persistence is disabled for the run: the original file stays byte-identical and Settings shows the persistence warning banner. The bad section is `warn!`ed per section (`config [overlay] invalid …; using defaults for [overlay]`).",
    "typed-invalid config persistence",
)
replace_required(
    "docs/development.md",
    "│       └── ci.yml          fmt / clippy / test / build / cargo-deny on push/PR",
    "│       └── ci.yml          fmt / clippy / test / build / audit / metrics on push/PR",
    "development CI tree",
)
replace_required(
    "docs/development.md",
    "Launching from the Start menu (or at logon) surfaces only the tray icon and\nthe always-visible pill — no window, no console, no dialogs.",
    "On a genuine first-ever launch, WinGlance opens the tracking window once so the user can review Settings. Later Start-menu launches and logon starts surface only the tray icon and always-visible pill — no console or dialogs — unless the user explicitly opens the window from the tray.",
    "development startup behavior",
)
replace_required(
    "docs/development.md",
    "the overlay's cap-3 track cache (indefinite retention)",
    "the overlay's cap-8 track cache (indefinite retention, LRU-bounded)",
    "development track cache bound",
)
replace_required(
    "README.md",
    "`ci.yml` runs format/lint/test/release-build/cargo-deny on every push and PR, and\npublishes a GitHub Release (with the built exe and example config) only when manually\ndispatched from the Actions tab.",
    "`ci.yml` runs the Windows format/lint/test/release-build/audit/deny gate plus a pinned Rust static-metrics and unused-dependency gate on pushes to `main`/`checkpoint` and on pull requests. Cyclomatic and cognitive complexity must each stay below 22 per function/closure, and Halstead difficulty below 80. A manually dispatched release additionally runs pinned `cargo-mutants` and refuses to publish if any mutant survives. See `docs/quality.md` for what is enforced and which cross-language metrics are not meaningful for this Rust codebase.",
    "README CI wording",
)

Path("docs/quality.md").write_text(r'''# Quality gates

WinGlance treats the release checks as executable policy rather than a prose target.

## Enforced on normal CI

- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`
- `cargo build --release --locked`
- `cargo audit`
- `cargo deny check`
- `cargo machete` 0.9.2: no unused manifest dependencies
- `rust-code-analysis-cli` 0.0.25, checked per function/closure by
  `scripts/check_quality_metrics.py`:
  - cyclomatic complexity **< 22**
  - cognitive complexity **< 22**
  - Halstead difficulty **< 80**

The metrics job is separate from the Windows build job so a static-analysis-tool failure
cannot be mistaken for a compiler/test result.

## Enforced before a GitHub Release

A release dispatch must also pass `cargo-mutants` 27.1.0 with deterministic ordering.
The command exits non-zero when any viable mutant survives, so the release job cannot
publish with a non-zero surviving-mutant count.

## Audit targets that are not fabricated

The external audit also named **CRAP < 25** and zero `any`/`unknown` types. Those are
not reported as fake green checks:

- `any` / `unknown` are TypeScript type-system escape hatches and have no Rust analogue.
  Rust's corresponding hygiene is enforced by the compiler and Clippy warnings-as-errors.
- Function-level CRAP requires trustworthy function-level coverage mapped to the same
  function identities used by the complexity analyzer. The current Windows Rust CI stack
  does not provide a stable mapping that would justify presenting a CRAP number as a
  release gate. Complexity is therefore gated directly and behavior is independently
  pressure-tested by the zero-surviving-mutants release gate.

If a future coverage tool supplies stable function identities, add CRAP as an actual gate;
do not derive a decorative number from file-level coverage.
''', encoding="utf-8")

# Keep the historical audit intact but make its corrected count and post-audit disposition
# unambiguous. DATA-001 and SINGLE-001 were explicitly accepted by the maintainer.
a = Path("Analysis.md")
analysis = a.read_text(encoding="utf-8")
analysis = analysis.replace(
    "I found **one Critical, three High, ten Medium, and three Low** findings.",
    "I found **one Critical, three High, ten Medium, and two Low** findings.",
    1,
)
status = r'''

> **Post-audit implementation note (checkpoint):** The findings below describe the immutable audited head and are retained as historical evidence. The production-readiness implementation subsequently closed START-001, OVERLAY-001, DEDUP-001, A11Y-001/002/003, HIST-001, TOOLTIP-001, MON-001, HOVER-001, and CI-001. DATA-001 is an explicitly accepted developer-tool-only `-FreshInstall` behavior and was not changed at the maintainer's direction. SINGLE-001 remains the documented fail-closed same-user availability tradeoff: preventing dual config/log writers takes priority over attempting to defeat a same-user process that can already deny service in other ways. DOC-001/002/003 were reconciled with the implemented behavior. The corrected finding total is **1 Critical, 3 High, 10 Medium, 2 Low = 16**.
'''
if "Post-audit implementation note (checkpoint)" not in analysis:
    marker = "## Executive verdict\n"
    if marker not in analysis:
        raise SystemExit("Analysis executive verdict marker missing")
    analysis = analysis.replace(marker, status + "\n" + marker, 1)
a.write_text(analysis, encoding="utf-8")

# Remove temporary audit/verification scaffolding from the delivered tree.
for tmp in [Path(".github/.noop-ci"), *Path(".github").glob(".audit-trigger*")]:
    if tmp.exists():
        tmp.unlink()
Path(".github/scripts/audit_apply.py").unlink()
try:
    Path(".github/scripts").rmdir()
except OSError:
    pass

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "add", "-A"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "commit", "-m", "chore(release): enforce quality gates and reconcile docs"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
