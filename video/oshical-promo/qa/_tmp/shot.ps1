param(
  [string]$Dir = "D:\work\guanrenchi\video\oshical-promo\qa\_tmp\shots",
  [string]$Steps = "library,fan",
  [int]$W = 1684,
  [int]$H = 1052
)

Add-Type -AssemblyName System.Drawing
$sig = @'
using System;
using System.Runtime.InteropServices;
public class N2 {
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, IntPtr e);
  public struct RECT { public int Left, Top, Right, Bottom; }
}
'@
Add-Type -TypeDefinition $sig
[void][N2]::SetThreadDpiAwarenessContext([IntPtr](-4))

$p = Get-Process oshical -ErrorAction Stop | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
$hwnd = $p.MainWindowHandle

function Get-Rect { $r = New-Object N2+RECT; [void][N2]::GetWindowRect($hwnd, [ref]$r); return $r }

function Save-Shot([string]$name) {
  Start-Sleep -Milliseconds 500
  $r = Get-Rect
  $w = $r.Right - $r.Left; $ht = $r.Bottom - $r.Top
  $bmp = New-Object System.Drawing.Bitmap($w, $ht)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc(); [void][N2]::PrintWindow($hwnd, $hdc, 2); $g.ReleaseHdc($hdc); $g.Dispose()
  New-Item -ItemType Directory -Force -Path $Dir | Out-Null
  $path = Join-Path $Dir "$name.png"
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
  "$name -> ${w}x${ht}"
}

function Click-At([int]$imgX, [int]$imgY) {
  $r = Get-Rect
  $cw = $r.Right - $r.Left; $ch = $r.Bottom - $r.Top
  $x = $r.Left + [int]($imgX * $cw / $W)
  $y = $r.Top + [int]($imgY * $ch / $H)
  [void][N2]::SetForegroundWindow($hwnd)
  [void][N2]::SetCursorPos($x, $y)
  Start-Sleep -Milliseconds 90
  [void][N2]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 60
  [void][N2]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 550
  "click ($imgX,$imgY) -> ($x,$y)"
}

$r = Get-Rect
if (($r.Right - $r.Left) -ne $W -or ($r.Bottom - $r.Top) -ne $H) {
  [void][N2]::SetWindowPos($hwnd, [IntPtr]::Zero, $r.Left, $r.Top, $W, $H, 0x0000)
  Start-Sleep -Milliseconds 700
  $r = Get-Rect
}
"window $($r.Right - $r.Left)x$($r.Bottom - $r.Top) at $($r.Left),$($r.Top)"

foreach ($s in $Steps.Split(',')) {
  switch ($s.Trim()) {
    'library'  { Click-At 97 205; Save-Shot 'library' }
    'fan'      { Click-At 1276 262; Save-Shot 'fan' }
    'settings' { Click-At 97 262; Save-Shot 'settings' }
    'schedule' { Click-At 97 165; Start-Sleep -Milliseconds 300; Save-Shot 'schedule' }
    'timeline' { Click-At 1240 58; Save-Shot 'timeline' }
    'grid'     { Click-At 1268 58; Save-Shot 'grid' }
    'close-dialog' { Click-At 1227 205; Save-Shot 'after-close' }
    default { "skip $s" }
  }
}
