[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class NibPseudoTerminalChildModes {
    public delegate bool ConsoleCtrlHandler(uint controlType);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr GetStdHandle(int handleKind);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool GetConsoleMode(IntPtr handle, out uint mode);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetConsoleCtrlHandler(ConsoleCtrlHandler handler, bool add);

    private static readonly ConsoleCtrlHandler ctrlCShield = HandleConsoleControl;

    private static bool HandleConsoleControl(uint controlType) {
        return controlType == 0;
    }

    public static bool InstallCtrlCShield() {
        return SetConsoleCtrlHandler(ctrlCShield, true);
    }

    public static void RemoveCtrlCShield() {
        SetConsoleCtrlHandler(ctrlCShield, false);
    }
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
    $processMarker = $env:NIB_WINDOWS_PTY_PROCESS_MARKER
    Remove-Item Env:NIB_WINDOWS_PTY_CHILD_REQUEST -ErrorAction SilentlyContinue
    Remove-Item Env:NIB_WINDOWS_PTY_EXIT_MARKER -ErrorAction SilentlyContinue
    Remove-Item Env:NIB_WINDOWS_PTY_MODE_MARKER -ErrorAction SilentlyContinue
    Remove-Item Env:NIB_WINDOWS_PTY_PROCESS_MARKER -ErrorAction SilentlyContinue
    if ([string]::IsNullOrWhiteSpace($encodedRequest) -or
        [string]::IsNullOrWhiteSpace($exitMarker) -or
        [string]::IsNullOrWhiteSpace($modeMarker) -or
        [string]::IsNullOrWhiteSpace($processMarker)) {
        throw "Windows pseudoterminal child request is missing"
    }

    $requestJson = [Text.Encoding]::UTF8.GetString(
        [Convert]::FromBase64String($encodedRequest)
    )
    $request = $requestJson | ConvertFrom-Json
    $arguments = [string[]]@($request.arguments)
    $workingDirectory = [string]$request.working_directory
    if ([string]::IsNullOrWhiteSpace([string]$request.executable)) {
        throw "Windows pseudoterminal child request is invalid"
    }
    if (-not [string]::IsNullOrEmpty($workingDirectory)) {
        if ($workingDirectory.Length -gt 32768 -or
            -not [IO.Path]::IsPathFullyQualified($workingDirectory) -or
            -not (Test-Path -LiteralPath $workingDirectory -PathType Container)) {
            throw "Windows pseudoterminal working directory is invalid"
        }
        Set-Location -LiteralPath $workingDirectory
    }

    $consoleModesBefore = Get-NibPseudoTerminalChildModes
    if (-not [NibPseudoTerminalChildModes]::InstallCtrlCShield()) {
        throw "Unable to protect the pseudoterminal adapter from child Ctrl+C"
    }
    [Console]::Out.WriteLine("$processMarker$PID")
    try {
        # Keep PowerShell's native-command pipeline out of the Ctrl+C path.
        # The target and this adapter handle the shared console event independently.
        $startInfo = [Diagnostics.ProcessStartInfo]::new()
        $startInfo.FileName = [string]$request.executable
        $startInfo.UseShellExecute = $false
        $startInfo.WorkingDirectory = (Get-Location).ProviderPath
        foreach ($argument in $arguments) {
            $startInfo.ArgumentList.Add($argument)
        }
        $child = [Diagnostics.Process]::new()
        try {
            $child.StartInfo = $startInfo
            if (-not $child.Start()) {
                throw "Unable to start the pseudoterminal target process"
            }
            $child.WaitForExit()
            $childExitCode = [int]$child.ExitCode
        } finally {
            $child.Dispose()
        }
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
        [NibPseudoTerminalChildModes]::RemoveCtrlCShield()
    }
    [Console]::Out.WriteLine("$exitMarker$childExitCode")
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 255
}
