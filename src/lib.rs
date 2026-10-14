//! Raspberry Pi Pico 2 (RP2350) firmware that bridges a BQ41Z50 fuel gauge (SMBus) to a host
//! over USB CDC serial. The host drives it: each newline-terminated command gets one reply line,
//! `ok ...` or `err ...`, and the gauge is only read when a command asks for it.
//!
//!     ping                      -> ok pong
//!     get temp voltage soc      -> ok temp=25.10C voltage=15012mV soc=87%
//!     read                      -> ok temp=... voltage=... current=... soc=... cells=.../.../.../...mV
//!     fields                    -> ok fields: temp voltage current ...
//!     help                      -> ok commands: ...
//!     bootsel                   -> ok rebooting to BOOTSEL   (then the RP2350 USB drive appears, for reflashing)
//!     scan                      -> ok scan found: 0x0b       (every I2C address that ACKs)
//!     lines                     -> ok sda=1 scl=1            (raw pin levels; both idle high)
//!     smb word 0x09             -> ok 803e                   (plain SMBus reads that bypass the driver,
//!     smb block 0x54            -> ok len=4 data=00610000     for checking it; block/mac print the
//!     smb mac 0x0054 [delay_ms] -> ok len=6 data=5400...      length the gauge reports)
//!
//! Every function of the bq41z50 driver is reachable too. Raw data is hex bytes in wire
//! (little-endian) order; the register, command and buffer names are the driver's accessors,
//! dispatched by the generated `driver_api` module:
//!
//!     reg temperature cycle_count     -> ok temperature=a00b cycle_count=2a00
//!     regw at_rate 0000               -> ok
//!     mac mac_chem_id                 -> ok mac_chem_id=1012
//!     run mac_led_toggle              -> ok
//!     buf device_name                 -> ok device_name=0762713431...
//!     bufw authenticate <hex>         -> ok
//!     call unseal_fg 0x0414 0x3672    -> ok          (the driver's hand-written functions)
//!
//! A field the gauge refuses (e.g. while sealed) comes back as `name=!`; the command only
//! fails as a whole if every field does.
//!
//! This library holds the command handling, generic over the I2C bus, so it also builds on the
//! PC: `cargo test-host` runs it against a simulated gauge (src/sim_gauge.rs). The firmware
//! (src/main.rs) adds the RP2350 parts: USB, the LED, the bus itself, `lines` and `bootsel`.

#![cfg_attr(not(test), no_std)]

use core::fmt::Write as _;

use bq41z50::{Bq41z50, Bq41z50Error};
use embedded_batteries_async::smart_battery::{
    BatteryModeFields, CapacityModeSignedValue, CapacityModeValue, SmartBattery,
};
use embedded_hal_async::delay::DelayNs;
use embedded_hal_async::i2c::{Error, I2c};
use heapless::{String, Vec};

mod driver_api;
use driver_api::ApiError;

/// One value the host can ask for. The names in `FIELDS` are what commands and replies use.
#[derive(Clone, Copy, PartialEq)]
enum Field {
    // Measurements
    Temp,
    Voltage,
    Current,
    AvgCurrent,
    Soc,
    AbsSoc,
    Cells,
    MaxError,
    // Capacity and time
    RemainingCap,
    FullChargeCap,
    DesignCap,
    RunTimeToEmpty,
    AvgTimeToEmpty,
    AvgTimeToFull,
    // Charging
    ChargingCurrent,
    ChargingVoltage,
    CycleCount,
    // AtRate
    AtRate,
    AtRateTimeToFull,
    AtRateTimeToEmpty,
    AtRateOk,
    // Alarms
    RemainingCapAlarm,
    RemainingTimeAlarm,
    // Health
    Soh,
    SohFcc,
    SohEnergy,
    // Status registers, as raw hex
    BatteryStatus,
    BatteryMode,
    SafetyAlert,
    SafetyStatus,
    PfAlert,
    PfStatus,
    OperationStatus,
    ChargingStatus,
    GaugingStatus,
    ManufacturingStatus,
    // Turbo / battery trip point
    MaxTurboPower,
    SusTurboPower,
    TurboPackR,
    TurboSysR,
    TurboEdv,
    TurboCurrent,
    SusTurboCurrent,
    TurboRhfEffective,
    TurboVload,
    BtpDischargeSet,
    BtpChargeSet,
    // Device info
    ManufacturerName,
    DeviceName,
    Chemistry,
    ManufactureDate,
    SerialNumber,
    DesignVoltage,
    SpecInfo,
    DeviceType,
    FirmwareVersion,
    HardwareVersion,
    ChemId,
}

