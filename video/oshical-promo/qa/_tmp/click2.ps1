param(
  [int]$X,
  [int]$Y,
  [string]$Out = "",
  [int]$Delay = 800
)

Add-Type -AssemblyName System.Drawing
$sig = @'
using System;
using System.Runtime.InteropServices;
public class K5 {
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, IntPtr pid);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool f);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint f);
  public struct RECT { public int Left, Top, Right, Bottom; }
  public struct POINT { public int X, Y; }
}
'@
Add-Type -TypeDefinition $sig
[void][K5]::SetThreadDpiAwarenessContext([IntPtr](-4))

$proc = Get-Process oshical -ErrorAction Stop | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
$hwnd = $proc.MainWindowHandle
$r = New-Object K5+RECT; [void][K5]::GetWindowRect($hwnd, [ref]$r)

# 强制激活：把当前前台线程的输入队列附加到本线程
$fg = [K5]::GetForegroundWindow()
$fgThread = [K5]::GetWindowThreadProcessId($fg, [IntPtr]::Zero)
$me = [K5]::GetWindowThreadProcessId($hwnd, [IntPtr]::Zero)
[void][K5]::AttachThreadInput($me, $fgThread, $true)
[void][K5]::ShowWindow($hwnd, 9)
[void][K5]::BringWindowToTop($hwnd)
[void][K5]::SetForegroundWindow($hwnd)
[void][K5]::AttachThreadInput($me, $fgThread, $false)
Start-Sleep -Milliseconds 400

$now = [K5]::GetForegroundWindow()
if ($now -ne $hwnd) { "ABORT: activation failed fg=$now want=$hwnd"; exit 2 }

$sx = $r.Left + $X; $sy = $r.Top + $Y
$pt = New-Object K5+POINT; $pt.X = $sx; $pt.Y = $sy
$child = [K5]::WindowFromPoint($pt)
$root = [K5]::GetAncestor($child, 2)
if ($root -ne $hwnd) { "ABORT: window at ($sx,$sy) is $root not $hwnd"; exit 3 }

[void][K5]::SetCursorPos($sx, $sy)
Start-Sleep -Milliseconds 150
[void][K5]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 90
[void][K5]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
"click ($sx,$sy) ok"
Start-Sleep -Milliseconds $Delay

if ($Out) {
  $w = $r.Right - $r.Left; $ht = $r.Bottom - $r.Top
  $bmp = New-Object System.Drawing.Bitmap($w, $ht)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc(); [void][K5]::PrintWindow($hwnd, $hdc, 2); $g.ReleaseHdc($hdc); $g.Dispose()
  New-Item -ItemType Directory -Force -Path (Split-Path $Out) | Out-Null
  $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
  "shot -> $Out"
}
