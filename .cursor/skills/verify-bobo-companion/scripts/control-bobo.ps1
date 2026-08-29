#Requires -Version 5.1
<#
.SYNOPSIS
  Launch, doctor, drive, and clean up BoBo Companion for verification.

.EXAMPLE
  .\control-bobo.ps1 doctor
  .\control-bobo.ps1 launch
  .\control-bobo.ps1 screenshot -Path ..\artifacts\app-shell\ready.png
  .\control-bobo.ps1 click-rel -X 0.28 -Y 0.028
  .\control-bobo.ps1 cleanup
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0, Mandatory = $true)]
    [ValidateSet(
        'doctor',
        'launch',
        'cleanup',
        'screenshot',
        'click-rel',
        'click-label-approx',
        'read-ui-state',
        'cargo-test',
        'window-info'
    )]
    [string]$Command,

    [string]$Path,
    [double]$X,
    [double]$Y,
    [ValidateSet('enchant', 'macro', 'always-on-top', 'start', 'stop', 'new-macro')]
    [string]$Label,
    [string]$Filter,
    [switch]$Release
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ScriptDir = $PSScriptRoot
$SkillRoot = Split-Path -Parent $ScriptDir
$RepoRoot = (Resolve-Path (Join-Path $SkillRoot '..\..\..')).Path
$RunRoot = Join-Path $SkillRoot 'run'
$StateFile = Join-Path $RunRoot 'session.json'
$SandboxLocal = Join-Path $RunRoot 'LocalAppData'
$StagingBin = Join-Path $RunRoot 'bin'
$ArtifactsRoot = Join-Path $SkillRoot 'artifacts'
$WindowTitle = 'BoBo Companion'
$ExeName = 'diablo_masterwork_companion.exe'

if (-not ("BoboNative" -as [type])) {
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class BoboNative {
  public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)]
  public static extern IntPtr FindWindow(string lpClassName, string lpWindowName);
  [DllImport("user32.dll")]
  public static extern bool EnumWindows(EnumProc lpEnumFunc, IntPtr lParam);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)]
  public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
  [DllImport("user32.dll")]
  public static extern bool GetClientRect(IntPtr hWnd, out RECT lpRect);
  [DllImport("user32.dll")]
  public static extern bool ClientToScreen(IntPtr hWnd, ref POINT lpPoint);
  [DllImport("user32.dll")]
  public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")]
  public static extern bool IsWindow(IntPtr hWnd);
  [DllImport("user32.dll")]
  public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")]
  public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
  [DllImport("user32.dll")]
  public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdcBlt, uint nFlags);
  [DllImport("user32.dll")]
  public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
  [DllImport("user32.dll")]
  public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, UIntPtr dwExtraInfo);
  public const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
  public const uint MOUSEEVENTF_LEFTUP = 0x0004;
  [StructLayout(LayoutKind.Sequential)]
  public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
  [StructLayout(LayoutKind.Sequential)]
  public struct POINT { public int X; public int Y; }

  static uint _targetPid;
  static string _targetTitle;
  static IntPtr _found;

  static bool EnumCb(IntPtr hWnd, IntPtr lParam) {
    uint pid;
    GetWindowThreadProcessId(hWnd, out pid);
    if (pid != _targetPid || !IsWindowVisible(hWnd)) return true;
    var sb = new StringBuilder(512);
    GetWindowText(hWnd, sb, sb.Capacity);
    if (sb.ToString() == _targetTitle) { _found = hWnd; return false; }
    return true;
  }

  public static IntPtr FindVisibleByPidAndTitle(uint pid, string title) {
    _targetPid = pid;
    _targetTitle = title;
    _found = IntPtr.Zero;
    EnumWindows(EnumCb, IntPtr.Zero);
    return _found;
  }
}
"@
}

function Write-Json($Object, $FilePath) {
    $dir = Split-Path -Parent $FilePath
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    ($Object | ConvertTo-Json -Depth 6) | Set-Content -Encoding UTF8 -Path $FilePath
}

function Read-Session {
    if (-not (Test-Path $StateFile)) { return $null }
    return (Get-Content -Raw -Path $StateFile | ConvertFrom-Json)
}

