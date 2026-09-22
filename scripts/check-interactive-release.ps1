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
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).Path
$sourceRevision = (& git -C $repositoryRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($sourceRevision)) {
    throw "Unable to resolve the interactive smoke source revision"
}
$sourceClean = @(& git -C $repositoryRoot status --porcelain).Count -eq 0
if ($LASTEXITCODE -ne 0) {
    throw "Unable to inspect the interactive smoke source state"
}
$binaryVersion = (& $binaryPath version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or -not $binaryVersion.Contains($sourceRevision)) {
    throw "Release binary identity does not match source revision $sourceRevision"
}
$acceptanceEligible = $sourceClean
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
$originalClipboard = $null
$restoreClipboard = $false
foreach ($name in $environmentNames) {
    $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
}

try {
    try {
        $originalClipboard = Get-Clipboard -Raw -ErrorAction Stop
        $restoreClipboard = $true
    } catch {
        $restoreClipboard = $false
    }
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
    # Incremental redraw skips unchanged spaces, splitting complete status sentences.
    # These fresh segments are unique to consent success and quit confirmation in
    # this isolated startup, so each write still waits for its actual UI state.
    $tuiResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $tuiCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = "y"; WaitForOutput = "Work in this directory" },
            [pscustomobject]@{ Text = [string][char]17; WaitForOutput = "Allowed work" },
            [pscustomobject]@{ Text = [string][char]17; WaitForOutput = "again" }
        ) `
        -TimeoutMilliseconds 30000
    if ($tuiResult.ExitCode -ne 0 -or
        -not $tuiResult.ConsoleModesRestored -or
        -not $tuiResult.ChildConsoleModesRestored -or
        -not $tuiResult.Output.Contains("Work in this directory") -or
        -not $tuiResult.Output.Contains("$([char]27)[?1049l") -or
        -not $tuiResult.Output.Contains("$([char]27)[?2004l")) {
        throw "Windows interactive smoke did not restore the capable TUI terminal"
    }
    $persistedConfig = Get-Content -LiteralPath (Join-Path $fixture ".nib\config.toml") -Raw
    if (-not $persistedConfig.Contains("allowed = true")) {
        throw "Windows interactive smoke did not persist explicit workspace consent"
    }

    $tuiQuestionCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary --tui --run 'ask a question before continuing in TUI smoke'; exit `$LASTEXITCODE"
    $tuiQuestionResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $tuiQuestionCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = "$([char]27)OQ"; WaitForOutput = "Which verification mode?" },
            # Incremental redraws may split the command-overlay label with cursor
            # controls. Status output is the stable proof that F2 owned this input.
            [pscustomobject]@{ Text = "/status`r"; DelayMilliseconds = 300 },
            [pscustomobject]@{ Text = "2`r"; WaitForOutput = "Verification:" },
            [pscustomobject]@{ Text = "$([char]17)$([char]17)"; WaitForOutput = "Final answer: task complete" }
        ) `
        -TimeoutMilliseconds 60000
    if ($tuiQuestionResult.ExitCode -ne 0 -or
        -not $tuiQuestionResult.ConsoleModesRestored -or
        -not $tuiQuestionResult.ChildConsoleModesRestored -or
        -not $tuiQuestionResult.Output.Contains("Verification:")) {
        throw "Windows TUI F2 smoke did not preserve the pending question and command overlay"
    }

    $plainQuestionCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary --plain --run 'ask a question before continuing'; exit `$LASTEXITCODE"
    $plainQuestionResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $plainQuestionCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = ":command /status`r`n"; WaitForOutput = "Answer (number or text):" },
            [pscustomobject]@{ Text = "2`r`n`r`n"; WaitForOutput = "Configured approval preset:" },
            [pscustomobject]@{ Text = "/quit`r`n"; WaitForOutput = "You> " }
        ) `
        -TimeoutMilliseconds 60000
    if ($plainQuestionResult.ExitCode -ne 0 -or
        -not $plainQuestionResult.ConsoleModesRestored -or
        -not $plainQuestionResult.ChildConsoleModesRestored -or
        -not $plainQuestionResult.Output.Contains('"answer":"full"')) {
        throw "Windows plain :command smoke did not preserve and answer the pending question"
    }

    $env:TERM = "dumb"
    $env:NO_COLOR = "1"
    $plainCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary; exit `$LASTEXITCODE"
    $plainResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $plainCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = "/status`r`n/quit`r`n"; WaitForOutput = "You> " }
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

    $copySeedCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary run 'finish the release smoke' --session t047-copy-smoke --provider mock --model mock-model --max-steps 4 --yes; exit `$LASTEXITCODE"
    $copySeedResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $copySeedCommand) `
        -TimeoutMilliseconds 60000
    if ($copySeedResult.ExitCode -ne 0 -or
        -not $copySeedResult.Output.Contains("Agent run completed for session")) {
        throw "Windows clipboard smoke could not create its completed session"
    }

    $copyCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary --plain --session t047-copy-smoke; exit `$LASTEXITCODE"
    $copyResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $copyCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = "/copy`r`n/quit`r`n"; WaitForOutput = "You> " }
        ) `
        -TimeoutMilliseconds 30000
    if ($copyResult.ExitCode -ne 0 -or
        -not $copyResult.ConsoleModesRestored -or
        -not $copyResult.ChildConsoleModesRestored) {
        throw "Windows native clipboard smoke did not restore its console"
    }
    if ($copyResult.Output.Contains("Copied")) {
        $copiedText = Get-Clipboard -Raw -ErrorAction Stop
        if (-not $copiedText.Contains("Final answer: task complete")) {
            throw "Windows native clipboard backend reported success without delivering the expected text"
        }
    } elseif ($copyResult.Output.Contains("Copy requested via OSC52 (unconfirmed)")) {
        if (-not $copyResult.Output.Contains("$([char]27)]52;c;")) {
            throw "Windows OSC52 fallback did not emit its labeled request"
        }
    } else {
        throw "Windows clipboard smoke produced neither native success nor a labeled OSC52 fallback"
    }

    $env:TERM = "xterm-256color"
    $oneShotOutputs = [ordered]@{}
    foreach ($interruptCase in @(
        [pscustomobject]@{
            Label = "question"
            Goal = "ask a question before continuing"
            Prompt = "Question:"
            Delay = 0
            Yes = $false
            Forbidden = ""
        },
        [pscustomobject]@{
            Label = "approval"
            Goal = "one-shot interrupt approval"
            Prompt = "Approval required"
            Delay = 0
            Yes = $false
            Forbidden = "one-shot-approval-ran.txt"
        },
        [pscustomobject]@{
            Label = "terminal"
            Goal = "one-shot interrupt terminal"
            Prompt = "nib run: starting"
            Delay = 1500
            Yes = $true
            Forbidden = "one-shot-interrupt-completed.txt"
        }
    )) {
        $quotedGoal = Quote-NibPowerShellLiteral $interruptCase.Goal
        $yesArgument = if ($interruptCase.Yes) { " --yes" } else { "" }
        $oneShotCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary run $quotedGoal --provider mock --model mock-model --max-steps 5$yesArgument; exit `$LASTEXITCODE"
        $oneShotResult = Invoke-WindowsPseudoTerminal `
            -Executable $pwshPath `
            -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $oneShotCommand) `
            -InputChunks @(
                # This byte enters through ConPTY's console-input path, where processed
                # input delivers the native Ctrl+C console event to the foreground child.
                [pscustomobject]@{
                    Text = [string][char]3
                    WaitForOutput = $interruptCase.Prompt
                    DelayMilliseconds = $interruptCase.Delay
                }
            ) `
            -TimeoutMilliseconds 30000
        $oneShotOutputs[$interruptCase.Label] = $oneShotResult.Output
        if ($oneShotResult.ExitCode -eq 0 -or
            -not $oneShotResult.ConsoleModesRestored -or
            -not $oneShotResult.ChildConsoleModesRestored -or
            -not $oneShotResult.Output.Contains("Run cancelled.")) {
            throw "Windows one-shot Ctrl+C did not reconcile $($interruptCase.Goal)"
        }
        if (-not [string]::IsNullOrWhiteSpace($interruptCase.Forbidden) -and
            (Test-Path -LiteralPath (Join-Path $fixture $interruptCase.Forbidden))) {
            throw "Windows one-shot Ctrl+C allowed $($interruptCase.Goal) to mutate"
        }
    }
    $cancelledSessionCount = 0
    $sessionDirectory = Join-Path $fixture ".nib\profiles\default\sessions"
    if (Test-Path -LiteralPath $sessionDirectory -PathType Container) {
        foreach ($sessionFile in Get-ChildItem -LiteralPath $sessionDirectory -Filter "*.json" -File) {
            $sessionText = Get-Content -LiteralPath $sessionFile.FullName -Raw
            if ($sessionText.Contains('"outcome": "cancelled_by_user"')) {
                $cancelledSessionCount++
            }
        }
    }
    if ($cancelledSessionCount -lt 3) {
        throw "Windows one-shot Ctrl+C did not persist all three cancellation reconciliations"
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
    ) + @($oneShotOutputs.Values)) {
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

    if (-not [string]::IsNullOrWhiteSpace($env:NIB_INTERACTIVE_EVIDENCE_DIR)) {
        $evidenceDirectory = Join-Path $env:NIB_INTERACTIVE_EVIDENCE_DIR "Windows"
        New-Item -ItemType Directory -Force -Path $evidenceDirectory | Out-Null
        [IO.File]::WriteAllText(
            (Join-Path $evidenceDirectory "revision.txt"),
            "$sourceRevision`n",
            [Text.UTF8Encoding]::new($false)
        )
        $evidence = [ordered]@{
            platform = "Windows"
            source_revision = $sourceRevision
            source_clean = $sourceClean
            binary_version = $binaryVersion
            acceptance_eligible = $acceptanceEligible
            terminal_restoration = "passed"
            caller_modes_before = $tuiQuestionResult.CallerConsoleModesBefore
            caller_modes_after = $tuiQuestionResult.CallerConsoleModesAfter
            child_modes_before = $tuiQuestionResult.ChildConsoleModesBefore
            child_modes_after = $tuiQuestionResult.ChildConsoleModesAfter
            f2_prompt_command = "passed"
            plain_modal_command = "passed"
            clipboard = if ($copyResult.Output.Contains("Copied")) { "native" } else { "osc52_unconfirmed" }
            interruption_cases = 3
            durable_cancellations = $cancelledSessionCount
            privacy_scan = "passed"
        } | ConvertTo-Json -Depth 8
        [IO.File]::WriteAllText(
            (Join-Path $evidenceDirectory "summary.json"),
            $evidence.Replace($privateSentinel, "[fixture-secret]"),
            [Text.UTF8Encoding]::new($false)
        )
        [IO.File]::WriteAllText(
            (Join-Path $evidenceDirectory "tui-f2.txt"),
            $tuiQuestionResult.Output.Replace($privateSentinel, "[fixture-secret]"),
            [Text.UTF8Encoding]::new($false)
        )
        [IO.File]::WriteAllText(
            (Join-Path $evidenceDirectory "plain-modal-command.txt"),
            $plainQuestionResult.Output.Replace($privateSentinel, "[fixture-secret]"),
            [Text.UTF8Encoding]::new($false)
        )
        [IO.File]::WriteAllText(
            (Join-Path $evidenceDirectory "clipboard.txt"),
            $copyResult.Output.Replace($privateSentinel, "[fixture-secret]"),
            [Text.UTF8Encoding]::new($false)
        )
        foreach ($interruptLabel in $oneShotOutputs.Keys) {
            [IO.File]::WriteAllText(
                (Join-Path $evidenceDirectory "one-shot-interrupt-$interruptLabel.txt"),
                $oneShotOutputs[$interruptLabel].Replace($privateSentinel, "[fixture-secret]"),
                [Text.UTF8Encoding]::new($false)
            )
        }
    }

    Write-Output "Interactive release smoke passed (offline Windows ConPTY and TERM=dumb modes)."
} catch {
    $hostDiagnostics = [string]$_.Exception.Data["NibHostDiagnostics"]
    if (-not [string]::IsNullOrWhiteSpace($hostDiagnostics)) {
        # This fixture has one explicit secret sentinel; never print it in errors.
        [Console]::Error.WriteLine($hostDiagnostics.Replace($privateSentinel, "[fixture-secret]"))
    }
    throw
} finally {
    if ($restoreClipboard) {
        try {
            Set-Clipboard -Value $originalClipboard -ErrorAction Stop
        } catch {
            [Console]::Error.WriteLine("Warning: unable to restore the pre-smoke clipboard value")
        }
    }
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
