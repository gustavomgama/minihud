<#
.SYNOPSIS
  Pester tests for the pure helpers that back tools/coverage-live.ps1.

.DESCRIPTION
  The live harness trusts the expected-detour table and the per-scenario
  evidence decision to decide whether a scenario really produced coverage. A
  bug here is exactly the silent under-report this harness must prevent, so the
  decision logic is pure and tested here.

  Run with:

    powershell -ExecutionPolicy Bypass -Command "Invoke-Pester -Path tools/coverage-live.Tests.ps1"
#>
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $here 'coverage-live.helpers.ps1')

Describe 'coverage-live helpers' {
  It 'Get-FunctionHit returns the highest count for a suffix' {
    # Two instrumented builds can share a name with one all-zero; take the max.
    $fns = @(
      [pscustomobject]@{ name = '_RNvNtCsX_7hook_rt6detour12dxgi_present'; count = 0 },
      [pscustomobject]@{ name = '_RNvNtCsY_7hook_rt6detour12dxgi_present'; count = 460 }
    )
    Get-FunctionHit -Functions $fns -Suffix '6detour12dxgi_present' | Should Be 460
  }

  It 'Get-FunctionHit returns null for an absent suffix' {
    $fns = @([pscustomobject]@{ name = '_other'; count = 5 })
    Get-FunctionHit -Functions $fns -Suffix '6detour12dxgi_present' | Should Be $null
  }

  It 'Test-ScenarioEvidence is Ok when every expected detour has non-zero hits' {
    $fns = @(
      [pscustomobject]@{ name = 'a6detour17create_swap_chain'; count = 1 },
      [pscustomobject]@{ name = 'b6detour19create_dxgi_factory'; count = 1 }
    )
    $ev = Test-ScenarioEvidence -Functions $fns -Expected @('6detour17create_swap_chain','6detour19create_dxgi_factory')
    $ev.Ok | Should Be $true
    @($ev.Missing).Count | Should Be 0
  }

  It 'Test-ScenarioEvidence fails when an expected detour is present but zero' {
    # A target that failed to present still writes a profraw; a zero hit must
    # not be mistaken for evidence.
    $fns = @([pscustomobject]@{ name = 'a6detour16egl_swap_buffers'; count = 0 })
    $ev = Test-ScenarioEvidence -Functions $fns -Expected @('6detour16egl_swap_buffers')
    $ev.Ok | Should Be $false
    (@($ev.Missing) -contains '6detour16egl_swap_buffers') | Should Be $true
  }

  It 'Test-ScenarioEvidence fails when an expected detour is absent' {
    $fns = @([pscustomobject]@{ name = 'a6detour16gdi_swap_buffers'; count = 12 })
    $ev = Test-ScenarioEvidence -Functions $fns -Expected @('6detour16egl_swap_buffers')
    $ev.Ok | Should Be $false
    (@($ev.Missing) -contains '6detour16egl_swap_buffers') | Should Be $true
  }

  It 'Get-ScenarioEvidence names every batch scenario with non-empty detours' {
    $table = Get-ScenarioEvidence
    foreach ($tag in 'rt-d3d11','rt-d3d11-factory','rt-d3d9','rt-d3d9ex','rt-d3d12',
                     'rt-opengl','rt-opengl-delay','rt-opengl-layer','rt-angle','rt-dcomp',
                     'rt-vulkan','rt-capture','layer') {
      $table.Contains($tag) | Should Be $true
      @($table[$tag]).Count | Should BeGreaterThan 0
    }
  }
}