const FIELDS: &[(&str, Field)] = &[
    ("temp", Field::Temp),
    ("voltage", Field::Voltage),
    ("current", Field::Current),
    ("avg_current", Field::AvgCurrent),
    ("soc", Field::Soc),
    ("abs_soc", Field::AbsSoc),
    ("cells", Field::Cells),
    ("max_error", Field::MaxError),
    ("remaining_cap", Field::RemainingCap),
    ("full_charge_cap", Field::FullChargeCap),
    ("design_cap", Field::DesignCap),
    ("run_time_to_empty", Field::RunTimeToEmpty),
    ("avg_time_to_empty", Field::AvgTimeToEmpty),
    ("avg_time_to_full", Field::AvgTimeToFull),
    ("charging_current", Field::ChargingCurrent),
    ("charging_voltage", Field::ChargingVoltage),
    ("cycle_count", Field::CycleCount),
    ("at_rate", Field::AtRate),
    ("at_rate_time_to_full", Field::AtRateTimeToFull),
    ("at_rate_time_to_empty", Field::AtRateTimeToEmpty),
    ("at_rate_ok", Field::AtRateOk),
    ("remaining_cap_alarm", Field::RemainingCapAlarm),
    ("remaining_time_alarm", Field::RemainingTimeAlarm),
    ("soh", Field::Soh),
    ("soh_fcc", Field::SohFcc),
    ("soh_energy", Field::SohEnergy),
    ("battery_status", Field::BatteryStatus),
    ("battery_mode", Field::BatteryMode),
    ("safety_alert", Field::SafetyAlert),
    ("safety_status", Field::SafetyStatus),
    ("pf_alert", Field::PfAlert),
    ("pf_status", Field::PfStatus),
    ("operation_status", Field::OperationStatus),
    ("charging_status", Field::ChargingStatus),
    ("gauging_status", Field::GaugingStatus),
    ("manufacturing_status", Field::ManufacturingStatus),
    ("max_turbo_power", Field::MaxTurboPower),
    ("sus_turbo_power", Field::SusTurboPower),
    ("turbo_pack_r", Field::TurboPackR),
    ("turbo_sys_r", Field::TurboSysR),
    ("turbo_edv", Field::TurboEdv),
    ("turbo_current", Field::TurboCurrent),
    ("sus_turbo_current", Field::SusTurboCurrent),
    ("turbo_rhf_effective", Field::TurboRhfEffective),
    ("turbo_vload", Field::TurboVload),
    ("btp_discharge_set", Field::BtpDischargeSet),
    ("btp_charge_set", Field::BtpChargeSet),
    ("manufacturer_name", Field::ManufacturerName),
    ("device_name", Field::DeviceName),
    ("chemistry", Field::Chemistry),
    ("manufacture_date", Field::ManufactureDate),
    ("serial_number", Field::SerialNumber),
    ("design_voltage", Field::DesignVoltage),
    ("spec_info", Field::SpecInfo),
    ("device_type", Field::DeviceType),
    ("firmware_version", Field::FirmwareVersion),
    ("hardware_version", Field::HardwareVersion),
    ("chem_id", Field::ChemId),
];

/// What `read` returns: the values the dashboard charts.
const READ_FIELDS: [Field; 5] = [Field::Temp, Field::Voltage, Field::Current, Field::Soc, Field::Cells];

impl Field {
    fn parse(name: &str) -> Option<Self> {
        FIELDS.iter().find(|(n, _)| *n == name).map(|&(_, f)| f)
    }

    fn name(self) -> &'static str {
        FIELDS.iter().find(|(_, f)| *f == self).map_or("?", |&(n, _)| n)
    }
}

/// One reply line, without its CRLF.
pub type Reply = String<2048>;

/// Reads a BQ41Z50 register whose only field has the register's own name.
macro_rules! reg {
    ($bq:expr, $name:ident) => {
        $bq.device.$name().read_async().await?.$name()
    };
}

/// Runs a status MAC command and returns its raw little-endian value.
macro_rules! status {
    ($bq:expr, $name:ident, $bytes:literal) => {{
        let raw: [u8; $bytes] = $bq.device.$name().dispatch_out_async().await?.into();
        let mut wide = [0u8; 8];
        wide[..$bytes].copy_from_slice(&raw);
        u64::from_le_bytes(wide)
    }};
}

