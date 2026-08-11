param(
    [switch]$NoStart,
    [switch]$Unbounded,
    [string]$IndexMbps = ""
)

$ErrorActionPreference = "Stop"
$taskName = "Scry Search Daemon"
$sourceDaemonPath = Join-Path $PSScriptRoot "scryd.exe"
$sourceCliPath = Join-Path $PSScriptRoot "scry.exe"
$installRoot = Join-Path $env:LOCALAPPDATA "Programs\Scry Search"
$binPath = Join-Path $installRoot "bin"
$daemonPath = Join-Path $binPath "scryd.exe"

if (-not (Test-Path -LiteralPath $sourceDaemonPath)) {
    throw "scryd.exe must be beside this installer script"
}
if (-not (Test-Path -LiteralPath $sourceCliPath)) {
    throw "scry.exe must be beside this installer script"
}
if ($Unbounded -and $IndexMbps) {
    throw "Use either -Unbounded or -IndexMbps, not both"
}
if ($IndexMbps -and $IndexMbps -notmatch '^\d+$') {
    throw "-IndexMbps expects a non-negative integer"
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $arguments = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"")
    if ($NoStart) { $arguments += "-NoStart" }
    if ($Unbounded) { $arguments += "-Unbounded" }
    if ($IndexMbps) { $arguments += @("-IndexMbps", $IndexMbps) }
    $elevated = Start-Process -FilePath "powershell.exe" -Verb RunAs -ArgumentList $arguments -Wait -PassThru
    exit $elevated.ExitCode
}

$daemonArguments = if ($Unbounded) {
    "--unbounded"
} elseif ($IndexMbps) {
    "--index-mbps $IndexMbps"
} else {
    ""
}

if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
}

New-Item -ItemType Directory -Path $binPath -Force | Out-Null
Copy-Item -LiteralPath $sourceDaemonPath -Destination $daemonPath -Force
Copy-Item -LiteralPath $sourceCliPath -Destination (Join-Path $binPath "scry.exe") -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "uninstall-daemon.ps1") -Destination (Join-Path $installRoot "uninstall.ps1") -Force

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$pathEntries = @($userPath -split ';' | Where-Object { $_ })
if (-not ($pathEntries | Where-Object { $_.TrimEnd('\') -ieq $binPath.TrimEnd('\') })) {
    $newUserPath = (($pathEntries + $binPath) -join ';')
    [Environment]::SetEnvironmentVariable("Path", $newUserPath, "User")
}

$action = New-ScheduledTaskAction -Execute $daemonPath -Argument $daemonArguments
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity.Name
$taskPrincipal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $taskPrincipal -Settings $settings -Force | Out-Null

if (-not $NoStart) {
    Start-ScheduledTask -TaskName $taskName
}

Write-Host "Copied scryd.exe and scry.exe to $binPath."
Write-Host "Copied the uninstaller to $(Join-Path $installRoot 'uninstall.ps1')."
Write-Host "Added $binPath to the user PATH; 'scry' is available in new terminals."
Write-Host "Indexes are stored separately under $(Join-Path $env:LOCALAPPDATA 'scry')."
if (-not $NoStart) {
    Write-Host "The elevated daemon has been started in the background."
} else {
    Write-Host "The daemon was not started because -NoStart was supplied."
}
