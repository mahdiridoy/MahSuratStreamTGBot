# MahSurat Stream Bot installer (Windows PowerShell)
param([string]$InstallDir = "")
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}

$Repo = "__REPO__"
$Base = "https://github.com/$Repo/releases/download/bot-latest"
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\MovieBox-TgBot\bin" }
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$tmp = Join-Path $env:TEMP "moviebox-tgbot.zip"
Write-Host "  > Downloading bot"
Invoke-WebRequest "$Base/MovieBoxBot_Windows_x64.zip" -OutFile $tmp
Expand-Archive -Path $tmp -DestinationPath $InstallDir -Force
Remove-Item $tmp -Force

$cf = Join-Path $InstallDir "cloudflared.exe"
if (-not (Test-Path $cf)) {
    Write-Host "  > Downloading cloudflared (free tunnel for the in-Telegram player)"
    try { Invoke-WebRequest "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-windows-amd64.exe" -OutFile $cf }
    catch { Write-Host "  ! could not download cloudflared (player buttons will be disabled)" -ForegroundColor Yellow }
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ";") -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable("Path", ($userPath.TrimEnd(";") + ";" + $InstallDir), "User")
    $env:Path += ";$InstallDir"
}
Write-Host ""
Write-Host "  + Installed to $InstallDir" -ForegroundColor Green
Write-Host "  Start the bot with:  moviebox-tgbot   (first run asks for your bot token)"
