# Retain process handles while reconciling pipe ownership and graceful shutdown.
function InitializeProcessSupport {
    if ('ScryUpdateProcess' -as [type]) { return }
    Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
public sealed class ScryUpdateProcess : IDisposable {
    private IntPtr handle;
    public uint Id { get; private set; }
    private ScryUpdateProcess(uint id, IntPtr value) { Id=id; handle=value; }
    [DllImport("kernel32",SetLastError=true)] private static extern IntPtr OpenProcess(uint access,bool inherit,uint id);
    [DllImport("kernel32",SetLastError=true)] private static extern uint WaitForSingleObject(IntPtr value,uint milliseconds);
    [DllImport("kernel32",CharSet=CharSet.Unicode,SetLastError=true)] private static extern bool QueryFullProcessImageName(IntPtr value,uint flags,StringBuilder path,ref uint size);
    [DllImport("kernel32",CharSet=CharSet.Unicode,SetLastError=true)] public static extern IntPtr OpenEvent(uint access,bool inherit,string name);
    [DllImport("kernel32",SetLastError=true)] public static extern bool SetEvent(IntPtr value);
    [DllImport("kernel32")] public static extern bool CloseHandle(IntPtr value);
    [DllImport("kernel32",SetLastError=true)] public static extern bool GetNamedPipeServerProcessId(IntPtr pipe,out uint id);
    [DllImport("kernel32",CharSet=CharSet.Unicode,SetLastError=true)] private static extern bool WaitNamedPipe(string name,uint milliseconds);
    public static bool PipeIsMissing() {
        if(WaitNamedPipe(@"\\.\pipe\scry",1)) return false;
        int error=Marshal.GetLastWin32Error();
        if(error==2) return true;
        if(error==121) return false; // busy, not evidence of absence
        throw new Win32Exception(error);
    }
    public static ScryUpdateProcess Open(uint id) {
        if(id==0) throw new ArgumentException("Invalid pipe server process ID");
        IntPtr value=OpenProcess(0x00101000,false,id); // synchronize + query limited information
        if(value==IntPtr.Zero) {
            int error=Marshal.GetLastWin32Error();
            if(error==87) return null; // PID no longer identifies a process
            throw new Win32Exception(error);
        }
        return new ScryUpdateProcess(id,value);
    }
    public bool HasExited { get { return WaitForExit(0); } }
    public bool WaitForExit(uint milliseconds) {
        uint result=WaitForSingleObject(handle,milliseconds);
        if(result==0) return true;
        if(result==258) return false;
        throw new Win32Exception(Marshal.GetLastWin32Error());
    }
    public string ImagePath { get {
        var path=new StringBuilder(32768); uint size=32768;
        if(!QueryFullProcessImageName(handle,0,path,ref size)) throw new Win32Exception(Marshal.GetLastWin32Error());
        return path.ToString();
    } }
    public void Dispose() { if(handle!=IntPtr.Zero) { CloseHandle(handle); handle=IntPtr.Zero; } }
}
"@
}
function OpenDaemonLease($processId) { return [ScryUpdateProcess]::Open($processId) }
function ConnectDaemonPipe {
    $pipe=[IO.Pipes.NamedPipeClientStream]::new('.','scry',[IO.Pipes.PipeDirection]::InOut)
    try {
        try { $pipe.Connect(100) } catch [TimeoutException] {
            if ([ScryUpdateProcess]::PipeIsMissing()) { $pipe.Dispose(); return $null }
            return @{Pipe=$pipe;Stale=$true}
        }
        $serverId=[uint32]0
        if (-not [ScryUpdateProcess]::GetNamedPipeServerProcessId($pipe.SafePipeHandle.DangerousGetHandle(),[ref]$serverId)) {
            $errorCode=[Runtime.InteropServices.Marshal]::GetLastWin32Error()
            if ($errorCode -in @(109,232,233)) { return @{Pipe=$pipe;Stale=$true} }
            throw [ComponentModel.Win32Exception]::new($errorCode)
        }
        if ($serverId -eq 0) { throw 'Invalid pipe server identity' }
        return @{Pipe=$pipe;Id=$serverId}
    } catch [IO.IOException] {
        if (($_.Exception.HResult -band 0xffff) -in @(109,232,233)) { return @{Pipe=$pipe;Stale=$true} }
        $pipe.Dispose(); throw
    } catch { $pipe.Dispose(); throw }
}
function ReadDaemonPath($lease) {
    if ($lease.HasExited) { return $null }
    try { $path=[IO.Path]::GetFullPath($lease.ImagePath) }
    catch { if ($lease.HasExited) { return $null }; throw }
    if ($lease.HasExited) { return $null }
    return $path
}
function GetPipeOwnerLease($expectedPath) {
    for ($attempt=0;$attempt -lt 8;$attempt++) {
        $connection=ConnectDaemonPipe
        if (-not $connection) { return $null }
        $lease=$null
        try {
            if (-not $connection.Stale) { $lease=OpenDaemonLease $connection.Id }
            if ($lease) {
                $path=ReadDaemonPath $lease
                if ($path) {
                    if (-not $expectedPath -or $path -ine $expectedPath) { throw 'Another daemon owns the query pipe; its process was left running' }
                    $result=$lease; $lease=$null
                    return $result
                }
            }
        } finally {
            $connection.Pipe.Dispose()
            if ($lease) { $lease.Dispose() }
        }
        # A dead identity is not absence. Close this connection and probe anew.
        Start-Sleep -Milliseconds 25
    }
    throw 'Could not reconcile the query pipe owner within the retry budget'
}
function GetInstalledDaemonLeases($expectedPath) {
    $leases=[Collections.Generic.List[object]]::new()
    $candidates=[Diagnostics.Process]::GetProcessesByName('scryd')
    try {
        if ($candidates.Count -gt 64) { throw 'Too many daemon processes to reconcile safely' }
        foreach ($candidate in $candidates) {
            $lease=OpenDaemonLease $candidate.Id
            if (-not $lease) { continue }
            try {
                $path=ReadDaemonPath $lease
                if ($expectedPath -and $path -and $path -ieq $expectedPath) { $leases.Add($lease); $lease=$null }
            } finally { if ($lease) { $lease.Dispose() } }
        }
        return ,$leases
    } catch { foreach ($lease in $leases) { $lease.Dispose() }; throw }
    finally { foreach ($candidate in $candidates) { $candidate.Dispose() } }
}
function OpenDaemonStopEvent($processId) { return [ScryUpdateProcess]::OpenEvent(2,$false,('Local\ScryShutdown-'+$processId)) }
function SignalDaemonStopEvent($handle) { return [ScryUpdateProcess]::SetEvent($handle) }
function CloseDaemonStopEvent($handle) { [ScryUpdateProcess]::CloseHandle($handle)|Out-Null }
function StopDaemonLease($lease,$deadline) {
    if ($lease.HasExited) { return }
    $event=OpenDaemonStopEvent $lease.Id
    if ($event -eq [IntPtr]::Zero) {
        if ($lease.HasExited) { return }
        throw 'Daemon is still running but graceful stop is unavailable or denied'
    }
    try {
        if ($lease.HasExited) { return }
        if (-not (SignalDaemonStopEvent $event)) {
            if ($lease.HasExited) { return }
            throw 'Graceful daemon stop signal failed'
        }
    } finally { CloseDaemonStopEvent $event }
    $remaining=[Math]::Max(0,[Math]::Min(30000,($deadline-[DateTime]::UtcNow).TotalMilliseconds))
    if (-not $lease.WaitForExit([uint32]$remaining)) { throw 'Daemon did not finish graceful shutdown' }
}
function StopInstalledDaemon($expectedPath) {
    $deadline=[DateTime]::UtcNow.AddSeconds(30)
    if ($UpdateDeadline -lt $deadline) { $deadline=$UpdateDeadline }
    $quiet=0
    for ($round=0;$round -lt 8 -and [DateTime]::UtcNow -lt $deadline;$round++) {
        $owner=$null; $leases=$null
        try {
            $owner=GetPipeOwnerLease $expectedPath
            $leases=GetInstalledDaemonLeases $expectedPath
            if ($owner) { $leases.Add($owner); $owner=$null }
            if ($leases.Count -eq 0) { $quiet++ } else { $quiet=0 }
            foreach ($lease in $leases) { StopDaemonLease $lease $deadline }
            if ($quiet -ge 2) { return }
        } finally {
            if ($owner) { $owner.Dispose() }
            if ($leases) { foreach ($lease in $leases) { $lease.Dispose() } }
        }
        # Re-enumerate after shutdown: the task or another client may restart it.
        Start-Sleep -Milliseconds 25
    }
    throw 'Daemon ownership did not settle within the restart budget'
}
