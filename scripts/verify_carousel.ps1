# 端到端验证轮播：真启动 exe，间隔取两张窗口截图
#   carousel > 0 → 两张应当不同（自动换了封面）
#   carousel = 0 → 两张应当一样（关闭轮播）
# 同时验证界面确实拿到了 prefs 里的档位（Rust 的 get_carousel 命令接线）
param([switch]$Kill, [int]$WaitSec = 7, [int]$ExtraSec = 5)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class C {
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@

$root = "C:\Myfiles\repo\nbs\CoverArt"
$exe = Join-Path $root "src-tauri\target\release\coverart.exe"
$prefs = Join-Path $env:APPDATA "com.coverart.desktop\prefs.json"
$shotDir = Join-Path $env:TEMP "coverart-verify"
New-Item -ItemType Directory -Force -Path $shotDir, (Split-Path $prefs) | Out-Null

$running = @(Get-Process coverart -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
  if (-not $Kill) {
    Write-Output ("已有 CoverArt 在运行（PID " + (($running | ForEach-Object { $_.Id }) -join ", ") + "），本次跳过。要强制请加 -Kill。")
    return
  }
  $running | Stop-Process -Force
  Start-Sleep -Seconds 1
}

function Grab($h, $file) {
  $r = New-Object C+RECT
  [void][C]::GetClientRect($h, [ref]$r)
  $bmp = New-Object System.Drawing.Bitmap ($r.Right - $r.Left), ($r.Bottom - $r.Top)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc(); [void][C]::PrintWindow($h, $hdc, 2); $g.ReleaseHdc($hdc); $g.Dispose()
  $bmp.Save($file, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
}

function DiffPct($a, $b) {
  $ia = [System.Drawing.Image]::FromFile($a); $ib = [System.Drawing.Image]::FromFile($b)
  $ba = New-Object System.Drawing.Bitmap $ia; $bb = New-Object System.Drawing.Bitmap $ib
  $d = 0.0; $n = 0
  for ($x = 0; $x -lt $ba.Width; $x += 8) {
    for ($y = 0; $y -lt $ba.Height; $y += 8) {
      $ca = $ba.GetPixel($x, $y); $cb = $bb.GetPixel($x, $y)
      $d += ([math]::Abs($ca.R - $cb.R) + [math]::Abs($ca.G - $cb.G) + [math]::Abs($ca.B - $cb.B)) / 3.0
      $n++
    }
  }
  $ba.Dispose(); $bb.Dispose(); $ia.Dispose(); $ib.Dispose()
  return [math]::Round(100.0 * $d / $n / 255.0, 1)
}

foreach ($sec in @(5, 0)) {
  Set-Content -Path $prefs -Value "{`n  `"scale`": 100,`n  `"alwaysOnTop`": false,`n  `"carousel`": $sec`n}`n" -Encoding UTF8
  Get-Process coverart -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Seconds 1
  $p = Start-Process -FilePath $exe -PassThru
  Start-Sleep -Seconds 5
  $p.Refresh()
  $h = $p.MainWindowHandle
  [void][C]::SetForegroundWindow($h)
  Start-Sleep -Seconds $WaitSec
  $a = Join-Path $shotDir "carousel-$sec-a.png"
  $b = Join-Path $shotDir "carousel-$sec-b.png"
  Grab $h $a
  Start-Sleep -Seconds $ExtraSec
  Grab $h $b
  $diff = DiffPct $a $b
  $expect = if ($sec -gt 0) { "应当不同（自动轮播）" } else { "应当一样（已关闭）" }
  Write-Output ("carousel=$sec → 两次截图差异 " + $diff + "%   " + $expect)
  Stop-Process -Id $p.Id -Force
  Start-Sleep -Seconds 1
}

Set-Content -Path $prefs -Value "{`n  `"scale`": 100,`n  `"alwaysOnTop`": false,`n  `"carousel`": 8`n}`n" -Encoding UTF8
Write-Output "prefs 已恢复成默认（轮播 8 秒）"
