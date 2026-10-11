<#
.SYNOPSIS
  Pure helpers for tools/coverage-live.ps1 (side-effect free; dot-sourceable).

.DESCRIPTION
  The live harness must fail loudly when a scenario produces no evidence, and it
  must not mistake a present-but-zero counter for evidence. That decision is
  pure logic and lives here so tools/coverage-live.Tests.ps1 can pin it: a bug
  here is exactly the silent under-report the harness exists to prevent.
#>

<#
.SYNOPSIS
  The expected evidence per scenario: tag -> detour function-name suffixes that
  must have a hit count > 0 in that scenario's merged profile.

.DESCRIPTION
  Suffixes are the tail of the exported LLVM-mangled symbol (e.g.
  `_RNvNtCs.._7hook_rt6detour12dxgi_present..` ends with `6detour12dxgi_present`).
  `layer` is produced by hook_vk_layer.dll; the `rt-*` tags by hook_rt.dll.
#>
function Get-ScenarioEvidence {
  [CmdletBinding()]
  param()
  [ordered]@{
    'rt-d3d11'         = @('6detour12dxgi_present')
    'rt-d3d11-factory' = @('6detour17create_swap_chain', '6detour19create_dxgi_factory', '6detour20create_dxgi_factory1')
    'rt-d3d9'          = @('6detour12d3d9_present')
    'rt-d3d9ex'        = @('6detour15d3d9_present_ex')
    'rt-d3d12'         = @('6detour27d3d12_execute_command_lists')
    'rt-opengl'        = @('6detour16gdi_swap_buffers')
    'rt-opengl-delay'  = @('6detour16wgl_swap_buffers')
    'rt-opengl-layer'  = @('6detour22wgl_swap_layer_buffers')
    'rt-angle'         = @('6detour16egl_swap_buffers')
    'rt-dcomp'         = @('6detour33create_swap_chain_for_composition')
    'rt-d3d11-fullscreen' = @('6detour12dxgi_present')
    'rt-vulkan'        = @('6detour16vk_queue_present')
    'rt-capture'       = @('6detour12dxgi_present')
    'layer'            = @('13hook_vk_layer16mh_queue_present')
  }
}

<#
.SYNOPSIS
  Highest hit count for a mangled-name suffix across exported functions, or
  `$null` when no function matches.

.DESCRIPTION
  Two instrumented builds of the same crate can share the report (a stale binary
  hash beside the current one), so a name can appear twice with one entry
  all-zero; take the maximum, not the first match.
#>
function Get-FunctionHit {
  [CmdletBinding()]
  param(
    [AllowEmptyCollection()][object[]]$Functions,
    [Parameter(Mandatory = $true)][string]$Suffix
  )
  $m = $Functions |
    Where-Object { $_.name.EndsWith($Suffix) } |
    Sort-Object count -Descending |
    Select-Object -First 1
  if ($m) { [long]$m.count } else { $null }
}

<#
.SYNOPSIS
  Decide whether a scenario's exported functions carry the evidence it must.

.DESCRIPTION
  Every expected suffix must resolve to a hit count > 0 — a present-but-zero
  counter is NOT evidence (a target that failed to present still writes a
  profraw). Returns a hashtable: Ok (bool), Missing (suffixes at 0/absent),
  Hits (suffix -> count or $null).
#>
function Test-ScenarioEvidence {
  [CmdletBinding()]
  param(
    [AllowEmptyCollection()][object[]]$Functions,
    [Parameter(Mandatory = $true)][string[]]$Expected
  )
  $missing = New-Object System.Collections.ArrayList
  $hits = @{}
  foreach ($s in $Expected) {
    $c = Get-FunctionHit -Functions $Functions -Suffix $s
    $hits[$s] = $c
    if ($null -eq $c -or $c -le 0) { [void]$missing.Add($s) }
  }
  @{ Ok = ($missing.Count -eq 0); Missing = @($missing); Hits = $hits }
}

<#
.SYNOPSIS
  The expected printed api label(s) per scenario, and the fallback label(s) that
  must NOT appear in that scenario's host transcript.

