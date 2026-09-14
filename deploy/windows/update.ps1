#Requires -RunAsAdministrator
# Executed from the protected installation directory. Downloads only the fixed
# upstream release asset, verifies its digest and pins every executable/script
# to the Authenticode signer of the currently installed agent.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$mutex = New-Object Threading.Mutex($false,'Global\ScreenGuard.Update')
if (!$mutex.WaitOne(0)) { exit 0 }
try {
    $InstallDir = $PSScriptRoot
    $exe = Join-Path $InstallDir 'screenguard-agent-windows.exe'
    $installedSignature = Get-AuthenticodeSignature -LiteralPath $exe
    if ($installedSignature.Status -ne 'Valid') { throw 'Remote update requires a signed installed agent. Install a signed release first.' }
    $thumbprint = $installedSignature.SignerCertificate.Thumbprint
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $releases = Invoke-RestMethod -Uri 'https://api.github.com/repos/adambie/screenguard/releases?per_page=50' -Headers @{'User-Agent'='ScreenGuard-Windows'}
    $release = $releases | Where-Object { !$_.draft -and !$_.prerelease -and $_.tag_name -match '^v\d+\.\d+\.\d+$' -and @($_.assets | Where-Object name -eq 'screenguard-windows-x86_64.zip').Count -gt 0 } | Sort-Object { [version]$_.tag_name.Substring(1) } -Descending | Select-Object -First 1
    if (!$release) { throw 'No Windows release is available.' }
    $current = (& $exe --version).Trim()
    if ([version]$release.tag_name.Substring(1) -le [version]$current) { exit 0 }
    $Stage = Join-Path $env:ProgramData ('ScreenGuard\update-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $Stage | Out-Null
    $base = 'https://github.com/adambie/screenguard/releases/download/' + $release.tag_name + '/'
    $zip = Join-Path $Stage 'release.zip'
    Invoke-WebRequest -UseBasicParsing -Uri ($base+'screenguard-windows-x86_64.zip') -OutFile $zip
    $digest = (Invoke-WebRequest -UseBasicParsing -Uri ($base+'screenguard-windows-x86_64.zip.sha256')).Content.Trim().Split(' ')[0]
    if ($digest -notmatch '^[a-fA-F0-9]{64}$' -or (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash -ne $digest) { throw 'Release digest verification failed.' }
    $unpacked = Join-Path $Stage 'package'
    Expand-Archive -LiteralPath $zip -DestinationPath $unpacked
    $files = @('screenguard-agent-windows.exe','screenguard-tray-windows.exe','update.ps1','uninstall.ps1')
    foreach ($file in $files) {
        $sig = Get-AuthenticodeSignature -LiteralPath (Join-Path $unpacked $file)
        if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Thumbprint -ne $thumbprint) { throw "Untrusted release file: $file" }
    }
    $backup = Join-Path $Stage 'backup'
    New-Item -ItemType Directory -Path $backup | Out-Null
    foreach ($file in $files) { Copy-Item -LiteralPath (Join-Path $InstallDir $file) -Destination $backup }
    Stop-Service ScreenGuard -Force
    (Get-Service ScreenGuard).WaitForStatus('Stopped',[TimeSpan]::FromSeconds(30))
    Get-Process -Name screenguard-tray-windows -ErrorAction SilentlyContinue | Stop-Process -Force
    try {
        foreach ($file in $files) { Copy-Item -LiteralPath (Join-Path $unpacked $file) -Destination (Join-Path $InstallDir $file) -Force }
        Start-Service ScreenGuard
        Start-Sleep -Seconds 10
        if ((Get-Service ScreenGuard).Status -ne 'Running') { throw 'Updated service did not remain running.' }
    } catch {
        Stop-Service ScreenGuard -Force -ErrorAction SilentlyContinue
        foreach ($file in $files) { Copy-Item -LiteralPath (Join-Path $backup $file) -Destination (Join-Path $InstallDir $file) -Force }
        Start-Service ScreenGuard
        throw
    }
    Remove-Item -LiteralPath $Stage -Recurse -Force
} catch {
    $log = Join-Path $env:ProgramData 'ScreenGuard\logs\update.log'
    Add-Content -LiteralPath $log -Value ((Get-Date).ToString('o') + ' ' + $_.Exception.Message)
    throw
} finally { $mutex.ReleaseMutex(); $mutex.Dispose() }
