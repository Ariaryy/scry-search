$ErrorActionPreference = "Stop"
$taskName = "Scry Search Daemon"
$installRoot = Join-Path $env:LOCALAPPDATA "Programs\Scry Search"
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
    $_ -and $_.TrimEnd('\') -ine $binPath.TrimEnd('\')
})
[Environment]::SetEnvironmentVariable("Path", ($pathEntries -join ';'), "User")

if (Test-Path -LiteralPath $installRoot) {
    $removeScript = Join-Path $env:TEMP "scry-search-remove-$PID.ps1"
    $escapedRoot = $installRoot.Replace("'", "''")
    Set-Content -LiteralPath $removeScript -Value @"
Start-Sleep -Milliseconds 500
Remove-Item -LiteralPath '$escapedRoot' -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath `$PSCommandPath -Force -ErrorAction SilentlyContinue
"@
    Start-Process -FilePath "powershell.exe" -WindowStyle Hidden -ArgumentList @(
        "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$removeScript`""
    ) | Out-Null
}

Write-Host "Removed Scry Search and its startup task. Snapshot data was left intact."
