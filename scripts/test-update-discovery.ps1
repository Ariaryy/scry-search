$ErrorActionPreference='Stop'
$support=Join-Path $PSScriptRoot 'updater/release.ps1'
$check=Join-Path $PSScriptRoot 'updater/check-update.ps1'
. $support
foreach ($pair in @(@('1.0.0','1.0.0-alpha'),@('2.0.0','1.99.99'),@('1.0.0-alpha.10','1.0.0-alpha.2'),@('1.0.0-beta','1.0.0-99'))) {
    if ((CompareReleaseVersion (ParseReleaseVersion $pair[0]) (ParseReleaseVersion $pair[1])) -le 0) {throw 'Semantic release order failed'}
}
foreach ($invalid in @('1.0','01.0.0','1.0.0-01','1.0.0/evil','1.0.0;exit')) {
    if (ParseReleaseVersion $invalid) {throw 'Invalid semantic version accepted'}
}
foreach ($manifest in @(@{schema=2;version='1.0.0';ipc_version=1;index_format=10},@{schema=1;version='1.0.0';ipc_version='1';index_format=10})) {
    $rejected=$false
    try {$null=ValidateManifest $manifest 'v1.0.0'} catch {$rejected=$true}
    if(-not $rejected){throw 'Invalid manifest accepted'}
}
$shell=Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
foreach ($scenario in @('compatible','prerelease','current','none','unknown_schema','bad_digest')) {
    $wrapper=Join-Path ([IO.Path]::GetTempPath()) ('scry-discovery-test-'+[Guid]::NewGuid().ToString('N')+'.ps1')
    $fixture=@'
$ErrorActionPreference='Stop'
$CompatibleIpc=1;$CompatibleIndex=10;$CurrentVersion='1.0.0';$Channel='stable'
$global:DiscoveryManifests=@{}
function Release($tag,$ipc,$index,$schema=1) {
    $manifest=@{schema=$schema;version=$tag.Substring(1);ipc_version=$ipc;index_format=$index}|ConvertTo-Json -Compress
    $data=[Text.Encoding]::UTF8.GetBytes($manifest)
    $sha=[Security.Cryptography.SHA256]::Create()
    $digest=([BitConverter]::ToString($sha.ComputeHash($data))).Replace('-','').ToLowerInvariant();$sha.Dispose()
    if ($Scenario -eq 'bad_digest') {$digest='0'*64}
    $global:DiscoveryManifests[$tag]=$data
    return @{tag_name=$tag;draft=$false;prerelease=$tag.Contains('-');assets=@(@{name='scry-release.json';digest="sha256:$digest"},@{name="scry-search-$tag-windows-x86_64.zip";digest=('sha256:'+('0'*64))})}
}
$schema=if($Scenario -eq 'unknown_schema'){2}else{1}
$releases=@((Release 'v3.0.0' 2 11 $schema),(Release 'v2.0.0' 1 10),(Release 'v4.0.0-alpha.1' 1 10))
if($Scenario -eq 'none'){$releases=@((Release 'v3.0.0' 2 11))}
if($Scenario -eq 'prerelease'){$Channel='prerelease'}
if($Scenario -eq 'current'){$CurrentVersion='2.0.0'}
$global:DiscoveryMetadata=[Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject $releases -Depth 6))
function FetchReleaseBytes($url,$limit) {
    if ($url.Contains('api.github.com')) {return ,$global:DiscoveryMetadata}
    $tag=$url.Split('/')[-2]
    return ,$global:DiscoveryManifests[$tag]
}
'@
    try {
        $body=". '"+$support.Replace("'","''")+"';`n`$Scenario='$scenario';`n"+$fixture+"`n& '"+$check.Replace("'","''")+"'; exit `$LASTEXITCODE"
        Set-Content -LiteralPath $wrapper -Value $body -Encoding UTF8
        $output=& $shell -NoProfile -NonInteractive -File $wrapper
        $code=$LASTEXITCODE;$result=$output|ConvertFrom-Json
        $expected=if($scenario -eq 'none'){12}elseif($scenario -eq 'bad_digest'){22}else{0}
        if ($code -ne $expected -or $result.event -ne 'check_update' -or $result.schema -ne 1) {throw "Discovery $scenario failed: $output"}
        if($scenario -in @('compatible','unknown_schema') -and ($result.state -ne 'available' -or $result.target_version -ne 'v2.0.0')){throw 'Last compatible selection failed'}
        if($scenario -eq 'prerelease' -and $result.target_version -ne 'v4.0.0-alpha.1'){throw 'Prerelease selection failed'}
        if($scenario -eq 'current' -and $result.state -ne 'current'){throw 'Current classification failed'}
        if($scenario -eq 'none' -and $result.state -ne 'no_compatible_release'){throw 'Incompatible release selected'}
        if($scenario -eq 'bad_digest' -and $result.state -ne 'failed'){throw 'Bad digest selected'}
    } finally {Remove-Item -LiteralPath $wrapper -Force -ErrorAction SilentlyContinue}
}
Write-Output 'Release semantic order, channels, compatible fallback and manifest verification tests passed'
