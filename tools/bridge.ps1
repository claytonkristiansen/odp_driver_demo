# Relays the Pico 2's USB serial port to stdin/stdout.
#
# Started by tools/pico_port.py from WSL through powershell.exe, so WSL can reach a COM port that only
# Windows sees. Exits when either side closes.

param(
    # COM port to open, e.g. COM3. Found by USB VID/PID when omitted.
    [string]$Port = "",
    [string]$VidPid = "VID_C0DE&PID_CAFE"
)

$ErrorActionPreference = "Stop"

if (-not $Port) {
    $dev = Get-CimInstance Win32_PnPEntity |
        Where-Object { $_.PNPDeviceID -like "USB\$VidPid*" -and $_.Name -match '\((COM\d+)\)' } |
        Select-Object -First 1
    if (-not $dev) {
        [Console]::Error.WriteLine("bridge: no serial port with $VidPid found")
        exit 2
    }
    $null = $dev.Name -match '\((COM\d+)\)'
    $Port = $Matches[1]
}

$serial = New-Object System.IO.Ports.SerialPort $Port, 115200
$serial.DtrEnable = $true
$serial.RtsEnable = $true
$serial.ReadTimeout = [System.IO.Ports.SerialPort]::InfiniteTimeout
try {
    $serial.Open()
} catch {
    [Console]::Error.WriteLine("bridge: cannot open ${Port}: $($_.Exception.Message)")
    exit 3
}
[Console]::Error.WriteLine("bridge: connected to $Port")

$stdin = [Console]::OpenStandardInput()
$stdout = [Console]::OpenStandardOutput()
$serialStream = $serial.BaseStream

$toPort = $stdin.CopyToAsync($serialStream, 64)
$toHost = $serialStream.CopyToAsync($stdout, 64)
try {
    [System.Threading.Tasks.Task]::WaitAny(@($toPort, $toHost)) | Out-Null
} finally {
    if ($toHost.IsFaulted) {
        [Console]::Error.WriteLine("bridge: $Port closed: $($toHost.Exception.InnerException.Message)")
    }
    $serial.Close()
}
