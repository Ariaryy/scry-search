param([switch]$NoStart)

$ErrorActionPreference = "Stop"
$taskName = "Scry Daemon"
$daemonPath = Join-Path $PSScriptRoot "scryd.exe"

if (-not (Test-Path -LiteralPath $daemonPath)) {
    throw "scryd.exe must be beside this installer script"
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $arguments = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"")
    if ($NoStart) { $arguments += "-NoStart" }
    Start-Process -FilePath "powershell.exe" -Verb RunAs -ArgumentList $arguments -Wait
    exit $LASTEXITCODE
}

$action = New-ScheduledTaskAction -Execute $daemonPath
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity.Name
$taskPrincipal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $taskPrincipal -Settings $settings -Force | Out-Null

if (-not $NoStart) {
    Start-ScheduledTask -TaskName $taskName
}

Write-Host "Installed the elevated per-user Scry daemon task."