/// Capacities are mAh or cWh (10 mWh) depending on BatteryMode's CAPACITY_MODE bit.
fn capacity(value: CapacityModeValue) -> (u16, &'static str) {
    match value {
        CapacityModeValue::MilliAmpUnsigned(v) => (v, "mAh"),
        CapacityModeValue::CentiWattUnsigned(v) => (v, "cWh"),
    }
}

/// Writes an SBS string as a quoted value. The driver's string readers return the text
/// NUL-terminated, without the SMBus length byte.
fn write_text(reply: &mut Reply, text: &[u8]) {
    let _ = reply.push('"');
    for &b in text.iter().take_while(|&&b| b != 0) {
        let printable = b == b' ' || (b.is_ascii_graphic() && b != b'"');
        let _ = reply.push(if printable { b as char } else { '?' });
    }
    let _ = reply.push('"');
}

/// Reads one field from the gauge and appends its value (the part after `name=`) to the reply.
async fn read_field<B: I2c>(bq: &mut Bq41z50<B>, field: Field, reply: &mut Reply) -> Result<(), Bq41z50Error<B::Error>> {
    // SBS strings are a length byte plus up to 32 characters.
    let mut block = [0u8; 33];
    let _ = match field {
        // Standard SBS values, via the driver's SmartBattery impl.
        Field::Temp => write!(reply, "{:.2}C", bq.temperature_celsius().await?.to_degrees()),
        Field::Voltage => write!(reply, "{}mV", bq.voltage().await?),
        Field::Current => write!(reply, "{}mA", bq.current().await?),
        Field::AvgCurrent => write!(reply, "{}mA", bq.average_current().await?),
        Field::Soc => write!(reply, "{}%", bq.relative_state_of_charge().await?),
        Field::AbsSoc => write!(reply, "{}%", bq.absolute_state_of_charge().await?),
        Field::MaxError => write!(reply, "{}%", bq.max_error().await?),
        Field::RemainingCap => {
            let (v, unit) = capacity(bq.remaining_capacity().await?);
            write!(reply, "{v}{unit}")
        }
        Field::FullChargeCap => {
            let (v, unit) = capacity(bq.full_charge_capacity().await?);
            write!(reply, "{v}{unit}")
        }
        Field::DesignCap => {
            let (v, unit) = capacity(bq.design_capacity().await?);
            write!(reply, "{v}{unit}")
        }
        Field::RemainingCapAlarm => {
            let (v, unit) = capacity(bq.remaining_capacity_alarm().await?);
            write!(reply, "{v}{unit}")
        }
        Field::RunTimeToEmpty => write!(reply, "{}min", bq.run_time_to_empty().await?),
        Field::AvgTimeToEmpty => write!(reply, "{}min", bq.average_time_to_empty().await?),
        Field::AvgTimeToFull => write!(reply, "{}min", bq.average_time_to_full().await?),
        Field::RemainingTimeAlarm => write!(reply, "{}min", bq.remaining_time_alarm().await?),
        Field::ChargingCurrent => write!(reply, "{}mA", bq.charging_current().await?),
        Field::ChargingVoltage => write!(reply, "{}mV", bq.charging_voltage().await?),
        Field::CycleCount => write!(reply, "{}", bq.cycle_count().await?),
        Field::AtRate => match bq.at_rate().await? {
            CapacityModeSignedValue::MilliAmpSigned(v) => write!(reply, "{v}mA"),
            CapacityModeSignedValue::CentiWattSigned(v) => write!(reply, "{v}cW"),
        },
        Field::AtRateTimeToFull => write!(reply, "{}min", bq.at_rate_time_to_full().await?),
        Field::AtRateTimeToEmpty => write!(reply, "{}min", bq.at_rate_time_to_empty().await?),
        Field::AtRateOk => write!(reply, "{}", bq.at_rate_ok().await? as u8),
        Field::BatteryStatus => {
            let raw: [u8; 2] = bq.device.battery_status().read_async().await?.into();
            write!(reply, "0x{:04X}", u16::from_le_bytes(raw))
        }
        Field::BatteryMode => write!(reply, "0x{:04X}", bq.battery_mode().await?.into_bits()),
        Field::SpecInfo => write!(reply, "0x{:04X}", bq.specification_info().await?.into_bits()),
        Field::ManufactureDate => {
            // ManufactureDate's year field is biased by 1980; the driver leaves adding it to callers.
            let d = bq.manufacture_date().await?;
            write!(reply, "{:04}-{:02}-{:02}", d.year() + 1980, d.month(), d.day())
        }
        Field::SerialNumber => write!(reply, "{}", bq.serial_number().await?),
        Field::DesignVoltage => write!(reply, "{}mV", bq.design_voltage().await?),
        Field::ManufacturerName => {
            bq.manufacturer_name(&mut block).await?;
            write_text(reply, &block);
            Ok(())
        }
        Field::DeviceName => {
            bq.device_name(&mut block).await?;
            write_text(reply, &block);
            Ok(())
        }
        Field::Chemistry => {
            bq.device_chemistry(&mut block).await?;
            write_text(reply, &block);
            Ok(())
        }
        // BQ41Z50-specific registers and MAC commands, via the raw device.
        Field::Cells => write!(
            reply,
            "{}/{}/{}/{}mV",
            reg!(bq, cell_voltage_1),
            reg!(bq, cell_voltage_2),
            reg!(bq, cell_voltage_3),
            reg!(bq, cell_voltage_4),
        ),
        Field::Soh => write!(reply, "{}%", reg!(bq, state_of_health_soh)),
        // mAh / cWh are from the TRM.
        Field::SohFcc => write!(reply, "{}mAh", bq.device.mac_state_of_health().dispatch_out_async().await?.soh_fcc()),
        Field::SohEnergy => write!(reply, "{}cWh", bq.device.mac_state_of_health().dispatch_out_async().await?.soh_energy()),
        Field::SafetyAlert => write!(reply, "0x{:08X}", status!(bq, mac_safety_alert, 4)),
        Field::SafetyStatus => write!(reply, "0x{:08X}", status!(bq, mac_safety_status, 4)),
        Field::PfAlert => write!(reply, "0x{:08X}", status!(bq, mac_pf_alert, 4)),
        Field::PfStatus => write!(reply, "0x{:08X}", status!(bq, mac_pf_status, 4)),
        Field::OperationStatus => write!(reply, "0x{:08X}", status!(bq, mac_operation_status, 4)),
        Field::ChargingStatus => write!(reply, "0x{:010X}", status!(bq, mac_charging_status, 5)),
        Field::GaugingStatus => write!(reply, "0x{:012X}", status!(bq, mac_gauging_status, 6)),
        Field::ManufacturingStatus => write!(reply, "0x{:04X}", status!(bq, mac_manufacturing_status, 2)),
        // The .ddsl has no units for these; cW / mOhm / mV / mA are from the BQ41Z50 TRM.
        Field::MaxTurboPower => write!(reply, "{}cW", reg!(bq, max_turbo_power)),
        Field::SusTurboPower => write!(reply, "{}cW", reg!(bq, sus_turbo_power)),
        Field::TurboPackR => write!(reply, "{}mOhm", reg!(bq, turbo_pack_r)),
        Field::TurboSysR => write!(reply, "{}mOhm", reg!(bq, turbo_sys_r)),
        Field::TurboEdv => write!(reply, "{}mV", reg!(bq, turbo_edv)),
        Field::TurboCurrent => write!(reply, "{}mA", reg!(bq, turbo_current)),
        Field::SusTurboCurrent => write!(reply, "{}mA", reg!(bq, sus_turbo_current)),
        Field::TurboRhfEffective => write!(reply, "{}mOhm", reg!(bq, turbo_rhf_effective)),
        Field::TurboVload => write!(reply, "{}mV", reg!(bq, turbo_vload)),
        Field::BtpDischargeSet => write!(reply, "{}mV", reg!(bq, btp_discharge_set)),
        Field::BtpChargeSet => write!(reply, "{}mV", reg!(bq, btp_charge_set)),
        Field::DeviceType => write!(reply, "0x{:04X}", bq.device.mac_device_type().dispatch_out_async().await?.device_type()),
        Field::FirmwareVersion => {
            let fw = bq.device.mac_firmware_version().dispatch_out_async().await?;
            write!(
                reply,
                "\"device 0x{:04X} version {:X}.{:02X} build {}\"",
                fw.device_number(),
                fw.version() >> 8,
                fw.version() & 0xFF,
                fw.build_number()
            )
        }
        Field::HardwareVersion => write!(reply, "0x{:04X}",bq.device.mac_hardware_version().dispatch_out_async().await?.hw_vers()),
        Field::ChemId => write!(reply, "0x{:04X}", bq.device.mac_chem_id().dispatch_out_async().await?.chem_id()),
    };
    Ok(())
}

