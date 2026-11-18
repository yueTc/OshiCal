param(
  [int]$X,
  [int]$Y,
  [string]$Out = "",
  [int]$Delay = 700,
  [switch]$NoClick
)

Add-Type -AssemblyName System.Drawing
$sig = @'
using System;
using System.Runtime.InteropServices;
public class C3 {
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  public struct RECT { public int Left, Top, Right, Bottom; }
}
'@
Add-Type -TypeDefinition $sig
[void][C3]::SetThreadDpiAwarenessContext([IntPtr](-4))

$p = Get-Process oshical -ErrorAction Stop | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
$hwnd = $p.MainWindowHandle
$r = New-Object C3+RECT; [void][C3]::GetWindowRect($hwnd, [ref]$r)
"sized=$($r.Right - $r.Left)x$($r.Bottom - $r.Top) at $($r.Left),$($r.Top)"

if (-not $NoClick) {
  [void][C3]::BringWindowToTop($hwnd)
  [void][C3]::SetForegroundWindow($hwnd)
  Start-Sleep -Milliseconds 250
  $fg = [C3]::GetForegroundWindow()
  $sx = $r.Left + $X; $sy = $r.Top + $Y
  [void][C3]::SetCursorPos($sx, $sy)
  Start-Sleep -Milliseconds 120
  [void][C3]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 80
  [void][C3]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
  "click window($X,$Y) -> screen($sx,$sy) foregroundMatched=$($fg -eq $hwnd)"
  Start-Sleep -Milliseconds $Delay
}

if ($Out) {
  $w = $r.Right - $r.Left; $ht = $r.Bottom - $r.Top
  $bmp = New-Object System.Drawing.Bitmap($w, $ht)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc(); [void][C3]::PrintWindow($hwnd, $hdc, 2); $g.ReleaseHdc($hdc); $g.Dispose()
  New-Item -ItemType Directory -Force -Path (Split-Path $Out) | Out-Null
  $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
  "shot -> $Out"
}
