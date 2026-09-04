# Release quality gates

WinGlance uses two classes of release checks: Windows correctness/safety checks and repository-quality ratchets. The workflow lives in `.github/workflows/ci.yml` and analysis/mutation tools are pinned to explicit versions.

## Windows correctness gate

Every normal CI run executes on Windows and must pass:

- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`
- `cargo build --release --locked`
- release-executable artifact upload
- `cargo audit`
- `cargo deny check`

The release build and tests run in GitHub Actions; this audit/remediation workflow does not launch `WinGlance.exe` or GUI helper programs.

## Complexity and dependency ratchet

`rust-code-analysis-cli` 0.0.25 measures Rust function/method/closure spaces. The targets are:

- cyclomatic complexity `< 22`
- cognitive complexity `< 22`
- Halstead Difficulty `< 80`

The codebase contains mature Win32 dispatcher/rendering functions that are already above one or more targets. Refactoring those functions only to make a metric green would be a large behavior-risking rewrite, so CI uses a monotonic ratchet rather than pretending that historical debt does not exist.

The ratchet baseline is commit `601818d34500201f04d37aa23fc5d085e09c9cfd`, the last fully verified behavioral-remediation tree. For every measured function-name group:

1. a function below a target at the baseline must remain below it;
2. a function already above a target may not become worse;
3. a new function may not introduce an over-target value;
4. improvements are always accepted, so legacy debt can only stay level or decrease.

`scripts/check_quality_metrics.py` performs that comparison using per-space maxima. It normalizes scan paths to `src/...` so the live tree and archived baseline have stable identities, and ignores aggregate type/`impl` containers that would otherwise double-count nested functions and closures.

`cargo-machete` 0.9.2 also runs on every metrics job. `windows-core` is the sole declared scanner exception: `windows::core::implement!` expands to generated code that directly names `::windows_core`, which a source-only dependency scanner cannot observe. The direct dependency is required for that macro expansion to compile.

## Mutation gate

A release dispatch must also pass `cargo-mutants` 27.1.0 on Windows with no surviving viable mutants. The release job depends on the Windows check, metrics check, and mutation job, so a release cannot publish while any of them fails.

For maintenance verification on `checkpoint`, a commit whose message contains `[mutation]` also runs the mutation job without creating a release. This is intentionally opt-in because a full mutation pass is substantially more expensive than ordinary CI.

## Metrics that are not fabricated

CRAP `< 25` requires trustworthy function-level coverage joined to the same stable function identity used for complexity. The current Windows Rust toolchain in this repository does not provide that mapping reliably enough to make a defensible hard gate, so CI does not report a synthetic CRAP number.

Likewise, JavaScript/TypeScript-style `any`/`unknown` counts are not meaningful Rust metrics. Rust's relevant escape hatches are instead covered by the existing `unsafe` confinement review, Clippy warnings-as-errors, typed APIs, tests, dependency policy, and mutation testing.

These limitations are documented rather than converted into misleading zero-valued badges.
