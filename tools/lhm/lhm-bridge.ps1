#Requires -Version 5.0
<#
  lhm-bridge.ps1 — LibreHardwareMonitor sidecar for minihud.

  Loads LibreHardwareMonitorLib.dll (net472, no SDK needed), opens the
  hardware monitor, warms sensors up, then serves one JSON sensor dump
  per blank line on stdin:

      > \n
      < [{"hw":"Cpu:...","type":"Temperature","name":"...","value":42.0}, ...]

  Non-finite values (NaN from cold sensors) are filtered: System.Text.Json
  refuses them and Rust would choke. A closed stdin ends the loop cleanly.

  Usage:
      powershell -NoProfile -ExecutionPolicy Bypass -File lhm-bridge.ps1 -DllPath .\LibreHardwareMonitorLib.dll
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$DllPath
)

$ErrorActionPreference = 'Stop'

try {
    $ver = [Diagnostics.FileVersionInfo]::GetVersionInfo($DllPath).FileVersion
} catch {
    $ver = 'unknown'
}
Write-Output "lhm-bridge dll v$ver"

Add-Type -Path $DllPath

$computer = New-Object LibreHardwareMonitor.Hardware.Computer
$computer.IsCpuEnabled = $true
$computer.IsGpuEnabled = $true
$computer.IsMemoryEnabled = $true
$computer.IsMotherboardEnabled = $true
$computer.Open()

# Cold sensors read 0/NaN on the first cycles; warm up before serving.
for ($i = 0; $i -lt 3; $i++) {
    foreach ($h in $computer.Hardware) { $h.Update() }
    Start-Sleep -Milliseconds 300
}

function Get-Sensors($hardware, $prefix) {
    $out = @()
    foreach ($s in $hardware.Sensors) {
        $v = $s.Value
        # No IsFinite on .NET Framework: spell it out. NaN slips through
        # ConvertTo-Json otherwise (it throws) and poisons the stream.
        if ($null -ne $v) {
            $d = [double]$v
            if (-not [double]::IsNaN($d) -and -not [double]::IsInfinity($d)) {
                $out += [pscustomobject]@{
                    hw    = $prefix
                    type  = $s.SensorType.ToString()
                    name  = $s.Name
                    value = $d
                }
            }
        }
    }
    foreach ($sub in $hardware.SubHardware) {
        $out += Get-Sensors $sub ($prefix + '/' + $sub.Name)
    }
    return $out
}

Write-Output 'READY'

try {
    while (($line = [Console]::In.ReadLine()) -ne $null) {
        $all = @()
        foreach ($h in $computer.Hardware) {
            $h.Update()
            $all += Get-Sensors $h ($h.HardwareType.ToString() + ':' + $h.Name)
        }
        Write-Output ($all | ConvertTo-Json -Compress -Depth 3)
    }
} finally {
    $computer.Close()
}
