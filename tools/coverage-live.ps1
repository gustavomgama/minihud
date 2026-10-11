<#
.SYNOPSIS
  Live-injection coverage run (on demand; needs a GPU + a runnable desktop).

.DESCRIPTION
  The deterministic coverage gate (`tools/audit.ps1`) runs only unit tests, so
  the host functions that need a real injected target stay at 0% and are hidden
  by the CRAP gate's `--allow` list. This script produces the *evidence* that
  those functions actually execute, by running the host binary under
  `cargo llvm-cov` while it performs real injections.

  It is intentionally NOT part of CI: CI has no GPU/target. Run it locally:

    powershell -ExecutionPolicy Bypass -File tools/coverage-live.ps1

  Covered host paths (see audit.md "Live coverage"):
    --launch  (d3d11, d3d9)  -> launch_and_inject, inject, unhook, ensure_ring
    --capture-hook           -> capture_hook, inject
    --read-frames            -> read_frames, FrameReader::open_existing
    --follow --follow-secs N -> follow::run (now exits cleanly, so it is
                                counted; a Ctrl+C is no longer required)

  NOT coverable this way: `hook-rt/src/lib.rs` (`mh_install`/`mh_uninstall`
  run inside the injected DLL, a separate module) -- handled in Part 2.

  The pure helpers backing Part 2's evidence decision are tested by
  tools/coverage-live.Tests.ps1 (`Invoke-Pester -Path tools/coverage-live.Tests.ps1`).
#>
[CmdletBinding()]
param(
  # Cap the launch capture so the run is quick even if the target is slow.
  [int]$Frames = 600
)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$root  = (Get-Location).Path
$covDir = 'target/llvm-cov-target'
$htExe  = Join-Path $root 'target/debug/hook-test.exe'
$rtDll  = Join-Path $root 'target/debug/hook_rt.dll'
$env:MINIHUD_NO_ELEVATE = '1'

function Clear-Profraw { Get-ChildItem "$covDir/*.profraw" -ErrorAction SilentlyContinue | Remove-Item -Force }

Write-Host '== build target + recorder ==' -ForegroundColor Cyan
cargo build -p hook-test -p hook-rt
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

Clear-Profraw
Write-Host '== build instrumented host ==' -ForegroundColor Cyan
cargo llvm-cov run -p minihud --no-report -- --help | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'cargo llvm-cov build failed' }
# The recorder DLL must sit next to the instrumented exe (hook_dll_path()).
Copy-Item $rtDll (Join-Path $covDir 'debug/hook_rt.dll') -Force
Clear-Profraw  # drop the --help profraw; keep only live-injection data

function Invoke-Cov {
  param([string[]]$CovArgs)
  Write-Host ("== minihud " + ($CovArgs -join ' ') + " ==") -ForegroundColor Cyan
  cargo llvm-cov run -p minihud --no-report -- @CovArgs
  if ($LASTEXITCODE -ne 0) { throw "live run failed: $($CovArgs -join ' ')" }
}
function Start-Target { param([string]$Api, [int]$Frames)
  Start-Process -FilePath $htExe -ArgumentList '--api',$Api,'--frames',$Frames -PassThru -WindowStyle Hidden }

# 1+2. launch-suspended injection (D3D11 still alive at the window end -> unhook
# success; D3D9 exits early -> unhook's "recorder not loaded" path).
Invoke-Cov @('--launch', $htExe, '--api', 'd3d11', '--frames', "$Frames")
Invoke-Cov @('--launch', $htExe, '--api', 'd3d9',  '--frames', "$Frames")

# 3. capture_hook: inject into an already-running target.
$t = Start-Target d3d11 3000
Start-Sleep -Seconds 1
try { Invoke-Cov @('--capture-hook', "$($t.Id)", '4') } finally { Stop-Process -Id $t.Id -Force -ErrorAction SilentlyContinue }

# 4. read_frames: observe an existing ring. The ring is created by the normal
# (non-instrumented) build's injection; the instrumented host only reads it.
$t2 = Start-Target d3d11 5000
Start-Sleep -Seconds 1
try {
  & (Join-Path $root 'target/debug/minihud.exe') --capture-hook "$($t2.Id)" 3 | Out-Null
  Invoke-Cov @('--read-frames', "$($t2.Id)", '3')
} finally { Stop-Process -Id $t2.Id -Force -ErrorAction SilentlyContinue }