/// Space a reply must still have before another value is appended: the longest name plus a
/// 64-byte register as hex, with room left for the CRLF.
const REPLY_HEADROOM: usize = 200;

fn hex_into(reply: &mut Reply, bytes: &[u8]) {
    for b in bytes {
        let _ = write!(reply, "{b:02x}");
    }
}

/// Parses hex bytes ("0a1b", optionally with a 0x prefix) into `out`, returning the length.
fn parse_hex<'a>(text: &str, out: &'a mut [u8]) -> Option<&'a [u8]> {
    let text = text.strip_prefix("0x").unwrap_or(text);
    if !text.len().is_multiple_of(2) || text.len() / 2 > out.len() {
        return None;
    }
    for (i, pair) in text.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(core::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(&out[..text.len() / 2])
}

/// Parses a number written in decimal or with a 0x prefix.
fn parse_num<T: TryFrom<i64>>(text: Option<&str>) -> Option<T> {
    let text = text?;
    let (digits, negative) = match text.strip_prefix('-') {
        Some(rest) => (rest, true),
        None => (text, false),
    };
    let value = match digits.strip_prefix("0x") {
        Some(hex) => i64::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<i64>().ok()?,
    };
    T::try_from(if negative { -value } else { value }).ok()
}

fn write_api_error<E: Error>(reply: &mut Reply, name: &str, e: ApiError<E>) {
    reply.clear();
    let _ = match e {
        ApiError::UnknownName => write!(reply, "err unknown name '{name}'"),
        ApiError::ReadOnly => write!(reply, "err {name} is read-only"),
        ApiError::WrongKind(hint) => write!(reply, "err {name}: {hint}"),
        ApiError::WrongLength(n) => write!(reply, "err {name} takes exactly {n} bytes"),
        ApiError::BadArgs(usage) => write!(reply, "err usage: {name} {usage}"),
        ApiError::Gauge(e) => write!(reply, "err gauge: {e:?}"),
    };
}

