<#
.SYNOPSIS
    Builds the standalone WinGlance.exe binary.

.DESCRIPTION
    Optional throttling, format and all-target checks, a locked Cargo build,
    hard-gated advisory checks (cargo-audit + cargo-deny; escape with
    -SkipAudit), and optional restart of an already-running process.

.PARAMETER Clean
    Remove target before building.
.PARAMETER Release
    Build the optimized release profile.
.PARAMETER NoThrottle
    Use all CPU cores instead of half the available cores.
.PARAMETER Jobs
    Override Cargo's parallel job count.
.PARAMETER NoRestart
    Do not restart WinGlance.exe if it was running before the build.
.PARAMETER Start
    Launch WinGlance.exe after the build.
.PARAMETER SkipFormat
    Skip cargo fmt --check.
.PARAMETER SkipAudit
    Skip cargo-audit and cargo-deny advisory checks.
.PARAMETER FreshInstall
    Delete WinGlance's app data before the build. This is DEV-ONLY and explicit.
#>

[CmdletBinding()]
param(
    [switch]$Clean,
    [switch]$Release,
    [switch]$NoThrottle,
    [int]$Jobs,
    [switch]$NoRestart,
    [switch]$Start,
    [switch]$SkipFormat,
    [switch]$SkipAudit,
    [switch]$FreshInstall
)

# Fail fast on cmdlet errors, but not on native-command stderr. Under
# Windows PowerShell 5.1, $ErrorActionPreference = "Stop" turns any native
# stderr line piped through 2>&1 into a terminating error, which kills the
# script whenever a caller captures its output. Native commands are checked
# explicitly via $LASTEXITCODE instead; cmdlets that must fail hard carry an
# explicit -ErrorAction Stop.
$ErrorActionPreference = "Continue"
$AppName = "WinGlance"

function Write-Step($Message) {
    Write-Host ""
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Fail($Message) {
    Write-Host ""
    Write-Host "ERROR: $Message" -ForegroundColor Red
    exit 1
}

function Stop-AppProcesses {
    $processes = Get-Process -Name $AppName -ErrorAction SilentlyContinue
    if (-not $processes) { return }
    Write-Step "Stopping running $AppName.exe process(es) (pid: $($processes.Id -join ', '))"
    $processes | Stop-Process -Force -ErrorAction SilentlyContinue
    for ($index = 0; $index -lt 10; $index++) {
        Start-Sleep -Milliseconds 300
        if (-not (Get-Process -Name $AppName -ErrorAction SilentlyContinue)) { return }
    }
    if (Get-Process -Name $AppName -ErrorAction SilentlyContinue) {
        Fail "Could not stop $AppName.exe. Close it in Task Manager and retry."
    }
}

Set-Location -Path $PSScriptRoot
if (-not (Test-Path -LiteralPath ".\Cargo.toml")) {
    Fail "Cargo.toml not found in $PSScriptRoot"
}

$wasRunning = $null -ne (Get-Process -Name $AppName -ErrorAction SilentlyContinue)
Stop-AppProcesses

$dataDir = "$env:APPDATA\$AppName\$AppName\data"
if ($FreshInstall) {
    Write-Step "Wiping app data for explicit fresh-install simulation"
    if (Test-Path -LiteralPath $dataDir) {
        Remove-Item -LiteralPath $dataDir -Recurse -Force -ErrorAction Stop
        Write-Host "Removed: $dataDir" -ForegroundColor Yellow
    }
}

Write-Step "Checking tools"
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) { Fail "cargo not found. Install Rust from https://rustup.rs" }
& $cargo.Source --version
if (-not $SkipFormat) {
    $rustfmt = Get-Command rustfmt -ErrorAction SilentlyContinue
    if (-not $rustfmt) { Fail "rustfmt not found. Install the Rust component with rustup component add rustfmt" }
}
if (-not $SkipAudit) {
    # The advisory checks are part of the gate (AGENTS.md), so a missing
    # tool is a failure, not a skip — a guard that silently disables itself
    # is not a guard. -SkipAudit is the escape for quick loops.
    $audit = Get-Command cargo-audit -ErrorAction SilentlyContinue
    if (-not $audit) { Fail "cargo-audit not found. Install with 'cargo install cargo-audit' (or use -SkipAudit for a quick loop)" }
    $deny = Get-Command cargo-deny -ErrorAction SilentlyContinue
    if (-not $deny) { Fail "cargo-deny not found. Install with 'cargo install cargo-deny' (or use -SkipAudit for a quick loop)" }
}

