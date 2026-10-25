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

# T082: sessions are rows of the session database, $nibHome\nib.db. The smoke
# exports the default profile's sessions, read-only, as the pretty JSON
# documents nib wrote before (one <id>.json each), so every check below reads
# the persisted session itself. With a stop file it keeps exporting until that
# file appears, so a durable wait inside the pseudoterminal host observes the
# session while nib is still running.
$nibSessionExportScript = @'
import json, os, pathlib, sqlite3, sys, time
database, target = sys.argv[1], sys.argv[2]
stop = sys.argv[3] if len(sys.argv) > 3 else None

def export():
    if not os.path.isfile(database):
        return
    # A file URI: a Windows path is not a valid URI as it stands.
    uri = pathlib.Path(database).resolve().as_uri() + "?mode=ro"
    connection = sqlite3.connect(uri, uri=True, timeout=30)
    try:
        with connection:
            sessions = connection.execute(
                "SELECT scope, id, document FROM sessions WHERE profile_id = 'default'"
            ).fetchall()
            for scope, session_id, document in sessions:
                session = json.loads(document)
                for key, table in (("messages", "session_messages"), ("events", "session_events")):
                    session[key] = [
                        json.loads(body)
                        for (body,) in connection.execute(
                            f"SELECT body FROM {table} WHERE scope = ? AND session_id = ? ORDER BY idx",
                            (scope, session_id),
                        )
                    ]
                path = os.path.join(target, f"{session_id}.json")
                temporary = f"{path}.export"
                with open(temporary, "w", encoding="utf-8") as file:
                    json.dump(session, file, indent=2, ensure_ascii=False)
                os.replace(temporary, path)
    finally:
        connection.close()

if stop is None:
    export()
    sys.exit(0)
deadline = time.monotonic() + 300
last_error = None
while not os.path.exists(stop) and time.monotonic() < deadline:
    try:
        export()
        last_error = None
    except Exception as error:
        # nib may hold a write lock, or an observer may hold an exported file
        # open; the next round retries under the observer's own deadline. The
        # last failure is kept so a timeout can name its cause.
        last_error = repr(error)
    time.sleep(0.1)
if last_error is not None:
    with open(f"{stop}.log", "w", encoding="utf-8") as log:
        log.write(last_error)
'@

function Find-NibSmokePython {
    # The release workflow does not install Python; GitHub's Windows images
    # provide it. Fail clearly when it (or its sqlite3 module) is unavailable.
    foreach ($candidate in @("python", "python3")) {
        $command = Get-Command -Name $candidate -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($null -eq $command) { continue }
        try {
            & $command.Source -I -c "import sqlite3, sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)" 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) { return $command.Source }
        } catch {
            # An unusable candidate (such as an app-installer alias) is skipped.
        }
    }
    throw "required command is unavailable: python (3.8 or later, with sqlite3) to read the session database"
}

function Export-NibSmokeSessions {
    & $script:pythonPath -I $script:sessionExportPath (Join-Path $script:nibHome "nib.db") $script:sessionDirectory
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to export the Windows interactive smoke sessions from the session database"
    }
}

function Start-NibSmokeSessionExporter {
    $script:sessionExportStop = Join-Path $script:isolatedHome ("session-export-stop-" + [guid]::NewGuid().ToString("N"))
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $script:pythonPath
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    foreach ($argument in @(
        "-I",
        $script:sessionExportPath,
        (Join-Path $script:nibHome "nib.db"),
        $script:sessionDirectory,
        $script:sessionExportStop
    )) {
        $startInfo.ArgumentList.Add($argument)
    }
    $script:sessionExporter = [Diagnostics.Process]::Start($startInfo)
}