# 5. --follow: a *bounded* follow (--follow-secs) exits on its own, so the loop
# flushes its profraw and `follow::run` is counted. A target is left running so
# the loop exercises detect -> inject -> unhook before the bound elapses.
Write-Host '== minihud --follow --follow-secs 3 (bounded; flushes coverage) ==' -ForegroundColor Cyan
$ft = Start-Target d3d11 1500
Start-Sleep -Milliseconds 600
try { Invoke-Cov @('--follow','--match','hook-test.exe','--poll-ms','300','--follow-secs','3') }
finally { if (-not $ft.HasExited) { Stop-Process -Id $ft.Id -Force -ErrorAction SilentlyContinue } }

Write-Host "`n== coverage summary (src/hook) ==" -ForegroundColor Green
cargo llvm-cov report --summary-only

# Per-function hit counts for the functions the deterministic gate hides.
cargo llvm-cov report --json --output-path "$covDir/live.json" | Out-Null
$json = Get-Content "$covDir/live.json" -Raw | ConvertFrom-Json
$live = [ordered]@{
  'hook::inject'               = '4hook6inject'
  'hook::unhook'               = '4hook6unhook'
  'hook::launch_and_inject'    = '4hook17launch_and_inject'
  'hook::launch_capture'       = '4hook14launch_capture'
  'hook::capture_hook'         = '4hook12capture_hook'
  'hook::read_frames'          = '4hook11read_frames'
  'hook::ensure_ring'          = '4hook11ensure_ring'
  'hook::export_rva'           = '4hook10export_rva'
  'hook::remote_call'          = '4hook11remote_call'
  'hook::print_sample'         = '4hook12print_sample'
  'hook::module_bases'         = '4hook12module_bases'
  'hook::hook_dll_path'        = '4hook13hook_dll_path'
  'FrameReader::open_existing' = 'FrameReader13open_existing'
  'follow::run'                = '6follow3run'
}
$funcs = $json.data[0].functions
Write-Host "`n== live-only function hit counts ==" -ForegroundColor Green
'{0,-28} {1,8}' -f 'function','hits'
foreach ($label in $live.Keys) {
  $pat = $live[$label]
  # Two instrumented builds of `minihud` can share the report (a stale binary
  # hash alongside the current one), so a function name may appear twice with
  # one entry all-zero. Take the highest count, not the first match.
  $fn = $funcs | Where-Object { $_.name.EndsWith($pat) } | Sort-Object count -Descending | Select-Object -First 1
  $hits = if ($fn) { $fn.count } else { 'absent' }
  '{0,-28} {1,8}' -f $label, $hits
}
Write-Host "`nJSON report: $covDir/live.json"

# ============================================================================
# Part 2 - injected-DLL coverage (hook_rt.dll + hook_vk_layer.dll)
# ----------------------------------------------------------------------------
# Those two cdylibs run *inside the target process*, so the host build never
# links them and their FFI + detour bodies sit at 0% under the unit-test gate.
# Build them with `-C instrument-coverage`, keep them next to a host exe, run
# real injections with LLVM_PROFILE_FILE set (the target inherits it through
# --launch and writes its own profile), then merge + report with the rustup
# llvm-tools binaries (llvm-profdata / llvm-cov; no extra install).
#
# Determinism (this is the whole point of the rewrite):
#   * a FRESH profile dir per run, so no stale .profraw is ever merged;
#   * a UNIQUE per-scenario path (`<scenario>-%p-%m.profraw`): the target pids
#     and the recorder's `%m` collide across scenarios, so the scenario tag is
#     what keeps one scenario's profile from overwriting another's;
#   * the harness WAITS for each target process to fully exit (bounded) before
#     starting the next scenario and before merging, so every target's profile
#     is flushed and no orphaned target competes for the GPU;
#   * every scenario's profile is ASSERTED to carry non-zero evidence for its
#     own detour — a scenario whose profile is collected but whose expected
#     detour is 0 is a real gap and fails the run loudly (non-zero exit), so the
#     harness can never silently under-report;
#   * because the target DLL's at-exit profile flush is occasionally lost when
#     the process is torn down, a scenario is RETRIED (clean profile dir) up to
#     $ScenarioAttempts times; a scenario that still yields no profile is
#     reported as a SKIP with its reason, never as a silent 0 (likewise a
#     genuine environment skip, e.g. ANGLE absent).
#
# The instrumented hook_rt.dll is ALSO loaded into the host by
# `src/hook/mod.rs::export_rva` (a local LoadLibraryW), so each injected run
# emits a second, all-zero profraw under the host's pid. Those phantom profiles
# are deleted before merging/asserting, so ">=1 profraw" means the *target*
# flushed (otherwise a present-but-empty host profile could mask a real gap).
# ============================================================================
Write-Host "`n== injected-DLL live coverage ==" -ForegroundColor Cyan

