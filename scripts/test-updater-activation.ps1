# Explicit elevated acceptance test. Uses a separate protected directory and task.
# Network responses are local fixtures; production updater trust remains unchanged.
param([switch]$Elevated,[switch]$WorkerRun)
$ErrorActionPreference='Stop'
$repo=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$report=Join-Path $repo 'target/updater-activation-result.json'
if (-not $Elevated) {
    $command="& '"+$PSCommandPath.Replace("'","''")+"' -Elevated; exit `$LASTEXITCODE"
    $encoded=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
    $process=Start-Process -FilePath (Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe') -Verb RunAs -WindowStyle Hidden -ArgumentList @('-NoProfile','-NonInteractive','-EncodedCommand',$encoded) -PassThru
    $null=$process.Handle
    $process.WaitForExit()
    Get-Content -LiteralPath $report -Raw
    exit $process.ExitCode
}
# One approval covers bounded reruns of this fixed acceptance test. Signal
# files contain no commands, executable paths, or script content.
if (-not $WorkerRun) {
    $request=Join-Path $repo 'target/updater-acceptance.request'
    $stop=Join-Path $repo 'target/updater-acceptance.stop'
    $state=Join-Path $repo 'target/updater-acceptance-worker.json'
    foreach($signal in @($request,$stop)){Remove-Item -LiteralPath $signal -Force -ErrorAction SilentlyContinue}
    $deadline=[DateTime]::UtcNow.AddMinutes(20)
    $first=$true
    while([DateTime]::UtcNow -lt $deadline -and -not (Test-Path -LiteralPath $stop)) {
        if($first -or (Test-Path -LiteralPath $request)) {
            $first=$false
            Remove-Item -LiteralPath $request -Force -ErrorAction SilentlyContinue
            @{state='running';process_id=$PID}|ConvertTo-Json -Compress|Set-Content -LiteralPath $state -Encoding UTF8
            & (Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe') -NoProfile -NonInteractive -File $PSCommandPath -Elevated -WorkerRun
            @{state='waiting';last_exit_code=$LASTEXITCODE;process_id=$PID}|ConvertTo-Json -Compress|Set-Content -LiteralPath $state -Encoding UTF8
        }
        Start-Sleep -Milliseconds 500
    }
    @{state='stopped';process_id=$PID}|ConvertTo-Json -Compress|Set-Content -LiteralPath $state -Encoding UTF8
    exit 0
}
$env:PSModulePath=Join-Path $PSHOME 'Modules'
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$base=Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Scry Search Updater Validation'
$root=Join-Path $base $sid
$taskName='Scry Search Updater Validation'
$fixture=Join-Path $repo 'target/updater-activation-fixture'
$events=Join-Path $repo 'target/updater-activation-events.jsonl'
$oldPath=[Environment]::GetEnvironmentVariable('Path','User')
$resume=Join-Path $repo 'target/release/scryd.exe'
$resumeStopped=$false
$success=$false
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class ScryValidationStop {
 [DllImport("kernel32",CharSet=CharSet.Unicode)] public static extern IntPtr OpenEvent(uint access,bool inherit,string name);
 [DllImport("kernel32")] public static extern bool SetEvent(IntPtr handle);
 [DllImport("kernel32")] public static extern bool CloseHandle(IntPtr handle);
}
"@
function StopKnownDaemon($path) {
    foreach($process in @(Get-Process -Name scryd -ErrorAction SilentlyContinue|Where-Object {$_.Path -and [IO.Path]::GetFullPath($_.Path) -ieq [IO.Path]::GetFullPath($path)})) {
        $handle=[ScryValidationStop]::OpenEvent(2,$false,('Local\ScryShutdown-'+$process.Id))
        if($handle -eq [IntPtr]::Zero){throw 'Validation daemon has no graceful stop event'}
        try {if(-not [ScryValidationStop]::SetEvent($handle)){throw 'Validation stop failed'}}finally{[ScryValidationStop]::CloseHandle($handle)|Out-Null}
        if(-not $process.WaitForExit(30000)){throw 'Validation daemon did not stop'}
    }
}
try {
    if(Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue){throw 'Validation task already exists'}
    if(Test-Path -LiteralPath $root){throw 'Validation directory already exists'}
    [IO.Directory]::CreateDirectory($fixture)|Out-Null
    foreach($name in @('scry.exe','scryd.exe')){Copy-Item -LiteralPath (Join-Path $repo ('target/stable-build/release/'+$name)) -Destination (Join-Path $fixture $name) -Force}
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'uninstall-daemon.ps1') -Destination $fixture -Force
    @{schema=1;version='0.1.0';ipc_version=1;index_format=10}|ConvertTo-Json -Compress|Set-Content -LiteralPath (Join-Path $fixture 'scry-release.json') -Encoding UTF8
    $package=Join-Path $repo 'target/updater-activation-package.zip'
    Compress-Archive -Path (Join-Path $fixture '*') -DestinationPath $package -Force
    $digest=(Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash.ToLowerInvariant()
    $metadata=@{tag_name='v0.1.0';draft=$false;assets=@(@{name='scry-search-v0.1.0-windows-x86_64.zip';digest=('sha256:'+$digest)})}|ConvertTo-Json -Depth 5 -Compress
    $ast=[Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'updater/coordinator.ps1'),[ref]$null,[ref]$null)
    $assignment=$ast.Find({param($n)$n -is [Management.Automation.Language.AssignmentStatementAst] -and $n.Left.Extent.Text -eq '$engine'},$true)
    $engine=$assignment.Right.Expression.Value.Replace('# RELEASE_SUPPORT',(Get-Content -LiteralPath (Join-Path $PSScriptRoot 'updater/release.ps1') -Raw))
    $engine=$engine.Replace('# PROCESS_SUPPORT',(Get-Content -LiteralPath (Join-Path $PSScriptRoot 'updater/processes.ps1') -Raw))
    # Change only test-owned dependencies. The install, stop, readiness, ACL and
    # journal implementation under test is the production engine.
    $engine=$engine.Replace("'Scry Search'","'Scry Search Updater Validation'").Replace("'Scry Search Daemon'","'Scry Search Updater Validation'")
    foreach($name in @('Fetch','Send')) {
        $parsed=[Management.Automation.Language.Parser]::ParseInput($engine,[ref]$null,[ref]$null)
        $fn=$parsed.Find({param($n)$n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name},$true)
        $replacement=if($name -eq 'Fetch') {
            'function Fetch($url,$destination,$limit){if($url.EndsWith(".zip")){Copy-Item -LiteralPath $package -Destination $destination}else{Set-Content -LiteralPath $destination -Value $metadata -Encoding UTF8}}'
        } else {
            'function Send($event,$state,$message,$code){@{event=$event;state=$state;message=$message;exit_code=$code}|ConvertTo-Json -Compress|Add-Content -LiteralPath $events -Encoding UTF8}'
        }
        $engine=$engine.Remove($fn.Extent.StartOffset,$fn.Extent.EndOffset-$fn.Extent.StartOffset).Insert($fn.Extent.StartOffset,$replacement)
    }
    $engine=[regex]::Replace($engine,'(?s)\$pipe = \[IO.Pipes.NamedPipeClientStream\].*?\$writer.AutoFlush = \$true','$pipe=[IO.MemoryStream]::new();$writer=[IO.StreamWriter]::new($pipe)')
    $engine=[regex]::Replace($engine,'(?m)^exit \$code\s*$','if($code -ne 0){throw "Acceptance engine failed with code $code"}')
    $engine=$engine.Replace('exit 0','return')
    $Release='v0.1.0';$OriginSid=$sid
    $running=@(Get-Process -Name scryd -ErrorAction SilentlyContinue)
    if($running.Count -ne 1 -or [IO.Path]::GetFullPath($running[0].Path) -ine [IO.Path]::GetFullPath($resume)){throw 'Expected temporary PR daemon is not the sole running daemon'}
    StopKnownDaemon $resume
    $resumeStopped=$true
    & ([scriptblock]::Create($engine))
    $journal=Join-Path $root 'transaction.json'
    $committed=Get-Content -LiteralPath $journal -Raw|ConvertFrom-Json
    if($committed.phase -ne 'committed'){throw 'Initial activation did not commit'}
    # A second install exercises graceful shutdown and task preservation.
    $firstTarget=$committed.target
    & ([scriptblock]::Create($engine))
    $pending=Get-Content -LiteralPath $journal -Raw|ConvertFrom-Json
    if($pending.target -eq $firstTarget){throw 'Replacement did not select another binary directory'}
    $pending.phase='activated';$pending|ConvertTo-Json -Depth 4|Set-Content -LiteralPath $journal -Encoding UTF8
    Remove-Item -LiteralPath (Join-Path $root 'uninstall.ps1')
    [Environment]::SetEnvironmentVariable('Path',$oldPath,'User')
    & ([scriptblock]::Create($engine))
    $recovered=Get-Content -LiteralPath $journal -Raw|ConvertFrom-Json
    if($recovered.phase -ne 'committed' -or -not (Test-Path -LiteralPath (Join-Path $root 'uninstall.ps1'))){throw 'Recovery did not finish installation'}
    if(@([Environment]::GetEnvironmentVariable('Path','User') -split ';'|Where-Object {$_ -ieq $recovered.target}).Count -ne 1){throw 'Recovered PATH is incomplete'}
    $success=$true
    @{success=$true;tests=@('initial_activation','graceful_replacement','interrupted_activation_recovery');version='0.1.0'}|ConvertTo-Json -Compress|Set-Content -LiteralPath $report -Encoding UTF8
} catch {
    @{success=$false;message=$_.Exception.Message}|ConvertTo-Json -Compress|Set-Content -LiteralPath $report -Encoding UTF8
} finally {
    foreach($process in @(Get-Process -Name scryd -ErrorAction SilentlyContinue|Where-Object {$_.Path -and $_.Path.StartsWith($root+'\',[StringComparison]::OrdinalIgnoreCase)})){StopKnownDaemon $process.Path}
    if(Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue){Unregister-ScheduledTask -TaskName $taskName -Confirm:$false}
    [Environment]::SetEnvironmentVariable('Path',$oldPath,'User')
    if($resumeStopped){Start-Process -FilePath $resume -WorkingDirectory $repo -WindowStyle Hidden|Out-Null}
    $expected=[IO.Path]::GetFullPath((Join-Path $base $sid))
    if(Test-Path -LiteralPath $root){if([IO.Path]::GetFullPath($root) -ine $expected -or ((Get-Item -LiteralPath $root).Attributes -band [IO.FileAttributes]::ReparsePoint)){throw 'Unexpected validation cleanup path'};Remove-Item -LiteralPath $root -Recurse -Force}
}
if(-not $success){exit 1}
