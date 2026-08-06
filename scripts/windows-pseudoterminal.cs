using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading.Tasks;
using Microsoft.Win32.SafeHandles;

namespace Nib.ReleaseQualification
{
    public sealed class PseudoTerminalResult
    {
        public PseudoTerminalResult(int exitCode, string output)
        {
            ExitCode = exitCode;
            Output = output;
        }

        public int ExitCode { get; private set; }
        public string Output { get; private set; }
    }

    public static class WindowsConsoleChild
    {
        private const int StdInputHandle = -10;
        private const int StdOutputHandle = -11;
        private const int StdErrorHandle = -12;
        private const uint GenericRead = 0x80000000;
        private const uint GenericWrite = 0x40000000;
        private const uint FileShareRead = 0x00000001;
        private const uint FileShareWrite = 0x00000002;
        private const uint OpenExisting = 3;
        private const uint WaitObject0 = 0;
        private const uint Infinite = 0xffffffff;

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        private struct StartupInfo
        {
            internal int cb;
            internal IntPtr lpReserved;
            internal IntPtr lpDesktop;
            internal IntPtr lpTitle;
            internal int dwX;
            internal int dwY;
            internal int dwXSize;
            internal int dwYSize;
            internal int dwXCountChars;
            internal int dwYCountChars;
            internal int dwFillAttribute;
            internal int dwFlags;
            internal short wShowWindow;
            internal short cbReserved2;
            internal IntPtr lpReserved2;
            internal IntPtr hStdInput;
            internal IntPtr hStdOutput;
            internal IntPtr hStdError;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct ProcessInformation
        {
            internal IntPtr hProcess;
            internal IntPtr hThread;
            internal uint dwProcessId;
            internal uint dwThreadId;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr CreateFileW(
            string fileName,
            uint desiredAccess,
            uint shareMode,
            IntPtr securityAttributes,
            uint creationDisposition,
            uint flagsAndAttributes,
            IntPtr templateFile);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern IntPtr GetStdHandle(int standardHandle);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool SetStdHandle(int standardHandle, IntPtr handle);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CreateProcessW(
            string applicationName,
            StringBuilder commandLine,
            IntPtr processAttributes,
            IntPtr threadAttributes,
            [MarshalAs(UnmanagedType.Bool)] bool inheritHandles,
            uint creationFlags,
            IntPtr environment,
            string currentDirectory,
            ref StartupInfo startupInfo,
            out ProcessInformation processInformation);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CloseHandle(IntPtr handle);

        public static int Run(string executable, string[] arguments)
        {
            if (string.IsNullOrWhiteSpace(executable))
            {
                throw new ArgumentException("A Windows console child path is required.", "executable");
            }
            if (arguments == null)
            {
                throw new ArgumentNullException("arguments");
            }

            string application = Path.GetFullPath(executable);
            if (!File.Exists(application))
            {
                throw new FileNotFoundException("The Windows console child does not exist.", application);
            }

            IntPtr savedInput = GetStdHandle(StdInputHandle);
            IntPtr savedOutput = GetStdHandle(StdOutputHandle);
            IntPtr savedError = GetStdHandle(StdErrorHandle);
            IntPtr consoleInput = IntPtr.Zero;
            IntPtr consoleOutput = IntPtr.Zero;
            IntPtr process = IntPtr.Zero;
            IntPtr thread = IntPtr.Zero;

            try
            {
                consoleInput = OpenConsole("CONIN$");
                consoleOutput = OpenConsole("CONOUT$");
                SetStandardHandle(StdInputHandle, consoleInput, "input");
                SetStandardHandle(StdOutputHandle, consoleOutput, "output");
                SetStandardHandle(StdErrorHandle, consoleOutput, "error");

                StartupInfo startupInfo = new StartupInfo();
                startupInfo.cb = Marshal.SizeOf(typeof(StartupInfo));
                ProcessInformation processInformation;
                StringBuilder commandLine = new StringBuilder(
                    WindowsPseudoTerminal.BuildCommandLine(application, arguments));
                if (!CreateProcessW(
                    application,
                    commandLine,
                    IntPtr.Zero,
                    IntPtr.Zero,
                    false,
                    0,
                    IntPtr.Zero,
                    null,
                    ref startupInfo,
                    out processInformation))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                process = processInformation.hProcess;
                thread = processInformation.hThread;
                CloseOwnedHandle(ref thread);

                uint waitResult = WaitForSingleObject(process, Infinite);
                if (waitResult != WaitObject0)
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                uint childExitCode;
                if (!GetExitCodeProcess(process, out childExitCode))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                return unchecked((int)childExitCode);
            }
            finally
            {
                // This process is a single-purpose ConPTY root. Restore every inherited
                // handle best-effort before it exits, even if only a prefix was changed.
                SetStdHandle(StdErrorHandle, savedError);
                SetStdHandle(StdOutputHandle, savedOutput);
                SetStdHandle(StdInputHandle, savedInput);
                CloseOwnedHandle(ref thread);
                CloseOwnedHandle(ref process);
                CloseOwnedHandle(ref consoleOutput);
                CloseOwnedHandle(ref consoleInput);
            }
        }

        private static IntPtr OpenConsole(string name)
        {
            IntPtr handle = CreateFileW(
                name,
                GenericRead | GenericWrite,
                FileShareRead | FileShareWrite,
                IntPtr.Zero,
                OpenExisting,
                0,
                IntPtr.Zero);
            if (handle == new IntPtr(-1))
            {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "Unable to open the pseudoterminal console device " + name + ".");
            }
            return handle;
        }

        private static void SetStandardHandle(int standardHandle, IntPtr handle, string label)
        {
            if (!SetStdHandle(standardHandle, handle))
            {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "Unable to set the pseudoterminal child " + label + " handle.");
            }
        }

