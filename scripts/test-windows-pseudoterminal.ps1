[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

. (Join-Path $PSScriptRoot "invoke-windows-pseudoterminal.ps1")

$pwshPath = (Get-Process -Id $PID).Path
$probe = @'
if ([Console]::IsErrorRedirected) { exit 41 }
[Console]::Error.WriteLine("NIB_PSEUDOTERMINAL_READY")
'@
$result = Invoke-WindowsPseudoTerminal `
    -Executable $pwshPath `
    -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $probe) `
    -TimeoutMilliseconds 30000
if ($result.ExitCode -ne 0 -or
    -not $result.Output.Contains("NIB_PSEUDOTERMINAL_READY") -or
    -not $result.ConsoleModesRestored -or
    -not $result.ChildConsoleModesRestored) {
    throw "Windows pseudoterminal did not expose an interactive stderr handle: $($result.Output)"
}

$inputProbe = @'
$line = [Console]::ReadLine()
[Console]::WriteLine("NIB_PSEUDOTERMINAL_INPUT:" + $line)
'@
$inputResult = Invoke-WindowsPseudoTerminal `
    -Executable $pwshPath `
    -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $inputProbe) `
    -InputChunks @(
        [pscustomobject]@{ Text = "bounded-input`r`n"; DelayMilliseconds = 250 }
    ) `
    -TimeoutMilliseconds 30000
if ($inputResult.ExitCode -ne 0 -or
    -not $inputResult.Output.Contains("NIB_PSEUDOTERMINAL_INPUT:bounded-input") -or
    -not $inputResult.ConsoleModesRestored -or
    -not $inputResult.ChildConsoleModesRestored) {
    throw "Windows pseudoterminal did not preserve bounded delayed input"
}

$exitProbe = @'
[Console]::WriteLine("NIB_PSEUDOTERMINAL_EXIT")
exit 23
'@
$exitResult = Invoke-WindowsPseudoTerminal `
    -Executable $pwshPath `
    -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $exitProbe) `
    -TimeoutMilliseconds 30000
if ($exitResult.ExitCode -ne 23 -or
    -not $exitResult.Output.Contains("NIB_PSEUDOTERMINAL_EXIT") -or
    -not $exitResult.ConsoleModesRestored -or
    -not $exitResult.ChildConsoleModesRestored) {
    throw "Windows pseudoterminal did not preserve output and exit status"
}

$temporaryBase = if ([string]::IsNullOrWhiteSpace($env:RUNNER_TEMP)) {
    [IO.Path]::GetTempPath()
} else {
    $env:RUNNER_TEMP
}
$timeoutRoot = Join-Path $temporaryBase ("nib-pty-timeout-" + [guid]::NewGuid().ToString("N"))
$descendantPidFile = Join-Path $timeoutRoot "descendant.pid"
$descendantReadyFile = Join-Path $timeoutRoot "descendant.ready"
$probeArmedFile = Join-Path $timeoutRoot "probe.armed"
New-Item -ItemType Directory -Path $timeoutRoot -Force | Out-Null
$env:NIB_PTY_DESCENDANT_PID_FILE = $descendantPidFile
$env:NIB_PTY_DESCENDANT_READY_FILE = $descendantReadyFile
$env:NIB_PTY_PROBE_ARMED_FILE = $probeArmedFile
$env:NIB_PTY_RESISTANT_CHILD_SCRIPT = Join-Path `
    $PSScriptRoot `
    "test-windows-pseudoterminal-resistant-child.ps1"
