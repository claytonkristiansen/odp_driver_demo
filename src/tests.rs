//! The command protocol against a simulated gauge (src/sim_gauge.rs). Run with `cargo test-host`.

use std::collections::HashMap;
use std::string::{String as StdString, ToString};
use std::vec::Vec as StdVec;
use std::{format, vec};

use embassy_futures::block_on;

use crate::driver_api::{ACTIONS, BUFFERS, READ_COMMANDS, REGISTERS};
use crate::sim_gauge::{MBA, NoDelay, SimGauge, Transfer};
use crate::*;

/// The driver on a simulated gauge, and the delay the signature reads wait on.
struct Bench {
    bq: Bq41z50<SimGauge>,
    sim: SimGauge,
    delay: NoDelay,
}

impl Bench {
    fn new() -> Self {
        let sim = SimGauge::default();
        Self { bq: Bq41z50::new(sim.clone()), sim, delay: NoDelay::default() }
    }

    fn gauge(&self) -> &SimGauge {
        &self.sim
    }

    /// Runs one command line, as the firmware does for each line it receives.
    fn cmd(&mut self, line: &str) -> StdString {
        block_on(handle_command(&mut self.bq, &mut self.delay, line.as_bytes())).as_str().to_string()
    }

    /// Runs a command that must succeed and returns its `name=value` pairs.
    fn values(&mut self, line: &str) -> HashMap<StdString, StdString> {
        let reply = self.cmd(line);
        assert!(reply.starts_with("ok"), "{line} -> {reply}");
        let mut values = HashMap::new();
        let mut rest = reply[2..].trim_start();
        while !rest.is_empty() {
            let (name, after) = rest.split_once('=').unwrap_or_else(|| panic!("no '=' in {rest:?}"));
            // Quoted values may contain spaces.
            let end = if after.starts_with('"') { after[1..].find('"').map_or(after.len(), |i| i + 2) } else { after.find(' ').unwrap_or(after.len()) };
            values.insert(name.to_string(), after[..end].to_string());
            rest = after[end..].trim_start();
        }
        values
    }

    fn value(&mut self, line: &str, name: &str) -> StdString {
        self.values(line).remove(name).unwrap_or_else(|| panic!("{line}: no {name}"))
    }
}

// ---- Commands that don't touch the gauge -----------------------------------------------------

#[test]
fn ping_help_fields_and_bootsel() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("ping"), "ok pong");
    assert!(b.cmd("help").starts_with("ok commands: ping, help, fields, read, get <field>..."));
    let expected: StdVec<&str> = FIELDS.iter().map(|&(n, _)| n).collect();
    assert_eq!(b.cmd("fields"), format!("ok fields: {}", expected.join(" ")));
    assert_eq!(b.cmd("bootsel"), "ok rebooting to BOOTSEL");
    assert!(b.gauge().log().is_empty(), "none of these should touch the bus");
}

#[test]
fn bad_command_lines() {
    let mut b = Bench::new();
    assert_eq!(b.cmd(""), "err empty command");
    assert_eq!(b.cmd("   "), "err empty command");
    assert_eq!(b.cmd("frobnicate now"), "err unknown command 'frobnicate' (try help)");
    let reply = block_on(handle_command(&mut b.bq, &mut b.delay, &[0x67, 0x65, 0x74, 0xff]));
    assert_eq!(reply.as_str(), "err command is not valid text");
}

// ---- get / read ------------------------------------------------------------------------------