        private static void CloseOwnedHandle(ref IntPtr handle)
        {
            if (handle == IntPtr.Zero || handle == new IntPtr(-1))
            {
                handle = IntPtr.Zero;
                return;
            }
            IntPtr owned = handle;
            handle = IntPtr.Zero;
            CloseHandle(owned);
        }
    }

    public static class WindowsPseudoTerminal
    {
        private const uint ExtendedStartupInfoPresent = 0x00080000;
        private const long ProcThreadAttributePseudoConsole = 0x00020016;
        private const int StartfUseStdHandles = 0x00000100;
        private const uint WaitObject0 = 0;
        private const uint WaitTimeout = 258;

        [StructLayout(LayoutKind.Sequential)]
        private struct Coord
        {
            internal short X;
            internal short Y;
        }

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        private struct StartupInfo
        {
            internal int cb;
            internal IntPtr lpReserved;
            internal IntPtr lpDesktop;
            internal IntPtr lpTitle;
            internal int dwX;
            internal int dwY;
            internal int dwXSize;
            internal int dwYSize;
            internal int dwXCountChars;
            internal int dwYCountChars;
            internal int dwFillAttribute;
            internal int dwFlags;
            internal short wShowWindow;
            internal short cbReserved2;
            internal IntPtr lpReserved2;
            internal IntPtr hStdInput;
            internal IntPtr hStdOutput;
            internal IntPtr hStdError;
        }

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        private struct StartupInfoEx
        {
            internal StartupInfo StartupInfo;
            internal IntPtr lpAttributeList;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct ProcessInformation
        {
            internal IntPtr hProcess;
            internal IntPtr hThread;
            internal uint dwProcessId;
            internal uint dwThreadId;
        }

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CreatePipe(
            out IntPtr readPipe,
            out IntPtr writePipe,
            IntPtr pipeAttributes,
            uint size);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CloseHandle(IntPtr handle);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern int CreatePseudoConsole(
            Coord size,
            IntPtr input,
            IntPtr output,
            uint flags,
            out IntPtr pseudoConsole);

        [DllImport("kernel32.dll")]
        private static extern void ClosePseudoConsole(IntPtr pseudoConsole);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool InitializeProcThreadAttributeList(
            IntPtr attributeList,
            int attributeCount,
            int flags,
            ref IntPtr size);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool UpdateProcThreadAttribute(
            IntPtr attributeList,
            uint flags,
            IntPtr attribute,
            IntPtr value,
            IntPtr size,
            IntPtr previousValue,
            IntPtr returnSize);

        [DllImport("kernel32.dll")]
        private static extern void DeleteProcThreadAttributeList(IntPtr attributeList);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CreateProcessW(
            string applicationName,
            StringBuilder commandLine,
            IntPtr processAttributes,
            IntPtr threadAttributes,
            [MarshalAs(UnmanagedType.Bool)] bool inheritHandles,
            uint creationFlags,
            IntPtr environment,
            string currentDirectory,
            ref StartupInfoEx startupInfo,
            out ProcessInformation processInformation);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);

