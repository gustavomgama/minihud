# Full local audit — the counterpart to .github/workflows/audit.yml.
#
# Runs the same checks the CI job runs, in order, and exits non-zero on any
# hard-gate failure (fmt, clippy, test, deps, coverage, CRAP, duplication).
# Advisory reports (rustqual, mete) print but do not gate unless -Strict is
# passed.
#
#   powershell -ExecutionPolicy Bypass -File tools/audit.ps1
#   powershell -ExecutionPolicy Bypass -File tools/audit.ps1 -Strict
[CmdletBinding()]
param([switch]$Strict)

$ErrorActionPreference = 'Continue'
Set-Location (Join-Path $PSScriptRoot '..')
$fail = @()

function Invoke-Step {
    param([string]$Name, [scriptblock]$Body)
    Write-Host "== $Name ==" -ForegroundColor Cyan
    & $Body
    if ($LASTEXITCODE -ne 0) { $fail += $Name }
}

Invoke-Step 'fmt'         { cargo fmt --all -- --check }
Invoke-Step 'clippy'      { cargo clippy --workspace --all-targets -- -D warnings }
Invoke-Step 'test'        { cargo nextest run --workspace }
Invoke-Step 'machete'     { cargo machete }
Invoke-Step 'deny'        { cargo deny check }
Invoke-Step 'audit'       { cargo audit }
Invoke-Step 'coverage'    { cargo llvm-cov --workspace --lcov --output-path lcov.info }
Invoke-Step 'crap'        { cargo crap --lcov lcov.info --exclude 'src/main.rs' --exclude 'build.rs' --exclude 'xtask/**' --threshold 30 --fail-above }
Invoke-Step 'duplication' { jscpd src --min-lines 5 --threshold 5 }

Write-Host '== rustqual (advisory) ==' -ForegroundColor Cyan
if ($Strict) {
    rustqual --coverage lcov.info --min-quality-score 90
    if ($LASTEXITCODE -ne 0) { $fail += 'rustqual' }
} else {
    rustqual --coverage lcov.info --no-fail
}

Write-Host '== mete (advisory) ==' -ForegroundColor Cyan
mete analyze src

if ($fail.Count -gt 0) {
    Write-Host ("audit: FAIL - {0}" -f ($fail -join ', ')) -ForegroundColor Red
    exit 1
}
Write-Host 'audit: PASS' -ForegroundColor Green
exit 0