#[derive(Clone, Copy)]
enum Raw {
    Register,
    Command,
    Buffer,
}

/// `reg`, `mac` and `buf`: reads each named item and replies `name=<hex>`, or `name=!` when the
/// gauge refuses it. Unknown names fail the whole command.
async fn read_raw_items<'a, B: I2c>(
    bq: &mut Bq41z50<B>,
    kind: Raw,
    names: impl Iterator<Item = &'a str>,
    reply: &mut Reply,
) {
    let _ = reply.push_str("ok");
    let (mut count, mut failures, mut last_error) = (0, 0, None);
    let mut data = [0u8; driver_api::MAX_BYTES];
    for name in names {
        if reply.capacity() - reply.len() < REPLY_HEADROOM {
            reply.clear();
            let _ = reply.push_str("err reply too long - ask for fewer items");
            return;
        }
        let result = match kind {
            Raw::Register => driver_api::read_register(bq, name, &mut data).await,
            Raw::Command => driver_api::read_command(bq, name, &mut data).await,
            Raw::Buffer => driver_api::read_buffer(bq, name, &mut data).await,
        };
        count += 1;
        let _ = write!(reply, " {name}=");
        match result {
            Ok(n) => hex_into(reply, &data[..n]),
            Err(ApiError::Gauge(e)) => {
                let _ = reply.push('!');
                failures += 1;
                last_error = Some(e);
            }
            Err(e) => return write_api_error(reply, name, e),
        }
    }
    if count == 0 {
        reply.clear();
        let _ = reply.push_str("err name at least one item");
    } else if let Some(e) = last_error.filter(|_| failures == count) {
        reply.clear();
        let _ = write!(reply, "err gauge: {e:?}");
    }
}

