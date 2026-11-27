Add-Type @"
using System.Runtime.InteropServices;
public static class DPI { [DllImport("user32.dll")] public static extern bool SetProcessDPIAware(); }
"@
[void][DPI]::SetProcessDPIAware()
Add-Type @"
using System;
using System.Runtime.InteropServices;
public struct RECTE { public int L; public int T; public int R; public int B; }
public static class WE {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECTE r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
}
"@
$p = Get-Process rabbitcontrol-win -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { Write-Host "no-window"; exit 1 }
$h = $p.MainWindowHandle
[void][WE]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 400
$r = New-Object RECTE
[void][WE]::GetWindowRect($h, [ref]$r)
Write-Host ("rect: " + $r.L + "," + $r.T + " " + ($r.R-$r.L) + "x" + ($r.B-$r.T))
Add-Type -AssemblyName System.Drawing
$w = $r.R-$r.L; $ht = $r.B-$r.T
$b = New-Object System.Drawing.Bitmap($w, $ht)
$g = [System.Drawing.Graphics]::FromImage($b)
$g.CopyFromScreen($r.L, $r.T, 0, 0, $b.Size)
$b.Save('C:\Users\luoxiao\AppData\Local\Temp\rc-e2e-win.png')
$g.Dispose(); $b.Dispose()
Write-Host "saved"
