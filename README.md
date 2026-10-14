# DriverDemo: BQ41Z50 on Raspberry Pi Pico 2

Bridges a BQ41Z50 fuel gauge on SMBus (I2C0, 400 kHz) to a computer over USB serial. The host
sends text commands and the Pico 2 reads the gauge through the
[bq41z50 driver](../bq41z50) and replies: the SBS values (temperature, voltage, current, charge,
capacities, times, status), the BQ41Z50 status registers, and every register, MAC command, buffer
and helper function the driver has. A browser dashboard (`tools/web_dashboard.py`) drives it.

The onboard LED (GP25) blinks after SMBus transactions the gauge acknowledges, so a flicker
means the gauge is talking; it stays dark while the gauge NACKs (asleep, unpowered or miswired).
The blink runs in its own task, so it doesn't slow the bus down.

## Wiring

| Pico 2        | BQ41Z50 SMBus |
|---------------|---------------|
| GP4 (pin 6)   | SMBD (SDA)    |
| GP5 (pin 7)   | SMBC (SCL)    |
| GND (pin 8)   | GND / VSS     |

Use 4.7k-10k pull-ups to 3.3V (pin 36) if the gauge board doesn't have them.

## Build and flash

    cargo run --release          # builds, then flashes with tools/flash_jlink.sh

`tools/flash_jlink.sh` (the cargo runner) flashes over SWD with a J-Link from WSL, using the
Windows J-Link V8.80 tools, and waits for the board's USB serial port to come back on Windows. It
leaves a `driver-demo.elf` copy next to the binary for Ozone. With another SWD probe, change the
runner in `.cargo/config.toml` to `probe-rs run --chip RP2350`.

### Reaching the board from WSL

The board stays on Windows (e.g. as COM4); no `usbipd` needed. The Python tools run in WSL
and open it through `tools/bridge.ps1`, which `tools/pico_port.py` starts with `powershell.exe`
and which relays the COM port over its stdin/stdout. Opening the port takes about 1.5 s; after
that a command takes a few milliseconds. If the board is attached to WSL with `usbipd` instead,
or the tools run on native Linux or Windows, they open it directly with pyserial.
`--port COM4` or `--port /dev/ttyACM0` picks a port explicitly.

## Troubleshooting

- `err gauge: Bus(Abort(NoAcknowledge))` on every read: run `scan`. If it finds nothing and
  `lines` shows `sda=1 scl=1`, the Pico side is fine and the gauge isn't answering: it is in
  shutdown (it wakes when a charger voltage is applied to PACK+), unpowered, or not wired to
  GP4/GP5 with a shared ground.
- No reply at all, or replies that don't match the command: only one program can use the serial
  port. Stop the dashboard (Ctrl+C) or any terminal before running another tool.
  That includes other projects' bridges to the same COM port.
- "Plug in the Pico 2..." from WSL while the board is plugged in: it is attached to WSL with
  `usbipd` but pyserial isn't installed (`pip install pyserial`), or it's not showing up as a
  COM port on Windows. `usbipd detach --busid <id>` hands it back to Windows.

## Command protocol

Open the USB serial port (`/dev/ttyACM0` on Linux, a COM port on Windows) at any baud rate.
Send one command per line (`\n` or `\r\n`); each gets exactly one reply line starting with
`ok` or `err`. The gauge is only read when a command asks for it.

    > ping
    < ok pong
    > get temp soc              (any of the names from `fields`; values carry units)
    < ok temp=25.10C soc=87%
    > read                      (same as: get temp voltage current soc cells)
    < ok temp=25.10C voltage=15012mV current=-312mA soc=87% cells=3753/3752/3754/3753mV
    > fields
    < ok fields: temp voltage current avg_current soc ... chem_id
    > get bogus
    < err unknown field 'bogus' (try fields)

A field the gauge refuses replies `name=!`; if every field fails the reply is
`err gauge: ...` with the driver error. Commands are limited to 1024 bytes and 64 fields.

Every function of the bq41z50 driver is reachable as well. Raw data is hex bytes in wire
(little-endian) order, and names are the driver's accessor names:

