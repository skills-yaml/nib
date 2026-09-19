[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "windows-pseudoterminal-output.ps1")

# Parsing and the asynchronous output pump are portable; real console modes and
# process-tree cleanup remain covered by test-windows-pseudoterminal.ps1.
foreach ($name in @(
    "windows-pseudoterminal-output.ps1",
    "invoke-windows-pseudoterminal.ps1",
    "host-windows-pseudoterminal.ps1",
    "start-windows-pseudoterminal-child.ps1",
    "test-windows-pseudoterminal.ps1",
    "check-interactive-release.ps1"
)) {
    $tokens = $null
    $parseErrors = $null
    [Management.Automation.Language.Parser]::ParseFile(
        (Join-Path $PSScriptRoot $name), [ref]$tokens, [ref]$parseErrors
    ) | Out-Null
    if ($parseErrors.Count -ne 0) { throw "PowerShell parse failed for ${name}: $parseErrors" }
}

# Model a final append racing the first observation, followed immediately by EOF.
$finalAppend = [pscustomobject]@{ Completion = [Threading.Tasks.Task]::CompletedTask; Checks = 0 }
$finalAppend | Add-Member -MemberType ScriptMethod -Name Contains -Value {
    param([string]$expected)
    $this.Checks++
    return $this.Checks -gt 1
}
Wait-NibWindowsPseudoTerminalOutput -Capture $finalAppend -Expected "final-prompt" `
    -Stopwatch ([Diagnostics.Stopwatch]::StartNew()) -TimeoutMilliseconds 1000
if ($finalAppend.Checks -ne 2) { throw "Final prompt was not rechecked after EOF" }

$startInfo = [Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = (Get-Process -Id $PID).Path
$startInfo.UseShellExecute = $false
$startInfo.RedirectStandardInput = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
foreach ($argument in @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", @'
[Console]::Write("prompt-")
Start-Sleep -Milliseconds 150
[Console]::Write("ready")
$line = [Console]::ReadLine()
[Console]::WriteLine("received:" + $line)
'@)) { $startInfo.ArgumentList.Add($argument) }
$process = [Diagnostics.Process]::new()
$process.StartInfo = $startInfo
$started = $false
try {
    $started = $process.Start()
    if (-not $started) { throw "Failed to start portable output probe" }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $capture = [Nib.WindowsPseudoTerminal.OutputCapture]::new($process.StandardOutput)
    $errors = $process.StandardError.ReadToEndAsync()
    Wait-NibWindowsPseudoTerminalOutput -Capture $capture -Expected "prompt-ready" `
        -Stopwatch $clock -TimeoutMilliseconds 10000
    if ($process.HasExited -or $capture.Completion.IsCompleted) {
        throw "Prompt matching waited for EOF instead of live output"
    }

    # A prompt may consume most of the shared timeout before a requested delay.
    $delayStart = $clock.ElapsedMilliseconds
    $delayError = $null
    try {
        Wait-NibWindowsPseudoTerminalInputDelay -DelayMilliseconds 5000 `
            -Stopwatch $clock -TimeoutMilliseconds ([int]$delayStart + 100)
    } catch { $delayError = $_.Exception.Message }
    if ($delayError -ne "Windows pseudoterminal input delay exceeds the remaining child timeout" -or
        $clock.ElapsedMilliseconds - $delayStart -ge 1000 -or $capture.Contains("received:")) {
        throw "Prompt plus delay exhausted the deadline without failing before input"
    }

    $missingStart = $clock.ElapsedMilliseconds
    $missingError = $null
    try {
        Wait-NibWindowsPseudoTerminalOutput -Capture $capture -Expected "missing-prompt" `
            -Stopwatch $clock -TimeoutMilliseconds ([int]$missingStart + 200)
    } catch { $missingError = $_.Exception.Message }
    if ($missingError -ne "Timed out waiting for Windows pseudoterminal prompt: missing-prompt" -or
        $clock.ElapsedMilliseconds - $missingStart -ge 2000 -or
        $capture.Contains("received:")) {
        throw "Missing prompt did not stop at its absolute deadline before input"
    }

    $process.StandardInput.WriteLine("bounded-input")
    $process.StandardInput.Flush()
    if (-not $process.WaitForExit(10000) -or -not $capture.Completion.Wait(5000) -or
        -not $errors.Wait(5000)) { throw "Portable output probe did not terminate" }
    $capture.Completion.GetAwaiter().GetResult() | Out-Null
    if ($process.ExitCode -ne 0 -or
        $capture.Text -ne "prompt-readyreceived:bounded-input$([Environment]::NewLine)") {
        throw "Output capture did not preserve the complete chunked stream"
    }
    $closedError = $null
    try {
        Wait-NibWindowsPseudoTerminalOutput -Capture $capture -Expected "absent-after-exit" `
            -Stopwatch $clock -TimeoutMilliseconds ([int]$clock.ElapsedMilliseconds + 10000)
    } catch { $closedError = $_.Exception.Message }
    if ($closedError -ne "Windows pseudoterminal output ended before the expected prompt: absent-after-exit") {
        throw "Closed output did not fail the missing prompt immediately"
    }
} finally {
    if ($started -and -not $process.HasExited) {
        $process.Kill($true)
        $process.WaitForExit(5000) | Out-Null
    }
    $process.Dispose()
}
Write-Output "Windows pseudoterminal portable output checks passed."
