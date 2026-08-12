param(
    [switch]$NoStart,
    [switch]$Unbounded,
    [string]$IndexMbps = "",
    [switch]$PauseOnExit
)

$ErrorActionPreference = "Stop"
$taskName = "Scry Search Daemon"
$sourceDaemonPath = Join-Path $PSScriptRoot "scryd.exe"
$sourceCliPath = Join-Path $PSScriptRoot "scry.exe"
$installRoot = Join-Path $env:LOCALAPPDATA "Programs\Scry Search"
$binPath = Join-Path $installRoot "bin"
$daemonPath = Join-Path $binPath "scryd.exe"
$currentStep = "Checking the installation package"

function Write-Step {
    param([Parameter(Mandatory = $true)][string]$Message)

    Write-Host "  -> $Message" -ForegroundColor Cyan
}

function Clear-SetupDisplay {
    # `Clear-Host` can leave parts of the old viewport behind in some Windows
    # terminal hosts. Clearing the console buffer produces a clean setup screen.
    # Skip it when output is redirected so CI logs remain plain text.
    if ([Console]::IsOutputRedirected) {
        return
    }

    try {
        [Console]::Clear()
    }
    catch {
        Clear-Host
    }
}

function Wait-BeforeExit {
    if (-not $PauseOnExit -or [Console]::IsInputRedirected) {
        return
    }

    Write-Host ""
    $null = Read-Host "Press Enter to close this window"
}

function Get-InstalledDaemonProcess {
    Get-Process -Name "scryd" -ErrorAction SilentlyContinue | ForEach-Object {
        try {
            if ($_.Path -and $_.Path -ieq $daemonPath) {
                $_
            }
        }
        catch {
            # The process may exit between enumeration and reading its path.
        }
    }
}

function Get-ScryVersion {
    param([Parameter(Mandatory = $true)][string]$ExecutablePath)

    try {
        $output = & $ExecutablePath --version 2>$null | Select-Object -First 1
        if ($LASTEXITCODE -eq 0 -and $output -match '^scryd\s+(.+)$') {
            return $Matches[1].Trim()
        }
    }
    catch {
        # Older or damaged installations may not be able to report a version.
    }
    return "unknown"
}

function Stop-Setup {
    param(
        [Parameter(Mandatory = $true)][string]$Problem,
        [Parameter(Mandatory = $true)][string]$NextStep
    )

    Write-Host ""
    Write-Host "Scry Search setup could not continue." -ForegroundColor Red
    Write-Host $Problem
    Write-Host ""
    Write-Host "What to do next:" -ForegroundColor Yellow
    Write-Host "  $NextStep"
    Wait-BeforeExit
    exit 1
}

trap {
    Write-Host ""
    Write-Host "Scry Search setup failed." -ForegroundColor Red
    Write-Host "  Step: $currentStep"
    Write-Host "  Reason: $($_.Exception.Message)"
    Write-Host ""
    Write-Host "Setup may be partially complete. It is safe to run this installer again" -ForegroundColor Yellow
    Write-Host "after correcting the problem. Existing index data is not removed."
    Wait-BeforeExit
    exit 1
}

Clear-SetupDisplay
Write-Host "Scry Search setup" -ForegroundColor Cyan
Write-Host "=================" -ForegroundColor DarkGray
Write-Host "Installs the Scry daemon and CLI for the current Windows user."
Write-Host ""
Write-Step "Checking the release package"

if (-not (Test-Path -LiteralPath $sourceDaemonPath)) {
    Stop-Setup `
        -Problem "The release package is incomplete: scryd.exe was not found at '$sourceDaemonPath'." `
        -NextStep "Download and fully extract the Scry Search release ZIP, then run install-daemon.ps1 from that extracted folder. The source-repository script does not contain prebuilt executables."
}
if (-not (Test-Path -LiteralPath $sourceCliPath)) {
    Stop-Setup `
        -Problem "The release package is incomplete: scry.exe was not found at '$sourceCliPath'." `
        -NextStep "Download and fully extract the Scry Search release ZIP, then run install-daemon.ps1 from that extracted folder. Keep scry.exe, scryd.exe, and both setup scripts together."
}
if ($Unbounded -and $IndexMbps) {
    Stop-Setup `
        -Problem "The options -Unbounded and -IndexMbps cannot be used together." `
        -NextStep "Run the installer with either -Unbounded or -IndexMbps <number>, or omit both to use the default 128 MiB/s indexing limit."
}
if ($IndexMbps -and $IndexMbps -notmatch '^\d+$') {
    Stop-Setup `
        -Problem "The -IndexMbps value '$IndexMbps' is not a non-negative whole number." `
        -NextStep "Use a value such as -IndexMbps 64, or omit the option to use the default 128 MiB/s limit."
}

