<#
.SYNOPSIS
    Builds the standalone notch.exe binary.

.DESCRIPTION
    Follows NewsAggregator's Cargo workflow: optional throttling, format and
    all-target checks, a locked Cargo build, advisory dependency checks, and
    optional restart of an already-running process.

.PARAMETER Clean
    Remove target before building.
.PARAMETER Release
    Build the optimized release profile.
.PARAMETER NoThrottle
    Use all CPU cores instead of half the available cores.
.PARAMETER Jobs
    Override Cargo's parallel job count.
.PARAMETER NoRestart
    Do not restart notch.exe if it was running before the build.
.PARAMETER Start
    Launch notch.exe after the build.
.PARAMETER SkipFormat
    Skip cargo fmt --check.
.PARAMETER SkipAudit
    Skip cargo-audit and cargo-deny advisory checks.
.PARAMETER FreshInstall
    Delete Notch's app data before the build. This is DEV-ONLY and explicit.
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

$ErrorActionPreference = "Stop"
$AppName = "notch"

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
        Remove-Item -LiteralPath $dataDir -Recurse -Force
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

    $profileFlag = if ($Release) { "--release" } else { "" }
    Write-Step "Building notch.exe $profileFlag"
    cargo build $profileFlag --locked $cargoJobsFlag
    if ($LASTEXITCODE -ne 0) { Fail "cargo build failed (exit code $LASTEXITCODE)" }

    $targetDir = if ($Release) { "release" } else { "debug" }
    $exePath = Join-Path $PSScriptRoot "target\$targetDir\$AppName.exe"
    if (-not (Test-Path -LiteralPath $exePath)) { Fail "No executable at $exePath" }

    if (-not $SkipAudit) {
        $audit = Get-Command cargo-audit -ErrorAction SilentlyContinue
        if ($audit) {
            Write-Step "Running cargo-audit (advisory)"
            cargo audit
            if ($LASTEXITCODE -ne 0) { Write-Host "cargo-audit reported issues; continuing." -ForegroundColor Yellow }
        } else {
            Write-Host "cargo-audit not installed; skipping" -ForegroundColor Yellow
        }

        $deny = Get-Command cargo-deny -ErrorAction SilentlyContinue
        if ($deny) {
            Write-Step "Running cargo-deny (advisory)"
            cargo deny check
            if ($LASTEXITCODE -ne 0) { Write-Host "cargo-deny reported issues; continuing." -ForegroundColor Yellow }
        } else {
            Write-Host "cargo-deny not installed; skipping" -ForegroundColor Yellow
        }
    }

    if ($Start -or ($wasRunning -and -not $NoRestart)) {
        Write-Step "Launching $AppName.exe"
        Start-Process -FilePath $exePath
    }

    Write-Step "Done"
    Write-Host "Built: $exePath" -ForegroundColor Green
} finally {
    Remove-Item -Path env:CARGO_BUILD_JOBS -ErrorAction SilentlyContinue
}