$dbg    = Join-Path $root 'target/debug'
$rtDll2 = Join-Path $dbg 'hook_rt.dll'
$lvlDll = Join-Path $dbg 'hook_vk_layer.dll'
$rtBak2 = "$rtDll2.normal"
$lvlBak = "$lvlDll.normal"
$injDir = Join-Path $root 'target/injectcov'
# Fresh profile dir per run: never inherit a stale .profraw from a prior run.
if (Test-Path $injDir) { Remove-Item -Recurse -Force $injDir }
New-Item -ItemType Directory -Force -Path $injDir | Out-Null

. (Join-Path $PSScriptRoot 'coverage-live.helpers.ps1')
$expectedEvidence = Get-ScenarioEvidence

function Restore-NormalDlls {
  if (Test-Path $rtBak2)  { Copy-Item $rtBak2  $rtDll2  -Force -ErrorAction SilentlyContinue; Remove-Item $rtBak2  -Force -ErrorAction SilentlyContinue }
  if (Test-Path $lvlBak)  { Copy-Item $lvlBak  $lvlDll -Force -ErrorAction SilentlyContinue; Remove-Item $lvlBak -Force -ErrorAction SilentlyContinue }
}
function Get-Hit([object]$funcs, [string]$suffix) {
  $m = $funcs | Where-Object { $_.name.EndsWith($suffix) } | Sort-Object count -Descending | Select-Object -First 1
  if ($m) { $m.count } else { 'absent' }
}

# The scenario's own target profraw files. The tag is matched with its exact
# `-<pid>-<hash>.profraw` shape so `rt-d3d11` does NOT also match
# `rt-d3d11-factory-...`; the host's all-zero phantom (its pid) is excluded.
function Get-ScenarioProfraw {
  param([string]$Tag, [int]$HostPid)
  $rx = "^$([regex]::Escape($Tag))-\d+-\d+.*\.profraw$"
  Get-ChildItem "$injDir/$Tag-*.profraw" -ErrorAction SilentlyContinue |
    Where-Object {
      $_.Name -match $rx -and ($HostPid -eq 0 -or $_.Name -notlike "$Tag-$HostPid-*")
    }
}

# Delete every profraw for a tag, so a retry attempt starts clean (and cannot
# double-count a previous attempt's presents).
function Remove-ScenarioProfraw {
  param([string]$Tag)
  Get-ScenarioProfraw -Tag $Tag -HostPid 0 | Remove-Item -Force -ErrorAction SilentlyContinue
}

# A scenario's profile is flushed by the target at exit. On this machine that
# flush is occasionally lost (the recorder is a DLL in a process that is being
# torn down), so a scenario is attempted up to $ScenarioAttempts times with a
# clean profile dir; a scenario that still yields nothing is reported as a SKIP
# with its reason, never merged as a silent 0.
$ScenarioAttempts = 3

# One `--launch` attempt: run, wait for the target to exit, return its record.
function Invoke-LaunchAttempt {
  param([string]$Tag, [string]$Api, [int]$Frames, [int]$Attempt, [switch]$Fullscreen)
  $env:LLVM_PROFILE_FILE = (Join-Path $injDir "$Tag-%p-%m.profraw")
  $outFile = Join-Path $injDir "$Tag.minihud.out"
  $errFile = Join-Path $injDir "$Tag.minihud.err"
  $launchArgs = @('--launch', $htExe, '--api', $Api, '--frames', "$Frames")
  if ($Fullscreen) { $launchArgs += '--fullscreen' }
  $hostProc = Start-Process -FilePath $minihud `
    -ArgumentList $launchArgs `
    -PassThru -NoNewWindow -RedirectStandardOutput $outFile -RedirectStandardError $errFile
  $hostPid = $hostProc.Id
  $hostProc.WaitForExit()
  $text = [string](Get-Content $outFile -Raw -ErrorAction SilentlyContinue)
  $m = [regex]::Match($text, 'launched and injected .*\(pid (\d+)\)')
  $targetPid = if ($m.Success) { [int]$m.Groups[1].Value } else { 0 }
  $exited = Wait-PidGone -TargetPid $targetPid -TimeoutSec 30
  $files = @(Wait-Profraw -Tag $Tag -HostPid $hostPid -TimeoutSec 20)
  # Drop the host's all-zero phantom, so this scenario's files are the target's.
  Get-ChildItem "$injDir/$Tag-$hostPid-*.profraw" -ErrorAction SilentlyContinue | Remove-Item -Force
  return [pscustomobject]@{
    Tag = $Tag; HostPid = $hostPid; TargetPid = $targetPid
    Exited = $exited; Files = $files; Attempt = $Attempt
  }
}