.DESCRIPTION
  `Test-ScenarioEvidence` proves the recorder's detour *function* fired; this
  table proves the presenter drove the *intended* path and the host reported it.
  For the GL family the split matters: `opengl` presents through
  `gdi32!SwapBuffers` (`gl.gdiswapbuffers`), `opengl-delay` through the
  delay-loaded `opengl32!wglSwapBuffers` (`gl.wglswapbuffers`), and
  `opengl-layer` through `opengl32!wglSwapLayerBuffers`
  (`gl.wglswaplayerbuffers`). A presenter that silently fell back to
  `gdi32!SwapBuffers` would still pass the detour-evidence check if the intended
  detour fired at all, so a `Forbidden` fallback is asserted too.

  Keys are the scenarios that produce a minihud host transcript; `layer` is
  absent (the dynamic target is run directly, no host observer), so it is held
  to detour evidence only.
#>
function Get-ScenarioExpectedLabels {
  [CmdletBinding()]
  param()
  [ordered]@{
    'rt-d3d11'         = @{ Required = @('dxgi.present');            Forbidden = @() }
    'rt-d3d11-factory' = @{ Required = @('dxgi.present');            Forbidden = @() }
    'rt-d3d9'          = @{ Required = @('d3d9.present');            Forbidden = @() }
    'rt-d3d9ex'        = @{ Required = @('d3d9.presentex');          Forbidden = @('d3d9.present') }
    'rt-d3d12'         = @{ Required = @('d3d12.executecommandlists'); Forbidden = @() }
    'rt-opengl'        = @{ Required = @('gl.gdiswapbuffers');       Forbidden = @() }
    'rt-opengl-delay'  = @{ Required = @('gl.wglswapbuffers');       Forbidden = @('gl.gdiswapbuffers') }
    'rt-opengl-layer'  = @{ Required = @('gl.wglswaplayerbuffers');  Forbidden = @('gl.gdiswapbuffers') }
    'rt-angle'         = @{ Required = @('gl.eglswapbuffers');       Forbidden = @() }
    'rt-dcomp'         = @{ Required = @('dxgi.present');            Forbidden = @() }
    'rt-d3d11-fullscreen' = @{ Required = @('dxgi.present');         Forbidden = @() }
    'rt-vulkan'        = @{ Required = @('vk.queuepresent');         Forbidden = @() }
    'rt-capture'       = @{ Required = @('dxgi.present');            Forbidden = @() }
  }
}

<#
.SYNOPSIS
  Decide whether a host transcript carries the labels a scenario must (and none
  of the fallback labels it must not).

.DESCRIPTION
  api labels are printed as `<label> x<count>` (e.g. `dxgi.present x121`); the
  label is matched as a whole token, so `d3d9.present` does NOT match inside
  `d3d9.presentex`. Returns a hashtable: Ok (bool), Missing (required labels not
  printed), Violations (forbidden labels that were printed), Seen (every printed
  label).
#>
function Test-ScenarioLabels {
  [CmdletBinding()]
  param(
    [AllowEmptyString()][string]$Transcript,
    [Parameter(Mandatory = $true)][string[]]$Required,
    [AllowEmptyCollection()][string[]]$Forbidden = @()
  )
  $seen = @{}
  foreach ($m in [regex]::Matches([string]$Transcript, '(?<label>[A-Za-z_][A-Za-z0-9_.]*)\s+x\d+')) {
    $seen[$m.Groups['label'].Value] = $true
  }
  $missing = New-Object System.Collections.ArrayList
  foreach ($l in $Required) {
    if (-not $seen.ContainsKey($l)) { [void]$missing.Add($l) }
  }
  $violations = New-Object System.Collections.ArrayList
  foreach ($l in $Forbidden) {
    if ($seen.ContainsKey($l)) { [void]$violations.Add($l) }
  }
  @{
    Ok         = ($missing.Count -eq 0 -and $violations.Count -eq 0)
    Missing    = @($missing)
    Violations = @($violations)
    Seen       = @($seen.Keys)
  }
}
