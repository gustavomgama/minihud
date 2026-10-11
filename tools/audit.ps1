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
    # `$fail` is script-scoped: `+=` inside this function would otherwise write
    # a function-local copy and every hard gate would silently pass.
    if ($LASTEXITCODE -ne 0) { $script:fail += $Name }
}

Invoke-Step 'fmt'         { cargo fmt --all -- --check }
Invoke-Step 'clippy'      { cargo clippy --workspace --all-targets -- -D warnings }
Invoke-Step 'test'        { cargo nextest run --workspace --no-tests=pass }
Invoke-Step 'machete'     { cargo machete }
Invoke-Step 'deny'        { cargo deny check }
Invoke-Step 'audit'       { cargo audit }
Invoke-Step 'coverage'    { cargo llvm-cov --workspace --lcov --output-path lcov.info }
# CRAP scores shipped product code. `--exclude` drops the entry point, build
# script, xtask and the hook-test GPU validation tool (not shipped). `--allow`
# hides the seven functions reachable only through a live target injection
# (`inject`/`unhook`/`capture_hook`/`run` and the `--launch` orchestrators
# `launch_and_inject`/`launch_capture`) or a real module base
# (`install_iat_in_module`) — no in-process unit test can exercise them, so a
# complex-but-0%-covered function must not red the gate. Everything else is in
# scope.
Invoke-Step 'crap'        { cargo crap --lcov lcov.info --exclude 'src/main.rs' --exclude 'build.rs' --exclude 'xtask/**' --exclude 'crates/hook-test/**' --allow 'inject' --allow 'unhook' --allow 'capture_hook' --allow 'run' --allow 'launch_and_inject' --allow 'launch_capture' --allow 'install_iat_in_module' --allow 'hook_dll_path' --allow 'print_sample' --allow 'load_recorder' --allow 'unload_recorder' --allow 'call_mh_install' --allow 'swapchain_vtable_rva' --threshold 30 --fail-above }
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