function Stop-NibSmokeSessionExporter {
    if ($null -eq $script:sessionExporter) { return }
    try {
        [IO.File]::WriteAllText($script:sessionExportStop, "", [Text.UTF8Encoding]::new($false))
        if (-not $script:sessionExporter.WaitForExit(5000)) {
            $script:sessionExporter.Kill($true)
            $script:sessionExporter.WaitForExit(5000) | Out-Null
        } elseif ($script:sessionExporter.ExitCode -ne 0) {
            Write-Warning "session exporter exited with code $($script:sessionExporter.ExitCode)"
        }
        # A failing exporter surfaces its last error instead of a bare timeout.
        $exportLog = "$($script:sessionExportStop).log"
        if (Test-Path -LiteralPath $exportLog) {
            Write-Warning ("session exporter last failed: " + (Get-Content -LiteralPath $exportLog -Raw))
        }
    } finally {
        $script:sessionExporter.Dispose()
        $script:sessionExporter = $null
    }
}

function Test-NibSmokeFileContainsSentinel {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Sentinel
    )
    $stream = [IO.FileStream]::new(
        $Path,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete)
    )
    try {
        $memory = [IO.MemoryStream]::new()
        $stream.CopyTo($memory)
        # Latin-1 maps every byte to one character, so an ASCII sentinel is
        # found wherever its bytes are, like `grep -a`.
        return [Text.Encoding]::Latin1.GetString($memory.ToArray()).Contains($Sentinel)
    } finally {
        $stream.Dispose()
    }
}

