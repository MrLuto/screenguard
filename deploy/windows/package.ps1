[CmdletBinding()]
param([string]$Version = '0.10.9', [string]$TargetDir = 'target/release')
$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw 'Invalid version' }
$package = Join-Path $PSScriptRoot 'package'
if (Test-Path $package) { Remove-Item $package -Recurse -Force }
New-Item -ItemType Directory -Path $package | Out-Null
Copy-Item (Join-Path $TargetDir 'screenguard-agent-windows.exe'),(Join-Path $TargetDir 'screenguard-tray-windows.exe') $package
Copy-Item (Join-Path $PSScriptRoot 'install.ps1'),(Join-Path $PSScriptRoot 'uninstall.ps1'),(Join-Path $PSScriptRoot 'update.ps1') $package
Copy-Item 'LICENSE' $package
# Optional production signing. The certificate is imported by the release job,
# not stored in this repository. Sign scripts as well as executables.
if ($env:SCREENGUARD_SIGNING_THUMBPRINT) {
    $cert = Get-Item ('Cert:\CurrentUser\My\' + $env:SCREENGUARD_SIGNING_THUMBPRINT)
    Get-ChildItem $package -File | Where-Object Extension -in '.exe','.ps1' | ForEach-Object {
        $signature = Set-AuthenticodeSignature -FilePath $_.FullName -Certificate $cert -HashAlgorithm SHA256 -TimestampServer 'http://timestamp.digicert.com'
        if ($signature.Status -ne 'Valid') { throw "Signing failed: $($_.Name)" }
    }
}
New-Item -ItemType Directory -Path 'dist/windows' -Force | Out-Null
$archive = Join-Path (Resolve-Path 'dist/windows') 'screenguard-windows-x86_64.zip'
Compress-Archive -Path (Join-Path $package '*') -DestinationPath $archive -Force
[IO.File]::WriteAllText(($archive+'.sha256'), (Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant())
$command = Get-Command ISCC.exe -ErrorAction SilentlyContinue
$iscc = if ($command) { $command.Source } else { $null }
if (!$iscc) {
    $candidate = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
    if (Test-Path $candidate) { $iscc = $candidate }
}
if (!$iscc) { throw 'Inno Setup 6 is required to build the graphical installer.' }
& $iscc "/DAppVersion=$Version" (Join-Path $PSScriptRoot 'setup.iss')
if ($LASTEXITCODE -ne 0) { throw 'Installer build failed.' }
$setup = Join-Path $PSScriptRoot 'output/screenguard-windows-x86_64-setup.exe'
if ($env:SCREENGUARD_SIGNING_THUMBPRINT) {
    $sig = Set-AuthenticodeSignature -FilePath $setup -Certificate $cert -HashAlgorithm SHA256 -TimestampServer 'http://timestamp.digicert.com'
    if ($sig.Status -ne 'Valid') { throw 'Installer signing failed.' }
}
Copy-Item $setup 'dist/windows/' -Force