#[test]
fn get_formats_each_value_with_its_unit() {
    let mut b = Bench::new();
    b.gauge()
        .set_word(0x08, 2982) // 298.2 K
        .set_word(0x09, 15_900)
        .set_word(0x0A, (-1500i16) as u16)
        .set_word(0x0D, 87)
        .set_word(0x0F, 2500)
        .set_word(0x16, 0x00C0)
        .set_word(0x17, 42)
        .set_word(0x1B, (45 << 9) | (3 << 5) | 14) // 2025-03-14; the year is biased by 1980
        .set_word(0x3F, 3976)
        .set_word(0x3E, 3974)
        .set_word(0x3D, 3968)
        .set_word(0x3C, 3982)
        .set_block(0x20, b"Texas Instruments")
        .set_mac(0x0006, &0x1210u16.to_le_bytes())
        .set_mac(0x0051, &0x0000_0102u32.to_le_bytes())
        // The bytes a real bq41z50-R1 sends: device 0x4150, version 1.02, build 36.
        .set_mac(0x0002, &[0x41, 0x50, 0x01, 0x02, 0x00, 0x24, 0x00, 0x04, 0x75, 0x03, 0x00]);
    let v = b.values(
        "get temp voltage current soc remaining_cap battery_status cycle_count manufacture_date cells \
         manufacturer_name chem_id safety_status firmware_version",
    );
    let expect = [
        ("temp", "25.05C"),
        ("voltage", "15900mV"),
        ("current", "-1500mA"),
        ("soc", "87%"),
        ("remaining_cap", "2500mAh"),
        ("battery_status", "0x00C0"),
        ("cycle_count", "42"),
        ("manufacture_date", "2025-03-14"),
        ("cells", "3976/3974/3968/3982mV"),
        ("manufacturer_name", "\"Texas Instruments\""),
        ("chem_id", "0x1210"),
        ("safety_status", "0x00000102"),
        ("firmware_version", "\"device 0x4150 version 1.02 build 36\""),
    ];
    for (name, value) in expect {
        assert_eq!(v[name], value, "{name}");
    }
    assert_eq!(v.len(), expect.len());
}

#[test]
fn every_field_answers_from_a_gauge_reading_zeros() {
    let mut b = Bench::new();
    let names: StdVec<&str> = FIELDS.iter().map(|&(n, _)| n).collect();
    // The page asks in batches of 10.
    for batch in names.chunks(10) {
        let v = b.values(&format!("get {}", batch.join(" ")));
        for name in batch {
            assert!(v.get(*name).is_some_and(|x| x != "!"), "{name}: {:?}", v.get(*name));
        }
    }
}

#[test]
fn capacities_follow_the_capacity_mode() {
    let mut b = Bench::new();
    b.gauge().set_word(0x0F, 2500);
    assert_eq!(b.value("get remaining_cap", "remaining_cap"), "2500mAh");
    assert_eq!(b.cmd("call set_battery_mode 0x8000"), "ok");
    assert_eq!(b.value("get remaining_cap", "remaining_cap"), "2500cWh");
}

#[test]
fn read_is_the_charted_values() {
    let mut b = Bench::new();
    let v = b.values("read");
    let mut names: StdVec<&str> = v.keys().map(StdString::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["cells", "current", "soc", "temp", "voltage"]);
}

#[test]
fn get_errors() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("get"), "err get needs at least one field");
    assert_eq!(b.cmd("get soc nope"), "err unknown field 'nope' (try fields)");
    assert_eq!(b.cmd(&format!("get{}", " soc".repeat(65))), "err too many fields");
}

#[test]
fn a_refused_field_is_marked_and_the_rest_still_read() {
    let mut b = Bench::new();
    b.gauge().set_word(0x09, 15_000).refuse_command(0x08);
    let v = b.values("get temp voltage");
    assert_eq!(v["temp"], "!");
    assert_eq!(v["voltage"], "15000mV");
}

#[test]
fn every_field_refused_fails_the_command_with_the_reason() {
    let mut b = Bench::new();
    b.gauge().set_absent(true);
    let reply = b.cmd("get temp voltage");
    assert!(reply.starts_with("err gauge: Bus(BusError(NoAcknowledge(Address)))"), "{reply}");
}

// ---- reg / mac / buf: every generated arm ----------------------------------------------------

#[test]
fn every_register_reads_its_size_in_wire_order() {
    let mut b = Bench::new();
    b.gauge().set_word(0x08, 0x0BA0);
    assert_eq!(b.cmd("reg temperature"), "ok temperature=a00b");
    assert_eq!(b.gauge().log().last(), Some(&Transfer::WriteRead(vec![0x08], 2)));
    for &(name, size, _) in REGISTERS {
        let raw = b.value(&format!("reg {name}"), name);
        assert_eq!(raw.len(), size * 2, "{name}: {raw}");
    }
}

