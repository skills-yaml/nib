[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

try {
    . (Join-Path $PSScriptRoot "windows-pseudoterminal-output.ps1")
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
    $allowInterruptedChildWithoutExitMarker = [bool]$request.allow_interrupted_child_without_exit_marker
    if ([string]::IsNullOrWhiteSpace([string]$request.executable) -or
        $timeoutMilliseconds -lt 1) {
        throw "Windows pseudoterminal host request is invalid"
    }
    if ($inputChunks.Count -gt 64) {
        throw "Windows pseudoterminal input exceeds the 64 chunk limit"
    }
    if ($allowInterruptedChildWithoutExitMarker -and
        ($inputChunks.Count -ne 1 -or [string]$inputChunks[0].text -ne [string][char]3)) {
        throw "Missing-exit interruption qualification requires one native Ctrl+C input"
    }
    $totalInputBytes = 0
    $totalDelayMilliseconds = 0L
    foreach ($chunk in $inputChunks) {
        $chunkBytes = [Text.Encoding]::UTF8.GetByteCount([string]$chunk.text)
        $delayMilliseconds = [int]$chunk.delay_ms
        $promptBytes = [Text.Encoding]::UTF8.GetByteCount([string]$chunk.wait_for_output)
        if ($chunkBytes -gt 4096 -or
            $promptBytes -gt 4096 -or
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
        $stdoutCapture = [Nib.WindowsPseudoTerminal.OutputCapture]::new($process.StandardOutput)
        $stdoutTask = $stdoutCapture.Completion
        $stderrTask = $process.StandardError.ReadToEndAsync()
        foreach ($chunk in $inputChunks) {
            if (-not [string]::IsNullOrEmpty([string]$chunk.wait_for_output)) {
                Wait-NibWindowsPseudoTerminalOutput `
                    -Capture $stdoutCapture `
                    -Expected ([string]$chunk.wait_for_output) `
                    -Stopwatch $stopwatch `
                    -TimeoutMilliseconds $timeoutMilliseconds
            }
            $delayMilliseconds = [int]$chunk.delay_ms
            Wait-NibWindowsPseudoTerminalInputDelay `
                -DelayMilliseconds $delayMilliseconds `
                -Stopwatch $stopwatch `
                -TimeoutMilliseconds $timeoutMilliseconds
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

        $stdoutTask.GetAwaiter().GetResult() | Out-Null
        $output = $stdoutCapture.Text
        $hostError = $stderrTask.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) {
            throw "Windows headless console host failed: $($hostError.Trim())"
        }

        $markerPattern = [regex]::Escape($exitMarker) + "(?<code>-?[0-9]+)"
        $markerMatches = [regex]::Matches($output, $markerPattern)
        $interruptedChildWithoutExitMarker = $false
        if ($markerMatches.Count -eq 1) {
            $exitCode = [int]$markerMatches[0].Groups["code"].Value
        } elseif ($markerMatches.Count -eq 0 -and
            $allowInterruptedChildWithoutExitMarker -and
            $output.Contains("Run cancelled.")) {
            # Native Ctrl+C reaches both nib and its enclosing PowerShell process.
            # PowerShell can therefore run the adapter's finally block (publishing
            # restoration evidence) and then stop before the following exit marker.
            # Normalize that strictly qualified shell-interruption shape to a
            # non-success result without weakening ordinary child-exit validation.
            $exitCode = 1
            $interruptedChildWithoutExitMarker = $true
        } else {
            throw "Windows headless console child did not report one exit marker: $output"
        }
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
            if ($modeParts.Count -eq 3 -and
                $modeParts[1] -match '^[01A-Fa-f0-9-]{27}$' -and
                $modeParts[2] -match '^[01A-Fa-f0-9-]{27}$') {
                throw "Windows headless console child did not restore its console modes: before=$($modeParts[1]) after=$($modeParts[2])"
            }
            throw "Windows headless console child returned invalid console-mode evidence"
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
        interrupted_child_without_exit_marker = $interruptedChildWithoutExitMarker
    } | ConvertTo-Json -Compress -Depth 6))
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    while ($true) {
        Start-Sleep -Seconds 60
    }
}
