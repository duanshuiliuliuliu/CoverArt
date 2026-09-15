# 验证单实例：连开两个进程，第二个应当立刻退出，第一个继续运行
$exe = "C:\Myfiles\repo\nbs\CoverArt\src-tauri\target\release\coverart.exe"

Get-Process coverart -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 1
Write-Output ("清场后残留进程: " + @(Get-Process coverart -ErrorAction SilentlyContinue).Count)

Write-Output "启动第 1 个实例…"
$p1 = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 6
$p1.Refresh()
Write-Output ("  第 1 个实例 PID=" + $p1.Id + " 存活=" + (-not $p1.HasExited))

Write-Output "启动第 2 个实例…"
$p2 = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 6
$p2.Refresh()
Write-Output ("  第 2 个实例 PID=" + $p2.Id + " 存活=" + (-not $p2.HasExited) + "  ← 期望：已退出")

$running = @(Get-Process coverart -ErrorAction SilentlyContinue)
Write-Output ("当前 coverart 进程数: " + $running.Count + "  ← 期望：1")
Write-Output ("剩下的是不是第 1 个实例: " + (($running | ForEach-Object { $_.Id }) -contains $p1.Id))

$alive = $running | Select-Object -First 1
if ($alive) {
  Start-Sleep -Milliseconds 500
  $w = Get-Process -Id $alive.Id
  Write-Output ("  窗口标题: " + $w.MainWindowTitle + "   窗口可见: " + ($w.MainWindowHandle -ne 0))
  Stop-Process -Id $alive.Id -Force
  Write-Output "已关闭"
}
