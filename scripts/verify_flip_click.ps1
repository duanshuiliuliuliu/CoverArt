param([switch]$Kill)

# 复现并验证「翻面后点返回箭头，窗口被关掉」这个 bug：
#   1) 截图 A（正面）  2) 按空格翻到背面 → 截图 B
#   3) 点背面左上角的返回箭头 → 窗口应当仍在（修复前：正面那颗 ✕ 正好压在这个位置，会被点到 → 收起）
#   4) 截图 C 应当回到正面（和 A 相近）
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class I {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, UIntPtr e);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint f, UIntPtr e);
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  public static void ClickAt(int x, int y) {
    int sw = GetSystemMetrics(0), sh = GetSystemMetrics(1);
    int ax = (int)Math.Round(x * 65535.0 / (sw - 1));
    int ay = (int)Math.Round(y * 65535.0 / (sh - 1));
    mouse_event(0x8001, ax, ay, 0, UIntPtr.Zero);                 // MOVE | ABSOLUTE
    System.Threading.Thread.Sleep(120);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);                    // LEFTDOWN
    System.Threading.Thread.Sleep(60);
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);                    // LEFTUP
  }
  public static void Key(byte vk) {
    keybd_event(vk, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(50);
    keybd_event(vk, 0, 2, UIntPtr.Zero);                           // KEYUP
  }
}
"@

$root = "C:\Myfiles\repo\nbs\CoverArt"
$exe = Join-Path $root "src-tauri\target\release\coverart.exe"
$prefs = Join-Path $env:APPDATA "com.coverart.desktop\prefs.json"
$shotDir = Join-Path $env:TEMP "coverart-verify"
New-Item -ItemType Directory -Force -Path $shotDir | Out-Null

# 固定 100% 缩放，坐标才可预期
New-Item -ItemType Directory -Force -Path (Split-Path $prefs) | Out-Null
Set-Content -Path $prefs -Value "{`n  `"scale`": 100`n}`n" -Encoding UTF8

$running = @(Get-Process coverart -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
  if (-not $Kill) {
    Write-Output ("已有 CoverArt 在运行（PID " + (($running | ForEach-Object { $_.Id }) -join ", ") + "），单实例下新进程会直接退出，本次跳过。要强制验证请加 -Kill。")
    return
  }
  $running | Stop-Process -Force
  Start-Sleep -Seconds 1
}
$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 8
$p.Refresh()
$h = $p.MainWindowHandle
Write-Output ("窗口句柄: " + $h + "  可见: " + [I]::IsWindowVisible($h))

function Shot($name) {
  $cr = New-Object I+RECT; [void][I]::GetClientRect($h, [ref]$cr)
  $w = $cr.Right - $cr.Left; $ht = $cr.Bottom - $cr.Top
  $bmp = New-Object System.Drawing.Bitmap $w, $ht
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc(); [void][I]::PrintWindow($h, $hdc, 2); $g.ReleaseHdc($hdc); $g.Dispose()
  $file = Join-Path $shotDir ($name + ".png")
  $bmp.Save($file, [System.Drawing.Imaging.ImageFormat]::Png)
  $sum = 0.0; $n = 0
  for ($x = 0; $x -lt $bmp.Width; $x += 8) {
    for ($y = 0; $y -lt $bmp.Height; $y += 8) {
      $c = $bmp.GetPixel($x, $y); $sum += ($c.R + $c.G + $c.B) / 3; $n++
    }
  }
  $avg = [math]::Round($sum / $n, 1)
  $bmp.Dispose()
  Write-Output ("  截图 " + $name + ": 平均亮度=" + $avg + "  " + $file)
  return @{ File = $file; Avg = $avg }
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

[void][I]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 800
$A = Shot "flip-A-front"

Write-Output "按空格翻到背面…"
[I]::Key(0x20)
Start-Sleep -Milliseconds 1500
$B = Shot "flip-B-back"
Write-Output ("  正面 vs 背面 差异: " + (DiffPct $A.File $B.File) + "%  ← 期望明显不同")

# 背面左上角「返回箭头」的中心（CSS 约 34,23；DPI 125% → ×1.25）
$cr = New-Object I+RECT; [void][I]::GetClientRect($h, [ref]$cr)
$pt = New-Object I+POINT
$pt.X = 0; $pt.Y = 0
[void][I]::ClientToScreen($h, [ref]$pt)
$dpi = 120
$cx = $pt.X + [int](34 * $dpi / 96.0)
$cy = $pt.Y + [int](23 * $dpi / 96.0)
Write-Output ("点击背面返回箭头 @屏幕坐标 (" + $cx + "," + $cy + ")   客户区 " + ($cr.Right - $cr.Left) + "x" + ($cr.Bottom - $cr.Top))
[void][I]::SetForegroundWindow($h)
[I]::ClickAt($cx, $cy)
Start-Sleep -Milliseconds 1500

$visible = [I]::IsWindowVisible($h)
Write-Output ("点击之后窗口是否还在: " + $visible + "   ← 期望 True（修复前会变成 False）")
if ($visible) {
  $C = Shot "flip-C-after"
  Write-Output ("  点后 vs 正面 差异: " + (DiffPct $A.File $C.File) + "%  (小 = 已翻回正面)")
  Write-Output ("  点后 vs 背面 差异: " + (DiffPct $B.File $C.File) + "%  (大 = 确实翻回来了)")
}

Stop-Process -Id $p.Id -Force
Write-Output "已关闭进程"
