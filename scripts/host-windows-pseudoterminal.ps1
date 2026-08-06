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

    $sourcePath = Join-Path $PSScriptRoot "windows-pseudoterminal.cs"
    Add-Type -Path $sourcePath
    $childRequest = @{
        executable = [string]$request.executable
        arguments = @($arguments)
    } | ConvertTo-Json -Compress
    $env:NIB_WINDOWS_PTY_CHILD_REQUEST = [Convert]::ToBase64String(
        [Text.Encoding]::UTF8.GetBytes($childRequest)
    )
    try {
        $terminalRoot = (Get-Process -Id $PID).Path
        $terminalArguments = [string[]]@(
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-File",
            (Join-Path $PSScriptRoot "start-windows-pseudoterminal-child.ps1")
        )
        $result = [Nib.ReleaseQualification.WindowsPseudoTerminal]::Run(
            $terminalRoot,
            $terminalArguments,
            $timeoutMilliseconds
        )
    } finally {
        Remove-Item Env:NIB_WINDOWS_PTY_CHILD_REQUEST -ErrorAction SilentlyContinue
    }
    [Console]::Out.WriteLine((@{
        exit_code = $result.ExitCode
        output = $result.Output
    } | ConvertTo-Json -Compress))
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    while ($true) {
        Start-Sleep -Seconds 60
    }
}
