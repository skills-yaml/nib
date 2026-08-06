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
    if ([string]::IsNullOrWhiteSpace([string]$request.executable) -or
        $timeoutMilliseconds -lt 1) {
        throw "Windows pseudoterminal host request is invalid"
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

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $started = $false
    try {
        if (-not $process.Start()) {
            throw "Unable to start the Windows headless console host"
        }
        $started = $true
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($timeoutMilliseconds)) {
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
    } | ConvertTo-Json -Compress))
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    while ($true) {
        Start-Sleep -Seconds 60
    }
}