        public static PseudoTerminalResult Run(
            string executable,
            string[] arguments,
            int timeoutMilliseconds)
        {
            if (string.IsNullOrWhiteSpace(executable))
            {
                throw new ArgumentException("A Windows executable path is required.", "executable");
            }
            if (arguments == null)
            {
                throw new ArgumentNullException("arguments");
            }
            if (timeoutMilliseconds < 1)
            {
                throw new ArgumentOutOfRangeException("timeoutMilliseconds");
            }

            string application = Path.GetFullPath(executable);
            if (!File.Exists(application))
            {
                throw new FileNotFoundException("The pseudoterminal child does not exist.", application);
            }

            IntPtr inputRead = IntPtr.Zero;
            IntPtr inputWrite = IntPtr.Zero;
            IntPtr outputRead = IntPtr.Zero;
            IntPtr outputWrite = IntPtr.Zero;
            IntPtr pseudoConsole = IntPtr.Zero;
            IntPtr attributeList = IntPtr.Zero;
            bool attributeListInitialized = false;
            IntPtr process = IntPtr.Zero;
            IntPtr thread = IntPtr.Zero;
            SafeFileHandle outputHandle = null;
            Task<string> outputTask = null;
            bool childExited = false;

            try
            {
                CreateAnonymousPipe(out inputRead, out inputWrite, "input");
                CreateAnonymousPipe(out outputRead, out outputWrite, "output");

                Coord size = new Coord { X = 120, Y = 30 };
                int pseudoConsoleResult = CreatePseudoConsole(
                    size,
                    inputRead,
                    outputWrite,
                    0,
                    out pseudoConsole);
                if (pseudoConsoleResult < 0)
                {
                    Marshal.ThrowExceptionForHR(pseudoConsoleResult);
                }

                IntPtr attributeListSize = IntPtr.Zero;
                InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref attributeListSize);
                if (attributeListSize == IntPtr.Zero)
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                attributeList = Marshal.AllocHGlobal(attributeListSize);
                if (!InitializeProcThreadAttributeList(
                    attributeList,
                    1,
                    0,
                    ref attributeListSize))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                attributeListInitialized = true;
                if (!UpdateProcThreadAttribute(
                    attributeList,
                    0,
                    new IntPtr(ProcThreadAttributePseudoConsole),
                    pseudoConsole,
                    new IntPtr(IntPtr.Size),
                    IntPtr.Zero,
                    IntPtr.Zero))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }

                StartupInfoEx startupInfo = new StartupInfoEx();
                startupInfo.StartupInfo.cb = Marshal.SizeOf(typeof(StartupInfoEx));
                // This single-purpose root does not rely on inherited handles. Once
                // attached, it opens ConPTY's own console devices for the real child.
                startupInfo.StartupInfo.dwFlags = StartfUseStdHandles;
                startupInfo.lpAttributeList = attributeList;
                ProcessInformation processInformation;
                StringBuilder commandLine = new StringBuilder(BuildCommandLine(application, arguments));
                if (!CreateProcessW(
                    application,
                    commandLine,
                    IntPtr.Zero,
                    IntPtr.Zero,
                    false,
                    ExtendedStartupInfoPresent,
                    IntPtr.Zero,
                    null,
                    ref startupInfo,
                    out processInformation))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                process = processInformation.hProcess;
                thread = processInformation.hThread;
                CloseOwnedHandle(ref thread);
                CloseOwnedHandle(ref inputRead);
                CloseOwnedHandle(ref inputWrite);
                CloseOwnedHandle(ref outputWrite);

                outputHandle = new SafeFileHandle(outputRead, true);
                outputRead = IntPtr.Zero;
                SafeFileHandle capturedOutput = outputHandle;
                outputTask = Task.Run(delegate
                {
                    using (FileStream stream = new FileStream(
                        capturedOutput,
                        FileAccess.Read,
                        4096,
                        false))
                    using (StreamReader reader = new StreamReader(
                        stream,
                        new UTF8Encoding(false, false),
                        true,
                        4096,
                        false))
                    {
                        return reader.ReadToEnd();
                    }
                });

