#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    [string]$ServerUrl = '',
    [string]$CloudAccount = '',
    [string]$WebUiUrl = '',
    [switch]$AllowRemoteUpdate
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (![Environment]::Is64BitOperatingSystem -or [Environment]::OSVersion.Version.Build -lt 22000) { throw 'Windows 11 x64 is required.' }
$InstallDir = Join-Path $env:ProgramFiles 'ScreenGuard'
$DataDir = Join-Path $env:ProgramData 'ScreenGuard'

function Assert-NotReparse([string]$Path) {
    if ((Test-Path -LiteralPath $Path) -and ((Get-Item -LiteralPath $Path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Refusing reparse point: $Path"
    }
}
foreach ($path in @($InstallDir, $DataDir)) { Assert-NotReparse $path }
foreach ($name in @('screenguard-agent-windows.exe','screenguard-tray-windows.exe','update.ps1','uninstall.ps1')) {
    if (!(Test-Path -LiteralPath (Join-Path $PSScriptRoot $name))) { throw "Missing package file: $name" }
}
if ($ServerUrl -and $CloudAccount) { throw 'Choose ServerUrl or CloudAccount, not both.' }
if ($ServerUrl -and $ServerUrl -notmatch '^(https?|wss?)://') { throw 'Invalid server URL.' }
if ($WebUiUrl -and $WebUiUrl -notmatch '^https?://') { throw 'Invalid web UI URL.' }
if ($CloudAccount -and $CloudAccount -notmatch '@') { throw 'CloudAccount must be an email.' }

New-Item -ItemType Directory -Force -Path $InstallDir,$DataDir | Out-Null
# Refuse junctions before recursive ACL changes or elevated copies.
foreach ($root in @($InstallDir,$DataDir)) {
    Get-ChildItem -LiteralPath $root -Force -Recurse | ForEach-Object { Assert-NotReparse $_.FullName }
}
function Protect-Directory([string]$Path, [bool]$UserReadAccess) {
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true,$false)
    $admin = New-Object Security.Principal.SecurityIdentifier 'S-1-5-32-544'
    $system = New-Object Security.Principal.SecurityIdentifier 'S-1-5-18'
    $acl.SetOwner($admin)
    foreach ($sid in @($admin,$system)) {
        $rule = New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow')
        $acl.AddAccessRule($rule)
    }
    if ($UserReadAccess) {
        $users = New-Object Security.Principal.SecurityIdentifier 'S-1-5-32-545'
        $rule = New-Object Security.AccessControl.FileSystemAccessRule($users,'ReadAndExecute','ContainerInherit,ObjectInherit','None','Allow')
        $acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
    # Existing files inherit the new policy instead of retaining old explicit
    # user access or ownership from a previous installation.
    Get-ChildItem -LiteralPath $Path -Force | ForEach-Object {
        & icacls.exe $_.FullName /reset /T /Q | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "Could not reset permissions: $($_.FullName)" }
    }
    & icacls.exe $Path /setowner '*S-1-5-32-544' /T /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Could not protect ownership: $Path" }
}
Protect-Directory $DataDir $false
Protect-Directory $InstallDir $true
$service = Get-Service -Name ScreenGuard -ErrorAction SilentlyContinue
if ($service) { Stop-Service ScreenGuard -Force; $service.WaitForStatus('Stopped',[TimeSpan]::FromSeconds(30)) }
Get-Process -Name screenguard-tray-windows -ErrorAction SilentlyContinue | Stop-Process -Force
foreach ($name in @('screenguard-agent-windows.exe','screenguard-tray-windows.exe','update.ps1','uninstall.ps1')) {
    $source = Join-Path $PSScriptRoot $name
    $destination = Join-Path $InstallDir $name
    if ([IO.Path]::GetFullPath($source) -ne [IO.Path]::GetFullPath($destination)) { Copy-Item -LiteralPath $source -Destination $destination -Force }
}
$config = Join-Path $DataDir 'agent.toml'
if (!(Test-Path -LiteralPath $config)) {
    $lines = @('heartbeat_interval = 10','user_scan_interval = 300','cache_ttl_hours = 48','idle_seconds = 300','web_filter = true')
    $lines += 'allow_remote_update = ' + $AllowRemoteUpdate.IsPresent.ToString().ToLowerInvariant()
    if ($ServerUrl) { $lines += 'server_url = ' + (ConvertTo-Json -InputObject $ServerUrl -Compress) }
    if ($WebUiUrl) { $lines += 'webui_url = ' + (ConvertTo-Json -InputObject $WebUiUrl -Compress) }
    if ($CloudAccount) { $lines += 'cloud_account = ' + (ConvertTo-Json -InputObject $CloudAccount -Compress) }
    [IO.File]::WriteAllLines($config,$lines,(New-Object Text.UTF8Encoding $false))
}
$exe = Join-Path $InstallDir 'screenguard-agent-windows.exe'
if (!$service) { New-Service -Name ScreenGuard -DisplayName 'ScreenGuard parental control' -BinaryPathName ('"'+$exe+'" --service') -StartupType Automatic | Out-Null }
else { & sc.exe config ScreenGuard binPath= ('"'+$exe+'" --service') start= auto obj= LocalSystem | Out-Null; if ($LASTEXITCODE -ne 0) { throw 'Service configuration failed.' } }
& sc.exe failure ScreenGuard reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Service recovery configuration failed.' }
& sc.exe failureflag ScreenGuard 1 | Out-Null
# mDNS only; the HTTP proxy accepts loopback clients and needs no inbound LAN rule.
Get-NetFirewallRule -Name ScreenGuard-mDNS -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -Name ScreenGuard-mDNS -DisplayName 'ScreenGuard mDNS discovery' -Program $exe -Direction Inbound -Action Allow -Protocol UDP -LocalPort 5353 -Profile Private | Out-Null
Remove-ItemProperty -Path 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'ScreenGuardProxyCleanup' -ErrorAction SilentlyContinue
$uninstall = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\ScreenGuard'
New-Item -Path $uninstall -Force | Out-Null
New-ItemProperty -Path $uninstall -Name DisplayName -Value 'ScreenGuard' -PropertyType String -Force | Out-Null
New-ItemProperty -Path $uninstall -Name DisplayVersion -Value ((& $exe --version).Trim()) -PropertyType String -Force | Out-Null
New-ItemProperty -Path $uninstall -Name Publisher -Value 'ScreenGuard' -PropertyType String -Force | Out-Null
New-ItemProperty -Path $uninstall -Name UninstallString -Value ('powershell.exe -NoProfile -ExecutionPolicy Bypass -File "'+(Join-Path $InstallDir 'uninstall.ps1')+'"') -PropertyType String -Force | Out-Null
New-ItemProperty -Path $uninstall -Name NoModify -Value 1 -PropertyType DWord -Force | Out-Null
New-ItemProperty -Path $uninstall -Name NoRepair -Value 1 -PropertyType DWord -Force | Out-Null
Start-Service ScreenGuard
Start-Sleep -Seconds 3
if ((Get-Service ScreenGuard).Status -ne 'Running') { throw 'ScreenGuard failed to start; inspect the logs in ProgramData\ScreenGuard\logs.' }
Write-Host "ScreenGuard installed. Configuration: $config"
Write-Host "Pairing code: inspect the newest file in $DataDir\logs and approve the agent in the web UI."
