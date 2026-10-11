# minihud "critic" gate: coverage, CRAP (complexity x coverage), duplication,
# plus advisory rustqual / mete reports. Non-destructive: writes only
# `lcov.info` and `target/`.
#
#   powershell -ExecutionPolicy Bypass -File tools/critic.ps1
#   powershell -ExecutionPolicy Bypass -File tools/critic.ps1 -Strict
#
# Hard gates (fail the run): coverage must succeed, cargo-crap must stay under
# -MaxCrap, jscpd must stay under -MaxDupPct. rustqual/mete are advisory unless
# -Strict, which also gates rustqual on a minimum quality score.
[CmdletBinding()]
param(
    [double]$MaxCrap = 30,
    [double]$MaxDupPct = 5,
    [switch]$Strict
)

$ErrorActionPreference = 'Continue'
Set-Location (Join-Path $PSScriptRoot '..')

$fail = @()

Write-Host '== coverage (cargo-llvm-cov) ==' -ForegroundColor Cyan
cargo llvm-cov --workspace --lcov --output-path lcov.info
if ($LASTEXITCODE -ne 0) { $fail += 'coverage' }

Write-Host "== CRAP (cargo-crap, threshold $MaxCrap) ==" -ForegroundColor Cyan
# Score the exercised library code. `--exclude` drops the entry point, build
# script and xtask. `--allow` hides the tier-0 ETW live-only functions that only
# run against a real PresentMon child (`run`/`read_frames`/`session`/
# `supervise`) — no in-process unit test can exercise them, so a complex-but-0%-
# covered function must not red the gate.
cargo crap --lcov lcov.info --exclude 'src/main.rs' --exclude 'build.rs' --exclude 'xtask/**' --allow 'run' --allow 'read_frames' --allow 'session' --allow 'supervise' --threshold $MaxCrap --fail-above
if ($LASTEXITCODE -ne 0) { $fail += "crap>$MaxCrap" }

Write-Host "== duplication (jscpd, threshold $MaxDupPct%) ==" -ForegroundColor Cyan
jscpd src --min-lines 5 --threshold $MaxDupPct
if ($LASTEXITCODE -ne 0) { $fail += "duplication>$MaxDupPct%" }

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
    Write-Host ("critic: FAIL - {0}" -f ($fail -join ', ')) -ForegroundColor Red
    exit 1
}
Write-Host 'critic: PASS' -ForegroundColor Green
exit 0
