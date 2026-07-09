# LightTrack installer (Windows). Usage:
#   irm https://raw.githubusercontent.com/xkazm04/lighttrack/main/deploy/install.ps1 | iex
# Override the install dir with $env:LIGHTTRACK_BIN_DIR.
$ErrorActionPreference = 'Stop'

$repo = 'xkazm04/lighttrack'
$binDir = if ($env:LIGHTTRACK_BIN_DIR) { $env:LIGHTTRACK_BIN_DIR } else { "$env:LOCALAPPDATA\Programs\lighttrack" }
$target = 'x86_64-pc-windows-msvc'
$url = "https://github.com/$repo/releases/latest/download/lighttrack-$target.zip"

Write-Host "downloading $url"
New-Item -ItemType Directory -Force $binDir | Out-Null
$zip = Join-Path $env:TEMP 'lighttrack.zip'
Invoke-WebRequest -Uri $url -OutFile $zip
Expand-Archive -Force -Path $zip -DestinationPath $binDir
Remove-Item $zip

Write-Host "installed lt, lt-runner, lt-mcp, lighttrack-api to $binDir"
if (($env:PATH -split ';') -notcontains $binDir) {
  Write-Host "add it to PATH:  setx PATH `"$binDir;$env:PATH`""
}
