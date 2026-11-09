<#
.SYNOPSIS
  Installs glide on this PC. Right-click > "Run with PowerShell", or run from an
  elevated PowerShell in the folder that contains glide.exe.

  After installing, open glide from the Start menu and choose
  "Start hosting" or "Connect to a host". Everything else is in the app.
#>
$ErrorActionPreference = "Stop"

# Re-launch elevated if needed (firewall + QoS policy need admin).
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Start-Process powershell -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`""
    exit
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$dir = Join-Path $env:ProgramFiles "glide"
$exe = Join-Path $dir "glide.exe"

Write-Host "Installing glide to $dir" -ForegroundColor Cyan
Get-Process glide -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item (Join-Path $here "glide.exe") $exe -Force
Unblock-File $exe

# Shortcuts (Start menu for everyone + this user's desktop)
$shell = New-Object -ComObject WScript.Shell
foreach ($lnk in @(
    (Join-Path $env:ProgramData "Microsoft\Windows\Start Menu\Programs\glide.lnk"),
    (Join-Path ([Environment]::GetFolderPath("Desktop")) "glide.lnk"))) {
    $s = $shell.CreateShortcut($lnk)
    $s.TargetPath = $exe
    $s.WorkingDirectory = $dir
    $s.IconLocation = "$exe,0"
    $s.Description = "Share keyboard, mouse, clipboard and audio across your devices"
    $s.Save()
}

# Firewall: private/domain networks only.
Get-NetFirewallRule -DisplayName "glide*" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName "glide (TCP)" -Direction Inbound -Program $exe -Protocol TCP -Action Allow -Profile Private,Domain | Out-Null
New-NetFirewallRule -DisplayName "glide (UDP)" -Direction Inbound -Program $exe -Protocol UDP -Action Allow -Profile Private,Domain | Out-Null

# QoS: let Windows apply glide's high-priority (DSCP 46) marking so routers and
# Wi-Fi send its packets ahead of torrents and downloads.
Get-NetQosPolicy -Name "glide" -ErrorAction SilentlyContinue | Remove-NetQosPolicy -Confirm:$false
New-NetQosPolicy -Name "glide" -AppPathNameMatchCondition "glide.exe" -DSCPAction 46 -NetworkProfile All | Out-Null
$qosKey = "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\QoS"
New-Item -Force $qosKey | Out-Null
Set-ItemProperty $qosKey -Name "Do not use NLA" -Value "1" -Type String

# If this network is marked Public, the firewall rules above won't apply.
$public = Get-NetConnectionProfile -ErrorAction SilentlyContinue | Where-Object NetworkCategory -eq "Public"
if ($public) {
    Write-Host "`nNote: '$($public.Name -join ', ')' is set to Public. glide only works on Private networks." -ForegroundColor Yellow
    Write-Host "Change it in Settings > Network & internet > (your network) > Private network.`n" -ForegroundColor Yellow
}

Write-Host "glide installed. Opening it now..." -ForegroundColor Green
Start-Process $exe
Start-Sleep -Seconds 2
