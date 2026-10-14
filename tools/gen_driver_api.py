#!/usr/bin/env python3
"""Generate the firmware's register/command/buffer dispatch and the dashboard's metadata from
the BQ41Z50 driver's device description (bq41z50.ddsl).

    python3 tools/gen_driver_api.py [path/to/bq41z50.ddsl]

Writes:
  src/driver_api.rs     one match arm per register, MAC command and buffer, each calling the
                        driver's generated accessor (bq.device.<name>()...)
  tools/driver_api.json names, addresses, sizes, access, docs and bit fields for the web page

Re-run it whenever the driver's .ddsl changes; both outputs are checked in.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DDSL = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT.parent / "bq41z50" / "bq41z50.ddsl"

# Largest register/command payload the firmware buffers (the driver's LARGEST_REG_BYTES).
MAX_BYTES = 64
# SBS block reads return a length byte plus up to 32 bytes.
BUFFER_READ_BYTES = 33

# MAC commands that change the gauge's state in ways that are hard or impossible to undo.
# The page makes you type the name before running these.
DISRUPTIVE = {
    "MAC_SHUTDOWN_MODE", "MAC_SLEEP_MODE", "MAC_FUSE_TOGGLE", "MAC_FUSE", "MAC_PERMANENT_FAILURE",
    "MAC_SEAL", "MAC_DEVICE_RESET", "MAC_ROM_MODE", "MAC_LIFETIME_DATA_RST", "MAC_PF_DATA_RST",
    "MAC_BLK_BOX_REC_RESET", "MAC_CALIBRATION_MODE", "MAC_GAUGING", "MAC_FET_CTRL",
    "MAC_CHG_FET_TOGGLE", "MAC_DSG_FET_TOGGLE", "MAC_PCHG_FET_TOGGLE", "MAC_LIFETIME_DATA_COLLECTION",
    "MAC_BLACK_BLOCK_RECORDER", "MAC_PF_DATA_RST", "MAC_EXIT_CALIBRATION_OUTPUT_MODE",
    "MAC_STOP_OUTPUT_CCADC_CAL", "MAC_OUTPUT_SHORTED_CCADC_CAL", "MAC_STOP_OUTPUT_SHORTED_CCADC_CAL",
    "MAC_LIFETIME_DATA_SPEED_UP_MODE", "MAC_AUTO_CC_OFFSET", "MAC_LIFETIME_DATA_FLUSH",
}
# Commands that return data but also change state, so "read all" and watching skip them.
SIDE_EFFECT_READS = {"MAC_OUTPUT_CCADC_CAL", "MAC_OUTPUT_SHORTED_CCADC_CAL"}

# The driver's hand-written functions (lib.rs and the SmartBattery setters), dispatched by the
# firmware's `call` command. [name, args, description, disruptive]
HELPERS = [
    ["temperature_fahrenheit", [], "Temperature in °F (driver conversion)", False],
    ["read_security_keys", [], "Read the unseal / full-access keys (MAC 0x0035)", False],
    ["write_security_keys", ["keys: 8 bytes hex"], "Write the unseal / full-access keys (MAC 0x0035)", True],
    ["read_authentication_key", [], "Read the SHA authentication key (MAC 0x0037)", False],
    ["write_authentication_key", ["key: 16 bytes hex"], "Write the SHA authentication key (MAC 0x0037)", True],
    ["seal_fg", [], "Seal the gauge (MAC 0x0030)", True],
    ["unseal_fg", ["key_lower: u16", "key_upper: u16"], "Unseal with the unseal key", False],
    ["send_access_key", ["key_lower: u16", "key_upper: u16"], "Send an access key (unseal or full access)", False],
    ["read_mfg_info", [], "Read ManufacturerInfo (0x70)", False],
    ["write_mfg_info", ["data: up to 32 bytes hex"], "Write ManufacturerInfo (0x70)", True],
    ["read_mfg_info_c", [], "Read ManufacturerInfoC (MAC 0x007B)", False],
    ["write_mfg_info_c", ["key_lower: u16", "key_upper: u16", "data: up to 32 bytes hex"],
     "Write ManufacturerInfoC (MAC 0x007B)", True],
    ["read_charging_voltage_override", [], "Read the charging voltage override (MAC 0x00B0)", False],
    ["write_charging_voltage_override",
     ["low_temp_mv", "std_low_temp_mv", "std_hi_temp_mv", "hi_temp_mv", "recommended_temp_mv"],
     "Write the charging voltage override (MAC 0x00B0)", True],
    ["read_instruction_flash_signature", [], "Instruction flash signature (MAC 0x0004, waits 0.5 s)", False],
    ["read_static_df_signature", [], "Static data flash signature (MAC 0x0005, waits 1 s)", False],
    ["read_all_df_signature", [], "All data flash signature (MAC 0x0009, waits 1 s)", False],
    ["set_remaining_capacity_alarm", ["value: u16"], "SmartBattery::set_remaining_capacity_alarm (0x01)", False],
    ["set_remaining_time_alarm", ["minutes: u16"], "SmartBattery::set_remaining_time_alarm (0x02)", False],
    ["set_battery_mode", ["flags: u16"], "SmartBattery::set_battery_mode (0x03)", True],
    ["set_at_rate", ["value: i16"], "SmartBattery::set_at_rate (0x04)", False],
]


def parse(text):
    items, docs, item, fieldset, field = [], [], None, None, None
    # Named fieldsets, so `fields-out: OtherCommandFieldsOut,` can reuse one.
    named = {}
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("///"):
            docs.append(line[3:].strip())
            continue
        if not line or line.startswith("//"):
            if item is None:
                docs = [] if not line.startswith("//") else docs
            continue
        m = re.match(r"(register|command|buffer) (\w+) \{$", line)
        if m and item is None:
            item = {"kind": m[1], "name": m[2], "doc": " ".join(d for d in docs if d), "access": "RW",
                    "size": None, "fields": []}
            docs = []
            continue
        if item is None:
            docs = []
            continue
        if field is not None and field.get("enum") is not None:
            if line.startswith("}"):
                field = None
                continue
            v = re.match(r"(\w+)\s*(?:=|:)\s*(\w+)", line)
            if v:
                field["enum"][str(int(v[2], 0))] = v[1]
            continue
        if m := re.match(r"address: (0x[0-9A-Fa-f]+|\d+),", line):
            item["address"] = int(m[1], 0)
        elif m := re.match(r"access: (\w+),", line):
            item["access"] = m[1]
        elif m := re.match(r"fields(?:-out)?: fieldset (\w+)", line):
            fieldset = m[1]
            item["fieldsetName"] = m[1]
            if "fields-in" in line:
                raise SystemExit(f"{item['name']}: commands with input fields aren't supported")
        elif m := re.match(r"fields(?:-out)?: (\w+),$", line):
            item["reuses"] = m[1]
        elif line.startswith("fields-in"):
            raise SystemExit(f"{item['name']}: commands with input fields aren't supported")
        elif m := re.match(r"size-bytes: (\d+),", line):
            item["size"] = int(m[1])
        elif m := re.match(r"field (\w+) (\d+)(?::(\d+))?(?: (\w+))? -> (\w+)(?: as (enum )?(\w+))?(.*)$", line):
            hi = int(m[2])
            lo = int(m[3]) if m[3] is not None else hi
            field = {"name": m[1], "lo": lo, "hi": hi, "type": m[5]}
            if m[7]:
                field["enumName"] = m[7]
            if m[6] and m[8].strip().startswith("{"):
                field["enum"] = {}
            item["fields"].append(field)
            if field.get("enum") is None:
                field = None
        elif line in ("},", "}"):
            if fieldset:
                fieldset = None
            else:
                items.append(item)
                item = None
    for i in items:
        if "fieldsetName" in i:
            named[i.pop("fieldsetName")] = i
    for i in items:
        if "reuses" in i:
            src = named[i.pop("reuses")]
            i["size"], i["fields"] = src["size"], src["fields"]
    # Enums declared once and referenced again elsewhere ("as DegradationMode") share variants.
    enums = {f["enumName"]: f["enum"] for i in items for f in i["fields"] if f.get("enum")}
    for i in items:
        for f in i["fields"]:
            if "enumName" in f and "enum" not in f and f["enumName"] in enums:
                f["enum"] = enums[f["enumName"]]
            f.pop("enumName", None)
    return items


def rust(items):
    regs = [i for i in items if i["kind"] == "register"]
    cmds = [i for i in items if i["kind"] == "command"]
    bufs = [i for i in items if i["kind"] == "buffer"]
    for r in regs:
        assert r["size"] and r["size"] <= MAX_BYTES, r["name"]
    out = []
    w = out.append
    w("// @generated by tools/gen_driver_api.py from the bq41z50 driver's bq41z50.ddsl. Do not edit;")
    w("// re-run the generator instead.")
    w("//")
    w("// One match arm per register, MAC command and buffer. Each arm calls the driver's generated")
    w("// accessor and moves the fieldset's raw little-endian bytes in or out.")
    w("")
    w("use crate::{Gauge, GaugeError};")
    w("")
    w(f"/// Largest register or command payload, in bytes.")
    w(f"pub const MAX_BYTES: usize = {MAX_BYTES};")
    w(f"/// Bytes read for a buffer: the SBS block length byte plus up to 32 data bytes.")
    w(f"pub const BUFFER_READ_BYTES: usize = {BUFFER_READ_BYTES};")
    w("")
    w("pub enum ApiError {")
    w("    UnknownName,")
    w("    ReadOnly,")
    w("    /// A MAC command that returns data was used as an action, or the other way round.")
    w("    WrongKind(&'static str),")
    w("    WrongLength(usize),")
    w("    BadArgs(&'static str),")
    w("    Gauge(GaugeError),")
    w("}")
    w("")
    w("impl From<GaugeError> for ApiError {")
    w("    fn from(e: GaugeError) -> Self {")
    w("        ApiError::Gauge(e)")
    w("    }")
    w("}")
    w("")
    w("fn exact<const N: usize>(data: &[u8]) -> Result<[u8; N], ApiError> {")
    w("    data.try_into().map_err(|_| ApiError::WrongLength(N))")
    w("}")
    w("")

    w("/// Reads a register into `out` and returns its size.")
    w("pub async fn read_register(bq: &mut Gauge, name: &str, out: &mut [u8; MAX_BYTES]) -> Result<usize, ApiError> {")
    w("    let n = match name {")
    for r in regs:
        n, size = r["name"].lower(), r["size"]
        w(f'        "{n}" => {{')
        w(f"            let raw: [u8; {size}] = bq.device.{n}().read_async().await?.into();")
        w(f"            out[..{size}].copy_from_slice(&raw);")
        w(f"            {size}")
        w("        }")
    w("        _ => return Err(ApiError::UnknownName),")
    w("    };")
    w("    Ok(n)")
    w("}")
    w("")

    w("/// Writes a register from its raw bytes (exactly the register's size).")
    w("pub async fn write_register(bq: &mut Gauge, name: &str, data: &[u8]) -> Result<(), ApiError> {")
    w("    match name {")
    ro = [r["name"].lower() for r in regs if r["access"] == "RO"]
    for r in regs:
        if r["access"] == "RO":
            continue
        n, size = r["name"].lower(), r["size"]
        w(f'        "{n}" => {{')
        w(f"            let raw = exact::<{size}>(data)?;")
        w(f"            bq.device.{n}().write_async(|r| *r = raw.into()).await?;")
        w("        }")
    for i in range(0, len(ro), 4):
        chunk = " | ".join(f'"{n}"' for n in ro[i:i + 4])
        sep = " |" if i + 4 < len(ro) else " => return Err(ApiError::ReadOnly),"
        w(f"        {chunk}{sep}")
    w("        _ => return Err(ApiError::UnknownName),")
    w("    }")
    w("    Ok(())")
    w("}")
    w("")

    reads = [c for c in cmds if c["size"]]
    actions = [c for c in cmds if not c["size"]]
    w("/// Runs a MAC command that returns data, into `out`; returns the data size.")
    w("pub async fn read_command(bq: &mut Gauge, name: &str, out: &mut [u8; MAX_BYTES]) -> Result<usize, ApiError> {")
    w("    let n = match name {")
    for c in reads:
        n, size = c["name"].lower(), c["size"]
        w(f'        "{n}" => {{')
        w(f"            let raw: [u8; {size}] = bq.device.{n}().dispatch_out_async().await?.into();")
        w(f"            out[..{size}].copy_from_slice(&raw);")
        w(f"            {size}")
        w("        }")
    names = [c["name"].lower() for c in actions]
    for i in range(0, len(names), 3):
        chunk = " | ".join(f'"{n}"' for n in names[i:i + 3])
        sep = " |" if i + 3 < len(names) else ' => return Err(ApiError::WrongKind("it is an action; use run")),'
        w(f"        {chunk}{sep}")
    w("        _ => return Err(ApiError::UnknownName),")
    w("    };")
    w("    Ok(n)")
    w("}")
    w("")

    w("/// Runs a MAC command that takes and returns nothing (an action).")
    w("pub async fn run_command(bq: &mut Gauge, name: &str) -> Result<(), ApiError> {")
    w("    match name {")
    for c in actions:
        n = c["name"].lower()
        w(f'        "{n}" => bq.device.{n}().dispatch_async().await?,')
    names = [c["name"].lower() for c in reads]
    for i in range(0, len(names), 3):
        chunk = " | ".join(f'"{n}"' for n in names[i:i + 3])
        sep = " |" if i + 3 < len(names) else ' => return Err(ApiError::WrongKind("it returns data; use mac")),'
        w(f"        {chunk}{sep}")
    w("        _ => return Err(ApiError::UnknownName),")
    w("    }")
    w("    Ok(())")
    w("}")
    w("")

    w("/// Reads a buffer (SBS block: length byte, then data) into `out`; returns the bytes read.")
    w("pub async fn read_buffer(bq: &mut Gauge, name: &str, out: &mut [u8; MAX_BYTES]) -> Result<usize, ApiError> {")
    w("    let out = &mut out[..BUFFER_READ_BYTES];")
    w("    Ok(match name {")
    for b in bufs:
        n = b["name"].lower()
        w(f'        "{n}" => bq.device.{n}().read_async(out).await?,')
    w("        _ => return Err(ApiError::UnknownName),")
    w("    })")
    w("}")
    w("")

    w("/// Writes raw bytes to a buffer.")
    w("pub async fn write_buffer(bq: &mut Gauge, name: &str, data: &[u8]) -> Result<(), ApiError> {")
    w("    match name {")
    for b in bufs:
        n = b["name"].lower()
        if b["access"] == "RO":
            continue
        w(f'        "{n}" => {{')
        w(f"            bq.device.{n}().write_async(data).await?;")
        w("        }")
    ro_bufs = [b["name"].lower() for b in bufs if b["access"] == "RO"]
    if ro_bufs:
        w("        " + " | ".join(f'"{n}"' for n in ro_bufs) + " => return Err(ApiError::ReadOnly),")
    w("        _ => return Err(ApiError::UnknownName),")
    w("    }")
    w("    Ok(())")
    w("}")
    return "\n".join(out) + "\n"


def metadata(items):
    def entry(i):
        e = {"name": i["name"].lower(), "address": i.get("address"), "access": i["access"],
             "size": i["size"], "doc": i["doc"], "fields": i["fields"]}
        if i["kind"] == "command":
            e["kind"] = "read" if i["size"] else "action"
            e["disruptive"] = i["name"] in DISRUPTIVE
            e["sideEffects"] = i["name"] in SIDE_EFFECT_READS
            # Command addresses are 0x00_CC_SSSS: CC is the SBS command, SSSS the MAC code,
            # little-endian (Interface::dispatch_command): 0x440100 -> MAC 0x0001. Commands with
            # CC other than 0x44 (ManufacturerBlockAccess) are plain SMBus block reads of CC.
            sbs = (i["address"] >> 16) & 0xFF
            if sbs == 0x44:
                e["mac"] = ((i["address"] >> 8) & 0xFF) | ((i["address"] & 0xFF) << 8)
            else:
                e["sbs"] = sbs
        if i["kind"] == "buffer":
            e["size"] = BUFFER_READ_BYTES
        return e

    return {
        "source": "bq41z50.ddsl",
        "registers": [entry(i) for i in items if i["kind"] == "register"],
        "commands": [entry(i) for i in items if i["kind"] == "command"],
        "buffers": [entry(i) for i in items if i["kind"] == "buffer"],
        "helpers": [{"name": n, "args": a, "doc": d, "disruptive": x} for n, a, d, x in HELPERS],
    }


def main():
    items = parse(DDSL.read_text())
    counts = {k: sum(i["kind"] == k for i in items) for k in ("register", "command", "buffer")}
    (ROOT / "src" / "driver_api.rs").write_text(rust(items))
    (ROOT / "tools" / "driver_api.json").write_text(json.dumps(metadata(items), indent=1) + "\n")
    print(f"{counts['register']} registers, {counts['command']} commands, {counts['buffer']} buffers from {DDSL}")


if __name__ == "__main__":
    main()
