[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

try {
    $encodedRequest = $env:NIB_WINDOWS_PTY_CHILD_REQUEST
    $exitMarker = $env:NIB_WINDOWS_PTY_EXIT_MARKER
    Remove-Item Env:NIB_WINDOWS_PTY_CHILD_REQUEST -ErrorAction SilentlyContinue
    Remove-Item Env:NIB_WINDOWS_PTY_EXIT_MARKER -ErrorAction SilentlyContinue
    if ([string]::IsNullOrWhiteSpace($encodedRequest) -or
        [string]::IsNullOrWhiteSpace($exitMarker)) {
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

    & ([string]$request.executable) @arguments
    $childExitCode = [int]$LASTEXITCODE
    [Console]::Out.WriteLine("$exitMarker$childExitCode")
    exit 0
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 255
}