# The user's settings for the fixture project, where nib records its approvals
# and the workspace consent (T082 decision 3): <NIB_HOME>\projects\<key>\config.toml.
# The key hashes nib's canonical form of the fixture path, so instead of
# recomputing it the smoke finds the directory whose project.toml records the
# fixture as its root. The canonical form may carry a verbatim prefix or expand
# 8.3 short names, so the fixture's unique folder name decides a mismatch.
function Get-NibSmokeProjectSettingsFile {
    $projects = Join-Path $script:nibHome "projects"
    $settingsFiles = @()
    if (Test-Path -LiteralPath $projects -PathType Container) {
        foreach ($identity in Get-ChildItem -LiteralPath $projects -Directory |
                ForEach-Object { Join-Path $_.FullName "project.toml" } |
                Where-Object { Test-Path -LiteralPath $_ -PathType Leaf }) {
            $rootLine = Get-Content -LiteralPath $identity |
                Where-Object { $_ -match '^\s*root\s*=' } |
                Select-Object -First 1
            if ($null -eq $rootLine) { continue }
            $root = ($rootLine -replace '^\s*root\s*=\s*', '').Trim()
            if ($root.StartsWith("'") -and $root.EndsWith("'")) {
                $root = $root.Substring(1, $root.Length - 2)
            } elseif ($root.StartsWith('"') -and $root.EndsWith('"')) {
                $root = $root.Substring(1, $root.Length - 2).Replace('\\', '\')
            }
            $root = $root -replace '^\\\\\?\\UNC\\', '\\' -replace '^\\\\\?\\', ''
            $root = $root.TrimEnd('\')
            if ($root.Equals($script:fixture.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase) -or
                [IO.Path]::GetFileName($root).Equals(
                    [IO.Path]::GetFileName($script:fixture.TrimEnd('\')),
                    [StringComparison]::OrdinalIgnoreCase
                )) {
                $settingsFiles += (Join-Path (Split-Path -Parent $identity) "config.toml")
            }
        }
    }
    if ($settingsFiles.Count -ne 1) {
        throw "Windows interactive smoke found $($settingsFiles.Count) user project directories for its fixture"
    }
    return $settingsFiles[0]
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
# T082: nib keeps its user home (user settings, per-project approvals and
# trust, runtime state and the session database) in NIB_HOME. The smoke sets
# it inside the fixture, so it never reads or writes the real user home, nor
# an outer NIB_HOME it may inherit.
$nibHome = Join-Path $fixture "nib-home"
$sessionDirectory = Join-Path $fixture "sessions"
$sessionExportPath = Join-Path $isolatedHome "nib-session-export.py"
$sessionExporter = $null
$sessionExportStop = $null
$pythonPath = Find-NibSmokePython
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
    "NIB_HOME",
    "NIB_NO_UPDATE_CHECK",
    "NIB_ENABLE_INTERACTIVE_SMOKE",
    "TERM",
    "NO_COLOR"
)
$previousEnvironment = @{}
$originalClipboard = $null
$restoreClipboard = $false
$activeStage = "initialization"
$lastInterruptResult = $null
$lastInterruptSessionText = ""
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
        $nibHome, `
        $sessionDirectory, `
        (Join-Path $fixture ".nib") | Out-Null
    [IO.File]::WriteAllText($sessionExportPath, $nibSessionExportScript, [Text.UTF8Encoding]::new($false))
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
    [IO.File]::WriteAllText(
        (Join-Path $fixture ".gitignore"),
        ".nib/`nhome/`nxdg-config/`nnib-home/`nsessions/`n*.txt`n",
        [Text.UTF8Encoding]::new($false)
    )
    & git -C $fixture add .gitignore README.md
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to stage the isolated Windows interactive smoke baseline"
    }
    & git -C $fixture commit --quiet -m initial
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to commit the isolated Windows interactive smoke baseline"
    }
    $configText = @"
[llm]
active_provider = "mock"

[llm.providers.mock]
model = "mock-model"

[llm.providers.openai]
model = "gpt-5"
api_key = "$privateSentinel"

[agent]
answer_only = false

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
    $env:NIB_HOME = $nibHome
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
    $activeStage = "tui-startup"
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
    # The consent is the user's: it is in the user's settings for the project,
    # never in the project's own configuration.
    $projectSettings = Get-Content -LiteralPath (Get-NibSmokeProjectSettingsFile) -Raw
    if (-not $projectSettings.Contains("allowed = true")) {
        throw "Windows interactive smoke did not persist explicit workspace consent"
    }
    $projectConfig = Get-Content -LiteralPath (Join-Path $fixture ".nib\config.toml") -Raw
    if ($projectConfig.Contains("allowed")) {
        throw "Windows interactive smoke wrote workspace consent to the project configuration"
    }

    $tuiQuestionCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary --tui --run 'ask a question before continuing in TUI smoke'; exit `$LASTEXITCODE"
    $activeStage = "tui-f2-question"
    $tuiQuestionResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $tuiQuestionCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = "$([char]27)OQ"; WaitForOutput = "Which verification mode?" },
            # Incremental redraws may split the command-overlay label with cursor
            # controls. Status output is the stable proof that F2 owned this input.
            [pscustomobject]@{ Text = "/status`r"; DelayMilliseconds = 300 },
            # The status row can become visible before command-overlay input
            # ownership has returned to the question rows. Settle after the
            # marker so option 2 cannot be consumed by the closing overlay.
            # Number keys select a row; Enter answers with option 2 (full).
            [pscustomobject]@{ Text = "2`r"; WaitForOutput = "Verification:"; DelayMilliseconds = 300 },
            # Differential TUI redraws can split the longer final-answer text
            # with cursor controls. The terminal lifecycle label is shorter and
            # the persisted session assertion below proves the exact answer.
            [pscustomobject]@{ Text = "$([char]17)$([char]17)"; WaitForOutput = "completed" }
        ) `
        -TimeoutMilliseconds 60000
    if ($tuiQuestionResult.ExitCode -ne 0 -or
        -not $tuiQuestionResult.ConsoleModesRestored -or
        -not $tuiQuestionResult.ChildConsoleModesRestored -or
        -not $tuiQuestionResult.Output.Contains("Verification:")) {
        throw "Windows TUI F2 smoke did not preserve the pending question and command overlay"
    }
    Export-NibSmokeSessions
    $tuiQuestionSessions = @(
        Get-ChildItem -LiteralPath $sessionDirectory -Filter "*.json" -File |
            Where-Object {
                $text = Get-Content -LiteralPath $_.FullName -Raw
                $text.Contains("ask a question before continuing in TUI smoke") -and
                $text.Contains('"answer": "full"') -and
                $text.Contains('"outcome": "completed"')
            }
    )
    if ($tuiQuestionSessions.Count -ne 1) {
        throw "Windows TUI F2 smoke did not persist the exact answer and completed outcome"
    }

    $plainQuestionCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary --plain --run 'ask a question before continuing'; exit `$LASTEXITCODE"
    $activeStage = "plain-modal-command"
    $plainQuestionResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $plainQuestionCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = ":command /status`r`n"; WaitForOutput = "Answer (number" },
            [pscustomobject]@{ Text = "2`r`n`r`n"; WaitForOutput = "Configured approval preset:" },
            # ConPTY may divide the trailing prompt across incremental reads. The
            # lifecycle event is emitted only after the modal answer has been
            # consumed and the one-shot run has reconciled, so it is the stable
            # synchronization point before returning to the interactive prompt.
            [pscustomobject]@{ Text = "/quit`r`n"; WaitForOutput = "[stream ended] Run completed."; DelayMilliseconds = 200 }
        ) `
        -TimeoutMilliseconds 60000
    if ($plainQuestionResult.ExitCode -ne 0 -or
        -not $plainQuestionResult.ConsoleModesRestored -or
        -not $plainQuestionResult.ChildConsoleModesRestored -or
        -not $plainQuestionResult.Output.Contains('"answer":"full"')) {
        throw "Windows plain :command smoke did not preserve and answer the pending question"
    }
    Export-NibSmokeSessions
    $plainQuestionSessions = @(
        Get-ChildItem -LiteralPath $sessionDirectory -Filter "*.json" -File |
            Where-Object {
                $text = Get-Content -LiteralPath $_.FullName -Raw
                # Match the exact goal field: the earlier TUI fixture deliberately
                # extends the same phrase with "in TUI smoke".
                $text.Contains('"goal": "ask a question before continuing"') -and
                $text.Contains('"answer": "full"') -and
                $text.Contains('"outcome": "completed"')
            }
    )
    if ($plainQuestionSessions.Count -ne 1) {
        throw "Windows plain :command smoke did not persist the exact answer and completed outcome"
    }

    $env:TERM = "dumb"
    $env:NO_COLOR = "1"
    # Match the visible prompt prefix: ConPTY can render its trailing space as
    # cursor movement instead of a literal space in the captured output.
    $plainCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary; exit `$LASTEXITCODE"
    $activeStage = "plain-dumb-terminal"
    $plainResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $plainCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = "/status`r`n/quit`r`n"; WaitForOutput = "You>" }
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
    $activeStage = "clipboard-seed"
    $copySeedResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $copySeedCommand) `
        -TimeoutMilliseconds 60000
    if ($copySeedResult.ExitCode -ne 0 -or
        -not $copySeedResult.Output.Contains("Agent run completed for session")) {
        throw "Windows clipboard smoke could not create its completed session"
    }

    $copyCommand = "Set-Location -LiteralPath $quotedFixture; & $quotedBinary --plain --session t047-copy-smoke; exit `$LASTEXITCODE"
    $activeStage = "clipboard-delivery"
    $copyResult = Invoke-WindowsPseudoTerminal `
        -Executable $pwshPath `
        -Arguments @("-NoLogo", "-NoProfile", "-NonInteractive", "-Command", $copyCommand) `
        -InputChunks @(
            [pscustomobject]@{ Text = "/copy`r`n/quit`r`n"; WaitForOutput = "You>" }
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
            Yes = $false
            Forbidden = ""
            ExpectedEvent = "question_required"
        },
        [pscustomobject]@{
            Label = "approval"
            Goal = "one-shot interrupt approval"
            Yes = $false
            Forbidden = "one-shot-approval-ran.txt"
            ExpectedEvent = "approval_required"
        },
        [pscustomobject]@{
            Label = "terminal"
            Goal = "one-shot interrupt terminal"
            Yes = $true
            Forbidden = "one-shot-interrupt-completed.txt"
            ExpectedEvent = "tool_started"
        }
    )) {
        $interruptSessionId = "t047-native-interrupt-$($interruptCase.Label)"
        $interruptSessionPath = Join-Path $sessionDirectory "$interruptSessionId.json"
        $lastInterruptSessionText = $null
        $lastInterruptResult = $null
        Export-NibSmokeSessions
        $sessionsBeforeInterrupt = @(
            Get-ChildItem -LiteralPath $sessionDirectory -Filter "*.json" -File |
                ForEach-Object { $_.FullName }
        )
        $oneShotArguments = @(
            "run", $interruptCase.Goal,
            "--session", $interruptSessionId,
            "--provider", "mock", "--model", "mock-model", "--max-steps", "5"
        )
        if ($interruptCase.Yes) { $oneShotArguments += "--yes" }
        $activeStage = "one-shot-interrupt-$($interruptCase.Label)"
        # The durable wait below reads the exported session while nib runs.
        Start-NibSmokeSessionExporter
        try {
            $oneShotResult = Invoke-WindowsPseudoTerminal `
                -Executable $binaryPath `
                -Arguments $oneShotArguments `
                -WorkingDirectory $fixture `
                -InputChunks @(
                    # Attach to the ConPTY-backed console and generate the native Windows
                    # Ctrl+C control event; raw ETX input is not accepted as evidence.
                    [pscustomobject]@{
                        Text = ""
                        NativeCtrlC = $true
                        WaitForDirectory = $sessionDirectory
                        WaitForFileName = "$interruptSessionId.json"
                        WaitForFileContents = @(
                            ('"id": "' + $interruptSessionId + '"')
                            ('"goal": "' + $interruptCase.Goal + '"')
                            ('"kind": "' + $interruptCase.ExpectedEvent + '"')
                        )
                        DelayMilliseconds = 100
                    }
                ) `
                -TimeoutMilliseconds 30000 `
                -AllowInterruptedChildWithoutExitMarker
        } finally {
            Stop-NibSmokeSessionExporter
        }
        Export-NibSmokeSessions
        $lastInterruptResult = $oneShotResult
        $oneShotOutputs[$interruptCase.Label] = $oneShotResult.Output
        $newInterruptSessions = @(
            Get-ChildItem -LiteralPath $sessionDirectory -Filter "*.json" -File |
                Where-Object { $sessionsBeforeInterrupt -notcontains $_.FullName }
        )
        if (Test-Path -LiteralPath $interruptSessionPath -PathType Leaf) {
            $lastInterruptSessionText = Get-Content -LiteralPath $interruptSessionPath -Raw
        }
        if ($oneShotResult.ExitCode -eq 0 -or
            -not $oneShotResult.ConsoleModesRestored -or
            -not $oneShotResult.ChildConsoleModesRestored -or
            -not $oneShotResult.Output.Contains("Run cancelled.")) {
            throw "Windows one-shot Ctrl+C did not reconcile $($interruptCase.Goal)"
        }
        if ($newInterruptSessions.Count -ne 1 -or
            $newInterruptSessions[0].FullName -ne $interruptSessionPath) {
            throw "Windows one-shot Ctrl+C did not create one isolated session for $($interruptCase.Label)"
        }
        $interruptSessionText = $lastInterruptSessionText
        $interruptSession = $interruptSessionText | ConvertFrom-Json
        $expectedStageEvent = $interruptSession.events |
            Where-Object { $_.kind -eq $interruptCase.ExpectedEvent } |
            Select-Object -First 1
        $cancelledEvent = $interruptSession.events |
            Where-Object {
                $_.kind -eq "run_terminal" -and
                $_.details.outcome -eq "cancelled_by_user"
            } |
            Select-Object -First 1
        if (-not $interruptSessionText.Contains('"goal": "' + $interruptCase.Goal + '"') -or
            $null -eq $expectedStageEvent -or
            $null -eq $cancelledEvent -or
            [int64]$cancelledEvent.index -le [int64]$expectedStageEvent.index) {
            throw "Windows one-shot Ctrl+C lacked exact durable stage evidence for $($interruptCase.Label)"
        }
        if (-not [string]::IsNullOrWhiteSpace($interruptCase.Forbidden) -and
            (Test-Path -LiteralPath (Join-Path $fixture $interruptCase.Forbidden))) {
            throw "Windows one-shot Ctrl+C allowed $($interruptCase.Goal) to mutate"
        }
    }
    $cancelledSessionCount = 0
    Export-NibSmokeSessions
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
    Export-NibSmokeSessions
    if (Test-Path -LiteralPath $sessionDirectory -PathType Container) {
        foreach ($sessionFile in Get-ChildItem -LiteralPath $sessionDirectory -Filter "*.json" -File) {
            $sessionText = Get-Content -LiteralPath $sessionFile.FullName -Raw
            if ($sessionText.Contains($privateSentinel)) {
                throw "Windows interactive smoke persisted the inactive-provider sentinel"
            }
        }
    }
    foreach ($sessionDatabase in Get-ChildItem -LiteralPath $nibHome -Filter "nib.db*" -File) {
        if (Test-NibSmokeFileContainsSentinel -Path $sessionDatabase.FullName -Sentinel $privateSentinel) {
            throw "Windows interactive smoke persisted the inactive-provider sentinel through $($sessionDatabase.Name)"
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
    if (-not [string]::IsNullOrWhiteSpace($env:NIB_INTERACTIVE_EVIDENCE_DIR)) {
        try { Export-NibSmokeSessions } catch { }
        if (-not [string]::IsNullOrWhiteSpace($interruptSessionPath) -and
            (Test-Path -LiteralPath $interruptSessionPath -PathType Leaf)) {
            $lastInterruptSessionText = Get-Content -LiteralPath $interruptSessionPath -Raw
        }
        $failureEvidenceDirectory = Join-Path $env:NIB_INTERACTIVE_EVIDENCE_DIR "Windows"
        New-Item -ItemType Directory -Force -Path $failureEvidenceDirectory | Out-Null
        $failureText = @(
            "platform=Windows"
            "source_revision=$sourceRevision"
            "source_clean=$sourceClean"
            "binary_version=$binaryVersion"
            "acceptance_eligible=false"
            "stage=$activeStage"
            "failure=$($_.Exception.Message)"
            if ($null -ne $lastInterruptResult) {
                "interrupt_exit_code=$($lastInterruptResult.ExitCode)"
                "caller_modes_restored=$($lastInterruptResult.ConsoleModesRestored)"
                "child_modes_restored=$($lastInterruptResult.ChildConsoleModesRestored)"
                "reported_run_cancelled=$($lastInterruptResult.Output.Contains('Run cancelled.'))"
                "interrupted_without_exit_marker=$($lastInterruptResult.InterruptedChildWithoutExitMarker)"
            }
        ) -join "`n"
        [IO.File]::WriteAllText(
            (Join-Path $failureEvidenceDirectory "failure-summary.txt"),
            $failureText.Replace($privateSentinel, "[fixture-secret]") + "`n",
            [Text.UTF8Encoding]::new($false)
        )
        if ($null -ne $lastInterruptResult) {
            [IO.File]::WriteAllText(
                (Join-Path $failureEvidenceDirectory "failure-interrupt-output.txt"),
                $lastInterruptResult.Output.Replace($privateSentinel, "[fixture-secret]"),
                [Text.UTF8Encoding]::new($false)
            )
        }
        if (-not [string]::IsNullOrWhiteSpace($lastInterruptSessionText)) {
            [IO.File]::WriteAllText(
                (Join-Path $failureEvidenceDirectory "failure-interrupt-session.json"),
                $lastInterruptSessionText.Replace($privateSentinel, "[fixture-secret]"),
                [Text.UTF8Encoding]::new($false)
            )
        }
    }
    $hostDiagnostics = [string]$_.Exception.Data["NibHostDiagnostics"]
    if (-not [string]::IsNullOrWhiteSpace($hostDiagnostics)) {
        # This fixture has one explicit secret sentinel; never print it in errors.
        [Console]::Error.WriteLine($hostDiagnostics.Replace($privateSentinel, "[fixture-secret]"))
    }
    throw
} finally {
    try { Stop-NibSmokeSessionExporter } catch { }
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