#[test]
fn every_mac_read_sends_its_code_then_reads_the_response() {
    let mut b = Bench::new();
    b.gauge().set_mac(0x0006, &[0x12, 0x10]);
    assert_eq!(b.cmd("mac mac_chem_id"), "ok mac_chem_id=1210");
    assert_eq!(b.gauge().log()[0], Transfer::Write(vec![MBA, 2, 0x06, 0x00]));
    assert_eq!(b.gauge().log()[1], Transfer::WriteRead(vec![MBA], 3 + 2));
    for &(name, size) in READ_COMMANDS {
        let raw = b.value(&format!("mac {name}"), name);
        assert_eq!(raw.len(), size * 2, "{name}: {raw}");
    }
}

#[test]
fn sbs_block_reads_drop_the_count_byte() {
    let mut b = Bench::new();
    b.gauge().set_block(0x51, &[0x02, 0x01, 0x00, 0x00]);
    assert_eq!(b.cmd("mac safety_status"), "ok safety_status=02010000");
    assert_eq!(b.gauge().log(), [Transfer::WriteRead(vec![0x51], 5)]);
}

#[test]
fn every_action_sends_only_its_mac_code() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("run mac_seal"), "ok");
    assert_eq!(b.gauge().log(), [Transfer::Write(vec![MBA, 2, 0x30, 0x00])]);
    for name in ACTIONS {
        let before = b.gauge().log().len();
        assert_eq!(b.cmd(&format!("run {name}")), "ok", "{name}");
        assert_eq!(b.gauge().log().len(), before + 1, "{name} should be one write");
    }
}

#[test]
fn every_buffer_reads_a_length_byte_and_32_data_bytes() {
    let mut b = Bench::new();
    b.gauge().set_block(0x21, b"bq41z50");
    let raw = b.value("buf device_name", "device_name");
    assert!(raw.starts_with("07627134317a3530"), "{raw}");
    for &(name, _) in BUFFERS {
        assert_eq!(b.value(&format!("buf {name}"), name).len(), 33 * 2, "{name}");
    }
}

#[test]
fn reads_and_actions_are_not_interchangeable() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("mac mac_seal"), "err mac_seal: it is an action; use run");
    assert_eq!(b.cmd("run mac_chem_id"), "err mac_chem_id: it returns data; use mac");
    assert_eq!(b.cmd("reg nope"), "err unknown name 'nope'");
    assert_eq!(b.cmd("mac nope"), "err unknown name 'nope'");
    assert_eq!(b.cmd("buf nope"), "err unknown name 'nope'");
    assert_eq!(b.cmd("run nope"), "err unknown name 'nope'");
    assert_eq!(b.cmd("reg"), "err name at least one item");
}

#[test]
fn several_items_in_one_command_and_a_refused_one() {
    let mut b = Bench::new();
    b.gauge().set_word(0x09, 0x3E80).refuse_command(0x08);
    assert_eq!(b.cmd("reg voltage temperature"), "ok voltage=803e temperature=!");
    b.gauge().refuse_command(0x09);
    let reply = b.cmd("reg voltage temperature");
    assert!(reply.starts_with("err gauge: Bus(BusError(NoAcknowledge(Data)))"), "{reply}");
    b.gauge().set_mac(0x0001, &[0x41, 0x50]).refuse_mac(0x0006);
    assert_eq!(b.cmd("mac mac_device_type mac_chem_id"), "ok mac_device_type=4150 mac_chem_id=!");
}

#[test]
fn a_reply_that_would_overflow_asks_for_fewer_items() {
    let mut b = Bench::new();
    let all: StdVec<&str> = READ_COMMANDS.iter().map(|&(n, _)| n).collect();
    assert_eq!(b.cmd(&format!("mac {}", all.join(" "))), "err reply too long - ask for fewer items");
}

// ---- regw / bufw -----------------------------------------------------------------------------

