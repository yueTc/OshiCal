Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

$sig = @'
using System;
using System.Runtime.InteropServices;
public class Win {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hwnd, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hwnd, ref POINT p);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern IntPtr ChildWindowFromPointEx(IntPtr parent, POINT p, uint flags);
  [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr parent, EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  public delegate bool EnumProc(IntPtr hwnd, IntPtr lparam);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
'@
Add-Type -TypeDefinition $sig
[Win]::SetProcessDPIAware() | Out-Null

$p = Get-Process oshical -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if ($null -eq $p) { "no window"; exit 1 }
$hwnd = $p.MainWindowHandle
"hwnd=" + $hwnd + " pid=" + $p.Id

$r = New-Object Win+RECT
[Win]::GetWindowRect($hwnd, [ref]$r) | Out-Null
"window rect = " + $r.Left + "," + $r.Top + " " + ($r.Right - $r.Left) + "x" + ($r.Bottom - $r.Top)

$c = New-Object Win+RECT
[Win]::GetClientRect($hwnd, [ref]$c) | Out-Null
$origin = New-Object Win+POINT
[Win]::ClientToScreen($hwnd, [ref]$origin) | Out-Null
"client = " + $c.Right + "x" + $c.Bottom + " at screen " + $origin.X + "," + $origin.Y

$w = $r.Right - $r.Left
$h = $r.Bottom - $r.Top
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
[Win]::PrintWindow($hwnd, $hdc, 2) | Out-Null
$g.ReleaseHdc($hdc)
$g.Dispose()
$out = "D:\work\guanrenchi\video\oshical-promo\qa\_tmp\shot-now.png"
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
"printed " + $w + "x" + $h + " -> " + $out
$bmp.Dispose()

"children:"
$cb = [Win+EnumProc]{
  param($ch, $lp)
  $cr = New-Object Win+RECT
  [Win]::GetClientRect($ch, [ref]$cr) | Out-Null
  "  child hwnd=" + $ch + " " + $cr.Right + "x" + $cr.Bottom
  return $true
}
[Win]::EnumChildWindows($hwnd, $cb, [IntPtr]::Zero) | Out-Null
