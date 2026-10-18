<# Run from an elevated PowerShell. Uses a new private live daemon/cache only. #>
param(
    [Parameter(Mandatory=$true)][ValidatePattern('^[A-Za-z]:$')][string]$Volume,
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [Parameter(Mandatory=$true)][string]$Machine,
    [Parameter(Mandatory=$true)][string]$Power,
    [string]$Daemon='target/release/scryd.exe',
    [string]$Bench='target/release/scry-bench.exe',
    [ValidateRange(1,7200)][int]$IdleSeconds=600,
    [ValidateRange(0,600)][int]$SettleSeconds=30,
    [ValidateRange(0,100)][int]$VisibilitySamples=10,
    [ValidateRange(0,600)][int]$ConnectedIdleSeconds=60,
    [ValidateRange(0,100000)][int]$IndexMbps=128
)
$ErrorActionPreference='Stop'
$identity=[Security.Principal.WindowsIdentity]::GetCurrent()
if (!(New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run this script from an elevated PowerShell for raw NTFS indexing and observation.' }
$repoRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
function TaskPath($path) { if ([IO.Path]::IsPathRooted($path)) {return [IO.Path]::GetFullPath($path)}; return [IO.Path]::GetFullPath([IO.Path]::Combine($repoRoot,$path)) }
$destination=TaskPath $OutputDirectory
$scratch=[IO.Path]::Combine($repoRoot,'.scratch')
if (!$destination.StartsWith($scratch+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)) {throw 'Output must be inside .scratch'}
if ([IO.Directory]::Exists($destination)) {throw 'Output directory must be new'}
[IO.Directory]::CreateDirectory($destination)|Out-Null
$ancestor=Get-Item -LiteralPath $destination
while ($ancestor.FullName -ne (Get-Item -LiteralPath $repoRoot).FullName) {
    if (($ancestor.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {throw 'Output ancestry contains a reparse point'}
    $ancestor=$ancestor.Parent
    if ($null -eq $ancestor) {throw 'Output ancestry left repository'}
}
function Sha256($path) { $stream=[IO.File]::OpenRead($path);$sha=[Security.Cryptography.SHA256]::Create();try{return ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-','').ToLowerInvariant()}finally{$stream.Dispose();$sha.Dispose()} }
function StartInfo($executable,[string[]]$arguments) {
    if (@($arguments|Where-Object {$_.Contains('"') -or $_.Contains("`n") -or $_.Contains("`r")}).Count) {throw 'Unsupported quote/newline in arguments'}
    $start=New-Object Diagnostics.ProcessStartInfo
    $start.FileName=$executable;$start.WorkingDirectory=$repoRoot
    $start.Arguments=($arguments|ForEach-Object {'"'+$_+'"'}) -join ' '
    $start.UseShellExecute=$false;$start.CreateNoWindow=$true
    $start.RedirectStandardOutput=$true;$start.RedirectStandardError=$true
    return $start
}
function Captured($executable,[string[]]$arguments,$label) {
    $process=[Diagnostics.Process]::Start((StartInfo $executable $arguments))
    $stdout=$process.StandardOutput.ReadToEndAsync();$stderr=$process.StandardError.ReadToEndAsync()
    try{$process.WaitForExit();$code=$process.ExitCode}finally{if(!$process.HasExited){$process.Kill();$process.WaitForExit()}}
    [IO.File]::WriteAllText((Join-Path $destination "$label.stdout.log"),$stdout.Result)
    [IO.File]::WriteAllText((Join-Path $destination "$label.stderr.log"),$stderr.Result)
    $process.Dispose();return $code
}
$daemonPath=TaskPath $Daemon;$benchPath=TaskPath $Bench
$pipe="\\.\pipe\scry-bench-live-$PID"
$cache=Join-Path $destination 'cache'
$context=@{schema=1;daemon_sha256=(Sha256 $daemonPath);bench_sha256=(Sha256 $benchPath);machine=$Machine;power=$Power;query_threads=1;refinement_cache=$true;index_cache='fresh directory; no resume';os_cache='uncontrolled; not a cold-disk claim';index_volume_count=1;idle_seconds=$IdleSeconds;settle_seconds=$SettleSeconds;visibility_samples=$VisibilitySamples;installed_daemon='untouched';index_sampling_ms=50}
$context.connected_idle_seconds=$ConnectedIdleSeconds
$context.index_read_cap_mib_per_s=$IndexMbps
[IO.File]::WriteAllText((Join-Path $destination 'context.json'),($context|ConvertTo-Json))
# Reserve this file before indexing. Updating it after readiness then produces
# data-modification events rather than a new structural event during idle.
[IO.File]::WriteAllText((Join-Path $destination 'cold-index.json'),'')
$start=StartInfo $daemonPath @('--benchmark-live-cache',$cache,'--benchmark-pipe',$pipe,'--index-mbps',"$IndexMbps",$Volume)
$start.EnvironmentVariables['SCRY_QUERY_THREADS']='1'
$start.EnvironmentVariables['MIMALLOC_PURGE_DELAY']='1000'
$start.EnvironmentVariables.Remove('SCRY_NO_REFINEMENT_CACHE')
$timer=[Diagnostics.Stopwatch]::StartNew()
$server=[Diagnostics.Process]::Start($start)
$daemonStdout=$server.StandardOutput.ReadToEndAsync();$daemonStderr=$server.StandardError.ReadToEndAsync()
$memory=New-Object 'System.Collections.Generic.List[object]'
$stage='full-index'
try {
    $ready=$false
    while ($timer.Elapsed.TotalSeconds -lt 1800 -and !$server.HasExited) {
        $server.Refresh()
        $memory.Add([pscustomobject]@{elapsed_ms=$timer.Elapsed.TotalMilliseconds;private_bytes=$server.PrivateMemorySize64;working_set=$server.WorkingSet64;cpu_seconds=$server.TotalProcessorTime.TotalSeconds})
        $probe=New-Object IO.Pipes.NamedPipeClientStream('.', $pipe.Substring(9),[IO.Pipes.PipeDirection]::InOut)
        try{$probe.Connect(1);$ready=$true;break}catch{}finally{$probe.Dispose()}
        Start-Sleep -Milliseconds 50
    }
    if (!$ready) {throw 'Fresh live daemon did not become ready; inspect private daemon log'}
    $timer.Stop();$server.Refresh()
    $cold=@{schema=1;complete=$true;elapsed_ms=$timer.Elapsed.TotalMilliseconds;cpu_seconds=$server.TotalProcessorTime.TotalSeconds;sampled_private_peak=($memory|Measure-Object private_bytes -Maximum).Maximum;lifetime_working_set_peak=$server.PeakWorkingSet64;sample_interval_ms=50;samples=$memory;meaning='fresh-cache full NTFS index through query-pipe readiness; OS cache uncontrolled; no snapshot/journal resume'}
    [IO.File]::WriteAllText((Join-Path $destination 'cold-index.json'),($cold|ConvertTo-Json -Depth 5))
    Write-Output 'Fresh-cache full indexing complete. Starting query-memory, visibility and disconnected idle observations.'
    $stage='observe'
    $code=Captured $benchPath @('observe','--pipe',$pipe,'--output',(Join-Path $destination 'observe.json'),'--idle-seconds',"$IdleSeconds",'--settle-seconds',"$SettleSeconds",'--visibility-samples',"$VisibilitySamples") 'observer'
    if ($code -ne 0) {throw 'Lifecycle observation failed; inspect private observer log'}
    if ($ConnectedIdleSeconds -gt 0) {
        foreach ($connections in @(1,8)) {
            $code=Captured $benchPath @('observe','--pipe',$pipe,'--output',(Join-Path $destination "observe-connected-$connections.json"),'--idle-seconds',"$ConnectedIdleSeconds",'--settle-seconds','5','--visibility-samples','0','--query-repeats','1','--idle-connections',"$connections") "observer-connected-$connections"
            if ($code -ne 0) {throw 'Connected-idle observation failed'}
        }
    }
    $snapshot=Join-Path $cache ("index-"+$Volume.TrimEnd(':')+'.rkyv')
    $stage='fingerprint'
    $code=Captured $benchPath @('fingerprint','--snapshot',$snapshot,'--output',(Join-Path $destination 'fingerprint.json')) 'fingerprint'
    if ($code -ne 0) {throw 'Lifecycle fingerprint failed'}
    [IO.File]::WriteAllText((Join-Path $destination 'complete.json'),'{"complete":true}')
    Write-Output 'Lifecycle experiment complete. Installed daemon was not stopped or replaced.'
} catch {
    [IO.File]::WriteAllText((Join-Path $destination 'failure.json'),(@{complete=$false;stage=$stage;error_type=$_.Exception.GetType().Name;error_code=$_.Exception.HResult}|ConvertTo-Json))
    throw 'Lifecycle benchmark failed; inspect the aggregate failure record and private daemon/observer logs.'
} finally {
    if (!$server.HasExited) {$server.Kill()}
    $server.WaitForExit()
    [IO.File]::WriteAllText((Join-Path $destination 'daemon.stdout.log'),$daemonStdout.Result)
    [IO.File]::WriteAllText((Join-Path $destination 'daemon.stderr.log'),$daemonStderr.Result)
    $server.Dispose()
}