| Command | Driver call |
|---|---|
| `reg <name>...` / `regw <name> <hex>` | `bq.device.<name>().read_async()` / `.write_async()` (71 registers) |
| `mac <name>...` | `bq.device.<name>().dispatch_out_async()` (47 MAC commands that return data) |
| `run <name>` | `bq.device.<name>().dispatch_async()` (28 MAC actions) |
| `buf <name>...` / `bufw <name> <hex>` | `bq.device.<name>().read_async(buf)` / `.write_async(buf)` (5 buffers) |
| `call <function> [args]` | lib.rs functions and SmartBattery setters, e.g. `call unseal_fg 0x0414 0x3672` |

Diagnostics: `scan` lists every I2C address that ACKs, `lines` reports the raw SDA/SCL levels
(both idle high), `smb word|block|mac <code>` does plain SMBus reads that bypass the driver (block
and MAC reads print the length byte the gauge sends), and `bootsel` reboots into the USB
bootloader for reflashing.

The `reg`/`mac`/`run`/`buf` dispatch in `src/driver_api.rs` and the page's metadata in
`tools/driver_api.json` are generated from the driver's device description:

    python3 tools/gen_driver_api.py        # reads ../bq41z50/bq41z50.ddsl

## Driver issues found

Reading every register, MAC command and buffer through the driver and again with plain SMBus
reads (`smb`), on a bq41z50-R1 (firmware 1.02 build 36) turned up the issues below. All of them are fixed in the
driver by commit `f6bc114` ("fix driver issues found on bq41z50-R1 hardware"):

