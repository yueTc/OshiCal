Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

function Find-ByName([string]$Name, [int]$TimeoutMs = 4000) {
  $root = [System.Windows.Automation.AutomationElement]::RootElement
  $cond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::NameProperty, $Name)
  $sw = [Diagnostics.Stopwatch]::StartNew()
  do {
    $el = $root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $cond)
    if ($el) { return $el }
    Start-Sleep -Milliseconds 200
  } while ($sw.ElapsedMilliseconds -lt $TimeoutMs)
  return $null
}

function Invoke-ByName([string]$Name) {
  $el = Find-ByName $Name
  if (-not $el) { return "not found: $Name" }
  try {
    $p = $el.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $p.Invoke()
    return "invoked: $Name"
  } catch {
    try {
      $sel = $el.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
      $sel.Select()
      return "selected: $Name"
    } catch { return "no invoke pattern: $Name ($($_.Exception.Message.Split([char]10)[0]))" }
  }
}