$timeoutProbe = @'
$pwsh = (Get-Process -Id $PID).Path
$descendant = Start-Process -PassThru -FilePath $pwsh -ArgumentList @(
    "-NoLogo",
    "-NoProfile",
    "-NonInteractive",
    "-File",
    ('"' + $env:NIB_PTY_RESISTANT_CHILD_SCRIPT + '"')
)
for ($attempt = 1; $attempt -le 150; $attempt++) {
    if (Test-Path -LiteralPath $env:NIB_PTY_DESCENDANT_READY_FILE -PathType Leaf) { break }
    if ($descendant.HasExited) { exit 42 }
    Start-Sleep -Milliseconds 100
}
if (-not (Test-Path -LiteralPath $env:NIB_PTY_DESCENDANT_READY_FILE -PathType Leaf)) { exit 43 }
[IO.File]::WriteAllText($env:NIB_PTY_PROBE_ARMED_FILE, "armed")
while ($true) {
    if ($descendant.HasExited) { exit 44 }
    Start-Sleep -Milliseconds 100
}
'@
$stopwatch = [Diagnostics.Stopwatch]::StartNew()
$hostElapsedMilliseconds = $null
$timeoutFailed = $false
$timeoutResult = $null
$descendantPid = $null
try {
    try {
        $timeoutResult = Invoke-WindowsPseudoTerminal `
            -Executable $pwshPath `
            -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $timeoutProbe) `
            -TimeoutMilliseconds 20000 `
            -HostGraceMilliseconds 7000
    } catch {
        if ($_.Exception.Message -ne "Windows pseudoterminal host exceeded its bounded timeout") {
            throw
        }
        $modeEvidence = [string]$_.Exception.Data["NibConsoleModeEvidence"]
        if ([string]::IsNullOrWhiteSpace($modeEvidence) -or
            -not [bool](($modeEvidence | ConvertFrom-Json).restored)) {
            throw "Windows pseudoterminal timeout did not prove caller console restoration"
        }
        $timeoutFailed = $true
    } finally {
        $hostElapsedMilliseconds = $stopwatch.ElapsedMilliseconds
    }
    if (-not $timeoutFailed) {
        throw "Windows pseudoterminal timeout probe exited early with status $($timeoutResult.ExitCode): $($timeoutResult.Output)"
    }
    if ($hostElapsedMilliseconds -lt 18000 -or
        $hostElapsedMilliseconds -ge 35000) {
        throw "Windows pseudoterminal timeout fell outside its bounded window: $($hostElapsedMilliseconds)ms"
    }
    if (-not (Test-Path -LiteralPath $descendantPidFile -PathType Leaf)) {
        throw "Windows pseudoterminal timeout did not exercise a lingering descendant"
    }
    if (-not (Test-Path -LiteralPath $descendantReadyFile -PathType Leaf)) {
        throw "Windows pseudoterminal timeout did not observe a ready resistant descendant"
    }
    if (-not (Test-Path -LiteralPath $probeArmedFile -PathType Leaf)) {
        throw "Windows pseudoterminal timeout probe did not arm after descendant readiness"
    }
    $descendantPid = [int](Get-Content -LiteralPath $descendantPidFile -Raw)
    for ($attempt = 1; $attempt -le 50; $attempt++) {
        if ($null -eq (Get-Process -Id $descendantPid -ErrorAction SilentlyContinue)) {
            break
        }
        Start-Sleep -Milliseconds 100
    }
    if ($null -ne (Get-Process -Id $descendantPid -ErrorAction SilentlyContinue)) {
        throw "Windows pseudoterminal timeout left its descendant running"
    }
    $stopwatch.Stop()
    if ($stopwatch.ElapsedMilliseconds -ge 40000) {
        throw "Windows pseudoterminal timeout cleanup exceeded its end-to-end bound: $($stopwatch.ElapsedMilliseconds)ms"
    }
} finally {
    Remove-Item Env:NIB_PTY_DESCENDANT_PID_FILE -ErrorAction SilentlyContinue
    Remove-Item Env:NIB_PTY_DESCENDANT_READY_FILE -ErrorAction SilentlyContinue
    Remove-Item Env:NIB_PTY_PROBE_ARMED_FILE -ErrorAction SilentlyContinue
    Remove-Item Env:NIB_PTY_RESISTANT_CHILD_SCRIPT -ErrorAction SilentlyContinue
    if ($null -eq $descendantPid -and
        (Test-Path -LiteralPath $descendantPidFile -PathType Leaf)) {
        try {
            $descendantPid = [int](Get-Content -LiteralPath $descendantPidFile -Raw)
        } catch {
            $descendantPid = $null
        }
    }
    if ($null -ne $descendantPid) {
        Stop-Process -Id $descendantPid -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $timeoutRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output "Windows pseudoterminal smoke test passed."
