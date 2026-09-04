# Quality gates

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
