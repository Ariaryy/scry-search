# Deterministic lifecycle races plus a real retained process-handle check.
$ErrorActionPreference='Stop'
$support=Join-Path $PSScriptRoot 'updater/processes.ps1'
. $support
InitializeProcessSupport
function NewLease($number,$path,$exited=$false) {
    $lease=[pscustomobject]@{Id=$number;ImagePath=$path;HasExited=$exited;Disposed=0;Waited=0}
    $lease|Add-Member ScriptMethod Dispose {$this.Disposed++}
    $lease|Add-Member ScriptMethod WaitForExit {param($timeout)$this.Waited++;$this.HasExited=$true;return $true}
    return $lease
}
function NewConnection($number) {
    $pipe=[pscustomobject]@{Disposed=0}
    $pipe|Add-Member ScriptMethod Dispose {$this.Disposed++}
    return @{Id=$number;Pipe=$pipe}
}
$expected=[IO.Path]::GetFullPath((Join-Path ([IO.Path]::GetTempPath()) 'scry-process-fixture/scryd.exe'))
foreach($scenario in @('replacement','disconnected','absence','foreign','denied','path_denied','path_exit','persistent_stale','busy')) {
    & {
        . $support
        $state=[pscustomobject]@{Probes=0;Connections=[Collections.Generic.List[object]]::new()}
        $fixtureLease=NewLease 102 $expected
        if($scenario -eq 'foreign'){$fixtureLease.ImagePath=[IO.Path]::GetFullPath((Join-Path ([IO.Path]::GetTempPath()) 'another-daemon/scryd.exe'))}
        if($scenario -in @('path_denied','path_exit')) {
            $fixtureLease|Add-Member -Force ScriptProperty ImagePath {
                if($scenario -eq 'path_exit'){$this.HasExited=$true}
                throw [UnauthorizedAccessException]::new('query denied')
            }
        }
        function ConnectDaemonPipe {
            $state.Probes++
            if($state.Probes -gt 1 -and $scenario -in @('absence','path_exit')){return $null}
            $connection=NewConnection $(if($state.Probes -eq 1){101}else{102})
            if($scenario -eq 'busy' -or ($scenario -eq 'disconnected' -and $state.Probes -eq 1)){$connection.Stale=$true}
            $state.Connections.Add($connection)
            return $connection
        }
        function OpenDaemonLease($processId) {
            if($scenario -eq 'denied'){throw [UnauthorizedAccessException]::new('open denied')}
            if($scenario -eq 'persistent_stale'){return $null}
            if($scenario -in @('path_denied','path_exit')){return $fixtureLease}
            if($processId -eq 101){return $null}
            return $fixtureLease
        }
        $failure=$null;$owner=$null
        try {$owner=GetPipeOwnerLease $expected} catch {$failure=$_}
        if($scenario -in @('replacement','disconnected')) {
            if($failure -or -not $owner -or $owner.Id -ne 102 -or $state.Probes -ne 2){throw "Disappearing owner replacement failed: $failure; probes=$($state.Probes)"}
            $owner.Dispose()
        } elseif($scenario -in @('absence','path_exit')) {
            if($failure -or $owner -or $state.Probes -ne 2){throw 'Confirmed exit did not require a fresh probe'}
        } else {
            if(-not $failure -or $owner){throw "Unsafe ownership acceptance: $scenario"}
            if($scenario -in @('denied','path_denied') -and $state.Probes -ne 1){throw 'Access failure was treated as disappearance'}
            if($scenario -in @('persistent_stale','busy') -and $state.Probes -ne 8){throw 'Stale identity retry budget changed'}
        }
        foreach($connection in $state.Connections){if($connection.Pipe.Disposed -ne 1){throw 'Probe connection leaked'}}
        if($scenario -in @('replacement','disconnected','foreign','path_denied','path_exit') -and $fixtureLease.Disposed -ne 1){throw 'Owner handle leaked'}
    }
}
foreach($scenario in @('already_exited','event_disappeared','event_denied','exit_before_signal','exit_during_signal','wait','wait_timeout')) {
    & {
        . $support
        $fixtureLease=NewLease 103 $expected ($scenario -eq 'already_exited')
        if($scenario -eq 'wait_timeout'){$fixtureLease|Add-Member -Force ScriptMethod WaitForExit {param($timeout)$this.Waited++;return $false}}
        $state=[pscustomobject]@{Opened=0;Signalled=0;Closed=0}
        function OpenDaemonStopEvent($processId) {
            $state.Opened++
            if($scenario -eq 'already_exited'){throw 'Exited process event was opened'}
            if($scenario -in @('event_disappeared','exit_before_signal')){$fixtureLease.HasExited=$true}
            if($scenario -in @('event_disappeared','event_denied')){return [IntPtr]::Zero}
            return [IntPtr]1
        }
        function SignalDaemonStopEvent($handle) {
            $state.Signalled++
            if($scenario -eq 'exit_during_signal'){$fixtureLease.HasExited=$true;return $false}
            return $true
        }
        function CloseDaemonStopEvent($handle) {$state.Closed++}
        $failure=$null
        try {StopDaemonLease $fixtureLease ([DateTime]::UtcNow.AddSeconds(1))} catch {$failure=$_}
        if(($scenario -in @('event_denied','wait_timeout')) -ne [bool]$failure){throw "Incorrect graceful stop result: $scenario"}
        if($scenario -eq 'already_exited' -and $state.Opened -ne 0){throw 'Already exited process was signalled'}
        if($scenario -eq 'exit_before_signal' -and $state.Signalled -ne 0){throw 'Exited process was signalled'}
        if($scenario -in @('exit_before_signal','exit_during_signal','wait','wait_timeout') -and $state.Closed -ne 1){throw 'Stop event leaked'}
        if($scenario -eq 'wait' -and $fixtureLease.Waited -ne 1){throw 'Process handle was not waited'}
    }
}
& {
    . $support
    $UpdateDeadline=[DateTime]::UtcNow.AddSeconds(5)
    $state=[pscustomobject]@{Scans=0;Stopped=[Collections.Generic.List[int]]::new()}
    $first=NewLease 201 $expected;$replacement=NewLease 202 $expected
    function GetPipeOwnerLease($path){return $null}
    function GetInstalledDaemonLeases($path) {
        $state.Scans++
        $list=[Collections.Generic.List[object]]::new()
        if($state.Scans -eq 1){$list.Add($first)}
        if($state.Scans -eq 2){$list.Add($replacement)}
        return ,$list
    }
    function StopDaemonLease($lease,$deadline){$state.Stopped.Add($lease.Id);$lease.HasExited=$true}
    StopInstalledDaemon $expected
    if($state.Scans -ne 4 -or ($state.Stopped -join ',') -ne '201,202'){throw 'Replacement was not reconciled before quiescence'}
    if($first.Disposed -ne 1 -or $replacement.Disposed -ne 1){throw 'Shutdown process handle leaked'}
}
& {
    . $support
    $UpdateDeadline=[DateTime]::UtcNow.AddSeconds(5)
    $state=[pscustomobject]@{Probes=0;Stopped=0}
    $old=NewLease 301 $expected
    function GetPipeOwnerLease($path) {
        $state.Probes++
        if($state.Probes -gt 1){throw 'foreign replacement'}
        return $null
    }
    function GetInstalledDaemonLeases($path){$list=[Collections.Generic.List[object]]::new();$list.Add($old);return ,$list}
    function StopDaemonLease($lease,$deadline){$state.Stopped++;$lease.HasExited=$true}
    $failed=$false
    try {StopInstalledDaemon $expected} catch {$failed=$true}
    if(-not $failed -or $state.Stopped -ne 1 -or $state.Probes -ne 2){throw 'Foreign replacement after stop was not rejected'}
    if($old.Disposed -ne 1){throw 'Original shutdown handle leaked'}
}
# Exercise the actual native adapter without elevation or touching a daemon.
$fixturePipeName='scry-updater-process-test-'+[Guid]::NewGuid().ToString('N')
$fixtureCommand="`$server=[IO.Pipes.NamedPipeServerStream]::new('$fixturePipeName',[IO.Pipes.PipeDirection]::InOut);`$server.WaitForConnection();[Console]::In.ReadLine()|Out-Null;`$server.Dispose()"
$info=[Diagnostics.ProcessStartInfo]::new()
$info.FileName=Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
$info.Arguments='-NoProfile -NonInteractive -EncodedCommand '+[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($fixtureCommand))
$info.UseShellExecute=$false;$info.CreateNoWindow=$true;$info.RedirectStandardInput=$true
$child=[Diagnostics.Process]::Start($info)
$native=$null
$client=[IO.Pipes.NamedPipeClientStream]::new('.',$fixturePipeName,[IO.Pipes.PipeDirection]::InOut)
try {
    $client.Connect(5000)
    $nativeOwner=[uint32]0
    if(-not [ScryUpdateProcess]::GetNamedPipeServerProcessId($client.SafePipeHandle.DangerousGetHandle(),[ref]$nativeOwner) -or $nativeOwner -ne $child.Id){throw 'Real pipe owner was not identified'}
    $native=[ScryUpdateProcess]::Open($child.Id)
    if(-not $native -or $native.HasExited -or -not $native.ImagePath){throw 'Could not retain real process identity'}
    $child.StandardInput.WriteLine('finish')
    if(-not $native.WaitForExit(5000)){throw 'Real process did not exit'}
    # Windows may keep reporting the exited owner while a client retains its pipe.
    if([ScryUpdateProcess]::GetNamedPipeServerProcessId($client.SafePipeHandle.DangerousGetHandle(),[ref]$nativeOwner)){
        if($nativeOwner -ne $child.Id){throw 'Unexpected retained pipe identity'}
    }
    StopDaemonLease $native ([DateTime]::UtcNow.AddSeconds(1))
} finally {
    $client.Dispose()
    if($native){$native.Dispose()}
    if(-not $child.HasExited){$child.Kill()}
    $child.Dispose()
}
Write-Output 'Updater ownership, replacement, access-error and graceful-exit race tests passed'
