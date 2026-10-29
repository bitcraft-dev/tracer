# Drives a Rux window the way a person would, for probes and user tests:
# screenshot it, click, drag, scroll, type. Coordinates are window-relative
# physical pixels, as the screenshot shows them.
#
#   tools/drive.ps1 -Proc tracer-probes -Do move  -X 0 -Y 0 -X2 1920 -Y2 1040
#   tools/drive.ps1 -Proc tracer-probes -Do shot  -Out shot.png
#   tools/drive.ps1 -Proc tracer-probes -Do click -X 100 -Y 200
#   tools/drive.ps1 -Proc tracer-probes -Do drag  -X 100 -Y 200 -X2 300 -Y2 200
#   tools/drive.ps1 -Proc tracer-probes -Do keys  -Text "^k"      (SendKeys syntax)
#   tools/drive.ps1 -Proc tracer-probes -Do rect
param(
  [string]$Proc = "tracer-probes",
  [string]$Do = "shot",
  [string]$Out = "",
  [int]$X = 0, [int]$Y = 0, [int]$X2 = 0, [int]$Y2 = 0,
  [string]$Text = ""
)

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class W {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int hh, bool r);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@
[W]::SetProcessDPIAware() | Out-Null

$p = Get-Process $Proc -ErrorAction Stop | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $p) { throw "no window for $Proc" }
$h = $p.MainWindowHandle
[W]::ShowWindow($h, 9) | Out-Null
[W]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 150

$r = New-Object W+RECT
[W]::GetClientRect($h, [ref]$r) | Out-Null
$o = New-Object W+POINT
[W]::ClientToScreen($h, [ref]$o) | Out-Null
$w = $r.R - $r.L; $hh = $r.B - $r.T

function MoveTo($cx, $cy) { [W]::SetCursorPos($o.X + $cx, $o.Y + $cy) | Out-Null }

switch ($Do) {
  "move" { [W]::MoveWindow($h, $X, $Y, $X2, $Y2, $true) | Out-Null; "moved to $X,$Y size $X2 x $Y2" }
  "rect" { "client {0}x{1} at {2},{3}" -f $w, $hh, $o.X, $o.Y }
  "shot" {
    $bmp = New-Object System.Drawing.Bitmap $w, $hh
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($o.X, $o.Y, 0, 0, $bmp.Size)
    $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    "saved $Out ($w x $hh)"
  }
  "click" {
    MoveTo $X $Y; Start-Sleep -Milliseconds 60
    [W]::mouse_event(0x02, 0, 0, 0, [IntPtr]::Zero); Start-Sleep -Milliseconds 40
    [W]::mouse_event(0x04, 0, 0, 0, [IntPtr]::Zero)
    "clicked $X,$Y"
  }
  "hover" { MoveTo $X $Y; "hover $X,$Y" }
  "drag" {
    MoveTo $X $Y; Start-Sleep -Milliseconds 60
    [W]::mouse_event(0x02, 0, 0, 0, [IntPtr]::Zero)
    $steps = 20
    for ($i = 1; $i -le $steps; $i++) {
      MoveTo ([int]($X + ($X2 - $X) * $i / $steps)) ([int]($Y + ($Y2 - $Y) * $i / $steps))
      Start-Sleep -Milliseconds 15
    }
    [W]::mouse_event(0x04, 0, 0, 0, [IntPtr]::Zero)
    "dragged $X,$Y -> $X2,$Y2"
  }
  "wheel" {
    MoveTo $X $Y
    # The delta is a signed int passed as a DWORD; 0xFFFFFFFF is -1 in
    # PowerShell 5.1, so reinterpret the bytes rather than mask.
    $delta = [BitConverter]::ToUInt32([BitConverter]::GetBytes([int]$Text * 120), 0)
    [W]::mouse_event(0x0800, 0, 0, $delta, [IntPtr]::Zero)
    "wheel $Text at $X,$Y"
  }
  "keys" { [System.Windows.Forms.SendKeys]::SendWait($Text); "sent $Text" }
}