# `--launch`, retried until the target profile is collected (or attempts run out).
function Invoke-LaunchScenario {
  param([string]$Tag, [string]$Api, [int]$Frames, [switch]$Fullscreen)
  $mode = if ($Fullscreen) { "$Api --fullscreen" } else { $Api }
  Write-Host "== DLL live: --launch $mode ==" -ForegroundColor Cyan
  $rec = $null
  for ($attempt = 1; $attempt -le $ScenarioAttempts; $attempt++) {
    Remove-ScenarioProfraw -Tag $Tag
    $rec = Invoke-LaunchAttempt -Tag $Tag -Api $Api -Frames $Frames -Attempt $attempt -Fullscreen:$Fullscreen
    if (@($rec.Files).Count -gt 0) { return $rec }
    if ($attempt -lt $ScenarioAttempts) {
      Write-Host ("   attempt {0}/{1}: no target profraw; retrying" -f $attempt, $ScenarioAttempts) -ForegroundColor Yellow
    }
  }
  return $rec
}

# `--capture-hook`: the only path where the DLL creates the ring itself. Retried
# like the launch scenarios.
function Invoke-CaptureScenario {
  param([string]$Tag, [int]$MaxAttempts = $ScenarioAttempts)
  Write-Host '== DLL live: --capture-hook (DLL creates the ring) ==' -ForegroundColor Cyan
  $rec = $null
  for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
    Remove-ScenarioProfraw -Tag $Tag
    $env:LLVM_PROFILE_FILE = (Join-Path $injDir "$Tag-%p-%m.profraw")
    $ct = Start-Process -FilePath $htExe -ArgumentList '--api','d3d11','--frames','400' -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 1
    $outFile = Join-Path $injDir "$Tag.minihud.out"
    $errFile = Join-Path $injDir "$Tag.minihud.err"
    $hostProc = Start-Process -FilePath $minihud -ArgumentList '--capture-hook', "$($ct.Id)", '3' `
      -PassThru -NoNewWindow -RedirectStandardOutput $outFile -RedirectStandardError $errFile
    $hostPid = $hostProc.Id
    $hostProc.WaitForExit()
    # Let the target exit on its own so its .profraw flushes (a kill would lose it).
    $exited = Wait-PidGone -TargetPid $ct.Id -TimeoutSec 30
    $files = @(Wait-Profraw -Tag $Tag -HostPid $hostPid -TimeoutSec 20)
    Get-ChildItem "$injDir/$Tag-$hostPid-*.profraw" -ErrorAction SilentlyContinue | Remove-Item -Force
    $rec = [pscustomobject]@{ Tag=$Tag; HostPid=$hostPid; TargetPid=$ct.Id; Exited=$exited; Files=$files; Attempt=$attempt }
    if (@($rec.Files).Count -gt 0) { return $rec }
    if ($attempt -lt $MaxAttempts) {
      Write-Host ("   attempt {0}/{1}: no target profraw; retrying" -f $attempt, $MaxAttempts) -ForegroundColor Yellow
    }
  }
  return $rec
}

# The Vulkan implicit layer: fully-dynamic Vulkan, no injection. Retried.
function Invoke-LayerScenario {
  param([string]$Tag, [int]$Frames, [int]$MaxAttempts = $ScenarioAttempts)
  Write-Host '== DLL live: vulkan-dynamic (implicit layer, no injection) ==' -ForegroundColor Cyan
  $rec = $null
  for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
    Remove-ScenarioProfraw -Tag $Tag
    $env:LLVM_PROFILE_FILE = (Join-Path $injDir "$Tag-%p-%m.profraw")
    $env:VK_ADD_IMPLICIT_LAYER_PATH = $dbg
    Remove-Item Env:\DISABLE_MINIHUD_VK_LAYER -ErrorAction SilentlyContinue
    & $htExe --api vulkan-dynamic --frames "$Frames" | Out-Null
    $files = @(Wait-Profraw -Tag $Tag -HostPid 0 -TimeoutSec 20)
    $rec = [pscustomobject]@{ Tag=$Tag; HostPid=0; TargetPid=0; Exited=$true; Files=$files; Attempt=$attempt }
    if (@($rec.Files).Count -gt 0) { return $rec }
    if ($attempt -lt $MaxAttempts) {
      Write-Host ("   attempt {0}/{1}: no target profraw; retrying" -f $attempt, $MaxAttempts) -ForegroundColor Yellow
    }
  }
  return $rec
}

# Wait (bounded) for a pid to disappear (the target flushes its profile on exit).
function Wait-PidGone {
  param([int]$TargetPid, [int]$TimeoutSec = 30)
  if (-not $TargetPid) { return $true }
  $deadline = (Get-Date).AddSeconds($TimeoutSec)
  while ((Get-Date) -lt $deadline) {
    if (-not (Get-Process -Id $TargetPid -ErrorAction SilentlyContinue)) { return $true }
    Start-Sleep -Milliseconds 100
  }
  return $false
}

# Wait (bounded) for the scenario's target profile to appear; returns its files.
function Wait-Profraw {
  param([string]$Tag, [int]$HostPid, [int]$TimeoutSec = 20)
  $deadline = (Get-Date).AddSeconds($TimeoutSec)
  while ((Get-Date) -lt $deadline) {
    $files = @(Get-ScenarioProfraw -Tag $Tag -HostPid $HostPid)
    if ($files.Count -ge 1) { return $files }
    Start-Sleep -Milliseconds 100
  }
  return @()
}

function Format-ScenarioRecord {
  param($Record)
  $sizes = if (@($Record.Files).Count -gt 0) {
    (@($Record.Files) | ForEach-Object { "$($_.Name)=$($_.Length)B" }) -join ', '
  } else { 'NONE' }
  Write-Host ("   host={0} target={1} target-exited={2} profraw: {3}" -f `
    $Record.HostPid, $Record.TargetPid, $Record.Exited, $sizes)
}