$newVersion = Get-ScryVersion -ExecutablePath $sourceDaemonPath
$installedVersion = if (Test-Path -LiteralPath $daemonPath) {
    Get-ScryVersion -ExecutablePath $daemonPath
} else {
    $null
}

if ($installedVersion) {
    if ($installedVersion -eq $newVersion) {
        Write-Step "Reinstalling Scry Search $newVersion"
    } else {
        Write-Step "Updating Scry Search: $installedVersion -> $newVersion"
    }
} else {
    Write-Step "Installing Scry Search $newVersion"
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $currentStep = "Requesting administrator permission"
    Write-Step "Requesting administrator permission for raw NTFS indexing"
    $arguments = @(
        "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"", "-PauseOnExit"
    )
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

$currentStep = "Stopping the previous daemon task"
if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Write-Step "Stopping the existing startup task"
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
}

# Task Scheduler stops asynchronously. Wait for the installed daemon to release
# its executable before replacing it; if it remains alive, terminate only the
# process running from Scry's install directory, never a development build.
$runningDaemon = @(Get-InstalledDaemonProcess)
if ($runningDaemon.Count -gt 0) {
    Write-Step "Waiting for the existing daemon to close"
    for ($attempt = 0; $attempt -lt 25 -and $runningDaemon.Count -gt 0; $attempt++) {
        Start-Sleep -Milliseconds 200
        $runningDaemon = @(Get-InstalledDaemonProcess)
    }
}
if ($runningDaemon.Count -gt 0) {
    Write-Step "Closing the existing installed daemon"
    $runningDaemon | Stop-Process -Force
    $runningDaemon | Wait-Process -Timeout 5 -ErrorAction SilentlyContinue
}
if (@(Get-InstalledDaemonProcess).Count -gt 0) {
    throw "The installed Scry daemon did not close. End '$daemonPath' in Task Manager, then run setup again."
}

$currentStep = "Copying Scry Search files"
Write-Step "Installing the daemon and CLI to '$binPath'"
New-Item -ItemType Directory -Path $binPath -Force | Out-Null
Copy-Item -LiteralPath $sourceDaemonPath -Destination $daemonPath -Force
Copy-Item -LiteralPath $sourceCliPath -Destination (Join-Path $binPath "scry.exe") -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "uninstall-daemon.ps1") -Destination (Join-Path $installRoot "uninstall.ps1") -Force

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$pathEntries = @($userPath -split ';' | Where-Object { $_ })
if (-not ($pathEntries | Where-Object { $_.TrimEnd('\') -ieq $binPath.TrimEnd('\') })) {
    $currentStep = "Updating the user PATH"
    Write-Step "Adding the Scry CLI directory to the user PATH"
    $newUserPath = (($pathEntries + $binPath) -join ';')
    [Environment]::SetEnvironmentVariable("Path", $newUserPath, "User")
} else {
    Write-Step "The Scry CLI directory is already on the user PATH"
}

# `New-ScheduledTaskAction` rejects an explicitly empty `-Argument`. Omit the
# parameter for the default configuration so installation reaches task
# registration instead of stopping after the binaries and PATH are updated.
$action = if ($daemonArguments) {
    New-ScheduledTaskAction -Execute $daemonPath -Argument $daemonArguments
} else {
    New-ScheduledTaskAction -Execute $daemonPath
}
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity.Name
$taskPrincipal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
$currentStep = "Registering the startup task"
Write-Step "Registering the elevated per-user startup task"
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $taskPrincipal -Settings $settings -Force | Out-Null

if (-not $NoStart) {
    $currentStep = "Starting the Scry daemon"
    Write-Step "Starting the daemon"
    Start-ScheduledTask -TaskName $taskName
}

Write-Host ""
Write-Host "Scry Search is ready." -ForegroundColor Green
if ($installedVersion -and $installedVersion -ne $newVersion) {
    Write-Host "  Version:     $installedVersion -> $newVersion"
} else {
    Write-Host "  Version:     $newVersion"
}
Write-Host "  Programs:    $binPath"
Write-Host "  Uninstaller: $(Join-Path $installRoot 'uninstall.ps1')"
Write-Host "  Index data:  $(Join-Path $env:LOCALAPPDATA 'scry')"
Write-Host "  Startup:     Per-user scheduled task '$taskName'"
if (-not $NoStart) {
    Write-Host "  Daemon:      Started in the background"
} else {
    Write-Host "  Daemon:      Not started (-NoStart was supplied)"
}
Wait-BeforeExit