#[test]
fn register_writes_go_out_as_one_write_and_read_back() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("regw at_rate 18fc"), "ok");
    assert_eq!(b.gauge().log(), [Transfer::Write(vec![0x04, 0x18, 0xFC])]);
    assert_eq!(b.cmd("reg at_rate"), "ok at_rate=18fc");
    for &(name, size, writable) in REGISTERS {
        let data = "00".repeat(size);
        let expected = if writable { "ok".to_string() } else { format!("err {name} is read-only") };
        assert_eq!(b.cmd(&format!("regw {name} {data}")), expected);
    }
}

#[test]
fn register_write_errors() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("regw at_rate 00"), "err at_rate takes exactly 2 bytes");
    assert_eq!(b.cmd("regw at_rate 0x0000"), "ok");
    assert_eq!(b.cmd("regw at_rate zz00"), "err usage: at_rate <name> <hex bytes>");
    assert_eq!(b.cmd("regw at_rate"), "err usage: at_rate <name> <hex bytes>");
    assert_eq!(b.cmd("regw"), "err usage: regw <name> <hex bytes>");
    assert_eq!(b.cmd("regw nope 0000"), "err unknown name 'nope'");
}

#[test]
fn buffer_writes() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("bufw authenticate 0102"), "ok");
    assert_eq!(b.gauge().log(), [Transfer::Write(vec![0x2F, 0x01, 0x02])]);
    assert_eq!(b.cmd("bufw device_name 00"), "err device_name is read-only");
    for &(name, writable) in BUFFERS {
        let expected = if writable { "ok".to_string() } else { format!("err {name} is read-only") };
        assert_eq!(b.cmd(&format!("bufw {name} 00")), expected);
    }
}

// ---- call ------------------------------------------------------------------------------------

#[test]
fn call_reads() {
    let mut b = Bench::new();
    b.gauge()
        .set_word(0x08, 2982)
        .set_mac(0x00B0, &[0xA0, 0x0F, 0x68, 0x10, 0x68, 0x10, 0x04, 0x10, 0x68, 0x10]);
    assert_eq!(b.cmd("call temperature_fahrenheit"), "ok temperature_fahrenheit=77.09F");
    assert_eq!(b.cmd("call read_charging_voltage_override"), "ok read_charging_voltage_override=4000/4200/4200/4100/4200mV");
    assert_eq!(b.value("call read_mfg_info", "read_mfg_info").len(), 33 * 2);
    assert_eq!(b.value("call read_mfg_info_c", "read_mfg_info_c").len(), 32 * 2);
}

#[test]
fn keys_written_with_call_read_back() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("call write_security_keys 0102030405060708"), "ok");
    assert_eq!(b.gauge().log(), [Transfer::Write(vec![MBA, 10, 0x35, 0x00, 1, 2, 3, 4, 5, 6, 7, 8])]);
    assert_eq!(b.cmd("call read_security_keys"), "ok read_security_keys=0102030405060708");
    let key = "00112233445566778899aabbccddeeff";
    assert_eq!(b.cmd(&format!("call write_authentication_key {key}")), "ok");
    assert_eq!(b.cmd("call read_authentication_key"), format!("ok read_authentication_key={key}"));
}

#[test]
fn unseal_sends_both_key_halves_decimal_or_hex() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("call unseal_fg 0x0414 13938"), "ok");
    assert_eq!(b.gauge().mac_codes_sent(), [0x0414, 0x3672]);
    assert_eq!(b.cmd("call seal_fg"), "ok");
    assert_eq!(b.gauge().mac_codes_sent().last(), Some(&0x0030));
}

#[test]
fn setters_write_their_register() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("call set_at_rate -500"), "ok");
    assert_eq!(b.gauge().log(), [Transfer::Write(vec![0x04, 0x0C, 0xFE])]);
    assert_eq!(b.cmd("call set_remaining_time_alarm 10"), "ok");
    assert_eq!(b.gauge().log().last(), Some(&Transfer::Write(vec![0x02, 10, 0])));
}

