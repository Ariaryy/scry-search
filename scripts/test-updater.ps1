# Focused, non-elevated package and helper contract tests. No installed task changes.
$ErrorActionPreference = 'Stop'
$source = Join-Path $PSScriptRoot 'updater/coordinator.ps1'
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($source, [ref]$null, [ref]$errors)
if ($errors) { throw ($errors | Out-String) }
$assignment = $ast.Find({ param($n) $n -is [Management.Automation.Language.AssignmentStatementAst] -and $n.Left.Extent.Text -eq '$engine' }, $true)
$engine = $assignment.Right.Expression.Value
$engineAst = [Management.Automation.Language.Parser]::ParseInput($engine, [ref]$null, [ref]$errors)
if ($errors) { throw ($errors | Out-String) }
$validate = $engineAst.Find({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'ValidateArchive' }, $true)
. ([scriptblock]::Create($validate.Extent.Text))
foreach ($name in @('DecodeEvent','CancellationCode','EncodeHelper')) {
    $fn = $ast.Find({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name }, $true)
    . ([scriptblock]::Create($fn.Extent.Text))
}
$expandedEngine=$engine.Replace('# RELEASE_SUPPORT',(Get-Content (Join-Path $PSScriptRoot 'updater/release.ps1') -Raw)).Replace('# PROCESS_SUPPORT',(Get-Content (Join-Path $PSScriptRoot 'updater/processes.ps1') -Raw))
$maximumPrefix=("`$Release='v"+('a9b8c7d6'*8)+"';`$PipeName='scry-update-"+('1234567890abcdef'*2)+"';`$OriginSid='S-1-5-"+((1000000001..1000000015)-join '-')+"';")
$encodedHelper=EncodeHelper ($maximumPrefix+$expandedEngine)
if($encodedHelper.Length -gt 32000){throw 'Embedded helper exceeds Windows command-line budget'}
$fn = $engineAst.Find({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'RecoveryAction' }, $true)
. ([scriptblock]::Create($fn.Extent.Text))
if ((CancellationCode ([ComponentModel.Win32Exception]::new(1223))) -ne 20) { throw 'UAC denial lost' }
if ((CancellationCode ([Exception]::new('failure'))) -ne 1) { throw 'Generic failure misclassified' }
$null=DecodeEvent '{"schema":1,"event":"result","state":"updated","exit_code":0}'
foreach ($text in @('{','{"schema":2,"event":"result"}','{"schema":1,"event":"result","exit_code":999}','{"schema":1,"event":"command"}')) {
    $rejected=$false
    try { $null=DecodeEvent $text } catch { $rejected=$true }
    if (-not $rejected) { throw 'Malformed helper event accepted' }
}
$root=Join-Path ([IO.Path]::GetTempPath()) 'scry-updater-test'
$target=Join-Path $root 'releases/v1'
$pending=@{schema=1;phase='activated';target=$target;previous_path=(Join-Path $root 'bin/scryd.exe')}
if ((RecoveryAction $pending $root (Join-Path $target 'scryd.exe')) -ne 'resume') { throw 'Activation recovery failed' }
if ((RecoveryAction $pending $root $pending.previous_path) -ne 'restore') { throw 'Pre-activation restore failed' }
$pending.phase='prepared'
if ((RecoveryAction $pending $root $pending.previous_path) -ne 'retry') { throw 'Prepared retry failed' }
$pending.phase='activated';$pending.target=Join-Path $root '../escape'
$rejected=$false
try { $null=RecoveryAction $pending $root $pending.previous_path } catch { $rejected=$true }
if (-not $rejected) { throw 'Recovery path escape accepted' }
$fn = $engineAst.Find({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'FinalizeInstallation' }, $true)
. ([scriptblock]::Create($fn.Extent.Text))
$completionRoot=Join-Path ([IO.Path]::GetTempPath()) ('scry-completion-'+[Guid]::NewGuid().ToString('N'))
$completionRelease=Join-Path $completionRoot 'releases/v1'
$originalPath=[Environment]::GetEnvironmentVariable('Path','User')
try {
    [IO.Directory]::CreateDirectory($completionRelease)|Out-Null
    Set-Content -LiteralPath (Join-Path $completionRelease 'uninstall-daemon.ps1') -Value '# verified fixture'
    $stale=Join-Path $completionRoot 'releases/previous'
    [Environment]::SetEnvironmentVariable('Path',($originalPath+';'+$stale),'User')
    FinalizeInstallation $completionRoot $completionRelease
    # Recovery must finish both operations even after a prior partial attempt.
    Remove-Item -LiteralPath (Join-Path $completionRoot 'uninstall.ps1')
    FinalizeInstallation $completionRoot $completionRelease
    $entries=[Environment]::GetEnvironmentVariable('Path','User') -split ';'
    if (@($entries|Where-Object {$_ -ieq $completionRelease}).Count -ne 1 -or $entries -contains $stale) {throw 'Recovery PATH completion failed'}
    if ((Get-Content -LiteralPath (Join-Path $completionRoot 'uninstall.ps1') -Raw).Trim() -ne '# verified fixture') {throw 'Recovery uninstaller completion failed'}
} finally {
    [Environment]::SetEnvironmentVariable('Path',$originalPath,'User')
    foreach($file in @((Join-Path $completionRelease 'uninstall-daemon.ps1'),(Join-Path $completionRoot 'uninstall.ps1'))) {Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue}
    foreach($directory in @($completionRelease,(Join-Path $completionRoot 'releases'),$completionRoot)) {Remove-Item -LiteralPath $directory -ErrorAction SilentlyContinue}
}
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
function CheckArchive($name, $accepted, $attributes=0) {
    $memory = [IO.MemoryStream]::new()
    $zip = [IO.Compression.ZipArchive]::new($memory,[IO.Compression.ZipArchiveMode]::Create,$true)
    $entry = $zip.CreateEntry($name)
    $entry.ExternalAttributes=$attributes
    $zip.Dispose()
    $memory.Position=0
    $zip = [IO.Compression.ZipArchive]::new($memory,[IO.Compression.ZipArchiveMode]::Read)
    $ok=$true
    try { ValidateArchive $zip } catch { $ok=$false }
    finally { $zip.Dispose(); $memory.Dispose() }
    if ($ok -ne $accepted) { throw "Unexpected archive validation result: $name" }
}
CheckArchive 'scry.exe' $true
CheckArchive 'docs/users/cli.md' $true
foreach ($name in @('../scry.exe','docs/../../scry.exe','/scry.exe','C:/scry.exe','docs\scry.exe','scry.exe:stream')) { CheckArchive $name $false }
CheckArchive 'link' $false (-1610612736)
# A case-insensitive duplicate must not select ambiguous executable bytes.
$memory=[IO.MemoryStream]::new()
$zip=[IO.Compression.ZipArchive]::new($memory,[IO.Compression.ZipArchiveMode]::Create,$true)
$null=$zip.CreateEntry('scry.exe'); $null=$zip.CreateEntry('SCRY.exe'); $zip.Dispose(); $memory.Position=0
$zip=[IO.Compression.ZipArchive]::new($memory,[IO.Compression.ZipArchiveMode]::Read)
$rejected=$false
try { ValidateArchive $zip } catch { $rejected=$true } finally { $zip.Dispose(); $memory.Dispose() }
if (-not $rejected) { throw 'Case duplicate accepted' }
$oversized=[pscustomobject]@{Entries=@([pscustomobject]@{FullName='scry.exe';Length=67108865;ExternalAttributes=0})}
$rejected=$false
try {ValidateArchive $oversized} catch {$rejected=$true}
if(-not $rejected){throw 'Oversized expanded entry accepted'}
Write-Output 'Updater parser and malicious archive tests passed'
