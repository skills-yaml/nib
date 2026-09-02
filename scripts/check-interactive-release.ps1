[CmdletBinding()]
param(
    [string]$Binary = (Join-Path $PSScriptRoot "..\target\release\nib.exe")
)

$ErrorActionPreference = "Stop"

. (Join-Path $PSScriptRoot "invoke-windows-pseudoterminal.ps1")

function Quote-NibPowerShellLiteral {
    param([Parameter(Mandatory = $true)][string]$Value)
    return "'" + $Value.Replace("'", "''") + "'"
}

function Invoke-NibRedirectedPlain {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $started = $false
    try {
        if (-not $process.Start()) {
            throw "Unable to start the redirected Windows plain-mode smoke"
        }
        $started = $true
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.StandardInput.Write("/status`r`n/quit`r`n")
        $process.StandardInput.Close()
        if (-not $process.WaitForExit(30000)) {
            $process.Kill($true)
            if (-not $process.WaitForExit(5000)) {
                throw "Unable to stop the timed-out redirected Windows plain-mode smoke"
            }
            throw "The redirected Windows plain-mode smoke exceeded its timeout"
        }
        if (-not $stdoutTask.Wait(5000) -or -not $stderrTask.Wait(5000)) {
            throw "Timed out while draining redirected Windows plain-mode output"
        }
        return [pscustomobject]@{
            ExitCode = $process.ExitCode
            Output = $stdoutTask.GetAwaiter().GetResult()
            ErrorOutput = $stderrTask.GetAwaiter().GetResult()
        }
    } finally {
        if ($started -and -not $process.HasExited) {
            $process.Kill($true)
            if (-not $process.WaitForExit(5000)) {
                throw "Unable to stop the redirected Windows plain-mode smoke"
            }
        }
        $process.Dispose()
    }
}

$binaryPath = (Resolve-Path -LiteralPath $Binary).Path
$temporaryBase = if ([string]::IsNullOrWhiteSpace($env:RUNNER_TEMP)) {
    [IO.Path]::GetTempPath()
} else {
    $env:RUNNER_TEMP
}
$fixture = Join-Path $temporaryBase ("nib-interactive-smoke-" + [guid]::NewGuid().ToString("N"))
$isolatedHome = Join-Path $fixture "home"
$isolatedConfig = Join-Path $fixture "xdg-config"
$privateSentinel = "interactive-private-sentinel-windows-q7v9k2"
$environmentNames = @(
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GOOGLE_API_KEY",
    "XAI_API_KEY",
    "META_API_KEY",
    "OPENROUTER_API_KEY",
    "NIB_MANAGED_PROCESS_SCOPE",
    "NIB_SKILLS_DIR",
    "HOME",
    "USERPROFILE",
    "XDG_CONFIG_HOME",
    "NIB_NO_UPDATE_CHECK",
    "NIB_ENABLE_INTERACTIVE_SMOKE",
    "TERM",
    "NO_COLOR"
)
$previousEnvironment = @{}
foreach ($name in $environmentNames) {
    $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
}

try {
    New-Item -ItemType Directory -Force -Path `
        $fixture, `
        $isolatedHome, `
        $isolatedConfig, `
        (Join-Path $fixture ".nib") | Out-Null
    & git -C $fixture init --quiet
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to initialize the isolated Windows interactive smoke repository"
    }
    & git -C $fixture config user.email nib-smoke@example.invalid
    & git -C $fixture config user.name "nib interactive smoke"
    [IO.File]::WriteAllText(
        (Join-Path $fixture "README.md"),
        "interactive Windows smoke fixture`n",
        [Text.UTF8Encoding]::new($false)
    )
    $configText = @"
[llm]
active_provider = "mock"

[llm.providers.mock]
model = "mock-model"

[llm.providers.openai]
model = "gpt-5"
api_key = "$privateSentinel"

[skills]
enabled = false

