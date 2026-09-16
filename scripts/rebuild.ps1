# 重新构建 exe。正在运行的实例会锁住 coverart.exe，导致构建报 "Access is denied"，
# 所以这里先把它关掉（应用没有未保存状态，重启后会自动回到原来那张封面）。
$running = @(Get-Process coverart -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
  Write-Output ("先关闭正在运行的 CoverArt（PID " + (($running | ForEach-Object { $_.Id }) -join ", ") + "）…")
  $running | Stop-Process -Force
  Start-Sleep -Seconds 1
}

Push-Location (Split-Path $PSScriptRoot -Parent)
try {
  npx tauri build --no-bundle
} finally {
  Pop-Location
}
