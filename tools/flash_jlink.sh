#!/usr/bin/env bash
# Flash an ELF onto the Pico 2 over SWD with a SEGGER J-Link (Windows J-Link tools, run from WSL),
# then wait for its USB serial port to come back on Windows. The tools reach it there through
# tools/bridge.ps1, so it doesn't need attaching to WSL.
#
#   tools/flash_jlink.sh [path/to/driver-demo]   (also the cargo runner: cargo run --release)
set -euo pipefail

ELF=${1:-target/thumbv8m.main-none-eabihf/release/driver-demo}
JLINK=${JLINK:-"/mnt/c/Program Files/SEGGER/JLink_V880/JLink.exe"}

ps() { powershell.exe -NoProfile -Command "$1" | tr -d '\r'; }
com_port() {
    ps "Get-CimInstance Win32_PnPEntity | Where-Object { \$_.PNPDeviceID -like 'USB\\VID_C0DE&PID_CAFE*' -and \$_.Name -match '\\((COM\\d+)\\)' } | ForEach-Object { \$Matches[1] } | Select-Object -First 1"
}

script=$(mktemp --suffix=.jlink)
trap 'rm -f "$script"' EXIT
cat > "$script" <<JLINK
si SWD
speed 4000
device RP2350_M33_0
connect
loadfile "$(wslpath -w "$(realpath "$ELF")")"
r
g
exit
JLINK
# J-Link only loads files with an ELF extension, so load a .elf copy (Ozone can open the same one).
[[ $ELF == *.elf ]] || { cp "$ELF" "$ELF.elf"; sed -i "s|$(wslpath -w "$(realpath "$ELF")" | sed 's/\\/\\\\/g')|$(wslpath -w "$(realpath "$ELF.elf")" | sed 's/\\/\\\\/g')|" "$script"; }
"$JLINK" -NoGui 1 -ExitOnError 1 -CommandFile "$(wslpath -w "$script")" | tr -d '\r' | grep -E "Flash download: Total|O.K.|ERROR|Error|Cannot" || true

echo "Waiting for the board to enumerate..."
port=""
for _ in $(seq 30); do
    port=$(com_port)
    [[ -n $port ]] && break
    sleep 1
done
[[ -n $port ]] || { echo "Board did not come back as a COM port (still attached to WSL? usbipd detach --busid <id>)" >&2; exit 1; }
echo "Ready: $port"
