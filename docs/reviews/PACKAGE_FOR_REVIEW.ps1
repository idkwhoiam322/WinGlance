<#
.SYNOPSIS
    Creates WinGlance.zip — the code-only archive for external review.

.DESCRIPTION
    Uses the developer's Zip-Dir helper (Windows PowerShell profile). The archive is
    meant for an external reviewer agent: source, docs, configs, and workflows only —
    no build artifacts, no git history, no transient data.

    The shipped artifact of this repo is a single, self-contained `WinGlance.exe`; this zip
    is the source for auditing that contract.

.EXAMPLE
    powershell -File docs\reviews\PACKAGE_FOR_REVIEW.ps1

.NOTES
    Excluded (do NOT re-add — they must not appear in the review payload):
      - target/ and release/: build artifacts + the compiled exe.
      - .git/: git history.
      - .opencode/: agent scratch/runtime state.
      - The zip itself.

    Verification before submitting (expect 0 artifact/iconish paths,
    ~tens of entries):
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [System.IO.Compression.ZipFile]::OpenRead("$PWD\WinGlance.zip")
        $names = $zip.Entries | ForEach-Object { $_.FullName }
        $zip.Dispose()
        "entries: $($names.Count)"
        "artifacts: $($names | Where-Object { $_ -match 'target|release|\.git|\.opencode' } | Measure-Object | Select-Object -ExpandProperty Count)"

    Reminder: regenerate the zip before EVERY submission; it is never rebuilt automatically.

    The audit instructions for the reviewer live in FULL_CODE_REVIEW_PROMPT.md.
#>

$profilePath = "$env:USERPROFILE\OneDrive\Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1"
if (-not (Test-Path -LiteralPath $profilePath)) {
    throw "PowerShell profile not found: $profilePath (Zip-Dir lives there)"
}
. $profilePath

# docs/reviews -> repo root (two levels up); the zip lands in the repo root.
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if (-not (Test-Path -LiteralPath (Join-Path $repoRoot "Cargo.toml"))) {
    throw "Repo root not found at: $repoRoot"
}
Push-Location -Path $repoRoot
try {
    # Zip-Dir takes the full path of the folder to zip (its leaf name becomes the
    # output name: WinGlance.zip in the current directory).
    Zip-Dir $repoRoot -Ignore 'target/*;release/*;.git/*;.opencode/*' -Clean
}
finally {
    Pop-Location
}
