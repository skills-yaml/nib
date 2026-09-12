[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class NibPseudoTerminalChildModes {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr GetStdHandle(int handleKind);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool GetConsoleMode(IntPtr handle, out uint mode);
}
'@

function Get-NibPseudoTerminalChildModes {
    $snapshot = [ordered]@{}
    foreach ($entry in @(
        @{ Name = "input"; Kind = -10 },
        @{ Name = "output"; Kind = -11 },
        @{ Name = "error"; Kind = -12 }
    )) {
        $handle = [NibPseudoTerminalChildModes]::GetStdHandle($entry.Kind)
        [uint32]$mode = 0
        $valid = $handle -ne [IntPtr]::Zero -and
            $handle -ne [IntPtr](-1) -and
            [NibPseudoTerminalChildModes]::GetConsoleMode($handle, [ref]$mode)
        $snapshot[$entry.Name] = [ordered]@{
            valid = [bool]$valid
            mode = if ($valid) { [uint32]$mode } else { $null }
        }
    }
    return [pscustomobject]$snapshot
}

function ConvertTo-NibPseudoTerminalModeToken {
    param([Parameter(Mandatory = $true)][object]$Snapshot)

    $parts = foreach ($name in @("input", "output", "error")) {
        $entry = $Snapshot.$name
        if ([bool]$entry.valid) {
            "1{0:X8}" -f [uint32]$entry.mode
        } else {
            "0--------"
        }
    }
    return $parts -join ""
}

try {
    $encodedRequest = $env:NIB_WINDOWS_PTY_CHILD_REQUEST
    $exitMarker = $env:NIB_WINDOWS_PTY_EXIT_MARKER
    $modeMarker = $env:NIB_WINDOWS_PTY_MODE_MARKER
    Remove-Item Env:NIB_WINDOWS_PTY_CHILD_REQUEST -ErrorAction SilentlyContinue
    Remove-Item Env:NIB_WINDOWS_PTY_EXIT_MARKER -ErrorAction SilentlyContinue
    Remove-Item Env:NIB_WINDOWS_PTY_MODE_MARKER -ErrorAction SilentlyContinue
    if ([string]::IsNullOrWhiteSpace($encodedRequest) -or
        [string]::IsNullOrWhiteSpace($exitMarker) -or
        [string]::IsNullOrWhiteSpace($modeMarker)) {
        throw "Windows pseudoterminal child request is missing"
    }

    $requestJson = [Text.Encoding]::UTF8.GetString(
        [Convert]::FromBase64String($encodedRequest)
    )
    $request = $requestJson | ConvertFrom-Json
    $arguments = [string[]]@($request.arguments)
    if ([string]::IsNullOrWhiteSpace([string]$request.executable)) {
        throw "Windows pseudoterminal child request is invalid"
    }

    $consoleModesBefore = Get-NibPseudoTerminalChildModes
    try {
        & ([string]$request.executable) @arguments
        $childExitCode = [int]$LASTEXITCODE
    } finally {
        $consoleModesAfter = Get-NibPseudoTerminalChildModes
        $consoleModesRestored = (
            ($consoleModesBefore | ConvertTo-Json -Compress -Depth 4) -eq
            ($consoleModesAfter | ConvertTo-Json -Compress -Depth 4)
        )
        $beforeToken = ConvertTo-NibPseudoTerminalModeToken $consoleModesBefore
        $afterToken = ConvertTo-NibPseudoTerminalModeToken $consoleModesAfter
        $restoredToken = if ($consoleModesRestored) { "1" } else { "0" }
        $modeText = "{0}:{1}:{2}" -f $restoredToken, $beforeToken, $afterToken
        $encodedModes = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes($modeText)
        )
        [Console]::Out.WriteLine("$modeMarker$encodedModes")
    }
    [Console]::Out.WriteLine("$exitMarker$childExitCode")
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 255
}
