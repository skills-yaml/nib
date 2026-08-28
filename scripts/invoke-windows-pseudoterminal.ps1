if ($null -eq ("Nib.WindowsPseudoTerminal.NativeMethods" -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

namespace Nib.WindowsPseudoTerminal {
    public static class NativeMethods {
        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern IntPtr GetStdHandle(int handleKind);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool GetConsoleMode(IntPtr handle, out uint mode);
    }
}
'@
}

function Get-NibWindowsConsoleModeSnapshot {
    $snapshot = [ordered]@{}
    foreach ($entry in @(
        @{ Name = "input"; Kind = -10 },
        @{ Name = "output"; Kind = -11 },
        @{ Name = "error"; Kind = -12 }
    )) {
        $handle = [Nib.WindowsPseudoTerminal.NativeMethods]::GetStdHandle($entry.Kind)
        [uint32]$mode = 0
        $valid = $handle -ne [IntPtr]::Zero -and
            $handle -ne [IntPtr](-1) -and
            [Nib.WindowsPseudoTerminal.NativeMethods]::GetConsoleMode($handle, [ref]$mode)
        $snapshot[$entry.Name] = [ordered]@{
            valid = [bool]$valid
            mode = if ($valid) { [uint32]$mode } else { $null }
        }
    }
    return [pscustomobject]$snapshot
}

function Test-NibWindowsConsoleModesEqual {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Before,

        [Parameter(Mandatory = $true)]
        [object]$After
    )

    return (
        ($Before | ConvertTo-Json -Compress -Depth 4) -eq
        ($After | ConvertTo-Json -Compress -Depth 4)
    )
}

function Invoke-WindowsPseudoTerminal {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string]$Executable,

        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,

        [Parameter(Mandatory = $true)]
        [ValidateRange(1, 300000)]
        [int]$TimeoutMilliseconds,

        [object[]]$InputChunks = @(),

        [ValidateRange(5000, 60000)]
        [int]$HostGraceMilliseconds = 40000
    )

    if ($InputChunks.Count -gt 64) {
        throw "Windows pseudoterminal input exceeds the 64 chunk limit"
    }
    $normalizedChunks = [Collections.Generic.List[object]]::new()
    $totalInputBytes = 0
    $totalDelayMilliseconds = 0L
    foreach ($chunk in $InputChunks) {
        if ($null -eq $chunk -or $null -eq $chunk.PSObject.Properties["Text"]) {
            throw "Windows pseudoterminal input chunk is missing text"
        }
        $text = [string]$chunk.Text
        $delayMilliseconds = if ($null -eq $chunk.PSObject.Properties["DelayMilliseconds"]) {
            0
        } else {
            [int]$chunk.DelayMilliseconds
        }
        $chunkBytes = [Text.Encoding]::UTF8.GetByteCount($text)
        if ($chunkBytes -gt 4096) {
            throw "Windows pseudoterminal input chunk exceeds 4096 bytes"
        }
        if ($delayMilliseconds -lt 0 -or $delayMilliseconds -gt 10000) {
            throw "Windows pseudoterminal input delay is outside 0..10000 milliseconds"
        }
        $totalInputBytes += $chunkBytes
        $totalDelayMilliseconds += $delayMilliseconds
        if ($totalInputBytes -gt 32768) {
            throw "Windows pseudoterminal input exceeds 32768 bytes"
        }
        if ($totalDelayMilliseconds -ge $TimeoutMilliseconds) {
            throw "Windows pseudoterminal input delays consume the child timeout"
        }
        $normalizedChunks.Add([ordered]@{
            text = $text
            delay_ms = $delayMilliseconds
        })
    }

    $requestJson = @{
        executable = $Executable
        arguments = @($Arguments)
        timeout_ms = $TimeoutMilliseconds
        input_chunks = @($normalizedChunks)
    } | ConvertTo-Json -Compress -Depth 5
    $encodedRequest = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($requestJson))

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = (Get-Process -Id $PID).Path
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.ArgumentList.Add("-NoLogo")
    $startInfo.ArgumentList.Add("-NoProfile")
    $startInfo.ArgumentList.Add("-NonInteractive")
    $startInfo.ArgumentList.Add("-File")
    $startInfo.ArgumentList.Add((Join-Path $PSScriptRoot "host-windows-pseudoterminal.ps1"))
    $startInfo.Environment["NIB_WINDOWS_PTY_REQUEST"] = $encodedRequest

    $outerModesBefore = Get-NibWindowsConsoleModeSnapshot
    $outerModesAfter = $null
    $outerModesRestored = $false
    $result = $null
    $pendingError = $null
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $started = $false
    try {
        if (-not $process.Start()) {
            throw "Unable to start the bounded Windows pseudoterminal host"
        }
        $started = $true
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $hostTimeout = [Math]::Min(
            [int]::MaxValue,
            [long]$TimeoutMilliseconds + $HostGraceMilliseconds
        )
        if (-not $process.WaitForExit([int]$hostTimeout)) {
            $process.Kill($true)
            if (-not $process.WaitForExit(5000)) {
                throw "Unable to stop the timed-out Windows pseudoterminal host"
            }
            $stdoutTask.GetAwaiter().GetResult() | Out-Null
            $stderrTask.GetAwaiter().GetResult() | Out-Null
            throw "Windows pseudoterminal host exceeded its bounded timeout"
        }

        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) {
            throw "Windows pseudoterminal host failed: $($stderr.Trim())"
        }

        $response = $stdout | ConvertFrom-Json
        if ($null -eq $response -or
            $null -eq $response.exit_code -or
            $null -eq $response.output -or
            $null -eq $response.console_modes_restored -or
            $null -eq $response.console_modes_before -or
            $null -eq $response.console_modes_after) {
            throw "Windows pseudoterminal host returned an invalid response"
        }
        if (-not [bool]$response.console_modes_restored) {
            throw "Windows pseudoterminal child did not restore its console modes"
        }
        $result = [pscustomobject]@{
            ExitCode = [int]$response.exit_code
            Output = [string]$response.output
            ChildConsoleModesBefore = $response.console_modes_before
            ChildConsoleModesAfter = $response.console_modes_after
            ChildConsoleModesRestored = [bool]$response.console_modes_restored
        }
    } catch {
        $pendingError = $_
    } finally {
        if ($started -and -not $process.HasExited) {
            $process.Kill($true)
            $process.WaitForExit(5000) | Out-Null
        }
        $process.Dispose()
        $outerModesAfter = Get-NibWindowsConsoleModeSnapshot
        $outerModesRestored = Test-NibWindowsConsoleModesEqual `
            -Before $outerModesBefore `
            -After $outerModesAfter
    }

    $modeEvidence = @{
        before = $outerModesBefore
        after = $outerModesAfter
        restored = [bool]$outerModesRestored
    } | ConvertTo-Json -Compress -Depth 6
    if (-not $outerModesRestored) {
        throw "Windows pseudoterminal host did not restore caller console modes: $modeEvidence"
    }
    if ($null -ne $pendingError) {
        $pendingError.Exception.Data["NibConsoleModeEvidence"] = $modeEvidence
        throw $pendingError.Exception
    }

    $result | Add-Member -NotePropertyName CallerConsoleModesBefore -NotePropertyValue $outerModesBefore
    $result | Add-Member -NotePropertyName CallerConsoleModesAfter -NotePropertyValue $outerModesAfter
    $result | Add-Member -NotePropertyName ConsoleModesRestored -NotePropertyValue $true
    return $result
}