function Save-Session($Session) {
    Write-Json $Session $StateFile
}

function Get-BoboHwnd {
    # PowerShell $null becomes "" for [string] P/Invoke args; use NullString for lpClassName.
    [IntPtr][BoboNative]::FindWindow([NullString]::Value, $WindowTitle)
}

function Get-BoboHwndForPid([int]$ProcessId) {
    [IntPtr][BoboNative]::FindVisibleByPidAndTitle([uint32]$ProcessId, $WindowTitle)
}

function Assert-OurInstance {
    $session = Read-Session
    if ($null -eq $session) {
        throw "No verification session. Run launch first, or refuse: do not drive a user-owned BoBo Companion."
    }
    $hwnd = Get-BoboHwndForPid ([int]$session.pid)
    if ($hwnd -eq [IntPtr]::Zero) { $hwnd = Get-BoboHwnd }
    if ($hwnd -eq [IntPtr]::Zero -or -not [BoboNative]::IsWindow($hwnd)) {
        throw "BoBo Companion window not found for verification session PID $($session.pid)."
    }
    $procId = 0
    [void][BoboNative]::GetWindowThreadProcessId($hwnd, [ref]$procId)
    if ([int]$procId -ne [int]$session.pid) {
        throw "Window PID $procId is not the verification session PID $($session.pid). Refusing to drive a foreign instance."
    }
    return @{ Session = $session; Hwnd = $hwnd; Pid = $procId }
}

function Get-SourceExe {
    if ($Release) {
        return (Join-Path $RepoRoot 'target\release\diablo_masterwork_companion.exe')
    }
    return (Join-Path $RepoRoot 'target\debug\diablo_masterwork_companion.exe')
}

function Ensure-Built {
    $exe = Get-SourceExe
    if (Test-Path $exe) { return $exe }
    Push-Location $RepoRoot
    try {
        if ($Release) { cargo build --release } else { cargo build }
    } finally {
        Pop-Location
    }
    if (-not (Test-Path $exe)) { throw "Build finished but exe missing: $exe" }
    return $exe
}

function Invoke-Doctor {
    $issues = @()
    $hwnd = Get-BoboHwnd
    $session = Read-Session
    $foreign = $false
    $owned = $false
    $pidInfo = $null

    if ($hwnd -ne [IntPtr]::Zero) {
        $procId = 0
        [void][BoboNative]::GetWindowThreadProcessId($hwnd, [ref]$procId)
        $pidInfo = $procId
        if ($null -ne $session -and [int]$procId -eq [int]$session.pid) {
            $owned = $true
        } else {
            $foreign = $true
            $issues += "A BoBo Companion window exists (PID $procId) that is not this verification session."
        }
    } else {
        $issues += 'BoBo Companion window is not open.'
    }

    if ($null -eq $session) {
        $issues += 'No verification session.json (launch has not recorded an owned instance).'
    } elseif (-not (Get-Process -Id $session.pid -ErrorAction SilentlyContinue)) {
        $issues += "Session PID $($session.pid) is not running."
    }

    $uiState = Join-Path $SandboxLocal 'BoBo Companion\ui-state.json'
    $report = [ordered]@{
        ok              = ($issues.Count -eq 0)
        window_title    = $WindowTitle
        hwnd            = if ($hwnd -eq [IntPtr]::Zero) { $null } else { $hwnd.ToInt64() }
        window_pid      = $pidInfo
        owned_instance  = $owned
        foreign_instance= $foreign
        session_pid     = if ($session) { $session.pid } else { $null }
        sandbox_local   = $SandboxLocal
        ui_state_path   = $uiState
        ui_state_exists = (Test-Path $uiState)
        staging_exe     = (Join-Path $StagingBin $ExeName)
        staging_exe_ok  = (Test-Path (Join-Path $StagingBin $ExeName))
        issues          = $issues
    }
    $report | ConvertTo-Json -Depth 5
    if (-not $report.ok) { exit 2 }
}

