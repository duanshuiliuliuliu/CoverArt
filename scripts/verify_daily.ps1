# Verifies the daily-20 pipeline end to end (ASCII only, PS 5.1 safe):
#   1. wipe daily.json, launch the app  -> window shows the frosted loading
#      layer first, then a cover; daily.json is written with today's 20 albums
#   2. launch it again                  -> daily.json is NOT rewritten
#      (i.e. "already fetched today" goes straight to the carousel)
#   3. ids of the newest day must not appear in the previous days
# NOTE: it stops any running coverart.exe and deletes daily.json first —
#       that is the point (we need the cold "nothing fetched yet" path).
param([switch]$KeepShots)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class W {
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@

$root = "C:\Myfiles\repo\nbs\CoverArt"
$exe = Join-Path $root "src-tauri\target\release\coverart.exe"
$daily = Join-Path $env:APPDATA "com.coverart.desktop\daily.json"
$shots = Join-Path $env:TEMP "coverart-daily-verify"
New-Item -ItemType Directory -Force -Path $shots | Out-Null

function Grab($h, $file) {
  $r = New-Object W+RECT
  [void][W]::GetClientRect($h, [ref]$r)
  if ($r.Right -le 0) { return $false }
  $bmp = New-Object System.Drawing.Bitmap ($r.Right - $r.Left), ($r.Bottom - $r.Top)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc(); [void][W]::PrintWindow($h, $hdc, 2); $g.ReleaseHdc($hdc); $g.Dispose()
  $bmp.Save($file, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
  return $true
}

Get-Process coverart -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 1
Remove-Item -LiteralPath $daily -ErrorAction SilentlyContinue
Write-Output ("[1] daily.json wiped. exists = " + (Test-Path $daily))

$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Milliseconds 1800
$p.Refresh()
$loading = Join-Path $shots "phase1-loading.png"
if (Grab $p.MainWindowHandle $loading) { Write-Output ("    loading-phase shot: " + $loading) }
Start-Sleep -Seconds 12
$p.Refresh()
$cover = Join-Path $shots "phase2-cover.png"
if (Grab $p.MainWindowHandle $cover) { Write-Output ("    cover-phase shot:   " + $cover) }

if (-not (Test-Path $daily)) { Write-Output "    FAIL: daily.json was not created"; exit 1 }
$json = Get-Content -LiteralPath $daily -Raw -Encoding UTF8 | ConvertFrom-Json
$today = $json.days[-1]
$stamp1 = (Get-Item $daily).LastWriteTime
Write-Output ("    days = " + $json.days.Count + "  newest = " + $today.date + "  albums = " + $today.albums.Count + "  written at " + $stamp1.ToString("HH:mm:ss"))
$today.albums | Select-Object -First 3 | ForEach-Object { Write-Output ("      " + $_.artist + " / " + $_.title + "  (" + $_.date + ")") }
if ($today.albums.Count -ne 20) { Write-Output "    FAIL: expected 20 albums"; exit 1 }

Start-Sleep -Seconds 2
Stop-Process -Id $p.Id -Force
Start-Sleep -Seconds 2
Write-Output "[2] starting again (today is already fetched)"
$p2 = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 8
$stamp2 = (Get-Item $daily).LastWriteTime
Write-Output ("    mtime before/after = " + $stamp1.ToString("HH:mm:ss") + " / " + $stamp2.ToString("HH:mm:ss"))
if ($stamp1 -ne $stamp2) { Write-Output "    FAIL: daily.json was rewritten, cache was not reused"; exit 1 }
Write-Output "    OK: reused the local copy (no network round trip)"

Write-Output "[3] duplicate check against the previous 60 days"
$hist = @{}
$dups = 0
foreach ($day in $json.days) {
  foreach ($a in $day.albums) {
    $k = $a.id
    if ($day.date -eq $today.date) {
      if ($hist.ContainsKey($k)) { $dups++ }
    } else {
      $hist[$k] = 1
    }
  }
}
Write-Output ("    previous-day ids = " + $hist.Count + "  repeats in today = " + $dups)
if ($dups -ne 0) { Write-Output "    FAIL: today repeats an album from the last 60 days"; exit 1 }

Stop-Process -Id $p2.Id -Force
if (-not $KeepShots) { Remove-Item -LiteralPath $shots -Recurse -Force -ErrorAction SilentlyContinue }
Write-Output "ALL OK"
