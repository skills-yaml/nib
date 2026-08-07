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

        [ValidateRange(5000, 60000)]
        [int]$HostGraceMilliseconds = 40000
    )

    $requestJson = @{
        executable = $Executable
        arguments = @($Arguments)
        timeout_ms = $TimeoutMilliseconds
    } | ConvertTo-Json -Compress
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
            $null -eq $response.output) {
            throw "Windows pseudoterminal host returned an invalid response"
        }
        return [pscustomobject]@{
            ExitCode = [int]$response.exit_code
            Output = [string]$response.output
        }
    } finally {
        if ($started -and -not $process.HasExited) {
            $process.Kill($true)
            $process.WaitForExit(5000) | Out-Null
        }
        $process.Dispose()
    }
}
