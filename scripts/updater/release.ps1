# Shared release metadata validation. No installation changes.
function ParseReleaseVersion($text) {
    if ($text.Length -gt 64 -or $text -notmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$') { return $null }
    $parts=@()
    foreach ($part in @($Matches[1],$Matches[2],$Matches[3])) {
        $number=0L
        if (-not [long]::TryParse($part,[ref]$number)) { return $null }
        $parts+= $number
    }
    $pre=$Matches[4]
    if ($pre) {
        foreach ($part in $pre.Split('.')) { if ($part -match '^0[0-9]+$') { return $null } }
    }
    return @{numbers=$parts;pre=$pre;text=$text}
}
function CompareReleaseVersion($a,$b) {
    for ($i=0;$i -lt 3;$i++) { if ($a.numbers[$i] -ne $b.numbers[$i]) { return [Math]::Sign($a.numbers[$i].CompareTo($b.numbers[$i])) } }
    if (-not $a.pre -and -not $b.pre) {return 0}
    if (-not $a.pre) {return 1}
    if (-not $b.pre) {return -1}
    $left=$a.pre.Split('.');$right=$b.pre.Split('.')
    for ($i=0;$i -lt [Math]::Min($left.Count,$right.Count);$i++) {
        $x=$left[$i];$y=$right[$i]
        if ($x -ceq $y) {continue}
        $xn=$x -match '^[0-9]+$';$yn=$y -match '^[0-9]+$'
        if ($xn -and -not $yn) {return -1}
        if ($yn -and -not $xn) {return 1}
        if ($xn -and $x.Length -ne $y.Length) {return [Math]::Sign($x.Length-$y.Length)}
        return [Math]::Sign([StringComparer]::Ordinal.Compare($x,$y))
    }
    return [Math]::Sign($left.Count-$right.Count)
}
function ValidateManifest($manifest,$tag) {
    if ($manifest.schema -ne 1 -or $manifest.version -cne $tag.Substring(1) -or -not (ParseReleaseVersion $manifest.version)) { throw 'Invalid or unsupported release manifest' }
    foreach ($value in @($manifest.ipc_version,$manifest.index_format)) {
        if ($value -isnot [int] -and $value -isnot [long]) {throw 'Invalid compatibility version'}
        if ($value -lt 1 -or $value -gt 65535) {throw 'Invalid compatibility version'}
    }
    return $manifest
}
function FetchReleaseBytes($url,$limit) {
    $remaining=[int]($NetworkDeadline-[DateTime]::UtcNow).TotalMilliseconds
    if ($remaining -le 0) {throw 'Release discovery deadline expired'}
    $request=[Net.HttpWebRequest]::Create($url)
    $request.UserAgent='Scry-Updater/1'
    $request.Timeout=[Math]::Min(15000,$remaining)
    $request.ReadWriteTimeout=$request.Timeout
    $response=$request.GetResponse()
    try {
        if ($response.ContentLength -gt $limit) {throw 'Release metadata exceeds bound'}
        $input=$response.GetResponseStream();$memory=[IO.MemoryStream]::new()
        try {
            $buffer=New-Object byte[] 4096
            while (($count=$input.Read($buffer,0,$buffer.Length)) -gt 0) {
                if ($memory.Length+$count -gt $limit -or [DateTime]::UtcNow -gt $NetworkDeadline) {throw 'Release metadata exceeds budget'}
                $memory.Write($buffer,0,$count)
            }
            return ,$memory.ToArray()
        } finally {$memory.Dispose();$input.Dispose()}
    } finally {$response.Dispose()}
}
