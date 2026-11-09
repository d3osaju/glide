# Builds release glide.exe and assembles dist\ + dist\glide-windows-x64.zip
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot
# Use the toolchain's cargo directly (the rustup proxy can be blocked by Application Control).
$tc = (rustc --print sysroot).Trim()
$env:RUSTC = Join-Path $tc "bin\rustc.exe"
& (Join-Path $tc "bin\cargo.exe") build --release
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$dist = Join-Path $PSScriptRoot "dist"
Remove-Item -Recurse -Force $dist -ErrorAction SilentlyContinue
New-Item -ItemType Directory $dist | Out-Null
Copy-Item target\release\glide.exe, README.md, packaging\install.ps1, packaging\uninstall.ps1 $dist
# Double-click installer launcher
Set-Content -Encoding ascii (Join-Path $dist "Install glide.cmd") '@powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1"'
$zip = Join-Path $PSScriptRoot "glide-windows-x64.zip"
Compress-Archive -Path "$dist\*" -DestinationPath $zip -Force
Move-Item $zip $dist -Force
Get-ChildItem $dist | Format-Table Name, Length
