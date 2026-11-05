# Installs or updates Kadr for the current user: copies the release build to
# %LOCALAPPDATA%\Programs\Kadr, turns on autostart and starts the app.
#   cargo build --release
#   powershell -ExecutionPolicy Bypass -File install.ps1
$ErrorActionPreference = 'Stop'

$source = Join-Path $PSScriptRoot 'target\release\kadr.exe'
if (-not (Test-Path $source)) { throw "Build first: cargo build --release" }

$dir = Join-Path $env:LOCALAPPDATA 'Programs\Kadr'
$exe = Join-Path $dir 'kadr.exe'

Get-Process kadr -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $source $exe -Force

Set-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'Kadr' -Value "`"$exe`""
Start-Process $exe
"Kadr installed: $exe"
