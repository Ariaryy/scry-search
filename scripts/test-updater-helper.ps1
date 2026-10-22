# Exercise the real coordinator with a short-lived, unelevated fake helper.
# This never invokes UAC, downloads a package, or modifies an installation.
$ErrorActionPreference='Stop'
$source=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'updater/coordinator.ps1'))
$shell=Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
foreach ($scenario in @('success','offline','cancel','malformed','wrong_peer')) {
    $wrapper=Join-Path ([IO.Path]::GetTempPath()) ('scry-coordinator-test-'+[Guid]::NewGuid().ToString('N')+'.ps1')
    $mock=@'
$Release='v1'
function Start-Process {
    param($FilePath,$Verb,$WindowStyle,$ArgumentList,[switch]$PassThru)
    if ($Scenario -eq 'cancel') { throw [ComponentModel.Win32Exception]::new(1223) }
    $code=if ($Scenario -eq 'offline') {21} else {0}
    $state=if ($code) {'failed'} else {'updated'}
    $event=@{schema=1;event='result';state=$state;message='test helper result';exit_code=$code}|ConvertTo-Json -Compress
    if ($Scenario -eq 'malformed') {$event='{"schema":99,"event":"result"}'}
    $script="`$p=[IO.Pipes.NamedPipeClientStream]::new('.','$pipeName',[IO.Pipes.PipeDirection]::Out);`$p.Connect(5000);`$w=[IO.StreamWriter]::new(`$p);`$w.WriteLine('$event');`$w.Dispose();`$p.Dispose();exit $code"
    $encoded=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
    $info=[Diagnostics.ProcessStartInfo]::new()
    $info.FileName=Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
    $info.Arguments='-NoProfile -NonInteractive -EncodedCommand '+$encoded
    $info.UseShellExecute=$false;$info.CreateNoWindow=$true
    $child=[Diagnostics.Process]::Start($info)
    if ($Scenario -eq 'wrong_peer') { return [pscustomobject]@{Id=0} }
    return $child
}
'@
    try {
        $body="`$Scenario='$scenario';`n"+$mock+"`n& '"+$source.Replace("'","''")+"'; exit `$LASTEXITCODE"
        Set-Content -LiteralPath $wrapper -Value $body -Encoding UTF8
        $output=& $shell -NoProfile -NonInteractive -File $wrapper
        $actual=$LASTEXITCODE
        $expected=switch($scenario) {'success'{0} 'offline'{21} 'cancel'{20} 'malformed'{1} 'wrong_peer'{1}}
        if ($actual -ne $expected) { throw "Coordinator $scenario returned $actual, expected $expected" }
        $results=@($output | ForEach-Object {$_|ConvertFrom-Json} | Where-Object {$_.event -eq 'result'})
        if ($results.Count -ne 1 -or $results[0].exit_code -ne $expected) { throw "Coordinator $scenario result contract failed" }
    } finally { Remove-Item -LiteralPath $wrapper -Force -ErrorAction SilentlyContinue }
}
Write-Output 'Updater coordinator success, failure, UAC cancellation, wrong-peer and malformed-result tests passed'