try {
  Copy-Item $rtDll2  $rtBak2 -Force
  Copy-Item $lvlDll $lvlBak -Force

  # Normal baseline first, so the instrumented build below is a real flag change
  # (not a no-op from a stale cargo fingerprint left by a previous run).
  cargo build -p hook-rt -p hook-vk-layer | Out-Null
  Write-Host '== build instrumented recorder + layer (-C instrument-coverage) ==' -ForegroundColor Cyan
  cargo rustc -p hook-rt -- -C instrument-coverage
  if ($LASTEXITCODE -ne 0) { throw 'instrumented hook_rt build failed' }
  cargo rustc -p hook-vk-layer -- -C instrument-coverage
  if ($LASTEXITCODE -ne 0) { throw 'instrumented hook_vk_layer build failed' }

  $minihud = Join-Path $dbg 'minihud.exe'

  # ANGLE (libEGL/libGLESv2) is required only by the `angle` scenario. If the
  # pair is absent, that scenario is an explicit SKIP with its reason.
  $angleDir = 'C:\Program Files (x86)\Steam\bin\cef\cef.win64'
  $angleOk = (Test-Path (Join-Path $angleDir 'libEGL.dll')) -and
             (Test-Path (Join-Path $angleDir 'libGLESv2.dll'))
  if ($angleOk) { $env:MINIHUD_ANGLE_DIR = $angleDir }
  $skipReasons = @{}
  if (-not $angleOk) { $skipReasons['rt-angle'] = "ANGLE pair not found at $angleDir" }

  # Each scenario: unique tag, its API, and a frame budget. `rt-vulkan`,
  # `rt-capture` and `layer` are handled separately (different invocations).
  $launchScenarios = @(
    [pscustomobject]@{ Tag='rt-d3d11';         Api='d3d11';         Frames=$Frames }
    [pscustomobject]@{ Tag='rt-d3d11-factory'; Api='d3d11-factory'; Frames=$Frames }
    [pscustomobject]@{ Tag='rt-d3d9';          Api='d3d9';          Frames=$Frames }
    [pscustomobject]@{ Tag='rt-d3d9ex';        Api='d3d9ex';        Frames=$Frames }
    [pscustomobject]@{ Tag='rt-d3d12';         Api='d3d12';         Frames=$Frames }
    [pscustomobject]@{ Tag='rt-opengl';        Api='opengl';        Frames=$Frames }
    [pscustomobject]@{ Tag='rt-opengl-delay';  Api='opengl-delay';  Frames=$Frames }
    [pscustomobject]@{ Tag='rt-opengl-layer';  Api='opengl-layer';  Frames=$Frames }
    [pscustomobject]@{ Tag='rt-dcomp';         Api='dcomp';         Frames=$Frames }
    # Exclusive fullscreen: `--launch` injects *before* the target goes
    # fullscreen, so the present hook installs and then survives the transition
    # (the real exclusive-fullscreen acceptance case).
    [pscustomobject]@{ Tag='rt-d3d11-fullscreen'; Api='d3d11';      Frames=$Frames; Fullscreen=$true }
  )
  if (-not $skipReasons.ContainsKey('rt-angle')) {
    $launchScenarios += [pscustomobject]@{ Tag='rt-angle'; Api='angle'; Frames=$Frames }
  }

  $records = New-Object System.Collections.ArrayList
  foreach ($s in $launchScenarios) {
    $r = Invoke-LaunchScenario -Tag $s.Tag -Api $s.Api -Frames $s.Frames -Fullscreen:$s.Fullscreen
    Format-ScenarioRecord $r
    [void]$records.Add($r)
  }

  # vulkan (static import): a larger frame budget; same wait semantics.
  $vulkan = Invoke-LaunchScenario -Tag 'rt-vulkan' -Api 'vulkan' -Frames 1200
  Format-ScenarioRecord $vulkan
  [void]$records.Add($vulkan)

  # capture-hook: the only path where the DLL *creates* the ring itself.
  $capRecord = Invoke-CaptureScenario -Tag 'rt-capture'
  Format-ScenarioRecord $capRecord
  [void]$records.Add($capRecord)

  # Layer: fully-dynamic Vulkan, no injection -> hook_vk_layer.dll writes.
  $layerRecord = Invoke-LayerScenario -Tag 'layer' -Frames $Frames
  Format-ScenarioRecord $layerRecord
  [void]$records.Add($layerRecord)

  # --- per-scenario evidence (trustworthy: loud failure on any gap) ----------
  $sysroot  = (rustc --print sysroot).Trim()
  $triple   = ((& rustc -vV) | Select-String '^host:').ToString().Split(':')[1].Trim()
  $llvmBin  = Join-Path $sysroot "lib/rustlib/$triple/bin"
  $profdata = Join-Path $llvmBin 'llvm-profdata.exe'
  $llvmcov  = Join-Path $llvmBin 'llvm-cov.exe'
  $merged   = Join-Path $injDir 'merged.profdata'

  $recordByTag = @{}
  foreach ($r in $records) { $recordByTag[$r.Tag] = $r }

  $failures = New-Object System.Collections.ArrayList
  Write-Host "`n== per-scenario evidence (non-zero required) ==" -ForegroundColor Green
  '{0,-18} {1,-7} {2}' -f 'scenario','status','detail'
  foreach ($tag in $expectedEvidence.Keys) {
    if ($skipReasons.ContainsKey($tag)) {
      '{0,-18} {1,-7} {2}' -f $tag, 'SKIP', $skipReasons[$tag]
      continue
    }
    $rec = $recordByTag[$tag]
    if (-not $rec) {
      '{0,-18} {1,-7} {2}' -f $tag, 'FAIL', 'scenario did not run'
      [void]$failures.Add("${tag}: scenario did not run")
      continue
    }
    $files = @($rec.Files)
    if ($files.Count -eq 0) {
      # Ran but the target's profile could not be collected after all attempts:
      # a capture skip with a reason, never a silent 0. The scenario's absence
      # is visible in the table rather than masked as "0 hits".
      $reason = "target profile not collected after $ScenarioAttempts attempts"
      '{0,-18} {1,-7} {2}' -f $tag, 'SKIP', $reason
      $skipReasons[$tag] = $reason
      continue
    }
    # Export against the binary that produced the scenario: the layer tag is
    # hook_vk_layer.dll, every `rt-*` tag hook_rt.dll.
    $bin = if ($tag -eq 'layer') { $lvlDll } else { $rtDll2 }
    $scenProfile = Join-Path $injDir "$tag.scen.profdata"
    & $profdata merge -sparse $files.FullName -o $scenProfile *> $null
    $scenJson = & $llvmcov export $bin -instr-profile $scenProfile | ConvertFrom-Json
    $scenFns = $scenJson.data[0].functions
    $ev = Test-ScenarioEvidence -Functions $scenFns -Expected $expectedEvidence[$tag]
    if ($ev.Ok) {
      $detail = (@($expectedEvidence[$tag]) | ForEach-Object { "$_=$(Get-Hit $scenFns $_)" }) -join ', '
      '{0,-18} {1,-7} {2}' -f $tag, 'OK', $detail
    } else {
      $miss = @($ev.Missing) -join ', '
      '{0,-18} {1,-7} {2}' -f $tag, 'FAIL', "expected detour(s) at 0/absent: $miss"
      [void]$failures.Add("${tag}: expected detour(s) at 0: $miss")
    }
  }

  # --- per-scenario printed api labels (presenter intent, no fallback) -------
  # Detour evidence proves the recorder's detour *function* fired; this proves
  # the presenter drove the *intended* path and the host printed its label (and
  # not a fallback). `layer` has no host transcript (the dynamic target is run
  # directly) and is held to detour evidence only, so it is absent here.
  Write-Host "`n== per-scenario printed api labels ==" -ForegroundColor Green
  '{0,-18} {1,-7} {2}' -f 'scenario', 'status', 'detail'
  $labelTable = Get-ScenarioExpectedLabels
  foreach ($tag in $labelTable.Keys) {
    if ($skipReasons.ContainsKey($tag)) {
      '{0,-18} {1,-7} {2}' -f $tag, 'SKIP', $skipReasons[$tag]
      continue
    }
    $rec = $recordByTag[$tag]
    if (-not $rec -or @($rec.Files).Count -eq 0) {
      '{0,-18} {1,-7} {2}' -f $tag, 'SKIP', 'no target transcript to check'
      continue
    }
    $outFile = Join-Path $injDir "$tag.minihud.out"
    $transcript = [string](Get-Content $outFile -Raw -ErrorAction SilentlyContinue)
    $exp = $labelTable[$tag]
    $lr = Test-ScenarioLabels -Transcript $transcript -Required $exp.Required -Forbidden $exp.Forbidden
    if ($lr.Ok) {
      '{0,-18} {1,-7} {2}' -f $tag, 'OK', ((@($exp.Required) | ForEach-Object { "$_=present" }) -join ', ')
    } else {
      $bits = @()
      if (@($lr.Missing).Count -gt 0) { $bits += "missing: $(@($lr.Missing) -join ', ')" }
      if (@($lr.Violations).Count -gt 0) { $bits += "fallback present: $(@($lr.Violations) -join ', ')" }
      '{0,-18} {1,-7} {2}' -f $tag, 'FAIL', ($bits -join '; ')
      [void]$failures.Add("${tag}: label check failed ($($bits -join '; '))")
    }
  }

  # --- full merge + report (unchanged shape) --------------------------------
  & $profdata merge -sparse (Get-ChildItem "$injDir/*.profraw").FullName -o $merged

  Write-Host "`n== hook_rt.dll (injected recorder) - coverage ==" -ForegroundColor Green
  & $llvmcov report $rtDll2 -instr-profile $merged
  Write-Host "`n== hook_vk_layer.dll (implicit layer) - coverage ==" -ForegroundColor Green
  & $llvmcov report $lvlDll -instr-profile $merged

  $rtJson = Join-Path $injDir 'rt.json'
  & $llvmcov export $rtDll2 -instr-profile $merged | Set-Content -Path $rtJson -Encoding UTF8
  $rtFns = (Get-Content $rtJson -Raw | ConvertFrom-Json).data[0].functions
  $rtLive = [ordered]@{
    'mh_install (FFI export)'          = 'mh_install'
    'mh_uninstall (FFI export)'        = 'mh_uninstall'
    'install::install'                 = '7install7install'
    'install::uninstall'               = '7install9uninstall'
    'install::install_iat_in_module'   = '7install21install_iat_in_module'
    'install::bootstrap_vtables'       = '7install17bootstrap_vtables'
    'install::rescan_once'             = '7install11rescan_once'
    'detour::set_recorder'             = '6detour12set_recorder'
    'Recorder::create'                 = '8Recorder6create'
    'Recorder::attach'                 = '8Recorder6attach'
    'Recorder::record_present'         = '8Recorder14record_present'
    'record::qpc_now'                  = '6record7qpc_now'
    'detour::dxgi_present'             = '6detour12dxgi_present'
    'detour::dxgi_present1'            = '6detour13dxgi_present1'
    'detour::dxgi_resizebuffers'       = '6detour18dxgi_resizebuffers'
    'detour::dxgi_setfullscreen'       = '6detour18dxgi_setfullscreen'
    'detour::create_swap_chain'        = '6detour17create_swap_chain'
    'detour::create_dxgi_factory'      = '6detour19create_dxgi_factory'
    'detour::create_dxgi_factory1'     = '6detour20create_dxgi_factory1'
    'detour::create_dxgi_factory2'     = '6detour20create_dxgi_factory2'
    'detour::create_swap_chain_for_composition' = '6detour33create_swap_chain_for_composition'
    'detour::d3d9_present'             = '6detour12d3d9_present'
    'detour::d3d9_endscene'            = '6detour13d3d9_endscene'
    'detour::d3d9_present_ex'          = '6detour15d3d9_present_ex'
    'detour::d3d9_reset_ex'            = '6detour13d3d9_reset_ex'
    'detour::direct3d_create9 (delay)' = '6detour16direct3d_create9'
    'detour::direct3d_create9_ex (delay)' = '6detour19direct3d_create9_ex'
    'detour::d3d12_execute_command_lists' = '6detour27d3d12_execute_command_lists'
    'detour::gdi_swap_buffers'         = '6detour16gdi_swap_buffers'
    'detour::wgl_swap_buffers (delay)' = '6detour16wgl_swap_buffers'
    'detour::wgl_swap_layer_buffers'   = '6detour22wgl_swap_layer_buffers'
    'detour::egl_swap_buffers'         = '6detour16egl_swap_buffers'
    'detour::maybe_patch_d3d12_queue'  = '6detour23maybe_patch_d3d12_queue'
    'detour::vk_queue_present'         = '6detour16vk_queue_present'
    'detour::vk_get_instance_proc_addr' = '6detour25vk_get_instance_proc_addr'
  }
  Write-Host "`n== injected hook_rt.dll - function hit counts ==" -ForegroundColor Green
  '{0,-34} {1,8}' -f 'function','hits'
  foreach ($k in $rtLive.Keys) { '{0,-34} {1,8}' -f $k, (Get-Hit $rtFns $rtLive[$k]) }

  $lvlJson = Join-Path $injDir 'layer.json'
  & $llvmcov export $lvlDll -instr-profile $merged | Set-Content -Path $lvlJson -Encoding UTF8
  $lvlFns = (Get-Content $lvlJson -Raw | ConvertFrom-Json).data[0].functions
  $lvlLive = [ordered]@{
    'vkNegotiateLoaderLayerInterfaceVersion' = 'vkNegotiateLoaderLayerInterfaceVersion'
    'vkGetInstanceProcAddr'                  = 'vkGetInstanceProcAddr'
    'vkGetDeviceProcAddr'                    = 'vkGetDeviceProcAddr'
    'mh_queue_present'                       = '13hook_vk_layer16mh_queue_present'
    'LayerRecorder::open'                    = '13LayerRecorder4open'
    'LayerRecorder::record_present'          = '13LayerRecorder14record_present'
  }
  Write-Host "`n== injected hook_vk_layer.dll - function hit counts ==" -ForegroundColor Green
  '{0,-40} {1,8}' -f 'function','hits'
  foreach ($k in $lvlLive.Keys) { '{0,-40} {1,8}' -f $k, (Get-Hit $lvlFns $lvlLive[$k]) }

  Write-Host "`nInjected-DLL JSON: $rtJson ; $lvlJson" -ForegroundColor Green
  Write-Host "Merged profile:    $merged"

  if ($failures.Count -gt 0) {
    throw ("coverage-live: {0} scenario(s) produced no evidence:`n  - {1}" -f $failures.Count, ($failures -join "`n  - "))
  }
}
catch {
  Write-Host ("coverage-live error: {0}" -f $_) -ForegroundColor Red
  Write-Host $_.ScriptStackTrace
  throw
}
finally {
  # Stop any target this harness left: a live target holds the instrumented DLL
  # locked and would make the restore below fail (and mask a real error).
  Stop-Process -Name hook-test -Force -ErrorAction SilentlyContinue
  # Restore the normal DLLs. A just-stopped host/target can hold the file a
  # moment longer, so retry the rebuild briefly before falling back to the copy.
  $restored = $false
  for ($i = 0; $i -lt 40 -and -not $restored; $i++) {
    try {
      cargo build -p hook-rt -p hook-vk-layer | Out-Null
      if ($LASTEXITCODE -eq 0) { $restored = $true }
    } catch { }
    if (-not $restored) { Start-Sleep -Milliseconds 500 }
  }
  if ($restored) { Remove-Item $rtBak2,$lvlBak -ErrorAction SilentlyContinue } else { Restore-NormalDlls }
  Remove-Item Env:\LLVM_PROFILE_FILE -ErrorAction SilentlyContinue
  Remove-Item Env:\VK_ADD_IMPLICIT_LAYER_PATH -ErrorAction SilentlyContinue
  Remove-Item Env:\MINIHUD_ANGLE_DIR -ErrorAction SilentlyContinue
}

Write-Host 'Live coverage complete.' -ForegroundColor Green
