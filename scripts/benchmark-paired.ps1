<#
Repeat frozen-snapshot captures in alternating baseline/candidate process order.
Use two copies of the same daemon for an A/A control. This script never stops
the installed daemon. Raw captures, workload strings and logs stay in .scratch.
#>
param(
    [Parameter(Mandatory=$true)][string]$BaselineDaemon,
    [Parameter(Mandatory=$true)][string]$CandidateDaemon,
    [Parameter(Mandatory=$true)][string]$Snapshot,
    [Parameter(Mandatory=$true)][string]$Workload,
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [Parameter(Mandatory=$true)][string]$Machine,
    [Parameter(Mandatory=$true)][string]$Power,
    [string]$Bench = 'target/release/scry-bench.exe',
    [ValidateRange(1,20)][int]$Pairs = 3,
    [ValidateRange(1,10000)][int]$Samples = 40,
    [ValidateRange(1,100)][int]$Rounds = 3,
    [ValidateRange(0,100)][int]$Warmups = 2,
    [ValidateRange(1,64)][int]$QueryThreads = 1,
    [string]$TargetCase
)
$ErrorActionPreference = 'Stop'
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Set-Location -LiteralPath $repoRoot
function Get-TaskPath($path) {
    if ([System.IO.Path]::IsPathRooted($path)) { return [System.IO.Path]::GetFullPath($path) }
    return [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($repoRoot,$path))
}
$scratchRoot = [System.IO.Path]::GetFullPath((Join-Path $repoRoot '.scratch'))
$destination = [System.IO.Path]::GetFullPath((Join-Path $repoRoot $OutputDirectory))
if (!$destination.StartsWith($scratchRoot + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase)) { throw 'OutputDirectory must be within .scratch' }
if (Test-Path -LiteralPath $destination) { throw 'OutputDirectory must be new' }
[System.IO.Directory]::CreateDirectory($destination) | Out-Null
# Check junction-resolved ancestry too, using the runner's own output policy
# for every capture. Do not allow output under an existing reparse-point parent.
$ancestor = Get-Item -LiteralPath $destination
while ($ancestor.FullName -ne (Get-Item -LiteralPath $repoRoot).FullName) {
    if (($ancestor.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Output ancestry contains a reparse point' }
    $ancestor = $ancestor.Parent
    if ($null -eq $ancestor) { throw 'Output ancestry left repository' }
}
$benchPath = Get-TaskPath $Bench
$snapshotPath = Get-TaskPath $Snapshot
$workloadPath = Get-TaskPath $Workload
$python = (Get-Command python).Source
$binaries = @{baseline=(Get-TaskPath $BaselineDaemon); candidate=(Get-TaskPath $CandidateDaemon)}
function Get-Sha256($path) {
    $stream = [System.IO.File]::OpenRead($path)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return ([System.BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-','').ToLowerInvariant() }
    finally { $sha.Dispose(); $stream.Dispose() }
}
$hashes = @{}
foreach ($label in @('baseline','candidate')) { $hashes[$label]=Get-Sha256 $binaries[$label] }
$snapshotHash = Get-Sha256 $snapshotPath
$context = @{schema=1; pairs=$Pairs; samples=$Samples; rounds=$Rounds; warmups=$Warmups; query_threads=$QueryThreads; refinement_cache=$true; machine=$Machine; power=$Power; daemon_sha256=$hashes; bench_sha256=(Get-Sha256 $benchPath); snapshot_sha256=$snapshotHash; workload_sha256=(Get-Sha256 $workloadPath); same_binary_control=($hashes.baseline -eq $hashes.candidate); process_order='baseline,candidate; candidate,baseline; repeat'; background='installed daemon untouched; other activity operator controlled'}
[System.IO.File]::WriteAllText((Join-Path $destination 'context.json'),($context | ConvertTo-Json -Depth 5))
function New-StartInfo($executable, [string[]]$arguments) {
    if (@($arguments | Where-Object { $_.Contains('"') -or $_.Contains("`n") -or $_.Contains("`r") }).Count) { throw 'Arguments contain unsupported quote or newline' }
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $executable
    $start.WorkingDirectory = $repoRoot
    $start.Arguments = ($arguments | ForEach-Object { '"' + $_ + '"' }) -join ' '
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    return $start
}
function Invoke-Captured($executable, [string[]]$arguments, $prefix) {
    $process = [System.Diagnostics.Process]::Start((New-StartInfo $executable $arguments))
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    try { $process.WaitForExit(); $code=$process.ExitCode }
    finally { if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() } }
    [System.IO.File]::WriteAllText("$prefix.json",$stdout.Result)
    [System.IO.File]::WriteAllText("$prefix.stderr.log",$stderr.Result)
    $process.Dispose()
    return $code
}
for ($pair=0; $pair -lt $Pairs; $pair++) {
    $order = if ($pair % 2 -eq 0) { @('baseline','candidate') } else { @('candidate','baseline') }
    foreach ($label in $order) {
        if ((Get-Sha256 $snapshotPath) -ne $snapshotHash -or (Get-Sha256 $binaries[$label]) -ne $hashes[$label]) { throw 'Snapshot or binary changed' }
        $pipe = "\\.\pipe\scry-bench-paired-$PID-$pair-$label"
        $start = New-StartInfo $binaries[$label] @('--benchmark-snapshot',$snapshotPath,'--benchmark-pipe',$pipe)
        $start.EnvironmentVariables['SCRY_QUERY_THREADS'] = "$QueryThreads"
        $start.EnvironmentVariables['MIMALLOC_PURGE_DELAY'] = '1000'
        $start.EnvironmentVariables.Remove('SCRY_NO_REFINEMENT_CACHE')
        $server = [System.Diagnostics.Process]::Start($start)
        $daemonOut = $server.StandardOutput.ReadToEndAsync()
        $daemonError = $server.StandardError.ReadToEndAsync()
        $prefix = Join-Path $destination "pair-$pair-$label"
        try {
            $ready = $false
            $deadline = (Get-Date).AddSeconds(15)
            while ((Get-Date) -lt $deadline -and !$server.HasExited) {
                $probe = New-Object System.IO.Pipes.NamedPipeClientStream('.', $pipe.Substring(9), [System.IO.Pipes.PipeDirection]::InOut)
                try { $probe.Connect(100); $ready=$true; break } catch { Start-Sleep -Milliseconds 50 } finally { $probe.Dispose() }
            }
            if (!$ready) { throw 'Frozen daemon did not become ready' }
            $arguments = @('--snapshot',$snapshotPath,'--workload',$workloadPath,'--pipe',$pipe,'--output',"$prefix.jsonl",'--daemon-build',"sha256-$($hashes[$label])",'--machine',$Machine,'--power',$Power,'--samples',"$Samples",'--rounds',"$Rounds",'--warmups',"$Warmups")
            $code = Invoke-Captured $benchPath $arguments "$prefix-runner"
            if ($code -ne 0) { throw 'Capture failed; inspect private logs' }
        } finally {
            if (!$server.HasExited) { $server.Kill() }
            $server.WaitForExit()
            [System.IO.File]::WriteAllText("$prefix-daemon.stdout.log",$daemonOut.Result)
            [System.IO.File]::WriteAllText("$prefix-daemon.stderr.log",$daemonError.Result)
            $server.Dispose()
        }
        $code = Invoke-Captured $python @('-B','scripts/benchmark-summary.py',"$prefix.jsonl") "$prefix-summary"
        if ($code -ne 0) { throw 'Capture summary failed' }
        Write-Output "Completed pair $($pair+1) $label."
    }
    if ($TargetCase) {
        $code = Invoke-Captured $python @('-B','scripts/benchmark-summary.py',"$destination/pair-$pair-candidate.jsonl",'--before',"$destination/pair-$pair-baseline.jsonl",'--target-case',$TargetCase) (Join-Path $destination "pair-$pair-comparison")
        if ($code -gt 1) { throw 'Comparison evidence incompatible' }
        Write-Output "Pair $($pair+1) comparison exit $code (0 pass, 1 benefit/regression gate not met)."
    }
}
[System.IO.File]::WriteAllText((Join-Path $destination 'complete.json'),'{"complete":true}')
