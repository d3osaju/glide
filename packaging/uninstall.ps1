# Removes glide. Keeps your settings in %APPDATA%\in.zetalabs.glide unless -Purge is given.
param([switch]$Purge)
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    $extra = if ($Purge) { " -Purge" } else { "" }
    Start-Process powershell -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`"$extra"
    exit
}
$ErrorActionPreference = "Continue"
Get-Process glide -ErrorAction SilentlyContinue | Stop-Process -Force
Get-NetFirewallRule -DisplayName "glide*" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
Get-NetQosPolicy -Name "glide" -ErrorAction SilentlyContinue | Remove-NetQosPolicy -Confirm:$false
Remove-Item (Join-Path $env:ProgramData "Microsoft\Windows\Start Menu\Programs\glide.lnk") -ErrorAction SilentlyContinue
Remove-Item (Join-Path ([Environment]::GetFolderPath("Desktop")) "glide.lnk") -ErrorAction SilentlyContinue
Remove-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "glide" -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force (Join-Path $env:ProgramFiles "glide") -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA "Programs\glide") -ErrorAction SilentlyContinue
Remove-Item (Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\glide.lnk") -ErrorAction SilentlyContinue
if ($Purge) { Remove-Item -Recurse -Force (Join-Path $env:APPDATA "in.zetalabs.glide") -ErrorAction SilentlyContinue }
# Restore the mouse cursor in case glide was killed while hiding it.
Add-Type -Namespace W -Name U -MemberDefinition '[DllImport("user32.dll")] public static extern bool SystemParametersInfo(uint a, uint b, System.IntPtr c, uint d);'
[W.U]::SystemParametersInfo(0x57, 0, [IntPtr]::Zero, 0) | Out-Null
Write-Host "glide removed."
Start-Sleep -Seconds 2