function Invoke-Launch {
    $existing = Get-BoboHwnd
    if ($existing -ne [IntPtr]::Zero) {
        $procId = 0
        [void][BoboNative]::GetWindowThreadProcessId($existing, [ref]$procId)
        $session = Read-Session
        if ($null -eq $session -or [int]$procId -ne [int]$session.pid) {
            throw "Refuse launch: BoBo Companion already running (PID $procId). Close the user's instance first; single-instance mutex blocks a second copy."
        }
        Write-Output "Already launched as verification session PID $($session.pid)."
        return
    }

    if (Test-Path $RunRoot) {
        # Keep artifacts/; only reset runtime sandbox/bin/session.
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue (Join-Path $RunRoot 'LocalAppData')
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue (Join-Path $RunRoot 'bin')
        Remove-Item -Force -ErrorAction SilentlyContinue $StateFile
    }
    New-Item -ItemType Directory -Force -Path $SandboxLocal, $StagingBin, $ArtifactsRoot | Out-Null

    $source = Ensure-Built
    $staged = Join-Path $StagingBin $ExeName
    Copy-Item -Force $source $staged
    # Omit enchant config so load_native_config uses defaults (do not seed invalid partial JSON).
    Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $StagingBin 'enchant_config_native.json')

    # Redirect both so ui-state/macros and legacy APPDATA enchant config stay in the sandbox.
    $env:LOCALAPPDATA = $SandboxLocal
    $env:APPDATA = Join-Path $SandboxLocal 'Roaming'
    New-Item -ItemType Directory -Force -Path $env:APPDATA | Out-Null
    $proc = Start-Process -FilePath $staged -WorkingDirectory $StagingBin -PassThru
    $deadline = (Get-Date).AddSeconds(45)
    $hwnd = [IntPtr]::Zero
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
        if ($proc.HasExited) { throw "BoBo Companion exited during launch (code $($proc.ExitCode))." }
        $hwnd = Get-BoboHwndForPid $proc.Id
        if ($hwnd -eq [IntPtr]::Zero) {
            $hwnd = Get-BoboHwnd
        }
        if ($hwnd -ne [IntPtr]::Zero) {
            $procId = 0
            [void][BoboNative]::GetWindowThreadProcessId($hwnd, [ref]$procId)
            if ([int]$procId -eq [int]$proc.Id) { break }
        }
    }
    if ($hwnd -eq [IntPtr]::Zero) { throw 'Timed out waiting for BoBo Companion window.' }

    Save-Session ([ordered]@{
        pid           = $proc.Id
        hwnd          = $hwnd.ToInt64()
        staged_exe    = $staged
        sandbox_local = $SandboxLocal
        started_at    = (Get-Date).ToString('o')
        release       = [bool]$Release
    })
    Write-Output "Launched PID $($proc.Id) with LOCALAPPDATA=$SandboxLocal"
}

function Invoke-Cleanup {
    $session = Read-Session
    if ($null -ne $session) {
        $alive = Get-Process -Id $session.pid -ErrorAction SilentlyContinue
        if ($alive) {
            Stop-Process -Id $session.pid -Force
            Start-Sleep -Milliseconds 400
        }
    }
    # Never delete artifacts/. Reset only runtime state.
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue (Join-Path $RunRoot 'LocalAppData')
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue (Join-Path $RunRoot 'bin')
    Remove-Item -Force -ErrorAction SilentlyContinue $StateFile
    Write-Output "Cleanup complete. Evidence retained under $ArtifactsRoot"
}

function Invoke-Screenshot {
    if (-not $Path) { throw 'screenshot requires -Path' }
    $ctx = Assert-OurInstance
    $hwnd = $ctx.Hwnd
    [void][BoboNative]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 200

    Add-Type -AssemblyName System.Drawing
    $rect = New-Object BoboNative+RECT
    if (-not [BoboNative]::GetWindowRect($hwnd, [ref]$rect)) { throw 'GetWindowRect failed' }
    $width = [Math]::Max(1, $rect.Right - $rect.Left)
    $height = [Math]::Max(1, $rect.Bottom - $rect.Top)
    $bmp = New-Object System.Drawing.Bitmap $width, $height
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    try {
        $hdc = $g.GetHdc()
        try {
            # PW_RENDERFULLCONTENT = 2
            if (-not [BoboNative]::PrintWindow($hwnd, $hdc, 2)) {
                throw 'PrintWindow failed'
            }
        } finally {
            $g.ReleaseHdc($hdc)
        }
        $dir = Split-Path -Parent $Path
        if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
        $full = if ([System.IO.Path]::IsPathRooted($Path)) { $Path } else { Join-Path (Get-Location) $Path }
        $bmp.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
        Write-Output "Wrote $full (${width}x${height})"
    } finally {
        $g.Dispose()
        $bmp.Dispose()
    }
}