Describe 'coverage-live label helpers' {
  It 'Test-ScenarioLabels is Ok when the required label appears and no fallback does' {
    # The host transcript is the only place the *printed* api label lives; a
    # presenter that drives the intended detour must also print its label.
    $t = 'pid 5: 60.0 fps  1.00 ms  (10 frames in 100 ms, 10 records, 0 errors, api gl.wglswapbuffers x10)'
    $r = Test-ScenarioLabels -Transcript $t -Required @('gl.wglswapbuffers') -Forbidden @('gl.gdiswapbuffers')
    $r.Ok | Should Be $true
    @($r.Missing).Count | Should Be 0
    @($r.Violations).Count | Should Be 0
  }

  It 'Test-ScenarioLabels fails when the required label is absent' {
    # Catches a presenter that never reaches its intended swap path: the
    # target presents but the expected label never appears in the report.
    $t = 'pid 5: ... api gl.gdiswapbuffers x10'
    $r = Test-ScenarioLabels -Transcript $t -Required @('gl.wglswapbuffers') -Forbidden @()
    $r.Ok | Should Be $false
    (@($r.Missing) -contains 'gl.wglswapbuffers') | Should Be $true
  }

  It 'Test-ScenarioLabels fails when a fallback label is present' {
    # Catches opengl-delay silently driving gdi32!SwapBuffers (a fallback): the
    # report shows gl.gdiswapbuffers even though gl.wglswapbuffers also fired.
    $t = 'pid 5: ... api gl.wglswapbuffers x10, gl.gdiswapbuffers x2'
    $r = Test-ScenarioLabels -Transcript $t -Required @('gl.wglswapbuffers') -Forbidden @('gl.gdiswapbuffers')
    $r.Ok | Should Be $false
    (@($r.Violations) -contains 'gl.gdiswapbuffers') | Should Be $true
  }

  It 'Test-ScenarioLabels matches whole labels, not substrings' {
    # `d3d9.present` is a substring of `d3d9.presentex`; the Ex presenter must
    # not be failed for the plain-Present fallback, and vice versa.
    $t = 'pid 5: ... api d3d9.presentex x8, d3d9.endscene x8'
    $r = Test-ScenarioLabels -Transcript $t -Required @('d3d9.presentex') -Forbidden @('d3d9.present')
    $r.Ok | Should Be $true
    $r2 = Test-ScenarioLabels -Transcript $t -Required @('d3d9.present') -Forbidden @()
    $r2.Ok | Should Be $false
  }

  It 'Get-ScenarioExpectedLabels names every host-transcript scenario with requirements' {
    # The label table is the contract each presenter's printed report is held to.
    # `layer` is excluded: it runs the dynamic target directly (no minihud host
    # transcript), so it is asserted by detour evidence only.
    $table = Get-ScenarioExpectedLabels
    foreach ($tag in 'rt-d3d11','rt-d3d11-factory','rt-d3d9','rt-d3d9ex','rt-d3d12',
                     'rt-opengl','rt-opengl-delay','rt-opengl-layer','rt-angle','rt-dcomp',
                     'rt-vulkan','rt-capture') {
      $table.Contains($tag) | Should Be $true
      @($table[$tag].Required).Count | Should BeGreaterThan 0
    }
    $table.Contains('layer') | Should Be $false
  }

  It 'Get-ScenarioExpectedLabels forbids the GL fallback only for the wgl presenters' {
    # The GL presenters split three ways by the swap export they must call; each
    # must not fall back to gdi32!SwapBuffers. `opengl` itself *is* the gdi path.
    $table = Get-ScenarioExpectedLabels
    (@($table['rt-opengl-delay'].Forbidden) -contains 'gl.gdiswapbuffers') | Should Be $true
    (@($table['rt-opengl-layer'].Forbidden) -contains 'gl.gdiswapbuffers') | Should Be $true
    (@($table['rt-opengl-delay'].Required) -contains 'gl.wglswapbuffers') | Should Be $true
    (@($table['rt-opengl-layer'].Required) -contains 'gl.wglswaplayerbuffers') | Should Be $true
    (@($table['rt-opengl'].Required) -contains 'gl.gdiswapbuffers') | Should Be $true
    @($table['rt-opengl'].Forbidden).Count | Should Be 0
  }
}
