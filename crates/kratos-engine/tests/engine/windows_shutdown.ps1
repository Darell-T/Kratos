# Ctrl+C is console-wide on Windows. Start the actual binary in a separate hidden console,
# forward its stdout/stderr, and wait on its process handle after the test requests shutdown.
$ErrorActionPreference = 'Stop'
Add-Type @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public sealed class ConsoleEngine : IDisposable {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct StartupInfo {
        public uint size;
        public string reserved, desktop, title;
        public uint x, y, width, height, columns, rows, fill, flags;
        public ushort show, reservedSize;
        public IntPtr reservedBytes, stdin, stdout, stderr;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct ProcessInfo { public IntPtr process, thread; public uint id, threadId; }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcess(string app, IntPtr command, IntPtr processAttributes,
        IntPtr threadAttributes, bool inherit, uint flags, IntPtr environment,
        string directory, ref StartupInfo startup, out ProcessInfo process);
    [DllImport("kernel32.dll")] static extern IntPtr GetStdHandle(int handle);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetHandleInformation(IntPtr handle, uint mask, uint flags);
    [DllImport("kernel32.dll")] static extern bool FreeConsole();
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool AttachConsole(uint id);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetConsoleCtrlHandler(IntPtr handler, bool add);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GenerateConsoleCtrlEvent(uint signal, uint group);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr handle, uint millis);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr handle, out uint code);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr handle, uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    ProcessInfo process;
    static void Check(bool succeeded) { if (!succeeded) throw new Win32Exception(); }

    public ConsoleEngine(string executable) {
        var startup = new StartupInfo { size = (uint)Marshal.SizeOf(typeof(StartupInfo)),
            flags = 0x101, show = 0, stdin = GetStdHandle(-10),
            stdout = GetStdHandle(-11), stderr = GetStdHandle(-12) };
        Check(SetHandleInformation(startup.stdin, 1, 1));
        Check(SetHandleInformation(startup.stdout, 1, 1));
        Check(SetHandleInformation(startup.stderr, 1, 1));
        // CREATE_NEW_CONSOLE, STARTF_USESHOWWINDOW + STARTF_USESTDHANDLES, SW_HIDE.
        Check(CreateProcess(executable, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, true,
            0x10, IntPtr.Zero, null, ref startup, out process));
        CloseHandle(process.thread);
    }
    public void Interrupt() {
        FreeConsole();
        Check(AttachConsole(process.id));
        Check(SetConsoleCtrlHandler(IntPtr.Zero, true));
        Check(GenerateConsoleCtrlEvent(0, 0));
    }
    public void WaitForCleanExit() {
        uint result = WaitForSingleObject(process.process, 8000);
        if (result == 258) throw new TimeoutException("engine still running eight seconds after Ctrl+C");
        if (result != 0) throw new Win32Exception();
        uint code;
        Check(GetExitCodeProcess(process.process, out code));
        if (code != 0) throw new Exception("engine exit code: " + code);
    }
    public void Dispose() {
        if (WaitForSingleObject(process.process, 0) == 258) {
            Check(TerminateProcess(process.process, 1));
            if (WaitForSingleObject(process.process, 1000) != 0)
                throw new TimeoutException("could not clean up the engine");
        }
        CloseHandle(process.process);
    }
}
'@
$engine = [ConsoleEngine]::new($env:KRATOS_SHUTDOWN_ENGINE)
try {
    if ([Console]::ReadLine() -ne 'shutdown') { throw 'shutdown test closed its input' }
    $engine.Interrupt()
    $engine.WaitForCleanExit()
} finally {
    $engine.Dispose()
}