$cargoJobsFlag = ""
if (-not $NoThrottle) {
    $cores = [Environment]::ProcessorCount
    $jobCount = if ($Jobs -gt 0) { $Jobs } else { [math]::Max(1, [math]::Floor($cores / 2)) }
    $env:CARGO_BUILD_JOBS = "$jobCount"
    $cargoJobsFlag = "-j$jobCount"
    Write-Step "Using $jobCount of $cores CPU cores"
} else {
    Write-Step "Using all CPU cores"
}

try {
    if ($Clean) {
        Write-Step "Cleaning old build artifacts"
        cargo clean
        if ($LASTEXITCODE -ne 0) { Fail "cargo clean failed (exit code $LASTEXITCODE)" }
    }

    if (-not $SkipFormat) {
        Write-Step "Checking Rust formatting"
        cargo fmt --all -- --check
        if ($LASTEXITCODE -ne 0) { Fail "Formatting check failed. Run cargo fmt --all and retry." }
    }

    Write-Step "Checking all Cargo targets"
    cargo check --all-targets --locked $cargoJobsFlag
    if ($LASTEXITCODE -ne 0) { Fail "cargo check --all-targets failed (exit code $LASTEXITCODE)" }

    Write-Step "Linting all targets"
    cargo clippy --all-targets --locked $cargoJobsFlag -- -D warnings
    if ($LASTEXITCODE -ne 0) { Fail "Clippy failed (exit code $LASTEXITCODE). Fix the warnings and retry." }

    Write-Step "Running tests"
    cargo test --locked $cargoJobsFlag --quiet --no-fail-fast
    if ($LASTEXITCODE -ne 0) { Fail "Tests failed (exit code $LASTEXITCODE). Fix the failures and retry." }

    $profileFlag = if ($Release) { "--release" } else { "" }
    Write-Step "Building WinGlance.exe $profileFlag"
    cargo build $profileFlag --locked $cargoJobsFlag
    if ($LASTEXITCODE -ne 0) { Fail "cargo build failed (exit code $LASTEXITCODE)" }

    $targetDir = if ($Release) { "release" } else { "debug" }
    $exePath = Join-Path $PSScriptRoot "target\$targetDir\$AppName.exe"
    if (-not (Test-Path -LiteralPath $exePath)) { Fail "No executable at $exePath" }

    if (-not $SkipAudit) {
        # Hard gates: a vulnerable or license-violating dependency fails the
        # build, matching what CI enforces. The tools were verified present
        # in the tool check above, so a non-zero exit here is a real finding.
        Write-Step "Running cargo-audit (advisory)"
        cargo audit
        if ($LASTEXITCODE -ne 0) { Fail "cargo-audit found advisories (exit code $LASTEXITCODE). Vet them and retry, or use -SkipAudit." }

        Write-Step "Running cargo-deny (advisory)"
        cargo deny check
        if ($LASTEXITCODE -ne 0) { Fail "cargo-deny check failed (exit code $LASTEXITCODE). Fix the violations and retry, or use -SkipAudit." }
    }

    if ($Start -or ($wasRunning -and -not $NoRestart)) {
        Write-Step "Launching $AppName.exe"
        Start-Process -FilePath $exePath -ErrorAction Stop
    }

    Write-Step "Done"
    Write-Host "Built: $exePath" -ForegroundColor Green
} finally {
    Remove-Item -Path env:CARGO_BUILD_JOBS -ErrorAction SilentlyContinue
}
