# DriverDemo: BQ41Z50 on Raspberry Pi Pico 2

Polls a BQ41Z50 fuel gauge over SMBus (I2C0, 100 kHz) and prints temperature,
pack voltage, current, relative SoC and cell 1-4 voltages over USB serial every second.

## Wiring

| Pico 2        | BQ41Z50 SMBus |
|---------------|---------------|
| GP4 (pin 6)   | SMBD (SDA)    |
| GP5 (pin 7)   | SMBC (SCL)    |
| GND (pin 8)   | GND / VSS     |

Use 4.7k-10k pull-ups to 3.3V (pin 36) if the gauge board doesn't have them.

## Build and flash

    cargo run --release          # builds and writes target/.../release/driver-demo.uf2

Hold BOOTSEL while plugging in the Pico 2, then copy the `.uf2` onto the `RP2350` drive.
With an SWD probe instead, change the runner in `.cargo/config.toml` to
`probe-rs run --chip RP2350`.

## Read the output

Open the USB serial port (`/dev/ttyACM0` on Linux, a COM port on Windows) at any baud rate:

    temp=25.10C voltage=15012mV current=-312mA soc=87% cells=3753/3752/3754/3753mV

## Debugging with SEGGER Ozone (J-Link)

Ozone's generated project hard-faults on RP2350 because its default `AfterTargetReset` /
`AfterTargetDownload` call `_SetupTarget()`, which loads SP/PC from the vector table and
skips the boot ROM (the ROM sets up the RCP before launching the image). In the `.jdebug`:

    void AfterTargetReset (void) {
      //_SetupTarget();      // let the boot ROM run
    }

    void AfterTargetDownload (void) {
      Exec.Reset();          // start from the boot ROM, not the flash loader
    }

Device `RP2350_M33_0`, SWD, SVD file `rp235x.svd` in this directory.

## Live dashboard (for demos)

`tools/live_plot.py` auto-detects the Pico 2 and live-graphs temperature, voltage and current
with large readouts, sized for a projector:

    pip install pyserial matplotlib
    python3 tools/live_plot.py              # or --port COM5 / --port /dev/ttyACM0
    python3 tools/live_plot.py --demo       # simulated data, no hardware needed

Close any other serial terminal first; only one program can hold the port.
