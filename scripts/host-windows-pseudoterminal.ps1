[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

try {
    $encodedRequest = $env:NIB_WINDOWS_PTY_REQUEST
    Remove-Item Env:NIB_WINDOWS_PTY_REQUEST -ErrorAction SilentlyContinue
    if ([string]::IsNullOrWhiteSpace($encodedRequest)) {
        throw "Windows pseudoterminal host request is missing"
    }

    $requestJson = [Text.Encoding]::UTF8.GetString(
        [Convert]::FromBase64String($encodedRequest)
    )
    $request = $requestJson | ConvertFrom-Json
    $arguments = [string[]]@($request.arguments)
    $timeoutMilliseconds = [int]$request.timeout_ms
    $inputChunks = @($request.input_chunks)
    if ([string]::IsNullOrWhiteSpace([string]$request.executable) -or
        $timeoutMilliseconds -lt 1) {
        throw "Windows pseudoterminal host request is invalid"
    }
    if ($inputChunks.Count -gt 64) {
        throw "Windows pseudoterminal input exceeds the 64 chunk limit"
    }
    $totalInputBytes = 0
    $totalDelayMilliseconds = 0L
    foreach ($chunk in $inputChunks) {
        $chunkBytes = [Text.Encoding]::UTF8.GetByteCount([string]$chunk.text)
        $delayMilliseconds = [int]$chunk.delay_ms
        if ($chunkBytes -gt 4096 -or
            $delayMilliseconds -lt 0 -or
            $delayMilliseconds -gt 10000) {
            throw "Windows pseudoterminal input chunk is invalid"
        }
        $totalInputBytes += $chunkBytes
        $totalDelayMilliseconds += $delayMilliseconds
    }
    if ($totalInputBytes -gt 32768 -or
        $totalDelayMilliseconds -ge $timeoutMilliseconds) {
        throw "Windows pseudoterminal input request is unbounded"
    }

    $conhostPath = Join-Path $env:SystemRoot "System32\conhost.exe"
    $childPath = Join-Path $PSScriptRoot "start-windows-pseudoterminal-child.ps1"
    if (-not (Test-Path -LiteralPath $conhostPath -PathType Leaf) -or
        -not (Test-Path -LiteralPath $childPath -PathType Leaf)) {
        throw "Windows headless console host or child adapter is missing"
    }

    $childRequest = @{
        executable = [string]$request.executable
        arguments = @($arguments)
    } | ConvertTo-Json -Compress
    $encodedChildRequest = [Convert]::ToBase64String(
        [Text.Encoding]::UTF8.GetBytes($childRequest)
    )
    $exitMarker = "NIB_PSEUDOTERMINAL_EXIT_$([guid]::NewGuid().ToString('N')):"
    # Keep marker plus compact mode evidence below the configured console width so
    # conhost cannot visually wrap the Base64 payload.
    $modeMarker = "NM_$([guid]::NewGuid().ToString('N')):"

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $conhostPath
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.ArgumentList.Add("--headless")
    $startInfo.ArgumentList.Add("--width")
    $startInfo.ArgumentList.Add("120")
    $startInfo.ArgumentList.Add("--height")
    $startInfo.ArgumentList.Add("30")
    $startInfo.ArgumentList.Add("--")
    $startInfo.ArgumentList.Add((Get-Process -Id $PID).Path)
    $startInfo.ArgumentList.Add("-NoLogo")
    $startInfo.ArgumentList.Add("-NoProfile")
    $startInfo.ArgumentList.Add("-NonInteractive")
    $startInfo.ArgumentList.Add("-File")
    $startInfo.ArgumentList.Add($childPath)
    $startInfo.Environment["NIB_WINDOWS_PTY_CHILD_REQUEST"] = $encodedChildRequest
    $startInfo.Environment["NIB_WINDOWS_PTY_EXIT_MARKER"] = $exitMarker
    $startInfo.Environment["NIB_WINDOWS_PTY_MODE_MARKER"] = $modeMarker

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $started = $false
    try {
        if (-not $process.Start()) {
            throw "Unable to start the Windows headless console host"
        }
        $started = $true
        $stopwatch = [Diagnostics.Stopwatch]::StartNew()
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        foreach ($chunk in $inputChunks) {
            $delayMilliseconds = [int]$chunk.delay_ms
            if ($delayMilliseconds -gt 0) {
                Start-Sleep -Milliseconds $delayMilliseconds
            }
            $remainingMilliseconds = $timeoutMilliseconds - [int]$stopwatch.ElapsedMilliseconds
            if ($remainingMilliseconds -lt 1) {
                throw "The Windows pseudoterminal child exceeded its timeout"
            }
            $writeTask = $process.StandardInput.WriteAsync([string]$chunk.text)
            if (-not $writeTask.Wait($remainingMilliseconds)) {
                throw "Timed out while writing Windows pseudoterminal input"
            }
            $writeTask.GetAwaiter().GetResult() | Out-Null
            $process.StandardInput.Flush()
        }
        # Keep the headless-console input pipe open until the console child exits.
        # conhost treats pipe EOF as terminal closure, so closing it here races a
        # cold child before it can publish its exit and mode markers. Input remains
        # bounded above, and the same absolute deadline still kills the whole tree.
        $remainingMilliseconds = $timeoutMilliseconds - [int]$stopwatch.ElapsedMilliseconds
        if ($remainingMilliseconds -lt 1 -or
            -not $process.WaitForExit($remainingMilliseconds)) {
            $process.Kill($true)
            if (-not $process.WaitForExit(5000)) {
                throw "Unable to stop the timed-out Windows headless console host"
            }
            throw "The Windows pseudoterminal child exceeded its timeout"
        }
        if (-not $stdoutTask.Wait(5000) -or -not $stderrTask.Wait(5000)) {
            throw "Timed out while draining Windows pseudoterminal output"
        }

        $output = $stdoutTask.GetAwaiter().GetResult()
        $hostError = $stderrTask.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) {
            throw "Windows headless console host failed: $($hostError.Trim())"
        }

        $markerPattern = [regex]::Escape($exitMarker) + "(?<code>-?[0-9]+)"
        $markerMatches = [regex]::Matches($output, $markerPattern)
        if ($markerMatches.Count -ne 1) {
            throw "Windows headless console child did not report one exit marker: $output"
        }
        $exitCode = [int]$markerMatches[0].Groups["code"].Value
        $capturedOutput = [regex]::Replace($output, $markerPattern + "\r?\n?", "")

        $modePattern = [regex]::Escape($modeMarker) + "(?<evidence>[A-Za-z0-9+/=]+)"
        $modeMatches = [regex]::Matches($capturedOutput, $modePattern)
        if ($modeMatches.Count -ne 1) {
            throw "Windows headless console child did not report one mode marker"
        }
        $modeText = [Text.Encoding]::UTF8.GetString(
            [Convert]::FromBase64String($modeMatches[0].Groups["evidence"].Value)
        )
        $modeParts = $modeText.Split(":")
        if ($modeParts.Count -ne 3 -or
            $modeParts[0] -ne "1" -or
            $modeParts[1] -ne $modeParts[2]) {
            throw "Windows headless console child did not restore its console modes"
        }
        $consoleModesBefore = $modeParts[1]
        $consoleModesAfter = $modeParts[2]
        $capturedOutput = [regex]::Replace(
            $capturedOutput,
            $modePattern + "\r?\n?",
            ""
        )
    } finally {
        if ($started -and -not $process.HasExited) {
            $process.Kill($true)
            $process.WaitForExit(5000) | Out-Null
        }
        $process.Dispose()
    }
    [Console]::Out.WriteLine((@{
        exit_code = $exitCode
        output = $capturedOutput
        console_modes_before = $consoleModesBefore
        console_modes_after = $consoleModesAfter
        console_modes_restored = $true
    } | ConvertTo-Json -Compress -Depth 6))
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    while ($true) {
        Start-Sleep -Seconds 60
    }
}
