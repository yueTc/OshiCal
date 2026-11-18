param(
  [int]$X = 0,
  [int]$Y = 0,
  [string]$Out = "D:\work\guanrenchi\video\oshical-promo\qa\_tmp\shot-now.png"
)
Add-Type -AssemblyName System.Drawing

$sig = @'
using System;
using System.Runtime.InteropServices;
public class Win2 {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT r);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
'@
Add-Type -TypeDefinition $sig
[Win2]::SetProcessDPIAware() | Out-Null

$p = Get-Process oshical -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if ($null -eq $p) { "no window"; exit 1 }
$hwnd = $p.MainWindowHandle

if ($X -ne 0 -or $Y -ne 0) {
  $lp = [IntPtr](($Y -shl 16) -bor ($X -band 0xFFFF))
  [Win2]::PostMessage($hwnd, 0x0200, [IntPtr]::Zero, $lp) | Out-Null
  Start-Sleep -Milliseconds 120
  [Win2]::PostMessage($hwnd, 0x0201, [IntPtr]1, $lp) | Out-Null
  Start-Sleep -Milliseconds 80
  [Win2]::PostMessage($hwnd, 0x0202, [IntPtr]::Zero, $lp) | Out-Null
  "posted click at " + $X + "," + $Y
  Start-Sleep -Milliseconds 900
}

$r = New-Object Win2+RECT
[Win2]::GetWindowRect($hwnd, [ref]$r) | Out-Null
$w = $r.Right - $r.Left
$h = $r.Bottom - $r.Top
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
[Win2]::PrintWindow($hwnd, $hdc, 2) | Out-Null
$g.ReleaseHdc($hdc)
$g.Dispose()
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
"printed " + $w + "x" + $h + " -> " + $Out
$bmp.Dispose()