/// `call`: the driver's hand-written functions (lib.rs and the SmartBattery setters).
async fn call_helper<'a, B: I2c>(
    bq: &mut Bq41z50<B>,
    delay: &mut impl DelayNs,
    name: &str,
    mut args: impl Iterator<Item = &'a str>,
    reply: &mut Reply,
) -> Result<(), ApiError<B::Error>> {
    let mut bytes = [0u8; 33];
    let _ = reply.push_str("ok");
    match name {
        "temperature_fahrenheit" => {
            let _ = write!(reply, " {name}={:.2}F", bq.temperature_fahrenheit().await?.to_degrees());
        }
        "read_security_keys" => {
            let mut keys = [0u8; 8];
            bq.read_security_keys(&mut keys).await?;
            let _ = write!(reply, " {name}=");
            hex_into(reply, &keys);
        }
        "write_security_keys" => {
            let data = args.next().and_then(|a| parse_hex(a, &mut bytes)).ok_or(ApiError::BadArgs("<8 bytes hex>"))?;
            let keys: [u8; 8] = data.try_into().map_err(|_| ApiError::WrongLength(8))?;
            bq.write_security_keys(&keys).await?;
        }
        "read_authentication_key" => {
            let mut key = [0u8; 16];
            bq.read_authentication_key(&mut key).await?;
            let _ = write!(reply, " {name}=");
            hex_into(reply, &key);
        }
        "write_authentication_key" => {
            let data = args.next().and_then(|a| parse_hex(a, &mut bytes)).ok_or(ApiError::BadArgs("<16 bytes hex>"))?;
            let key: [u8; 16] = data.try_into().map_err(|_| ApiError::WrongLength(16))?;
            bq.write_authentication_key(&key).await?;
        }
        "seal_fg" => bq.seal_fg().await?,
        "unseal_fg" | "send_access_key" => {
            let usage = ApiError::BadArgs("<key_lower u16> <key_upper u16>");
            let (Some(lower), Some(upper)) = (parse_num::<u16>(args.next()), parse_num::<u16>(args.next())) else {
                return Err(usage);
            };
            if name == "unseal_fg" {
                bq.unseal_fg(lower, upper).await?;
            } else {
                bq.send_access_key(lower, upper).await?;
            }
        }
        "read_mfg_info" => {
            bq.read_mfg_info(&mut bytes).await?;
            let _ = write!(reply, " {name}=");
            hex_into(reply, &bytes);
        }
        "write_mfg_info" => {
            let data = args.next().and_then(|a| parse_hex(a, &mut bytes[..32])).ok_or(ApiError::BadArgs("<up to 32 bytes hex>"))?;
            bq.write_mfg_info(data).await?;
        }
        "read_mfg_info_c" => {
            bq.read_mfg_info_c(&mut bytes[..32]).await?;
            let _ = write!(reply, " {name}=");
            hex_into(reply, &bytes[..32]);
        }
        "write_mfg_info_c" => {
            let usage = ApiError::BadArgs("<key_lower u16> <key_upper u16> <up to 32 bytes hex>");
            let (Some(lower), Some(upper)) = (parse_num::<u16>(args.next()), parse_num::<u16>(args.next())) else {
                return Err(usage);
            };
            let data = args.next().and_then(|a| parse_hex(a, &mut bytes[..32])).ok_or(usage)?;
            bq.write_mfg_info_c(lower, upper, data).await?;
        }
        "read_charging_voltage_override" => {
            let o = bq.read_charging_voltage_override().await?;
            let _ = write!(
                reply,
                " {name}={}/{}/{}/{}/{}mV",
                o.low_temp_chrg_mv, o.std_low_temp_chrg_mv, o.std_hi_temp_chrg_mv, o.hi_temp_chrg_mv, o.recommended_temp_chrg_mv
            );
        }
        "write_charging_voltage_override" => {
            let mut mv = [0u16; 5];
            for v in &mut mv {
                *v = parse_num(args.next()).ok_or(ApiError::BadArgs(
                    "<low_temp_mv> <std_low_temp_mv> <std_hi_temp_mv> <hi_temp_mv> <recommended_temp_mv>",
                ))?;
            }
            bq.write_charging_voltage_override(&bq41z50::ChargingVoltageOverride {
                low_temp_chrg_mv: mv[0],
                std_low_temp_chrg_mv: mv[1],
                std_hi_temp_chrg_mv: mv[2],
                hi_temp_chrg_mv: mv[3],
                recommended_temp_chrg_mv: mv[4],
            })
            .await?;
        }
        "read_instruction_flash_signature" => {
            let sig = bq.read_instruction_flash_signature(delay).await?;
            let _ = write!(reply, " {name}=0x{:04X}", sig.insn_flsh_sig());
        }
        "read_static_df_signature" => {
            let sig = bq.read_static_df_signature(delay).await?;
            let _ = write!(reply, " {name}=0x{:04X}", sig.static_df_sig());
        }
        "read_all_df_signature" => {
            let sig = bq.read_all_df_signature(delay).await?;
            let _ = write!(reply, " {name}=0x{:04X}", sig.static_chem_df_sig());
        }
        "set_remaining_capacity_alarm" => {
            let v = parse_num(args.next()).ok_or(ApiError::BadArgs("<u16>"))?;
            bq.set_remaining_capacity_alarm(CapacityModeValue::MilliAmpUnsigned(v)).await?;
        }
        "set_remaining_time_alarm" => {
            let v = parse_num(args.next()).ok_or(ApiError::BadArgs("<minutes u16>"))?;
            bq.set_remaining_time_alarm(v).await?;
        }
        "set_battery_mode" => {
            let v: u16 = parse_num(args.next()).ok_or(ApiError::BadArgs("<flags u16>"))?;
            bq.set_battery_mode(BatteryModeFields::from(v)).await?;
        }
        "set_at_rate" => {
            let v = parse_num(args.next()).ok_or(ApiError::BadArgs("<i16>"))?;
            bq.set_at_rate(CapacityModeSignedValue::MilliAmpSigned(v)).await?;
        }
        _ => return Err(ApiError::UnknownName),
    }
    Ok(())
}

