$ErrorActionPreference = "Stop"
$taskName = "Scry Search Daemon"
$installSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$installRoot = Join-Path (Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Scry Search') $installSid
$binPath = Join-Path $installRoot "bin"

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $elevated = Start-Process -FilePath "powershell.exe" -Verb RunAs -ArgumentList @(
        "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`""
    ) -Wait -PassThru
    exit $elevated.ExitCode
}

if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$pathEntries = @($userPath -split ';' | Where-Object {
    $_ -and $_.TrimEnd('\') -ine $binPath.TrimEnd('\') -and $_ -notlike "$installRoot\releases\*"
})
[Environment]::SetEnvironmentVariable("Path", ($pathEntries -join ';'), "User")

if (Test-Path -LiteralPath $installRoot) {
    $expected = [IO.Path]::GetFullPath((Join-Path (Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Scry Search') $installSid))
    if ([IO.Path]::GetFullPath($installRoot) -ine $expected -or ((Get-Item -LiteralPath $installRoot).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Refusing an unexpected installation removal path.'
    }
    Remove-Item -LiteralPath $installRoot -Recurse -Force
}

Write-Host "Removed Scry Search from $installRoot, its PATH entry, and its startup task."
Write-Host "Index data was preserved under $(Join-Path $env:LOCALAPPDATA 'scry')."