[daemons]
cron_enabled = false
curator_enabled = false
"@
    [IO.File]::WriteAllText(
        (Join-Path $fixture ".nib\config.toml"),
        $configText.TrimStart(),
        [Text.UTF8Encoding]::new($false)
    )

    foreach ($credentialName in @(
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GOOGLE_API_KEY",
        "XAI_API_KEY",
        "META_API_KEY",
        "OPENROUTER_API_KEY",
        "NIB_MANAGED_PROCESS_SCOPE",
        "NIB_SKILLS_DIR"
    )) {
        [Environment]::SetEnvironmentVariable($credentialName, $null, "Process")
    }
    $env:HOME = $isolatedHome
    $env:USERPROFILE = $isolatedHome
    $env:XDG_CONFIG_HOME = $isolatedConfig
    $env:NIB_NO_UPDATE_CHECK = "1"
    $env:NIB_ENABLE_INTERACTIVE_SMOKE = "1"

    $pwshPath = (Get-Process -Id $PID).Path
    $quotedFixture = Quote-NibPowerShellLiteral $fixture
    $quotedBinary = Quote-NibPowerShellLiteral $binaryPath

    $env:TERM = "xterm-256color"
    $env:NO_COLOR = "1"
    $tuiCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary --tui; exit `$LASTEXITCODE"
    $tuiResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $tuiCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = [string][char]17; DelayMilliseconds = 1200 }
        ) `
        -TimeoutMilliseconds 30000
    if ($tuiResult.ExitCode -ne 0 -or
        -not $tuiResult.ConsoleModesRestored -or
        -not $tuiResult.ChildConsoleModesRestored -or
        -not $tuiResult.Output.Contains("$([char]27)[?1049l") -or
        -not $tuiResult.Output.Contains("$([char]27)[?2004l")) {
        throw "Windows interactive smoke did not restore the capable TUI terminal"
    }

    $env:TERM = "dumb"
    $env:NO_COLOR = "1"
    $plainCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary; exit `$LASTEXITCODE"
    $plainResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $plainCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = "/status`r`n/quit`r`n"; DelayMilliseconds = 600 }
        ) `
        -TimeoutMilliseconds 30000
    if ($plainResult.ExitCode -ne 0 -or
        -not $plainResult.ConsoleModesRestored -or
        -not $plainResult.ChildConsoleModesRestored -or
        -not $plainResult.Output.Contains("mode: plain") -or
        -not $plainResult.Output.Contains("Configured approval preset:") -or
        -not $plainResult.Output.Contains("Goodbye. Session saved")) {
        throw "Windows TERM=dumb interactive smoke did not preserve plain-mode operations"
    }
    foreach ($fullScreenSequence in @(
        "$([char]27)[?1049",
        "$([char]27)[?2004"
    )) {
        if ($plainResult.Output.Contains($fullScreenSequence)) {
            throw "Windows TERM=dumb fallback emitted a full-screen terminal sequence"
        }
    }

    $redirectedResult = Invoke-NibRedirectedPlain `
        -Executable $binaryPath `
        -WorkingDirectory $fixture
    if ($redirectedResult.ExitCode -ne 0 -or
        -not $redirectedResult.Output.Contains("mode: plain") -or
        -not $redirectedResult.Output.Contains("Configured approval preset:") -or
        -not $redirectedResult.Output.Contains("Goodbye. Session saved")) {
        throw "Windows redirected plain-mode smoke did not preserve plain operations"
    }
    if ($redirectedResult.Output.Contains([string][char]27) -or
        $redirectedResult.ErrorOutput.Contains([string][char]27)) {
        throw "Windows redirected TERM=dumb/NO_COLOR output emitted an ANSI escape"
    }

    foreach ($output in @(
        $tuiResult.Output,
        $plainResult.Output,
        $redirectedResult.Output,
        $redirectedResult.ErrorOutput
    )) {
        if ($output.Contains($privateSentinel) -or $output.Contains('"arguments"')) {
            throw "Windows interactive smoke exposed private configuration or raw arguments"
        }
    }
    $sessionDirectory = Join-Path $fixture ".nib\profiles\default\sessions"
    if (Test-Path -LiteralPath $sessionDirectory -PathType Container) {
        foreach ($sessionFile in Get-ChildItem -LiteralPath $sessionDirectory -Filter "*.json" -File) {
            $sessionText = Get-Content -LiteralPath $sessionFile.FullName -Raw
            if ($sessionText.Contains($privateSentinel)) {
                throw "Windows interactive smoke persisted the inactive-provider sentinel"
            }
        }
    }

    Write-Output "Interactive release smoke passed (offline Windows ConPTY and TERM=dumb modes)."
} finally {
    foreach ($name in $environmentNames) {
        [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], "Process")
    }
    if ($fixture.StartsWith($temporaryBase, [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $fixture -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path -LiteralPath $fixture) {
            throw "Windows interactive smoke could not remove its isolated fixture: $fixture"
        }
    }
}
