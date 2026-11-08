Add-Type -AssemblyName System.Drawing
$sig = @'
using System;
using System.Runtime.InteropServices;
public class UI {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint nFlags);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hWnd, ref POINT p);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int n);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  public struct RECT { public int Left, Top, Right, Bottom; }
  public struct POINT { public int X, Y; }
}
'@
Add-Type -TypeDefinition $sig

function Get-AppWindow {
  $p = Get-Process oshical | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
  return $p.MainWindowHandle
}

function Save-Window([string]$Path) {
  $h = Get-AppWindow
  $r = New-Object UI+RECT
  [void][UI]::GetWindowRect($h, [ref]$r)
  $w = $r.Right - $r.Left; $ht = $r.Bottom - $r.Top
  $bmp = New-Object System.Drawing.Bitmap($w, $ht)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc()
  [void][UI]::PrintWindow($h, $hdc, 2)
  $g.ReleaseHdc($hdc); $g.Dispose()
  New-Item -ItemType Directory -Force -Path (Split-Path $Path) | Out-Null
  $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  return @{ left = $r.Left; top = $r.Top; w = $w; h = $ht }
}

function Focus-App {
  $h = Get-AppWindow
  [void][UI]::ShowWindow($h, 9)     # SW_RESTORE
  [void][UI]::SetForegroundWindow($h)
  Start-Sleep -Milliseconds 250
  return $h
}

function Click-At([int]$ClientX, [int]$ClientY) {
  $h = Get-AppWindow
  $p = New-Object UI+POINT
  [void][UI]::ClientToScreen($h, [ref]$p)
  $sx = $p.X + $ClientX; $sy = $p.Y + $ClientY
  [void][UI]::SetCursorPos($sx, $sy)
  Start-Sleep -Milliseconds 90
  [UI]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 40
  [UI]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 260
  return "$sx,$sy"
}

function Wheel-Up([int]$Times = 12) {
  $h = Get-AppWindow
  $p = New-Object UI+POINT
  [void][UI]::ClientToScreen($h, [ref]$p)
  [void][UI]::SetCursorPos($p.X + 660, $p.Y + 400)
  Start-Sleep -Milliseconds 80
  for ($i = 0; $i -lt $Times; $i++) {
    [UI]::mouse_event(0x0800, 0, 0, 120, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 45
  }
  Start-Sleep -Milliseconds 300
}
