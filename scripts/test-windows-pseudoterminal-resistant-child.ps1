[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class NibResistantConsoleChild
{
    private delegate bool ConsoleCtrlHandler(uint controlType);
    private static readonly ConsoleCtrlHandler Handler = IgnoreClose;

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetConsoleCtrlHandler(ConsoleCtrlHandler handler, bool add);

    public static void Install()
    {
        if (!SetConsoleCtrlHandler(Handler, true))
        {
            throw new InvalidOperationException("Unable to install the console close handler.");
        }
    }

    private static bool IgnoreClose(uint controlType)
    {
        return controlType == 2;
    }
}
'@

[NibResistantConsoleChild]::Install()
[IO.File]::WriteAllText($env:NIB_PTY_DESCENDANT_PID_FILE, [string]$PID)
Start-Sleep -Seconds 60
