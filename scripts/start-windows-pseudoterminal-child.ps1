[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

try {
    $encodedRequest = $env:NIB_WINDOWS_PTY_CHILD_REQUEST
    Remove-Item Env:NIB_WINDOWS_PTY_CHILD_REQUEST -ErrorAction SilentlyContinue
    if ([string]::IsNullOrWhiteSpace($encodedRequest)) {
        throw "Windows pseudoterminal child request is missing"
    }

    $requestJson = [Text.Encoding]::UTF8.GetString(
        [Convert]::FromBase64String($encodedRequest)
    )
    $request = $requestJson | ConvertFrom-Json
    $arguments = [string[]]@($request.arguments)
    if ([string]::IsNullOrWhiteSpace([string]$request.executable)) {
        throw "Windows pseudoterminal child request is invalid"
    }

    Add-Type -Path (Join-Path $PSScriptRoot "windows-pseudoterminal.cs")
    $exitCode = [Nib.ReleaseQualification.WindowsConsoleChild]::Run(
        [string]$request.executable,
        $arguments
    )
    exit $exitCode
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 255
}
