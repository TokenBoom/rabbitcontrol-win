Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty, 'RabbitControl')
$win = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $cond)
$all = $win.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
foreach ($el in $all) {
  $name = $el.Current.Name
  $type = $el.Current.ControlType.ProgrammaticName
  if ($name -or $type -match 'Edit') { Write-Host ($type + ' | name=[' + $name + '] | autoId=' + $el.Current.AutomationId) }
}
