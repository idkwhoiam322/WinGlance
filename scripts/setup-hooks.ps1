$repoRoot = git rev-parse --show-toplevel
$scriptsDir = Join-Path $repoRoot "scripts"
$hookSrc = Join-Path $scriptsDir "pre-commit"
$gitHooksDir = Join-Path $repoRoot ".git\hooks"
$hookDst = Join-Path $gitHooksDir "pre-commit"

if (-not (Test-Path -LiteralPath $hookSrc)) {
    throw "Hook source not found: $hookSrc"
}
if (-not (Test-Path -LiteralPath $gitHooksDir)) {
    New-Item -ItemType Directory -Path $gitHooksDir | Out-Null
}
Copy-Item -LiteralPath $hookSrc -Destination $hookDst -Force
Write-Host "Installed pre-commit hook to $hookDst"
