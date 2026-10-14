#Requires -Version 5.0
<#
   lhm-bridge.ps1 — LibreHardwareMonitor sidecar for minihud.

   Loads LibreHardwareMonitorLib.dll (net472, no SDK needed), opens EVERY LHM
   hardware category, warms sensors up, then serves one JSON sensor dump per
   blank line on stdin:

       > \n
       < [{"hw":"Cpu:...","type":"Temperature","name":"...","value":42.0}, ...]

   Non-finite values (NaN from cold sensors) are filtered: System.Text.Json
   refuses them and Rust would choke. A closed stdin ends the loop cleanly.

   The rows stay flat — (hw, type, name, value) — because the payload carries no
   node identity; the host numbers the sensors that collide (two same-model
   GPUs) so no sensor is overwritten.

   Stdout is the protocol: the version banner, `READY`, then one JSON line per
   tick. Anything the operator should see goes to stderr.

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

# Every LHM hardware category the assembly exposes, so the flat payload carries
# the complete inventory: storage/NVMe, network, battery, controllers and PSU
# beside CPU, GPU, memory and motherboard. Nothing is curated out here.
$categories = @(
    'IsBatteryEnabled'
    'IsControllerEnabled'
    'IsCpuEnabled'
    'IsGpuEnabled'
    'IsMemoryEnabled'
    'IsMotherboardEnabled'
    'IsNetworkEnabled'
    'IsPsuEnabled'
    'IsStorageEnabled'
)

# Fan controllers and power monitors are read over USB HID, which LHM does
# through HidSharp — an optional assembly that ships beside the LHM DLL.
# Opening those two categories without it makes LHM's Open() throw half-way
# through the enumeration, which would serve no sensor at all, so they are
# dropped on the retry instead.
$hidCategories = @('IsControllerEnabled', 'IsPsuEnabled')

function New-Monitor($withHidCategories) {
    $computer = New-Object LibreHardwareMonitor.Hardware.Computer
    foreach ($name in $categories) {
        if ($withHidCategories -or $hidCategories -notcontains $name) {
            $computer.$name = $true
        }
    }
    $computer.Open()
    return $computer
}

try {
    $computer = New-Monitor $true
} catch {
    # A second, fresh Computer object: the failed Open() already enumerated the
    # non-HID groups, so reusing it would report every node twice.
    [Console]::Error.WriteLine("lhm-bridge: $($_.Exception.Message)")
    [Console]::Error.WriteLine('lhm-bridge: controller and PSU categories closed (HidSharp.dll missing or unusable beside the LHM DLL; see README)')
    $computer = New-Monitor $false
}

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
