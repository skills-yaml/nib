if ($null -eq ("Nib.WindowsPseudoTerminal.OutputCapture" -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Text;
using System.Threading.Tasks;

namespace Nib.WindowsPseudoTerminal {
    public sealed class OutputCapture {
        private readonly object gate = new object();
        private readonly StringBuilder text = new StringBuilder();
        public Task Completion { get; private set; }

        public OutputCapture(StreamReader reader) {
            Completion = DrainAsync(reader);
        }

        private async Task DrainAsync(StreamReader reader) {
            var buffer = new char[4096];
            int count;
            while ((count = await reader.ReadAsync(buffer, 0, buffer.Length).ConfigureAwait(false)) != 0) {
                lock (gate) { text.Append(buffer, 0, count); }
            }
        }

        public bool Contains(string expected) {
            lock (gate) { return text.ToString().IndexOf(expected, StringComparison.Ordinal) >= 0; }
        }

        public string Text {
            get { lock (gate) { return text.ToString(); } }
        }
    }
}
'@
}

function Wait-NibWindowsPseudoTerminalOutput {
    param(
        [Parameter(Mandatory = $true)][object]$Capture,
        [Parameter(Mandatory = $true)][string]$Expected,
        [Parameter(Mandatory = $true)][Diagnostics.Stopwatch]$Stopwatch,
        [Parameter(Mandatory = $true)][int]$TimeoutMilliseconds
    )

    while (-not $Capture.Contains($Expected)) {
        if ($Stopwatch.ElapsedMilliseconds -ge $TimeoutMilliseconds) {
            throw "Timed out waiting for Windows pseudoterminal prompt: $Expected"
        }
        if ($Capture.Completion.IsCompleted) {
            $Capture.Completion.GetAwaiter().GetResult() | Out-Null
            # The final read can append the prompt between Contains and EOF.
            if ($Capture.Contains($Expected)) { return }
            throw "Windows pseudoterminal output ended before the expected prompt: $Expected"
        }
        Start-Sleep -Milliseconds 25
    }
}

function Wait-NibWindowsPseudoTerminalInputDelay {
    param(
        [Parameter(Mandatory = $true)][int]$DelayMilliseconds,
        [Parameter(Mandatory = $true)][Diagnostics.Stopwatch]$Stopwatch,
        [Parameter(Mandatory = $true)][int]$TimeoutMilliseconds
    )

    $remainingMilliseconds = $TimeoutMilliseconds - $Stopwatch.ElapsedMilliseconds
    if ($DelayMilliseconds -ge $remainingMilliseconds) {
        throw "Windows pseudoterminal input delay exceeds the remaining child timeout"
    }
    if ($DelayMilliseconds -gt 0) {
        Start-Sleep -Milliseconds $DelayMilliseconds
    }
}