                uint waitResult = WaitForSingleObject(process, checked((uint)timeoutMilliseconds));
                bool timedOut = waitResult == WaitTimeout;
                if (timedOut)
                {
                    outputHandle.Dispose();
                    throw new TimeoutException("The Windows pseudoterminal child exceeded its timeout.");
                }
                if (waitResult != WaitObject0)
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                childExited = true;

                uint childExitCode;
                if (!GetExitCodeProcess(process, out childExitCode))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }

                ClosePseudoConsoleBounded(ref pseudoConsole, 5000);
                if (!outputTask.Wait(30000))
                {
                    throw new TimeoutException("Timed out while draining Windows pseudoterminal output.");
                }
                string output = outputTask.GetAwaiter().GetResult();
                return new PseudoTerminalResult(unchecked((int)childExitCode), output);
            }
            finally
            {
                if (outputHandle != null)
                {
                    outputHandle.Dispose();
                }
                CloseOwnedHandle(ref inputRead);
                CloseOwnedHandle(ref inputWrite);
                CloseOwnedHandle(ref outputRead);
                CloseOwnedHandle(ref outputWrite);
                CloseOwnedHandle(ref thread);
                CloseOwnedHandle(ref process);
                if (pseudoConsole != IntPtr.Zero)
                {
                    if (!childExited)
                    {
                        // Preserve the live child hierarchy for the outer process-tree kill.
                        pseudoConsole = IntPtr.Zero;
                    }
                    else
                    {
                        try
                        {
                            ClosePseudoConsoleBounded(ref pseudoConsole, 5000);
                        }
                        catch
                        {
                            // The caller runs this host behind a killable process boundary.
                        }
                    }
                }
                if (attributeList != IntPtr.Zero)
                {
                    if (attributeListInitialized)
                    {
                        DeleteProcThreadAttributeList(attributeList);
                    }
                    Marshal.FreeHGlobal(attributeList);
                }
            }
        }

        private static void CreateAnonymousPipe(
            out IntPtr readPipe,
            out IntPtr writePipe,
            string label)
        {
            if (!CreatePipe(out readPipe, out writePipe, IntPtr.Zero, 0))
            {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "Unable to create the pseudoterminal " + label + " pipe.");
            }
        }

        private static void ClosePseudoConsoleBounded(
            ref IntPtr pseudoConsole,
            int timeoutMilliseconds)
        {
            IntPtr closing = pseudoConsole;
            pseudoConsole = IntPtr.Zero;
            Task closeTask = Task.Run(delegate { ClosePseudoConsole(closing); });
            if (!closeTask.Wait(timeoutMilliseconds))
            {
                throw new TimeoutException("Timed out while closing the Windows pseudoterminal.");
            }
            closeTask.GetAwaiter().GetResult();
        }

        private static void CloseOwnedHandle(ref IntPtr handle)
        {
            if (handle == IntPtr.Zero)
            {
                return;
            }
            IntPtr owned = handle;
            handle = IntPtr.Zero;
            CloseHandle(owned);
        }

        internal static string BuildCommandLine(string executable, string[] arguments)
        {
            List<string> command = new List<string>(arguments.Length + 1);
            command.Add(QuoteArgument(executable));
            foreach (string argument in arguments)
            {
                if (argument == null)
                {
                    throw new ArgumentException("Pseudoterminal arguments cannot be null.", "arguments");
                }
                command.Add(QuoteArgument(argument));
            }
            return string.Join(" ", command.ToArray());
        }

        private static string QuoteArgument(string argument)
        {
            if (argument.Length > 0 &&
                argument.IndexOfAny(new[] { ' ', '\t', '\n', '\v', '"' }) < 0)
            {
                return argument;
            }

            StringBuilder quoted = new StringBuilder();
            quoted.Append('"');
            int backslashes = 0;
            foreach (char character in argument)
            {
                if (character == '\\')
                {
                    backslashes++;
                    continue;
                }
                if (character == '"')
                {
                    quoted.Append('\\', (backslashes * 2) + 1);
                    quoted.Append('"');
                    backslashes = 0;
                    continue;
                }
                quoted.Append('\\', backslashes);
                backslashes = 0;
                quoted.Append(character);
            }
            quoted.Append('\\', backslashes * 2);
            quoted.Append('"');
            return quoted.ToString();
        }
    }
}
