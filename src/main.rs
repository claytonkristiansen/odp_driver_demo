//! Raspberry Pi Pico 2 (RP2350) firmware that polls a BQ41Z50 fuel gauge over SMBus
//! and prints the readings once a second over USB CDC serial.
//!
//! Wiring: GP4 = SDA, GP5 = SCL, GND to the gauge's ground. The bus needs pull-ups
//! (typically 4.7k-10k to 3.3V) if the gauge board does not already have them.

#![no_std]
#![no_main]

use core::fmt::Write as _;

use bq41z50::{Bq41z50, Bq41z50Error};
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{I2C0, USB};
use embassy_rp::{bind_interrupts, i2c, usb};
use embassy_time::Timer;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embedded_batteries_async::smart_battery::SmartBattery;
use embedded_hal_async::i2c::{ErrorType, I2c, Operation};
use heapless::String;
use panic_halt as _;

// The RP2350 boot ROM only runs images that carry an IMAGE_DEF block.
#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: embassy_rp::block::ImageDef = embassy_rp::block::ImageDef::secure_exe();

bind_interrupts!(struct Irqs {
    I2C0_IRQ => i2c::InterruptHandler<I2C0>;
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

/// I2C bus that pulses the LED after every transaction the gauge ACKs.
/// NACKs and bus errors leave it dark, so a steady blink means the gauge is talking.
struct BlinkingBus {
    bus: i2c::I2c<'static, I2C0, i2c::Async>,
    led: Output<'static>,
}

impl ErrorType for BlinkingBus {
    type Error = i2c::Error;
}

impl I2c for BlinkingBus {
    async fn transaction(&mut self, address: u8, operations: &mut [Operation<'_>]) -> Result<(), Self::Error> {
        let result = self.bus.transaction(address, operations).await;
        if result.is_ok() {
            self.led.set_high();
            Timer::after_millis(30).await;
            self.led.set_low();
            Timer::after_millis(30).await;
        }
        result
    }
}

type Gauge = Bq41z50<BlinkingBus>;
type GaugeError = Bq41z50Error<i2c::Error>;

struct Reading {
    temperature: bq41z50::CentiCelsius,
    voltage_mv: u16,
    current_ma: i16,
    soc_percent: u8,
    cells_mv: [u16; 4],
}

async fn read_gauge(bq: &mut Gauge) -> Result<Reading, GaugeError> {
    Ok(Reading {
        // Standard SBS values, via the driver's SmartBattery impl.
        temperature: bq.temperature_celsius().await?,
        voltage_mv: bq.voltage().await?,
        current_ma: bq.current().await?,
        soc_percent: bq.relative_state_of_charge().await?,
        // Per-cell voltages are BQ41Z50-specific registers, so go through the raw device.
        cells_mv: [
            bq.device.cell_voltage_1().read_async().await?.cell_voltage_1(),
            bq.device.cell_voltage_2().read_async().await?.cell_voltage_2(),
            bq.device.cell_voltage_3().read_async().await?.cell_voltage_3(),
            bq.device.cell_voltage_4().read_async().await?.cell_voltage_4(),
        ],
    })
}

fn format_reading(result: Result<Reading, GaugeError>) -> String<160> {
    let mut line = String::new();
    let _ = match result {
        Ok(r) => write!(
            line,
            "temp={:.2}C voltage={}mV current={}mA soc={}% cells={}/{}/{}/{}mV\r\n",
            r.temperature.to_degrees(),
            r.voltage_mv,
            r.current_ma,
            r.soc_percent,
            r.cells_mv[0],
            r.cells_mv[1],
            r.cells_mv[2],
            r.cells_mv[3],
        ),
        Err(e) => write!(line, "read error: {e:?}\r\n"),
    };
    line
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // I2C0 on GP5 (SCL) / GP4 (SDA). The default config is 100 kHz, the SMBus maximum.
    // Onboard LED is GP25 on the non-wireless Pico 2.
    let mut bq = Bq41z50::new(BlinkingBus {
        bus: i2c::I2c::new_async(p.I2C0, p.PIN_5, p.PIN_4, Irqs, i2c::Config::default()),
        led: Output::new(p.PIN_25, Level::Low),
    });

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

    let report = async {
        loop {
            serial.wait_connection().await;
            // Only stream while a terminal has the port open (DTR asserted).
            while serial.dtr() {
                let line = format_reading(read_gauge(&mut bq).await);
                for chunk in line.as_bytes().chunks(64) {
                    let _ = serial.write_packet(chunk).await;
                }
                Timer::after_secs(1).await;
            }
            Timer::after_millis(50).await;
        }
    };

    join(usb.run(), report).await;
}
