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
    -not $result.Output.Contains("NIB_PSEUDOTERMINAL_READY")) {
    throw "Windows pseudoterminal did not expose an interactive stderr handle: $($result.Output)"
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
    -not $exitResult.Output.Contains("NIB_PSEUDOTERMINAL_EXIT")) {
    throw "Windows pseudoterminal did not preserve output and exit status"
}

$temporaryBase = if ([string]::IsNullOrWhiteSpace($env:RUNNER_TEMP)) {
    [IO.Path]::GetTempPath()
} else {
    $env:RUNNER_TEMP
}
$timeoutRoot = Join-Path $temporaryBase ("nib-pty-timeout-" + [guid]::NewGuid().ToString("N"))
$descendantPidFile = Join-Path $timeoutRoot "descendant.pid"
New-Item -ItemType Directory -Path $timeoutRoot -Force | Out-Null
$env:NIB_PTY_DESCENDANT_PID_FILE = $descendantPidFile
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
for ($attempt = 1; $attempt -le 50; $attempt++) {
    if (Test-Path -LiteralPath $env:NIB_PTY_DESCENDANT_PID_FILE -PathType Leaf) { break }
    if ($descendant.HasExited) { exit 42 }
    Start-Sleep -Milliseconds 100
}
if (-not (Test-Path -LiteralPath $env:NIB_PTY_DESCENDANT_PID_FILE -PathType Leaf)) { exit 43 }
Start-Sleep -Seconds 60
'@
$stopwatch = [Diagnostics.Stopwatch]::StartNew()
$timeoutFailed = $false
$descendantPid = $null
try {
    try {
        Invoke-WindowsPseudoTerminal `
            -Executable $pwshPath `
            -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $timeoutProbe) `
            -TimeoutMilliseconds 7000 `
            -HostGraceMilliseconds 7000 | Out-Null
    } catch {
        $timeoutFailed = $true
    }
    $stopwatch.Stop()
    if (-not $timeoutFailed -or $stopwatch.ElapsedMilliseconds -ge 20000) {
        throw "Windows pseudoterminal timeout was not bounded"
    }
    if (-not (Test-Path -LiteralPath $descendantPidFile -PathType Leaf)) {
        throw "Windows pseudoterminal timeout did not exercise a lingering descendant"
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
} finally {
    Remove-Item Env:NIB_PTY_DESCENDANT_PID_FILE -ErrorAction SilentlyContinue
    Remove-Item Env:NIB_PTY_RESISTANT_CHILD_SCRIPT -ErrorAction SilentlyContinue
    if ($null -ne $descendantPid) {
        Stop-Process -Id $descendantPid -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $timeoutRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output "Windows pseudoterminal smoke test passed."
