//! Raspberry Pi Pico 2 (RP2350) firmware that bridges a BQ41Z50 fuel gauge (SMBus) to a host
//! over USB CDC serial. The command protocol lives in the library (src/lib.rs); this file is the
//! board: USB, the activity LED, the I2C bus, and the commands that need the hardware (`scan`,
//! `smb`, `lines`, `bootsel`).
//!
//! Wiring: GP4 = SDA, GP5 = SCL, GND to the gauge's ground. The bus needs pull-ups
//! (typically 4.7k-10k to 3.3V) if the gauge board does not already have them.

#![no_std]
#![no_main]

use core::fmt::Write as _;

use bq41z50::Bq41z50;
use driver_demo::{Reply, handle_command, scan_bus, smb_raw};
use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_futures::yield_now;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{I2C0, USB};
use embassy_rp::{bind_interrupts, i2c, usb};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Delay, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use embedded_hal_async::i2c::{ErrorType, I2c, Operation};
use heapless::Vec;
use panic_halt as _;

// The RP2350 boot ROM only runs images that carry an IMAGE_DEF block.
#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: embassy_rp::block::ImageDef = embassy_rp::block::ImageDef::secure_exe();

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

/// `reboot()` flag for "restart into the USB bootloader" (RP2350 datasheet 5.5.10.1).
const REBOOT_TYPE_BOOTSEL: u32 = 0x0002;

/// Raised after every transaction the gauge ACKs; the LED task turns it into a blink.
static GAUGE_ACTIVITY: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// I2C bus that signals the LED after every transaction the gauge ACKs.
/// NACKs and bus errors leave it dark, so a steady blink means the gauge is talking.
/// The blink runs in its own task so it never slows the bus down.
///
/// Transactions use embassy-rp's blocking I2C: its async reads longer than the 16-byte FIFO
/// can wait forever for a batch that never completes (seen with embassy-rp 0.3.1 on SMBus block
/// reads), which wedges the whole firmware. A 33-byte block read takes ~1 ms at 400 kHz, and the
/// bus yields to the executor after each transaction so USB keeps running.
struct BlinkingBus {
    bus: i2c::I2c<'static, I2C0, i2c::Blocking>,
}

impl ErrorType for BlinkingBus {
    type Error = i2c::Error;
}

impl I2c for BlinkingBus {
    async fn transaction(&mut self, address: u8, operations: &mut [Operation<'_>]) -> Result<(), Self::Error> {
        let result = embedded_hal::i2c::I2c::transaction(&mut self.bus, address, operations);
        if result.is_ok() {
            GAUGE_ACTIVITY.signal(());
        }
        yield_now().await;
        result
    }
}


/// `lines`: the raw SDA/SCL levels. SIO GPIO_IN (RP2350 datasheet 3.1.11) reads the pads
/// whatever function owns them; both idle high.
fn pin_levels() -> Reply {
    let mut reply = Reply::new();
    let levels = unsafe { core::ptr::read_volatile(0xd000_0004 as *const u32) };
    let _ = write!(reply, "ok sda={} scl={}", (levels >> 4) & 1, (levels >> 5) & 1);
    reply
}

async fn send_line<'d>(serial: &mut CdcAcmClass<'d, usb::Driver<'d, USB>>, mut line: Reply) -> Result<(), EndpointError> {
    line.truncate(line.capacity() - 2);
    let _ = line.push_str("\r\n");
    let max = serial.max_packet_size() as usize;
    for chunk in line.as_bytes().chunks(max) {
        serial.write_packet(chunk).await?;
    }
    // A full-size last packet doesn't end the transfer; a zero-length packet does.
    if line.len().is_multiple_of(max) {
        serial.write_packet(&[]).await?;
    }
    Ok(())
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // I2C0 on GP5 (SCL) / GP4 (SDA) at 400 kHz (fast mode).
    // In an Option so `scan` can briefly take the bus back from the driver.
    let mut i2c_config = i2c::Config::default();
    i2c_config.frequency = 400_000;
    let mut bq = Some(Bq41z50::new(BlinkingBus {
        bus: i2c::I2c::new_blocking(p.I2C0, p.PIN_5, p.PIN_4, i2c_config),
    }));
    // Onboard LED is GP25 on the non-wireless Pico 2.
    let mut led = Output::new(p.PIN_25, Level::Low);

    // USB CDC-ACM serial port. A fixed serial number keeps the host's port name stable.
    let mut config_desc = [0; 256];
    let mut bos_desc = [0; 256];
    let mut control_buf = [0; 64];
    let mut state = State::new();
    let mut usb_config = embassy_usb::Config::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("OpenDevicePartnership");
    usb_config.product = Some("BQ41Z50 Demo");
    usb_config.serial_number = Some("0001");
    let mut builder = embassy_usb::Builder::new(
        usb::Driver::new(p.USB, Irqs),
        usb_config,
        &mut config_desc,
        &mut bos_desc,
        &mut [],
        &mut control_buf,
    );
    let mut serial = CdcAcmClass::new(&mut builder, &mut state, 64);
    let mut usb = builder.build();

    // Back-to-back transactions coalesce into one signal, so a busy bus blinks steadily.
    let blink = async {
        loop {
            GAUGE_ACTIVITY.wait().await;
            led.set_high();
            Timer::after_millis(30).await;
            led.set_low();
            Timer::after_millis(30).await;
        }
    };

    let commands = async {
        let mut packet = [0u8; 64];
        let mut line: Vec<u8, 1024> = Vec::new();
        loop {
            serial.wait_connection().await;
            line.clear();
            let mut overflowed = false;
            // Read errors mean the host went away; go back to waiting for it.
            'connected: while let Ok(n) = serial.read_packet(&mut packet).await {
                for &byte in &packet[..n] {
                    if byte != b'\r' && byte != b'\n' {
                        overflowed |= line.push(byte).is_err();
                        continue;
                    }
                    if line.is_empty() && !overflowed {
                        continue; // blank line, or the \n of a \r\n
                    }
                    let reply = if overflowed {
                        let mut r = Reply::new();
                        let _ = r.push_str("err command too long");
                        r
                    } else if line.as_slice() == b"scan" || line.starts_with(b"smb ") {
                        // These talk to the bus directly, so take it back from the driver for a moment.
                        let mut bus = bq.take().unwrap().device.free().bus;
                        let reply = if line.as_slice() == b"scan" {
                            scan_bus(&mut bus).await
                        } else {
                            smb_raw(&mut bus, &mut Delay, &line[4..]).await
                        };
                        bq = Some(Bq41z50::new(bus));
                        reply
                    } else if line.as_slice() == b"lines" {
                        pin_levels()
                    } else {
                        handle_command(bq.as_mut().unwrap(), &mut Delay, &line).await
                    };
                    let bootsel = !overflowed && line.as_slice() == b"bootsel";
                    line.clear();
                    overflowed = false;
                    if send_line(&mut serial, reply).await.is_err() {
                        break 'connected;
                    }
                    if bootsel {
                        // Give the reply time to reach the host, then let the ROM reboot into
                        // its USB mass-storage bootloader so the next build can be copied over.
                        Timer::after_millis(100).await;
                        embassy_rp::rom_data::reboot(REBOOT_TYPE_BOOTSEL, 10, 0, 0);
                    }
                }
            }
        }
    };

    join3(usb.run(), commands, blink).await;
}
