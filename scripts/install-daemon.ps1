param(
    [switch]$NoStart,
    [switch]$Unbounded,
    [string]$IndexMbps = ""
)

$ErrorActionPreference = "Stop"
$taskName = "Scry Search Daemon"
$daemonPath = Join-Path $PSScriptRoot "scryd.exe"

if (-not (Test-Path -LiteralPath $daemonPath)) {
    throw "scryd.exe must be beside this installer script"
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
$action = New-ScheduledTaskAction -Execute $daemonPath -Argument $daemonArguments
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity.Name
$taskPrincipal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $taskPrincipal -Settings $settings -Force | Out-Null

if (-not $NoStart) {
    Start-ScheduledTask -TaskName $taskName
}

Write-Host "Installed the elevated per-user Scry Search daemon task."
