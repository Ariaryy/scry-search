$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$env:PSModulePath=Join-Path $PSHOME 'Modules'
[Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12
$NetworkDeadline=[DateTime]::UtcNow.AddSeconds(60)
$code=21
try {
    $current=ParseReleaseVersion $CurrentVersion
    if (-not $current) { $code=1;throw 'Invalid current application version' }
    $bytes=FetchReleaseBytes 'https://api.github.com/repos/Ariaryy/scry-search/releases?per_page=20' 2097152
    $releases=[Text.Encoding]::UTF8.GetString($bytes)|ConvertFrom-Json
    $selected=$null
    $inspected=0
    foreach ($release in @($releases) | Select-Object -First 20) {
        if ($release.draft) {continue}
        $tag=$release.tag_name
        if ($tag -notmatch '^v[0-9A-Za-z.-]{1,64}$') {continue}
        $version=ParseReleaseVersion $tag.Substring(1)
        if (-not $version -or ($Channel -eq 'stable' -and ($release.prerelease -or $version.pre))) {continue}
        $assets=@($release.assets|Where-Object {$_.name -ceq 'scry-release.json'})
        $packages=@($release.assets|Where-Object {$_.name -ceq "scry-search-$tag-windows-x86_64.zip"})
        if ($assets.Count -ne 1 -or $packages.Count -ne 1) {continue}
        $asset=$assets[0]
        if ($asset.digest -notmatch '^sha256:[0-9a-f]{64}$' -or $packages[0].digest -notmatch '^sha256:[0-9a-f]{64}$') {continue}
        $data=FetchReleaseBytes "https://github.com/Ariaryy/scry-search/releases/download/$tag/scry-release.json" 4096
        $hash=[Security.Cryptography.SHA256]::Create()
        try {$digest=([BitConverter]::ToString($hash.ComputeHash($data))).Replace('-','').ToLowerInvariant()} finally {$hash.Dispose()}
        $code=22
        if ($digest -cne $asset.digest.Substring(7)) {throw 'Release manifest checksum mismatch'}
        $manifest=[Text.Encoding]::UTF8.GetString($data).TrimStart([char]0xFEFF)|ConvertFrom-Json
        if ($manifest.schema -ne 1) { $code=21;continue }
        $manifest=ValidateManifest $manifest $tag
        $inspected++
        if ($manifest.ipc_version -eq $CompatibleIpc -and $manifest.index_format -eq $CompatibleIndex -and (-not $selected -or (CompareReleaseVersion $version $selected.version) -gt 0)) {
            $selected=@{tag=$tag;version=$version;manifest=$manifest}
        }
        $code=21
    }
    $state='no_compatible_release';$target=$null;$detail=@{};$message='No compatible verified release in the 20-release discovery window';$code=12
    if ($selected) {
        $target=$selected.tag;$detail=$selected.manifest
        $state=if ((CompareReleaseVersion $selected.version $current) -gt 0) {'available'} else {'current'}
        $message=if ($state -eq 'available') {"Compatible release $target is available"} else {'No newer compatible release is available'}
        $code=0
    }
    @{schema=1;event='check_update';state=$state;target_version=$target;message=$message;exit_code=$code;detail=$detail;inspected_manifests=$inspected}|ConvertTo-Json -Compress
} catch {
    @{schema=1;event='check_update';state='failed';target_version=$null;message=$_.Exception.Message;exit_code=$code;detail=@{}}|ConvertTo-Json -Compress
}
exit $code
