# Embedded coordinator and fixed elevated update engine. No client paths or commands.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$env:PSModulePath = Join-Path $PSHOME 'Modules'
function Emit($event, $state, $message, $code) {
    @{schema=1;event=$event;state=$state;message=$message;exit_code=$code} | ConvertTo-Json -Compress
}
function DecodeEvent($text) {
    if ($text.Length -gt 4096) { throw 'Helper event exceeds bound' }
    $event = $text | ConvertFrom-Json
    if ($event.schema -ne 1 -or $event.event -notin @('progress','result')) { throw 'Invalid helper event' }
    if ($event.event -eq 'result' -and ($event.exit_code -notin @(0,1,20,21,22,23,24,25,26) -or $event.state -notin @('updated','recovered','failed','recovery_required'))) { throw 'Invalid helper result' }
    return $event
}
function CancellationCode($exception) {
    if ($exception.NativeErrorCode -eq 1223 -or $exception.InnerException.NativeErrorCode -eq 1223) { return 20 }
    return 1
}
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$pipeName = 'scry-update-' + [Guid]::NewGuid().ToString('N')
$engine = @'
# RELEASE_SUPPORT
$UpdateDeadline=[DateTime]::UtcNow.AddMinutes(14)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$env:PSModulePath = Join-Path $PSHOME 'Modules'
$pipe = [IO.Pipes.NamedPipeClientStream]::new('.', $PipeName, [IO.Pipes.PipeDirection]::Out)
$pipe.Connect(30000)
$writer = [IO.StreamWriter]::new($pipe, [Text.UTF8Encoding]::new($false))
$writer.AutoFlush = $true
function Send($event, $state, $message, $code) {
    try { $writer.WriteLine((@{schema=1;event=$event;state=$state;message=$message;exit_code=$code} | ConvertTo-Json -Compress)) } catch {}
}
function Step($message) { Send 'progress' 'updating' $message 0 }
function Fetch($url, $destination, $limit) {
    $request = [Net.HttpWebRequest]::Create($url)
    $request.UserAgent = 'Scry-Updater/1'
    $remaining=[int]($UpdateDeadline-[DateTime]::UtcNow).TotalMilliseconds
    if ($remaining -le 0) { throw 'Update deadline expired' }
    $request.Timeout = [Math]::Min(30000,$remaining)
    $request.ReadWriteTimeout = $request.Timeout
    $response = $request.GetResponse()
    try {
        if ($response.ContentLength -gt $limit) { throw 'Release download exceeds size limit' }
        $input = $response.GetResponseStream()
        $output = [IO.File]::Create($destination)
        try {
            $buffer = New-Object byte[] 65536
            $total = 0L
            while (($count = $input.Read($buffer, 0, $buffer.Length)) -gt 0) {
                if ([DateTime]::UtcNow -ge $UpdateDeadline) { throw 'Update deadline expired' }
                $total += $count
                if ($total -gt $limit) { throw 'Release download exceeds size limit' }
                $output.Write($buffer, 0, $count)
            }
        } finally { $output.Dispose(); $input.Dispose() }
    } finally { $response.Dispose() }
}
function ValidateArchive($zip) {
        if ($zip.Entries.Count -gt 512) { throw 'Too many archive entries' }
        $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        $total = 0L
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName
            $total += $entry.Length
            if ($total -gt 268435456 -or $entry.Length -gt 67108864 -or $name.Length -gt 240 -or $name -match '(^[/\\]|[:\\]|(^|/)\.\.?(/|$)|[\x00-\x1f]|[. ](/|$)|(^|/)(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(\.|/|$))' -or -not $seen.Add($name)) { $code=22; throw 'Unsafe release archive entry' }
            if (($entry.ExternalAttributes -shr 16 -band 0xF000) -eq 0xA000) { $code=22; throw 'Archive links are forbidden' }
        }
}
function RecoveryAction($pending, $root, $taskPath) {
    if ($pending.schema -ne 1 -or $pending.phase -notin @('prepared','activated','committed')) { throw 'Invalid recovery journal' }
    if ($pending.phase -ne 'activated') { return 'retry' }
    $target = [IO.Path]::GetFullPath($pending.target)
    if (-not $target.StartsWith($root + '\releases\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Invalid recovery target' }
    if (-not $taskPath -and -not $pending.previous_path) { return 'retry_initial' }
    if ($taskPath -ieq (Join-Path $target 'scryd.exe')) { return 'resume' }
    if ($taskPath -and $taskPath -ieq $pending.previous_path) { return 'restore' }
    throw 'Recovery task does not match journal'
}
function NoLink($path) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Reparse points are unsupported in the installation' }
}
function SecureDirectory($path) {
    NoLink $path
    if (Test-Path -LiteralPath $path) {
        $owner = (Get-Acl -LiteralPath $path).GetOwner([Security.Principal.SecurityIdentifier]).Value
        if ($owner -notin @('S-1-5-32-544','S-1-5-18')) { throw 'Unsupported installation directory owner' }
    }
    [IO.Directory]::CreateDirectory($path) | Out-Null
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)')
    Set-Acl -LiteralPath $path -AclObject $acl
}
function SaveJournal($phase) {
    @{schema=1;phase=$phase;previous_xml=$oldXml;previous_path=$oldPath;target=$final;tag=$tag} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath ($journal + '.new') -Encoding UTF8
    Move-Item -LiteralPath ($journal + '.new') -Destination $journal -Force
}
$code = 1
$mutated = $false
$root = $null
$stage = $null
$lock = $null
$oldXml = $null
try {
    if ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value -ne $OriginSid) { throw 'Elevation must use the originating Windows account' }
    if ($Release -ne 'latest' -and $Release -notmatch '^v[a-zA-Z0-9][a-zA-Z0-9.-]{0,63}$') { throw 'Invalid release tag' }
    if (-not [Environment]::Is64BitOperatingSystem -or $env:PROCESSOR_ARCHITECTURE -ne 'AMD64') { $code=23; throw 'Only Windows x86_64 installations are supported' }
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $base = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Scry Search'
    NoLink $base
    # Never take ownership of a preexisting user-owned program directory.
    if (Test-Path -LiteralPath $base) {
        $owner = (Get-Acl -LiteralPath $base).GetOwner([Security.Principal.SecurityIdentifier]).Value
        if ($owner -notin @('S-1-5-32-544','S-1-5-18')) { $code=23; throw 'Installation root has an unsupported owner' }
    }
    SecureDirectory $base
    $root = Join-Path $base $OriginSid
    SecureDirectory $root
    $lock = [IO.File]::Open((Join-Path $root 'update.lock'), 'OpenOrCreate', 'ReadWrite', 'None')
    $journal = Join-Path $root 'transaction.json'
    $taskName = 'Scry Search Daemon'
    $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    $oldPath = $null
    if ($task) {
        if ($task.Actions.Count -ne 1 -or $task.Principal.LogonType -ne 'Interactive' -or $task.Principal.RunLevel -ne 'Highest') { $code=23; throw 'Unsupported daemon task configuration' }
        $taskSid = if ($task.Principal.UserId -like 'S-1-*') { ([Security.Principal.SecurityIdentifier]::new($task.Principal.UserId)).Value } else { ([Security.Principal.NTAccount]::new($task.Principal.UserId)).Translate([Security.Principal.SecurityIdentifier]).Value }
        if ($taskSid -ne $OriginSid) { $code=23; throw 'Daemon task belongs to another user' }
        $oldPath = $task.Actions[0].Execute
        $legacy = Join-Path $env:LOCALAPPDATA 'Programs\Scry Search\bin\scryd.exe'
        if ($oldPath -ine $legacy -and $oldPath -ine (Join-Path $root 'bin\scryd.exe') -and $oldPath -notlike "$root\releases\*\scryd.exe") { $code=23; throw 'Custom daemon installation is unsupported' }
        if ($task.Actions[0].WorkingDirectory -or $task.Actions[0].Arguments -notmatch '^(\s*(--unbounded|--index-mbps(?:=|\s+)\d+|[A-Za-z]:\\?)\s*)*$') { $code=23; throw 'Unsupported daemon arguments or working directory' }
        $oldXml = Export-ScheduledTask -TaskName $taskName
    }
    if (Test-Path -LiteralPath $journal) {
        $pending = Get-Content -LiteralPath $journal -Raw | ConvertFrom-Json
        $code=24
        $recoveryAction = RecoveryAction $pending $root $oldPath
        if ($recoveryAction -eq 'retry_initial') { Remove-Item -LiteralPath $journal; $pending.phase='prepared' }
        if ($pending.phase -eq 'activated') {
            Step 'Recovering interrupted daemon activation'
            $recoveryPath = [IO.Path]::GetFullPath($pending.target)
            if (-not $recoveryPath.StartsWith($root + '\releases\', [StringComparison]::OrdinalIgnoreCase) ) { $code=24; throw 'Recovery task does not match journal' }
            NoLink $recoveryPath
            if (-not $task -or $task.Actions[0].Execute -ine (Join-Path $recoveryPath 'scryd.exe')) {
                if ($task -and $task.Actions[0].Execute -ieq $pending.previous_path -and $pending.previous_path.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) {
                    Start-ScheduledTask -TaskName $taskName
                    Remove-Item -LiteralPath $journal
                    Send 'result' 'recovered' 'Previous daemon restored before replacement activation; retry update' 0
                    exit 0
                }
                $code=24; throw 'Recovery task does not match journal'
            }
            Start-ScheduledTask -TaskName $taskName
            $status = & (Join-Path $recoveryPath 'scry.exe') status --json --version $pending.tag
            if ($LASTEXITCODE -ne 0) { $code=24; throw 'Interrupted release is not ready; index data and new binaries retained' }
            $pending.phase='committed'
            $pending | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $journal -Encoding UTF8
            Send 'result' 'recovered' 'Interrupted update recovered; run update again for another version' 0
            exit 0
        }
        # Prepared never started the replacement, so retry can safely proceed.

    }
    $code=21
    Step 'Downloading trusted release metadata'
    $stage = Join-Path $root ('stage-' + [Guid]::NewGuid().ToString('N'))
    SecureDirectory $stage
    $env:TEMP=$stage; $env:TMP=$stage
    $endpoint = if ($Release -eq 'latest') { 'latest' } else { 'tags/' + $Release }
    $metadataPath = Join-Path $stage 'release.json'
    Fetch ('https://api.github.com/repos/Ariaryy/scry-search/releases/' + $endpoint) $metadataPath 1048576
    $code=22
    $metadata = Get-Content -LiteralPath $metadataPath -Raw | ConvertFrom-Json
    $tag = $metadata.tag_name
    if ($tag -notmatch '^v[a-zA-Z0-9][a-zA-Z0-9.-]{0,63}$' -or ($Release -ne 'latest' -and $tag -cne $Release) -or $metadata.draft) { throw 'Invalid release metadata' }
    $assetName = "scry-search-$tag-windows-x86_64.zip"
    $assets = @($metadata.assets | Where-Object { $_.name -ceq $assetName })
    if ($assets.Count -ne 1 -or $assets[0].digest -notmatch '^sha256:[0-9a-f]{64}$') { $code=22; throw 'Release lacks a GitHub SHA-256 asset digest' }
    $expected = $assets[0].digest.Substring(7)
    $archive = Join-Path $stage 'package.zip'
    Step "Downloading Scry Search $tag"
    $code=21
    Fetch "https://github.com/Ariaryy/scry-search/releases/download/$tag/$assetName" $archive 134217728
    $code=22
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ine $expected) { $code=22; throw 'Release package checksum mismatch' }
    Step 'Validating release archive'
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        ValidateArchive $zip
        foreach ($name in @('scry.exe','scryd.exe','scry-release.json','uninstall-daemon.ps1')) {
            $entry = $zip.GetEntry($name)
            if (-not $entry -or $entry.Length -lt 1) { $code=22; throw 'Missing release executable' }
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $stage $name))
        }
    } finally { $zip.Dispose() }
    $manifest = Get-Content -LiteralPath (Join-Path $stage 'scry-release.json') -Raw | ConvertFrom-Json
    $manifest=ValidateManifest $manifest $tag
    if ($manifest.ipc_version -ne 1 -or $manifest.index_format -ne 10) { $code=26; throw 'Release requires a newer updater or an explicit index rebuild migration' }
    # Execute only bytes verified against the official release asset digest.
    $version = & (Join-Path $stage 'scryd.exe') --version
    if ($LASTEXITCODE -ne 0 -or $version.Trim() -cne ('scryd ' + $tag.Substring(1))) { $code=22; throw 'Release executable version mismatch' }
    $releases = Join-Path $root 'releases'
    SecureDirectory $releases
    $final = Join-Path $releases ($tag + '-' + [Guid]::NewGuid().ToString('N'))
    SecureDirectory $final
    Copy-Item -LiteralPath (Join-Path $stage 'scry.exe'),(Join-Path $stage 'scryd.exe') -Destination $final
    $code=1
    if ([DateTime]::UtcNow -ge $UpdateDeadline) { throw 'Update deadline expired before activation' }
    Step 'Preparing daemon restart'
    $running = @(Get-Process -Name scryd -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path -ieq $oldPath })
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class ScryStop {
 [DllImport("kernel32", CharSet=CharSet.Unicode, SetLastError=true)] public static extern IntPtr OpenEvent(uint access, bool inherit, string name);
 [DllImport("kernel32")] public static extern bool SetEvent(IntPtr handle);
 [DllImport("kernel32")] public static extern bool GetNamedPipeServerProcessId(IntPtr pipe, out uint pid);
 [DllImport("kernel32")] public static extern bool CloseHandle(IntPtr handle);
}
"@
    $probe=[IO.Pipes.NamedPipeClientStream]::new('.','scry',[IO.Pipes.PipeDirection]::InOut)
    try {
        try { $probe.Connect(100) } catch [TimeoutException] {}
        if ($probe.IsConnected) {
            $serverPid=0
            if (-not [ScryStop]::GetNamedPipeServerProcessId($probe.SafePipeHandle.DangerousGetHandle(),[ref]$serverPid)) { throw 'Cannot identify existing daemon' }
            $server=Get-Process -Id $serverPid
            if ($server.Path -ine $oldPath) { $code=23; throw 'Another daemon owns the query pipe; its process was left running' }
        }
    } finally { $probe.Dispose() }
    SaveJournal 'prepared'
    foreach ($process in $running) {
        $event = [ScryStop]::OpenEvent(2, $false, ('Local\ScryShutdown-' + $process.Id))
        if ($event -eq [IntPtr]::Zero) { $code=23; throw 'This legacy daemon must be stopped before updating; graceful stop is unsupported' }
        try { if (-not [ScryStop]::SetEvent($event)) { throw 'Graceful stop failed' } } finally { [ScryStop]::CloseHandle($event) | Out-Null }
        if (-not $process.WaitForExit(30000)) { $code=23; throw 'Daemon did not finish graceful shutdown' }
    }
    SaveJournal 'activated'
    $mutated = $true
    if ($task) {
        [xml]$xml = $oldXml
        $xml.Task.Actions.Exec.Command = Join-Path $final 'scryd.exe'
        Register-ScheduledTask -TaskName $taskName -Xml $xml.OuterXml -Force | Out-Null
    } else {
        $action = New-ScheduledTaskAction -Execute (Join-Path $final 'scryd.exe')
        $trigger = New-ScheduledTaskTrigger -AtLogOn -User $OriginSid
        $principal = New-ScheduledTaskPrincipal -UserId $OriginSid -LogonType Interactive -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
        Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
    }
    $scheduler = New-Object -ComObject 'Schedule.Service'
    $scheduler.Connect()
    $scheduler.GetFolder('\').GetTask($taskName).SetSecurityDescriptor("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGX;;;$OriginSid)", 0)
    SaveJournal 'activated'
    Step 'Starting daemon and checking compatibility'
    Start-ScheduledTask -TaskName $taskName
    $ready = $false
    $readinessDeadline=[DateTime]::UtcNow.AddMinutes(10)
    while ([DateTime]::UtcNow -lt $readinessDeadline -and [DateTime]::UtcNow -lt $UpdateDeadline) {
        $response = & (Join-Path $final 'scry.exe') status --json --version $tag
        if ($LASTEXITCODE -eq 0) {
            $status = $response | ConvertFrom-Json
            $daemon = Get-Process -Id $status.detail.server_pid -ErrorAction SilentlyContinue
            if ($daemon -and $daemon.Path -ieq (Join-Path $final 'scryd.exe')) { $ready=$true; break }
        }
        Start-Sleep -Seconds 1
    }
    if (-not $ready) { $code=25; throw 'Daemon readiness deadline expired; new binaries retained for diagnosis' }
    $userPath = [Environment]::GetEnvironmentVariable('Path','User')
    $entries = @($userPath -split ';' | Where-Object { $_ -and $_ -ine (Join-Path $env:LOCALAPPDATA 'Programs\Scry Search\bin') -and $_ -notlike "$root\releases\*" })
    [Environment]::SetEnvironmentVariable('Path', (($entries + $final) -join ';'), 'User')
    Copy-Item -LiteralPath (Join-Path $stage 'uninstall-daemon.ps1') -Destination (Join-Path $root 'uninstall.ps1') -Force
    SaveJournal 'committed'
    Send 'result' 'updated' "Scry Search $tag is ready" 0
    $code=0
} catch {
    # After activation, never silently restart old binaries against possibly
    # rewritten index data. Leave the durable journal for explicit recovery.
    $state = if ($mutated) { 'recovery_required' } else { 'failed' }
    Send 'result' $state $_.Exception.Message $code
} finally {
    if ($lock) { $lock.Dispose() }
    if ($stage -and $root -and [IO.Path]::GetFullPath($stage).StartsWith([IO.Path]::GetFullPath($root) + '\', [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
    }
    $writer.Dispose(); $pipe.Dispose()
}
exit $code
'@
$server = $null
$helper = $null
try {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class ScryPeer {
 [DllImport("kernel32", SetLastError=true)] public static extern bool GetNamedPipeClientProcessId(IntPtr pipe, out uint pid);
}
"@
    $security = [IO.Pipes.PipeSecurity]::new()
    $security.SetSecurityDescriptorSddlForm("D:P(A;;GA;;;$sid)(A;;GA;;;BA)(A;;GA;;;SY)")
    $server = [IO.Pipes.NamedPipeServerStream]::new($pipeName, [IO.Pipes.PipeDirection]::In, 1, [IO.Pipes.PipeTransmissionMode]::Byte, [IO.Pipes.PipeOptions]::Asynchronous, 4096, 4096, $security)
    $connected = $server.BeginWaitForConnection($null,$null)
    $prefix = "`$Release='$Release';`$PipeName='$pipeName';`$OriginSid='$sid';"
    $memory = [IO.MemoryStream]::new()
    $gzip = [IO.Compression.GZipStream]::new($memory, [IO.Compression.CompressionMode]::Compress, $true)
    $bytes = [Text.Encoding]::UTF8.GetBytes($prefix + $engine)
    $gzip.Write($bytes,0,$bytes.Length); $gzip.Dispose()
    $packed = [Convert]::ToBase64String($memory.ToArray()); $memory.Dispose()
    $bootstrap = "`$m=[IO.MemoryStream]::new([Convert]::FromBase64String('$packed'));`$g=[IO.Compression.GZipStream]::new(`$m,[IO.Compression.CompressionMode]::Decompress);`$r=[IO.StreamReader]::new(`$g); & ([scriptblock]::Create(`$r.ReadToEnd()))"
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($bootstrap))
    Emit 'progress' 'elevating' 'Waiting for administrator approval' 0
    $helper = Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe') -Verb RunAs -WindowStyle Hidden -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-EncodedCommand',$encoded) -PassThru
    if (-not $connected.AsyncWaitHandle.WaitOne(30000)) { throw 'Elevated helper connection timed out' }
    $server.EndWaitForConnection($connected)
    $peer = 0
    if (-not [ScryPeer]::GetNamedPipeClientProcessId($server.SafePipeHandle.DangerousGetHandle(), [ref]$peer) -or $peer -ne $helper.Id) { throw 'Elevated helper authentication failed' }
    $deadline = [DateTime]::UtcNow.AddMinutes(15)
    $line = [Collections.Generic.List[byte]]::new()
    $buffer = New-Object byte[] 1
    $result = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        $read = $server.ReadAsync($buffer,0,1)
        while (-not $read.Wait(1000)) {
            if ([DateTime]::UtcNow -ge $deadline) { throw 'Update helper deadline expired' }
        }
        if ($read.Result -eq 0) { break }
        if ($buffer[0] -eq 10) {
            $text = [Text.Encoding]::UTF8.GetString($line.ToArray()).TrimEnd("`r")
            $event = DecodeEvent $text
            if ($event.event -eq 'result') { $result=$event; break }
            $text
            $line.Clear()
        } else {
            if ($line.Count -ge 4096) { throw 'Helper event exceeds bound' }
            $line.Add($buffer[0])
        }
    }
    if (-not $helper.WaitForExit(10000) -or -not $result -or $helper.ExitCode -ne $result.exit_code) { throw 'Elevated helper did not return a confirmed result' }
    $result | ConvertTo-Json -Compress
    exit $helper.ExitCode
} catch {
    $code = CancellationCode $_.Exception
    $state = if ($code -eq 20) { 'cancelled' } else { 'failed' }
    Emit 'result' $state $_.Exception.Message $code
    exit $code
} finally { if ($server) { $server.Dispose() } }
