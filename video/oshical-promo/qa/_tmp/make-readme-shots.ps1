param(
  [string]$Src = "D:\work\guanrenchi\video\oshical-promo\assets",
  [string]$Dst = "D:\work\guanrenchi\docs\screenshots"
)

# Generates README screenshots from the current-version UI captures in $Src.
# Settings page: both server-address occurrences are painted out (repo rule:
# docs/11-github-release-and-updater-plan.md:349 must not publish internal addresses).

Add-Type -AssemblyName System.Drawing
$cs = @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
public class Cv {
  public static int[] Bbox(Bitmap b) {
    int w = b.Width, h = b.Height;
    BitmapData d = b.LockBits(new Rectangle(0,0,w,h), ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
    byte[] buf = new byte[d.Stride * h];
    Marshal.Copy(d.Scan0, buf, 0, buf.Length);
    b.UnlockBits(d);
    int minX = w, minY = h, maxX = -1, maxY = -1;
    for (int y = 0; y < h; y++) {
      int row = y * d.Stride;
      for (int x = 0; x < w; x++) {
        int i = row + x * 4;
        if (buf[i] != 0 || buf[i+1] != 0 || buf[i+2] != 0) {
          if (x < minX) minX = x;
          if (x > maxX) maxX = x;
          if (y < minY) minY = y;
          if (y > maxY) maxY = y;
        }
      }
    }
    if (maxX < 0) return new int[] {0,0,w,h};
    return new int[] { minX, minY, maxX - minX + 1, maxY - minY + 1 };
  }
}
'@
Add-Type -TypeDefinition $cs -ReferencedAssemblies System.Drawing

New-Item -ItemType Directory -Force -Path $Dst | Out-Null

function Crop-Save([string]$inPath, [string]$outPath) {
  $srcBmp = [System.Drawing.Bitmap]::FromFile($inPath)
  $bb = [Cv]::Bbox($srcBmp)
  $rect = New-Object System.Drawing.Rectangle($bb[0], $bb[1], $bb[2], $bb[3])
  $crop = $srcBmp.Clone($rect, $srcBmp.PixelFormat)
  $crop.Save($outPath, [System.Drawing.Imaging.ImageFormat]::Png)
  "$([System.IO.Path]::GetFileName($outPath)) <- $([System.IO.Path]::GetFileName($inPath)) bbox=$($bb[0]),$($bb[1]) $($bb[2])x$($bb[3])"
  $crop.Dispose()
  $srcBmp.Dispose()
}

Crop-Save ($Src + "\ui-timeline.png") ($Dst + "\schedule.png")
Crop-Save ($Src + "\ui-grid.png")     ($Dst + "\calendar.png")
Crop-Save ($Src + "\ui-library.png")  ($Dst + "\creator-library.png")

# settings page
$inPath = $Src + "\ui-settings.png"
$outPath = $Dst + "\settings-sync.png"
"in=$inPath"
$bmp = [System.Drawing.Bitmap]::FromFile($inPath)
"loaded " + $bmp.Width + "x" + $bmp.Height

$gr = [System.Drawing.Graphics]::FromImage($bmp)
# PageUnit must be Pixel, otherwise DPI scaling shifts every fill
$gr.PageUnit = [System.Drawing.GraphicsUnit]::Pixel
$gr.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::None

# 1) server address input: paint white including caret; inner edge x 282..942, bottom border y=388
$br1 = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(255, 255, 255, 255))
$gr.FillRectangle($br1, 282, 350, 660, 36)
$br1.Dispose()

# 2) status card address line: text occupies x 56..158 / y 1011..1019; cloud icon x 178..192 stays
$br2 = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(255, 228, 248, 239))
$gr.FillRectangle($br2, 50, 1009, 118, 14)
$br2.Dispose()

$gr.Dispose()
$bmp.Save($outPath, [System.Drawing.Imaging.ImageFormat]::Png)
"settings-sync.png <- ui-settings.png (addresses painted out) " + $bmp.Width + "x" + $bmp.Height
$bmp.Dispose()

Get-ChildItem $Dst -Filter *.png | ForEach-Object {
  $im = [System.Drawing.Image]::FromFile($_.FullName)
  "$($_.Name) $($im.Width)x$($im.Height) $([int]($_.Length/1024))KB"
  $im.Dispose()
}
