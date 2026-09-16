# 收藏功能端到端：真启动 exe，用 SendInput 右键 → 点心形 → 看文件系统
#   1) 右键：心形应当出现在鼠标处
#   2) 点一下：收藏 → favorites.json 多一条 + covers\<id>.jpg 落盘
#   3) 再点一下：取消收藏 → 索引还原 + 本地图片被删掉（按需求第 5 条）
# 断言用"相对变化"，所以无论测试前有没有收藏都不会误判，也不会破坏已有收藏。
param([switch]$Kill)

$ErrorActionPreference = "Stop"
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class F {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, UIntPtr e);
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  public static void Move(int x, int y) {
    int sw = GetSystemMetrics(0), sh = GetSystemMetrics(1);
    mouse_event(0x8001, (int)Math.Round(x * 65535.0 / (sw - 1)), (int)Math.Round(y * 65535.0 / (sh - 1)), 0, UIntPtr.Zero);
  }
  public static void Click(int x, int y, bool right) {
    Move(x, y);
    System.Threading.Thread.Sleep(120);
    uint down = right ? 0x0008u : 0x0002u;
    uint up = right ? 0x0010u : 0x0004u;
    mouse_event(down, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(60);
    mouse_event(up, 0, 0, 0, UIntPtr.Zero);
  }
}
"@

$root = "C:\Myfiles\repo\nbs\CoverArt"
$exe = Join-Path $root "src-tauri\target\release\coverart.exe"
$data = Join-Path $env:APPDATA "com.coverart.desktop"
$index = Join-Path $data "favorites.json"
$covers = Join-Path $data "covers"
$prefs = Join-Path $data "prefs.json"

# 测试期间关掉轮播：否则每 8 秒换一张，第二次点心形收藏的就不是同一张专辑了
$prefsBackup = $null
if (Test-Path $prefs) { $prefsBackup = Get-Content $prefs -Raw -Encoding UTF8 }
New-Item -ItemType Directory -Force -Path $data | Out-Null
Set-Content -Path $prefs -Value "{`n  `"scale`": 100,`n  `"alwaysOnTop`": false,`n  `"carousel`": 0`n}`n" -Encoding UTF8

$running = @(Get-Process coverart -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
  if (-not $Kill) { Write-Output ("已有 CoverArt 在运行（PID " + (($running | ForEach-Object { $_.Id }) -join ", ") + "），本次跳过。要强制请加 -Kill。"); return }
  $running | Stop-Process -Force; Start-Sleep -Seconds 1
}

function State {
  $count = 0
  if (Test-Path $index) {
    try { $count = @((Get-Content $index -Raw -Encoding UTF8 | ConvertFrom-Json).items).Count } catch { $count = -1 }
  }
  $files = @()
  if (Test-Path $covers) { $files = @(Get-ChildItem $covers -Filter *.jpg -ErrorAction SilentlyContinue) }
  return [pscustomobject]@{
    条数 = $count
    图片数 = $files.Count
    图片总KB = [int](($files | Measure-Object Length -Sum).Sum / 1KB)
  }
}

# 时区：封面在窗口里，右键点正中间
$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 9
$p.Refresh()
$h = $p.MainWindowHandle
[void][F]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 600
$cr = New-Object F+RECT; [void][F]::GetClientRect($h, [ref]$cr)
$pt = New-Object F+POINT; $pt.X = 0; $pt.Y = 0
[void][F]::ClientToScreen($h, [ref]$pt)
$cx = $pt.X + [int](($cr.Right - $cr.Left) / 2)
$cy = $pt.Y + [int](($cr.Bottom - $cr.Top) / 2)

$before = State
Write-Output ("测试前：" + ($before | ConvertTo-Json -Compress))

[F]::Click($cx, $cy, $true)         # 右键 → 心形出现在这里
Start-Sleep -Milliseconds 500
Write-Output ("右键后：心形应当出现在鼠标处（" + $cx + "," + $cy + "）")

[F]::Click($cx, $cy, $false)        # 点心形 → 收藏
Start-Sleep -Seconds 3              # 等下载封面 + 写索引
$after1 = State
Write-Output ("点一下之后：" + ($after1 | ConvertTo-Json -Compress) + "   ← 期望：条数/图片数比测试前多 1")

# 第二次点击要趁心形还在（它是"静置 2.6 秒"才收走）；
# 下载封面可能占用几秒，所以先等索引写出来，再在心形还在时补一次右键、然后点击。
Start-Sleep -Seconds 3
[F]::Click($cx, $cy, $true)         # 再右键一次：把心形叫回来（幂等，不影响收藏状态）
Start-Sleep -Milliseconds 400
[F]::Click($cx, $cy, $false)        # 点心形 → 取消收藏
Start-Sleep -Seconds 2
$after2 = State
Write-Output ("再点一下之后：" + ($after2 | ConvertTo-Json -Compress) + "   ← 期望：回到测试前，且本地图片被删掉")

$ok1 = ($after1.条数 -ne $before.条数) -or ($after1.图片数 -ne $before.图片数)
$ok2 = ($after2.条数 -eq $before.条数) -and ($after2.图片数 -eq $before.图片数)
Write-Output ("结论：收藏生效=" + $ok1 + "   取消收藏还原（含删除本地图片）=" + $ok2)
Stop-Process -Id $p.Id -Force

# 还原偏好：测试期间把轮播关掉了，这里恢复成默认 8 秒
Set-Content -Path $prefs -Value "{`n  `"scale`": 100,`n  `"alwaysOnTop`": false,`n  `"carousel`": 8`n}`n" -Encoding UTF8
Write-Output "prefs 已恢复（轮播 8 秒）"