function Invoke-ClickRel {
    if ($null -eq $X -or $null -eq $Y) { throw 'click-rel requires -X and -Y in 0..1 client fractions' }
    $ctx = Assert-OurInstance
    $hwnd = $ctx.Hwnd
    $client = New-Object BoboNative+RECT
    if (-not [BoboNative]::GetClientRect($hwnd, [ref]$client)) { throw 'GetClientRect failed' }
    $w = $client.Right - $client.Left
    $h = $client.Bottom - $client.Top
    $pt = New-Object BoboNative+POINT
    $pt.X = [int]([Math]::Round($X * $w))
    $pt.Y = [int]([Math]::Round($Y * $h))
    if (-not [BoboNative]::ClientToScreen($hwnd, [ref]$pt)) { throw 'ClientToScreen failed' }
    [void][BoboNative]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 150
    Add-Type -AssemblyName System.Windows.Forms
    [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point $pt.X, $pt.Y
    [BoboNative]::mouse_event([BoboNative]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 40
    [BoboNative]::mouse_event([BoboNative]::MOUSEEVENTF_LEFTUP, 0, 0, 0, [UIntPtr]::Zero)
    Write-Output "Clicked client fraction ($X,$Y) -> screen ($($pt.X),$($pt.Y))"
}

function Invoke-ClickLabelApprox {
    # Calibrated for preferred 900x1080 layout at ~100% DPI. Prefer features that also assert ui-state or cargo tests.
    $map = @{
        'enchant'       = @{ X = 0.22; Y = 0.035 }
        'macro'         = @{ X = 0.36; Y = 0.035 }
        'always-on-top' = @{ X = 0.90; Y = 0.035 }
        'start'         = @{ X = 0.12; Y = 0.94 }
        'stop'          = @{ X = 0.28; Y = 0.94 }
        'new-macro'     = @{ X = 0.10; Y = 0.16 }
    }
    if (-not $Label) { throw 'click-label-approx requires -Label' }
    $coords = $map[$Label]
    $script:X = $coords.X
    $script:Y = $coords.Y
    Invoke-ClickRel
}

function Invoke-ReadUiState {
    $ctx = Assert-OurInstance
    $path = Join-Path $ctx.Session.sandbox_local 'BoBo Companion\ui-state.json'
    if (-not (Test-Path $path)) { throw "ui-state.json missing at $path" }
    Get-Content -Raw -Path $path
}

function Invoke-CargoTest {
    Push-Location $RepoRoot
    try {
        if ($Filter) {
            cargo test $Filter -- --nocapture
        } else {
            cargo test -- --nocapture
        }
    } finally {
        Pop-Location
    }
}

function Invoke-WindowInfo {
    $ctx = Assert-OurInstance
    $rect = New-Object BoboNative+RECT
    [void][BoboNative]::GetWindowRect($ctx.Hwnd, [ref]$rect)
    [ordered]@{
        pid    = $ctx.Pid
        hwnd   = $ctx.Hwnd.ToInt64()
        left   = $rect.Left
        top    = $rect.Top
        right  = $rect.Right
        bottom = $rect.Bottom
        width  = $rect.Right - $rect.Left
        height = $rect.Bottom - $rect.Top
    } | ConvertTo-Json
}

switch ($Command) {
    'doctor' { Invoke-Doctor }
    'launch' { Invoke-Launch }
    'cleanup' { Invoke-Cleanup }
    'screenshot' { Invoke-Screenshot }
    'click-rel' { Invoke-ClickRel }
    'click-label-approx' { Invoke-ClickLabelApprox }
    'read-ui-state' { Invoke-ReadUiState }
    'cargo-test' { Invoke-CargoTest }
    'window-info' { Invoke-WindowInfo }
}