#[test]
fn signature_reads_wait_for_the_gauge() {
    let mut b = Bench::new();
    b.gauge().set_mac(0x0004, &0xF453u16.to_le_bytes());
    assert_eq!(b.cmd("call read_instruction_flash_signature"), "ok read_instruction_flash_signature=0xF453");
    assert_eq!(b.delay.total_ns, 500_000_000);
    b.cmd("call read_static_df_signature");
    b.cmd("call read_all_df_signature");
    assert_eq!(b.delay.total_ns, 2_500_000_000);
}

#[test]
fn call_argument_errors() {
    let mut b = Bench::new();
    assert_eq!(b.cmd("call nope"), "err unknown name 'nope'");
    assert_eq!(b.cmd("call unseal_fg 1"), "err usage: unseal_fg <key_lower u16> <key_upper u16>");
    assert_eq!(b.cmd("call unseal_fg 1 70000"), "err usage: unseal_fg <key_lower u16> <key_upper u16>");
    assert_eq!(b.cmd("call write_security_keys 0102"), "err write_security_keys takes exactly 8 bytes");
    assert_eq!(b.cmd("call write_security_keys"), "err usage: write_security_keys <8 bytes hex>");
    assert_eq!(b.cmd("call set_at_rate x"), "err usage: set_at_rate <i16>");
    assert!(b.gauge().log().is_empty(), "nothing should reach the bus");
}

// ---- smb and scan ----------------------------------------------------------------------------

fn smb(b: &mut Bench, args: &str) -> StdString {
    let mut bus = b.sim.clone();
    block_on(smb_raw(&mut bus, &mut b.delay, args.as_bytes())).as_str().to_string()
}

#[test]
fn smb_bypasses_the_driver() {
    let mut b = Bench::new();
    b.gauge().set_word(0x09, 0x3E80).set_block(0x54, &[0x00, 0x61, 0x00, 0x00]).set_mac(0x0054, &[0x00, 0x61]);
    assert_eq!(smb(&mut b, "word 0x09"), "ok 803e");
    assert_eq!(smb(&mut b, "block 0x54"), "ok len=4 data=00610000");
    // The count byte covers the echoed code and the 33 data bytes the 36-byte read leaves.
    assert_eq!(smb(&mut b, "mac 0x0054 20"), format!("ok len=35 data=54000061{}", "00".repeat(31)));
    assert_eq!(b.delay.total_ns, 20_000_000);
    assert!(smb(&mut b, "nope").starts_with("err usage: smb word <cmd>"));
    b.gauge().set_absent(true);
    assert_eq!(smb(&mut b, "word 0x09"), "err bus: BusError(NoAcknowledge(Address))");
}

#[test]
fn scan_finds_the_gauge() {
    let mut bus = SimGauge::default();
    assert_eq!(block_on(scan_bus(&mut bus)).as_str(), "ok scan found: 0x0b");
    bus.set_absent(true);
    assert_eq!(block_on(scan_bus(&mut bus)).as_str(), "ok scan found: none");
}

// ---- parsing helpers -------------------------------------------------------------------------

#[test]
fn hex_parsing() {
    let mut out = [0u8; 4];
    assert_eq!(parse_hex("0a1B", &mut out), Some(&[0x0A, 0x1B][..]));
    assert_eq!(parse_hex("0x0a1b", &mut out), Some(&[0x0A, 0x1B][..]));
    assert_eq!(parse_hex("", &mut out), Some(&[][..]));
    assert_eq!(parse_hex("abc", &mut out), None, "odd length");
    assert_eq!(parse_hex("zz", &mut out), None);
    assert_eq!(parse_hex("0102030405", &mut out), None, "longer than the buffer");
}

#[test]
fn number_parsing() {
    assert_eq!(parse_num::<u16>(Some("1044")), Some(1044));
    assert_eq!(parse_num::<u16>(Some("0x0414")), Some(0x0414));
    assert_eq!(parse_num::<i16>(Some("-500")), Some(-500));
    assert_eq!(parse_num::<i16>(Some("-0x10")), Some(-16));
    assert_eq!(parse_num::<u16>(Some("-1")), None);
    assert_eq!(parse_num::<u16>(Some("70000")), None);
    assert_eq!(parse_num::<u16>(Some("ten")), None);
    assert_eq!(parse_num::<u16>(None), None);
}
