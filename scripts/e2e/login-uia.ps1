Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty, 'RabbitControl')
$win = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $cond)
function Find-ById($window, $id) {
  $c = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::AutomationIdProperty, $id)
  $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $c)
}
$email = Find-ById $win 'Email'
if ($email) {
  $vp = $email.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
  $vp.SetValue('luoxiao123@outlook.com')
  Write-Host 'email-set'
}
$pw = Find-ById $win 'Password'
if ($pw) {
  $vpp = $pw.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
  $vpp.SetValue('vem7hcn6fbu-epw0AXQ')
  Write-Host 'password-set'
}
$btn = Find-ById $win 'SignIn'
if ($btn) {
  $ip = $btn.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
  $ip.Invoke()
  Write-Host 'signin-invoked'
}
