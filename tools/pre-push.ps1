# Local pre-push gate - the exact hard checks .github/workflows/audit.yml runs,
# executed before every push so a red CI run never leaves this machine.
#
#   powershell -ExecutionPolicy Bypass -File tools/pre-push.ps1
#
# Wired in automatically through .githooks/pre-push; one-time setup per clone:
#   git config core.hooksPath .githooks
#
# Fail-fast: the first failing step exits 1 and the push is blocked. Advisory
# reports (rustqual, mete) are CI's continue-on-error extras and are not run
# here - the gate stays a couple of minutes, not a quarter hour.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Continue'
Set-Location (Join-Path $PSScriptRoot '..')

# No silent passes: a missing tool must fail loudly here, not let a later step
# inherit a stale $LASTEXITCODE.
$required = @(
    'cargo', 'cargo-fmt', 'cargo-clippy', 'cargo-nextest', 'cargo-machete',
    'cargo-deny', 'cargo-audit', 'cargo-llvm-cov', 'cargo-crap', 'jscpd'
)
$missing = $required | Where-Object { -not (Get-Command $_ -ErrorAction SilentlyContinue) }
if ($missing) {
    Write-Host ("pre-push: missing tools: {0}" -f ($missing -join ', ')) -ForegroundColor Red
    Write-Host 'pre-push: install them (see README) so the gate can run; refusing to push blind'
    exit 1
}

function Invoke-Step {
    param([string]$Name, [scriptblock]$Body)
    Write-Host "== $Name ==" -ForegroundColor Cyan
    & $Body
    if ($LASTEXITCODE -ne 0) {
        Write-Host ("pre-push: FAIL at '{0}' - push blocked" -f $Name) -ForegroundColor Red
        exit 1
    }
}

Invoke-Step 'fmt'         { cargo fmt --all -- --check }
Invoke-Step 'clippy'      { cargo clippy --workspace --all-targets -- -D warnings }
Invoke-Step 'test'        { cargo nextest run --workspace --no-tests=pass }
Invoke-Step 'machete'     { cargo machete }
Invoke-Step 'deny'        { cargo deny check }
Invoke-Step 'audit'       { cargo audit }
Invoke-Step 'coverage'    { cargo llvm-cov --workspace --lcov --output-path lcov.info }
Invoke-Step 'crap'        { cargo crap --lcov lcov.info --exclude 'src/main.rs' --exclude 'build.rs' --exclude 'xtask/**' --allow 'run' --allow 'session' --threshold 30 --fail-above }
Invoke-Step 'duplication' { jscpd src --min-lines 5 --threshold 5 }

Write-Host 'pre-push: PASS' -ForegroundColor Green
exit 0
