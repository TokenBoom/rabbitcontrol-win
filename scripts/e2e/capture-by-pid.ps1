Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public struct RECTW2 { public int L; public int T; public int R; public int B; }
public static class W3 {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECTW2 r);
}
"@
$p = Get-Process rabbitcontrol-win -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { Write-Host "no process"; exit 1 }
$h = $p.MainWindowHandle
[void][W3]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 600
$r = New-Object RECTW2
[void][W3]::GetWindowRect($h, [ref]$r)
Write-Host ("rect: " + $r.L + "," + $r.T + " " + ($r.R-$r.L) + "x" + ($r.B-$r.T))
$pad = 30
$w = ($r.R-$r.L) + 2*$pad
$ht = ($r.B-$r.T) + 2*$pad
$b = New-Object System.Drawing.Bitmap($w, $ht)
$g = [System.Drawing.Graphics]::FromImage($b)
$g.CopyFromScreen($r.L-$pad, $r.T-$pad, 0, 0, $b.Size)
$b.Save('C:\Users\luoxiao\AppData\Local\Temp\rc-ui-v2.png')
$g.Dispose(); $b.Dispose()
Write-Host "saved"
