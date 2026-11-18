param([string]$Out = "D:\work\guanrenchi\video\oshical-promo\qa\_tmp\uia.txt")

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

$p = Get-Process oshical -ErrorAction Stop | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
$h = $p.MainWindowHandle
$root = [System.Windows.Automation.AutomationElement]::FromHandle($h)

$all = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
$lines = New-Object System.Collections.Generic.List[string]
foreach ($e in $all) {
  $n = $e.Current.Name
  $ct = $e.Current.ControlType.ProgrammaticName
  if ([string]::IsNullOrWhiteSpace($n) -and $ct -notmatch "Button|Edit|Tab") { continue }
  $r = $e.Current.BoundingRectangle
  $lines.Add(("{0}`t{1}`t{2}x{3}@{4},{5}" -f $ct, $n, [int]$r.Width, [int]$r.Height, [int]$r.X, [int]$r.Y))
}
New-Item -ItemType Directory -Force -Path (Split-Path $Out) | Out-Null
Set-Content -Path $Out -Value $lines -Encoding UTF8
"count=$($all.Count) file=$Out"
