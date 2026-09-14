#Requires -RunAsAdministrator
[CmdletBinding()]
param([switch]$RemoveData)
$ErrorActionPreference = 'Stop'
$InstallDir = $PSScriptRoot
$service = Get-Service ScreenGuard -ErrorAction SilentlyContinue
if ($service) {
    Stop-Service ScreenGuard -Force
    $service.WaitForStatus('Stopped',[TimeSpan]::FromSeconds(30))
}
# Give live helpers time to restore their previous proxy/PAC settings.
Start-Sleep -Seconds 7
# A helper for a logged-out user restores its durable backup on the next logon.
# Keep a small per-user cleanup helper until then; no service or WFP rules remain.
$cleanup = Join-Path $InstallDir 'screenguard-tray-windows.exe'
$run = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run'
New-ItemProperty -Path $run -Name 'ScreenGuardProxyCleanup' -Value ('"'+$cleanup+'" --restore-proxy') -PropertyType String -Force | Out-Null
Get-Process -Name screenguard-tray-windows -ErrorAction SilentlyContinue | Stop-Process -Force
if ($service) { & sc.exe delete ScreenGuard | Out-Null; if ($LASTEXITCODE -ne 0) { throw 'Service removal failed.' } }
Get-NetFirewallRule -Name ScreenGuard-mDNS -ErrorAction SilentlyContinue | Remove-NetFirewallRule
Remove-Item -LiteralPath (Join-Path $InstallDir 'screenguard-agent-windows.exe') -Force
Remove-Item -LiteralPath (Join-Path $InstallDir 'update.ps1') -Force -ErrorAction SilentlyContinue
if ($RemoveData) { Remove-Item -LiteralPath (Join-Path $env:ProgramData 'ScreenGuard') -Recurse -Force }
Remove-Item -LiteralPath 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\ScreenGuard' -Recurse -Force -ErrorAction SilentlyContinue
Write-Host 'ScreenGuard service removed. A proxy cleanup helper remains for users who are currently logged out.'