| Issue | Affects | Symptom |
|---|---|---|
| Block reads keep the SMBus length byte | All 27 block registers: `0x50`-`0x58`, `0x60`-`0x67`, `0x6A`-`0x6B`, `0x71`-`0x78` | Data shifted one byte, last byte lost. SafetyAlert reads `0x00000004` (looks like OCC1) instead of 0; StateOfHealth FCC reads 13060 instead of 3635 mAh. The MAC versions read correctly. Fix in `Interface::read_register`: for block commands read `len + 1` bytes and drop the first. |
| `.ddsl` sizes differ from what the gauge sends | Registers `gauging_status` (6 vs 4), `afe_reg` (6 vs 21), `gauge_status_3` (32 vs 24), `cb_status` (20 vs 19); MACs `mac_hardware_version` (2 vs 1), `mac_instruction_flash_signature` (2 vs 1), `mac_gauging_status` (6 vs 5), `mac_afe_reg` (6 vs 21), `mac_cb_status` (20 vs 19) | Bytes the gauge sends are dropped, or (`afe_reg`) padding is returned as data |
| `MAC_FIRMWARE_VERSION` fields are big-endian, `.ddsl` says little-endian | `device_number`, `version`, `build_number` | Reads `0x5041` / `0x0201` / `0x2400` instead of 0x4150 / 1.02 / 36 |
| Signature MACs read back with no delay | `mac_instruction_flash_signature`, `mac_static_df_signature`, `mac_all_df_signature` | The gauge NACKs while it computes. With a wait before the read: `mac_all_df_signature` answered after 1 s and `mac_instruction_flash_signature` after 0.5 s; `mac_static_df_signature` did not answer after 0.5 s (longer waits not tried) |
| `SmartBattery::battery_status()` returns an error when the error-code bits are non-zero | `battery_status()` | No value at all whenever the previous SBS command failed (EC is the last command's result, not a battery fault) |
| SBS strings are returned as the raw block | `manufacturer_name`, `device_name`, `device_chemistry` | Caller gets the length byte plus whatever follows it, and has to parse it |

The fix turns the 27 block registers into read-only commands, so they now appear under MAC reads
(read with `mac <name>`) rather than `reg`. The six the old `.ddsl` marked writable
(`lifetime_data_block_6`/`7`/`8`/`11`/`12`, `gauge_status_2`) can no longer be written through
the driver.

Not a driver bug, but found on the way: embassy-rp 0.3.1's **async** I2C reads longer than the
16-byte FIFO can wait forever for a batch that never completes, which wedges the firmware until
reset. The gauge bus therefore uses embassy-rp's blocking I2C (see `BlinkingBus` in `src/main.rs`).

## What is hard-coded here (candidates for the driver)

Everything below works around a gap in the driver or its `.ddsl`. Each is marked in the code.

**Firmware (`src/main.rs`)**

1. Units for the BQ41Z50-specific registers come from the TRM, not the `.ddsl`: turbo power cW,
   turbo resistances mOhm, turbo/trip-point voltages mV, turbo currents mA, SOH energy cWh.
2. `manufacture_date` adds the 1980 year bias itself.
3. The `call` command is a hand-written match over the driver's lib.rs functions and the
   SmartBattery setters (their signatures aren't in the `.ddsl`).
4. Diagnostics (`scan`, `lines`, `smb`) talk to the bus directly; `smb` repeats the gauge address
   `0x0B` and the ManufacturerBlockAccess command `0x44`, which are private constants in the driver.

**Generator (`tools/gen_driver_api.py`)**

5. The MAC code or SBS block command for each command, derived the same way as
   `Interface::dispatch_command` (command addresses are `0x00_CC_SSSS`; `CC` = `0x44` is a MAC with
   code `SSSS` little-endian, `0x440100` -> `0x0001`; any other `CC` is a block read of `CC`).
6. `DISRUPTIVE`: MAC actions that are hard to undo (seal, shutdown, sleep, FET and fuse control,
    resets, calibration and gauging mode, lifetime/PF/black-box resets).
7. `SIDE_EFFECT_READS`: `mac_output_ccadc_cal` and `mac_output_shorted_ccadc_cal`, which return data
    but switch the gauge into calibration output mode.
8. `HELPERS`: the names, arguments and descriptions of the lib.rs functions and SmartBattery setters.
9. Buffer reads are 33 bytes (length byte + 32).

**Web page (`tools/dashboard.html`)**

10. `ALARM_BITS` / `ALL_BITS_ALARM`: which status bits are faults (every SafetyAlert/SafetyStatus/
    PFAlert/PFStatus bit; BatteryStatus RTA, RCA, TDA, OTA, TCA, OCA; BatteryMode CF;
    OperationStatus SS, PF). The bit *names* come from the `.ddsl`.
11. `STATUS_FIELDSETS`: which `.ddsl` register or command decodes each status value, and the labels, groups
    and unit formatting of the "All gauge data" table.

## Debugging with SEGGER Ozone (J-Link)

The firmware uses `cortex-m-rt` with the `set-vtor` and `set-sp` features, so the `Reset`
handler re-loads the stack pointer and sets `VTOR` from the image's own vector table before
doing anything else. This means the default Ozone-generated `.jdebug` works as-is: even
though `_SetupTarget()` jumps straight to `Reset` and skips the boot ROM, the firmware
initialises itself correctly.

Device `RP2350_M33_0`, SWD, SVD file `rp235x.svd` in this directory.

## Live dashboard (for demos)

`tools/web_dashboard.py` serves a web page: live readout tiles, temperature,
voltage and current charts with a hover crosshair, a cell-balance chart and a table of readings.
The page drives the board: every second it posts a
`get` for every value it shows to the script, which forwards it to the Pico 2 and returns the
parsed reply (about 100 ms on real hardware).

The Driver API section lists every register, MAC command, buffer and function from the driver,
decodes raw bytes into the `.ddsl` bit fields, and can read, write, run or call each one. Ticking
Watch adds an item to the 1-second poll; Read all snapshots everything readable (about 0.6 s).
Block registers on the Registers tab carry a warning (see "Driver issues found") pointing to the
MAC read that returns the same data correctly.
Writes and actions ask for confirmation, and ones that are hard to undo (seal, shutdown, FET and
fuse control, resets) make you type the name. From WSL it needs nothing beyond Python; elsewhere,
`pip install pyserial`:

    python3 tools/web_dashboard.py          # then open http://127.0.0.1:8000
    python3 tools/web_dashboard.py --demo   # simulated board answering the same protocol
    python3 tools/web_dashboard.py --host 0.0.0.0 --http-port 8080   # serve to other machines

Stop it with Ctrl+C. Several browser tabs can be open at once, but each one polls the board on
its own; the script is the only thing holding the serial port.