/// Runs one command line and returns the reply, without the trailing CRLF. `delay` is for the
/// signature reads, which wait for the gauge.
///
/// `scan`, `smb`, `lines` and `bootsel`'s reboot need the board, so the firmware handles those
/// itself; `scan_bus` and `smb_raw` below do the bus part.
pub async fn handle_command<B: I2c>(bq: &mut Bq41z50<B>, delay: &mut impl DelayNs, line: &[u8]) -> Reply {
    let mut reply = Reply::new();
    let Ok(line) = core::str::from_utf8(line) else {
        let _ = reply.push_str("err command is not valid text");
        return reply;
    };
    let mut words = line.split_whitespace();
    let mut fields: Vec<Field, 64> = Vec::new();
    match words.next() {
        Some("ping") => {
            let _ = reply.push_str("ok pong");
            return reply;
        }
        Some("bootsel") => {
            let _ = reply.push_str("ok rebooting to BOOTSEL");
            return reply;
        }
        Some("help") => {
            let _ = reply.push_str(
                "ok commands: ping, help, fields, read, get <field>..., reg <name>..., regw <name> <hex>, \
                 mac <name>..., run <name>, buf <name>..., bufw <name> <hex>, call <function> [args], scan, lines, smb, bootsel",
            );
            return reply;
        }
        Some("fields") => {
            let _ = reply.push_str("ok fields:");
            for (name, _) in FIELDS {
                let _ = write!(reply, " {name}");
            }
            return reply;
        }
        Some("read") => fields.extend_from_slice(&READ_FIELDS).unwrap(),
        Some(cmd @ ("reg" | "mac" | "buf")) => {
            let kind = match cmd {
                "reg" => Raw::Register,
                "mac" => Raw::Command,
                _ => Raw::Buffer,
            };
            read_raw_items(bq, kind, words, &mut reply).await;
            return reply;
        }
        Some(cmd @ ("regw" | "bufw")) => {
            let mut bytes = [0u8; driver_api::MAX_BYTES];
            let name = words.next().unwrap_or("");
            let result = match words.next().and_then(|hex| parse_hex(hex, &mut bytes)) {
                None => Err(ApiError::BadArgs("<name> <hex bytes>")),
                Some(data) if cmd == "regw" => driver_api::write_register(bq, name, data).await,
                Some(data) => driver_api::write_buffer(bq, name, data).await,
            };
            match result {
                Ok(()) => {
                    let _ = reply.push_str("ok");
                }
                Err(e) => write_api_error(&mut reply, if name.is_empty() { cmd } else { name }, e),
            }
            return reply;
        }
        Some("run") => {
            let name = words.next().unwrap_or("");
            match driver_api::run_command(bq, name).await {
                Ok(()) => {
                    let _ = reply.push_str("ok");
                }
                Err(e) => write_api_error(&mut reply, name, e),
            }
            return reply;
        }
        Some("call") => {
            let name = words.next().unwrap_or("");
            if let Err(e) = call_helper(bq, delay, name, words, &mut reply).await {
                write_api_error(&mut reply, name, e);
            }
            return reply;
        }
        Some("get") => {
            for name in words {
                let Some(field) = Field::parse(name) else {
                    let _ = write!(reply, "err unknown field '{name}' (try fields)");
                    return reply;
                };
                if fields.push(field).is_err() {
                    let _ = reply.push_str("err too many fields");
                    return reply;
                }
            }
            if fields.is_empty() {
                let _ = reply.push_str("err get needs at least one field");
                return reply;
            }
        }
        Some(other) => {
            let _ = write!(reply, "err unknown command '{other}' (try help)");
            return reply;
        }
        None => {
            let _ = reply.push_str("err empty command");
            return reply;
        }
    }

    let _ = reply.push_str("ok");
    let mut failures = 0;
    let mut last_error = None;
    for &field in &fields {
        if reply.capacity() - reply.len() < REPLY_HEADROOM {
            reply.clear();
            let _ = reply.push_str("err reply too long - ask for fewer fields");
            return reply;
        }
        let _ = write!(reply, " {}=", field.name());
        let start = reply.len();
        if let Err(e) = read_field(bq, field, &mut reply).await {
            reply.truncate(start);
            let _ = reply.push('!');
            failures += 1;
            last_error = Some(e);
        }
    }
    // Every field failing usually means the gauge isn't answering at all, so say why.
    if let Some(e) = last_error.filter(|_| failures == fields.len()) {
        reply.clear();
        let _ = write!(reply, "err gauge: {e:?}");
    }
    reply
}

