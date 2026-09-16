# 启动构建出来的 CoverArt.exe，核对：
#   1) 窗口尺寸/不可缩放/无标题栏   2) 托盘图标是否创建   3) 内容是否真的渲染出来
# 可选：-Scale 125 先写入 prefs.json 再启动，验证缩放档位
param([int]$Scale = 0, [switch]$Keep, [switch]$Kill)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class W {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out RECT r, int size);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }

  public static List<string> ClassesOf(uint pid) {
    var list = new List<string>();
    EnumWindows((h, p) => {
      uint wpid; GetWindowThreadProcessId(h, out wpid);
      if (wpid == pid) {
        var sb = new StringBuilder(256);
        GetClassName(h, sb, sb.Capacity);
        list.Add(sb.ToString());
      }
      return true;
    }, IntPtr.Zero);
    return list;
  }
}
"@

$root = "C:\Myfiles\repo\nbs\CoverArt"
$exe = Join-Path $root "src-tauri\target\release\coverart.exe"
$prefs = Join-Path $env:APPDATA "com.coverart.desktop\prefs.json"

# 单实例机制下，已有实例在跑时新进程会直接退出——默认不去动别人的进程
$running = @(Get-Process coverart -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
  if (-not $Kill) {
    Write-Output ("已有 CoverArt 在运行（PID " + (($running | ForEach-Object { $_.Id }) -join ", ") + "），单实例下新进程会直接退出，本次跳过。要强制验证请加 -Kill。")
    return
  }
  $running | Stop-Process -Force
  Start-Sleep -Seconds 1
}

if ($Scale -gt 0) {
  New-Item -ItemType Directory -Force -Path (Split-Path $prefs) | Out-Null
  Set-Content -Path $prefs -Value "{`n  `"scale`": $Scale`n}`n" -Encoding UTF8
  Write-Output ("已写入 prefs: scale=" + $Scale + "  (" + $prefs + ")")
}

$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 8
$p.Refresh()
Write-Output ("进程存活: " + (-not $p.HasExited) + "  窗口标题: " + $p.MainWindowTitle)

$h = $p.MainWindowHandle
if ($h -ne [IntPtr]::Zero) {
  $wr = New-Object W+RECT; [void][W]::GetWindowRect($h, [ref]$wr)
  $cr = New-Object W+RECT; [void][W]::GetClientRect($h, [ref]$cr)
  $dwm = New-Object W+RECT
  $dwmOk = [W]::DwmGetWindowAttribute($h, 9, [ref]$dwm, 16)
  $style = [W]::GetWindowLong($h, -16)
  $dpi = [W]::GetDpiForWindow($h)
  Write-Output ("客户区: " + ($cr.Right - $cr.Left) + "x" + ($cr.Bottom - $cr.Top) + "   外框: " + ($wr.Right - $wr.Left) + "x" + ($wr.Bottom - $wr.Top) + "   DPI: " + $dpi)
  if ($dwmOk -eq 0) { Write-Output ("DWM 可见边框: " + ($dwm.Right - $dwm.Left) + "x" + ($dwm.Bottom - $dwm.Top)) }
  Write-Output ("可拖边缩放(WS_THICKFRAME): " + [bool]($style -band 0x00040000) + "   可最大化(WS_MAXIMIZEBOX): " + [bool]($style -band 0x00010000))

  # 托盘图标：tray-icon crate 会建一个隐藏的消息窗口
  $classes = [W]::ClassesOf([uint32]$p.Id)
  $tray = $classes | Where-Object { $_ -like '*TrayIcon*' -or $_ -like '*tray*' }
  Write-Output ("进程窗口类: " + (($classes | Sort-Object -Unique) -join ", "))
  Write-Output ("托盘消息窗口: " + $(if ($tray) { "有 (" + ($tray -join ",") + ")" } else { "没找到" }))

  $shotDir = Join-Path $env:TEMP "coverart-verify"
  New-Item -ItemType Directory -Force -Path $shotDir | Out-Null
  $out = Join-Path $shotDir ("coverart-app" + $(if ($Scale -gt 0) { "-$Scale" } else { "" }) + ".png")
  [void][W]::SetForegroundWindow($h)
  $w = $wr.Right - $wr.Left; $ht = $wr.Bottom - $wr.Top
  # 启动瞬间偶尔会抓到还没绘制完成的白帧，颜色太少就等一会儿重拍
  for ($try = 1; $try -le 3; $try++) {
    Start-Sleep -Milliseconds 1200
    $bmp = New-Object System.Drawing.Bitmap $w, $ht
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    [void][W]::PrintWindow($h, $hdc, 2)
    $g.ReleaseHdc($hdc); $g.Dispose()
    $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
    $colors = @{}; $sum = 0.0; $n = 0
    for ($x = 0; $x -lt $bmp.Width; $x += 8) {
      for ($y = 0; $y -lt $bmp.Height; $y += 8) {
        $c = $bmp.GetPixel($x, $y); $sum += ($c.R + $c.G + $c.B) / 3; $n++
        $colors[("" + $c.R + "," + $c.G + "," + $c.B)] = 1
      }
    }
    $bmp.Dispose()
    Write-Output ("截图(第 " + $try + " 次): " + $out + "  平均亮度=" + [math]::Round($sum / $n, 1) + "  采样色数=" + $colors.Count)
    if ($colors.Count -ge 1000) { break }
  }
} else {
  Write-Output "没拿到主窗口句柄"
}

if (-not $Keep) { Stop-Process -Id $p.Id -Force; Write-Output "已关闭进程" }
