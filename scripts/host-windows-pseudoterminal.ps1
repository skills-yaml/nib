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
    $result = [Nib.ReleaseQualification.WindowsPseudoTerminal]::Run(
        [string]$request.executable,
        $arguments,
        $timeoutMilliseconds
    )
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