/// `scan`: every 7-bit address that ACKs a one-byte read.
pub async fn scan_bus<B: I2c>(bus: &mut B) -> Reply {
    let mut reply = Reply::new();
    let _ = reply.push_str("ok scan found:");
    let mut found = false;
    let mut byte = [0u8; 1];
    for address in 0x08..=0x77u8 {
        if bus.read(address, &mut byte).await.is_ok() {
            let _ = write!(reply, " 0x{address:02x}");
            found = true;
        }
    }
    if !found {
        let _ = reply.push_str(" none");
    }
    reply
}

/// Gauge SMBus address, and the ManufacturerBlockAccess command used for MAC reads.
const GAUGE_ADDRESS: u8 = 0x0B;
const MANUFACTURER_BLOCK_ACCESS: u8 = 0x44;

/// `smb`: plain SMBus reads straight on the bus, without the driver, as ground truth for it.
///   word <cmd>             read word
///   block <cmd>            block read; prints the length byte the gauge sends, then the data
///   mac <code> [delay_ms]  ManufacturerBlockAccess: write the code, optionally wait, block read
pub async fn smb_raw<B: I2c>(bus: &mut B, delay: &mut impl DelayNs, args: &[u8]) -> Reply {
    let mut reply = Reply::new();
    let text = core::str::from_utf8(args).unwrap_or("");
    let mut words = text.split_whitespace();
    let kind = words.next();
    let code: Option<u16> = parse_num(words.next());
    let wait_ms: u32 = parse_num(words.next()).unwrap_or(0);
    let mut data = [0u8; 40];
    let result = match (kind, code) {
        (Some("word"), Some(cmd)) => bus.write_read(GAUGE_ADDRESS, &[cmd as u8], &mut data[..2]).await.map(|_| 2),
        (Some("block"), Some(cmd)) => bus.write_read(GAUGE_ADDRESS, &[cmd as u8], &mut data[..34]).await.map(|_| 34),
        (Some("mac"), Some(code)) => {
            let [lo, hi] = code.to_le_bytes();
            match bus.write(GAUGE_ADDRESS, &[MANUFACTURER_BLOCK_ACCESS, 2, lo, hi]).await {
                Ok(()) => {
                    delay.delay_ms(wait_ms).await;
                    bus.write_read(GAUGE_ADDRESS, &[MANUFACTURER_BLOCK_ACCESS], &mut data[..36]).await.map(|_| 36)
                }
                Err(e) => Err(e),
            }
        }
        _ => {
            let _ = reply.push_str("err usage: smb word <cmd> | smb block <cmd> | smb mac <code> [delay_ms]");
            return reply;
        }
    };
    match result {
        Err(e) => {
            let _ = write!(reply, "err bus: {e:?}");
        }
        Ok(2) => {
            let _ = reply.push_str("ok ");
            hex_into(&mut reply, &data[..2]);
        }
        Ok(n) => {
            let len = (data[0] as usize).min(n - 1);
            let _ = write!(reply, "ok len={} data=", data[0]);
            hex_into(&mut reply, &data[1..=len]);
        }
    }
    reply
}
