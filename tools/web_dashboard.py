#!/usr/bin/env python3
"""Browser dashboard for the BQ41Z50 demo.

Serves a web page and bridges it to the Pico 2's USB serial port. The page drives the board:
it POSTs a command such as `get temp voltage` to /api/command every second, this script
forwards it to the Pico 2, the Pico 2 reads the gauge and replies, and the parsed reply goes
back to the page as JSON. The page has no external dependencies. From WSL the board stays on
Windows and is reached through tools/bridge.ps1 (see tools/pico_port.py); elsewhere it needs pyserial.

    python3 tools/web_dashboard.py              # auto-detects the Pico 2, serves http://127.0.0.1:8000
    python3 tools/web_dashboard.py --port COM5  # or name the serial port explicitly
    python3 tools/web_dashboard.py --demo       # simulated board, no hardware needed
    python3 tools/web_dashboard.py --http-port 8080 --host 0.0.0.0   # reachable from other machines
"""

import argparse
import errno
import json
import math
import random
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pico_port

# Reading every field takes well under a second, so 2 s is ample.
REPLY_TIMEOUT = 2.0
# The firmware buffers up to 1024 bytes per command line.
MAX_COMMAND_LEN = 1000
PAGE = Path(__file__).with_name("dashboard.html")
# Every register, MAC command, buffer and helper the firmware exposes; from tools/gen_driver_api.py.
DRIVER_API = Path(__file__).with_name("driver_api.json")


class PicoError(Exception):
    """Something between this script and the Pico 2 failed; the message is shown on the page."""


class Pico:
    """Owns the serial port and runs one command at a time, reconnecting as needed."""

    def __init__(self, port_name):
        self.port_name = port_name
        self.ser = None
        self.lock = threading.Lock()

    def _open(self):
        try:
            self.ser = pico_port.open_port(self.port_name, REPLY_TIMEOUT)
        except pico_port.NotFound as e:
            raise PicoError(str(e)) from e
        except OSError as e:
            if getattr(e, "errno", None) == errno.EACCES:
                raise PicoError("No permission to open the serial port - run: sudo usermod -aG dialout $USER") from e
            raise PicoError(f"Can't open the Pico 2's serial port: {e}") from e

    def _close(self):
        if self.ser is not None:
            try:
                self.ser.close()
            except OSError:
                pass
            self.ser = None

    def command(self, cmd):
        """Sends one command line and returns the Pico 2's `ok ...` / `err ...` reply line."""
        with self.lock:
            # A port handle from before an unplug fails on first use; retry once with a fresh one.
            for attempt in range(2):
                try:
                    if self.ser is None:
                        self._open()
                    self.ser.reset_input_buffer()
                    self.ser.write(cmd.encode("ascii") + b"\n")
                    deadline = time.monotonic() + REPLY_TIMEOUT
                    while time.monotonic() < deadline:
                        line = self.ser.readline().decode("ascii", errors="replace").strip()
                        if line.startswith(("ok", "err")):
                            return line
                    raise PicoError("The Pico 2 didn't answer - is the command firmware flashed?")
                except OSError as e:
                    self._close()
                    if attempt:
                        raise PicoError("Pico 2 disconnected - reconnecting...") from e
        raise AssertionError("unreachable")


class FakePico:
    """Answers the firmware's command protocol from a simulated 4-cell pack."""

    FIELDS = (
        "temp voltage current avg_current soc abs_soc cells max_error remaining_cap full_charge_cap "
        "design_cap run_time_to_empty avg_time_to_empty avg_time_to_full charging_current "
        "charging_voltage cycle_count at_rate at_rate_time_to_full at_rate_time_to_empty at_rate_ok "
        "remaining_cap_alarm remaining_time_alarm soh soh_fcc soh_energy battery_status battery_mode "
        "safety_alert safety_status pf_alert pf_status operation_status charging_status gauging_status "
        "manufacturing_status max_turbo_power sus_turbo_power turbo_pack_r turbo_sys_r turbo_edv "
        "turbo_current sus_turbo_current turbo_rhf_effective turbo_vload btp_discharge_set btp_charge_set "
        "manufacturer_name device_name chemistry manufacture_date serial_number design_voltage spec_info "
        "device_type firmware_version hardware_version chem_id"
    ).split()
    READ = ["temp", "voltage", "current", "soc", "cells"]
    DESIGN_MAH, FCC_MAH = 3000, 2850

    def __init__(self):
        self.lock = threading.Lock()
        self.start = time.monotonic()
        self.last = self.start
        self.temp, self.soc, self.amps, self.avg_amps = 25.0, 80.0, 0.0, 0.0
        self.offsets = [0.0, -0.004, 0.003, 0.001]
        self.stored = {}

    def _step(self):
        now = time.monotonic()
        dt, self.last = now - self.last, now
        self.amps = 1.5 * math.copysign(1, math.sin((now - self.start) / 30)) + random.uniform(-0.05, 0.05)
        self.avg_amps += (self.amps - self.avg_amps) * min(1, 0.2 * dt)
        self.soc = min(100, max(0, self.soc + self.amps / 20 * dt))
        self.temp += (25 + abs(self.amps) * 3 - self.temp) * min(1, 0.05 * dt) + random.uniform(-0.03, 0.03)

    def _values(self):
        cell = 3.4 + 0.8 * self.soc / 100 + self.amps * 0.025
        cells_mv = [round((cell + o) * 1000 + random.uniform(-1, 1)) for o in self.offsets]
        charging = self.amps > 0.01
        remaining = round(self.FCC_MAH * self.soc / 100)
        tte = round(remaining / -self.avg_amps / 1000 * 60) if self.avg_amps < -0.01 else 65535
        ttf = round((self.FCC_MAH - remaining) / self.avg_amps / 1000 * 60) if self.avg_amps > 0.01 else 65535
        # OperationStatus: PRES, plus DSG or CHG; SEC=3 (sealed). GaugingStatus: DSG when discharging, REST idle.
        op_status = 0x1 | (0x4 if charging else 0x2) | (3 << 8)
        gauging = (0 if charging else 0x40) | (0x800 if self.soc > 99 else 0)
        # BatteryStatus: INITIALIZED, DISCHARGING when not charging, FULLY_CHARGED near 100%.
        batt_status = 0x80 | (0 if charging else 0x40) | (0x20 if self.soc > 99 else 0)
        return {
            "temp": f"{self.temp:.2f}C",
            "voltage": f"{sum(cells_mv)}mV",
            "current": f"{round(self.amps * 1000)}mA",
            "avg_current": f"{round(self.avg_amps * 1000)}mA",
            "soc": f"{round(self.soc)}%",
            "abs_soc": f"{round(self.soc * self.FCC_MAH / self.DESIGN_MAH)}%",
            "cells": "/".join(map(str, cells_mv)) + "mV",
            "max_error": "1%",
            "remaining_cap": f"{remaining}mAh",
            "full_charge_cap": f"{self.FCC_MAH}mAh",
            "design_cap": f"{self.DESIGN_MAH}mAh",
            "run_time_to_empty": f"{tte}min",
            "avg_time_to_empty": f"{tte}min",
            "avg_time_to_full": f"{ttf}min",
            "charging_current": "1500mA" if self.soc < 99 else "0mA",
            "charging_voltage": "16800mV",
            "cycle_count": "42",
            "at_rate": "0mA",
            "at_rate_time_to_full": "65535min",
            "at_rate_time_to_empty": "65535min",
            "at_rate_ok": "1",
            "remaining_cap_alarm": "300mAh",
            "remaining_time_alarm": "10min",
            "soh": "95%",
            "soh_fcc": f"{self.FCC_MAH}mAh",
            "soh_energy": "4104cWh",
            "battery_status": f"0x{batt_status:04X}",
            "battery_mode": "0x6001",
            "safety_alert": "0x00000000",
            "safety_status": "0x00000000",
            "pf_alert": "0x00000000",
            "pf_status": "0x00000000",
            "operation_status": f"0x{op_status:08X}",
            "charging_status": f"0x{0x0000000400 if charging else 0:010X}",
            "gauging_status": f"0x{gauging:012X}",
            "manufacturing_status": "0x0018",
            "max_turbo_power": "4500cW",
            "sus_turbo_power": "2500cW",
            "turbo_pack_r": "85mOhm",
            "turbo_sys_r": "30mOhm",
            "turbo_edv": "12000mV",
            "turbo_current": "3200mA",
            "sus_turbo_current": "1800mA",
            "turbo_rhf_effective": "40mOhm",
            "turbo_vload": f"{sum(cells_mv) - 300}mV",
            "btp_discharge_set": "13200mV",
            "btp_charge_set": "16400mV",
            "manufacturer_name": '"Texas Instruments"',
            "device_name": '"bq41z50"',
            "chemistry": '"LION"',
            "manufacture_date": "2025-03-14",
            "serial_number": "1",
            "design_voltage": "14400mV",
            "spec_info": "0x0031",
            "device_type": "0x4150",
            "firmware_version": '"device 0x4150 version 1.02 build 36"',
            "hardware_version": "0x01",
            "chem_id": "0x1210",
        }

    RAW_COMMANDS = {"reg", "regw", "mac", "run", "buf", "bufw", "call"}
    _api = None

    @classmethod
    def api(cls):
        if cls._api is None:
            meta = json.loads(DRIVER_API.read_text())
            cls._api = {
                "reg": {e["name"]: e for e in meta["registers"]},
                "mac": {e["name"]: e for e in meta["commands"]},
                "buf": {e["name"]: e for e in meta["buffers"]},
                "call": {e["name"]: e for e in meta["helpers"]},
            }
        return cls._api

    def _register_bytes(self, entry):
        """A few registers carry the simulated pack; everything else reads back as stored or zero."""
        v = self._values()
        live = {
            "temperature": round((float(v["temp"][:-1]) + 273.15) * 10),
            "voltage": int(v["voltage"][:-2]),
            "current": int(v["current"][:-2]) & 0xFFFF,
            "avg_current": int(v["avg_current"][:-2]) & 0xFFFF,
            "relative_state_of_charge": int(v["soc"][:-1]),
            "absolute_state_of_charge": int(v["abs_soc"][:-1]),
            "remaining_capacity": int(v["remaining_cap"][:-3]),
            "full_charge_capacity": self.FCC_MAH,
            "design_capacity": self.DESIGN_MAH,
            "cycle_count": 42,
            "battery_status": int(v["battery_status"], 16),
            "operation_status": int(v["operation_status"], 16),
            "gauging_status": int(v["gauging_status"], 16),
        }
        cells = [int(c) for c in v["cells"][:-2].split("/")]
        for i, mv in enumerate(cells, 1):
            live[f"cell_voltage_{i}"] = mv
        if entry["name"] in self.stored:
            return self.stored[entry["name"]]
        return live.get(entry["name"], 0).to_bytes(entry["size"], "little")

    def _raw(self, words):
        kind, args = words[0], words[1:]
        api = self.api()
        if kind in ("reg", "mac", "buf"):
            if not args:
                return "err name at least one item"
            parts = []
            for name in args:
                entry = api[kind].get(name)
                if entry is None:
                    return f"err unknown name '{name}'"
                if kind == "mac" and entry["kind"] == "action":
                    return f"err {name}: it is an action; use run"
                if kind == "reg":
                    data = self._register_bytes(entry)
                elif kind == "buf":
                    text = {"manufacture_name": b"Texas Instruments", "device_name": b"bq41z50",
                            "device_chemistry": b"LION"}.get(name, b"")
                    data = bytes([len(text)]) + text.ljust(32, b"\0")
                else:
                    data = self.stored.get(name, bytes(entry["size"]))
                parts.append(f"{name}={data.hex()}")
            return "ok " + " ".join(parts)
        if kind in ("regw", "bufw"):
            if len(args) != 2:
                return f"err usage: {kind} <name> <hex bytes>"
            entry = api["reg" if kind == "regw" else "buf"].get(args[0])
            if entry is None:
                return f"err unknown name '{args[0]}'"
            if entry["access"] == "RO":
                return f"err {args[0]} is read-only"
            data = bytes.fromhex(args[1].removeprefix("0x"))
            if kind == "regw" and len(data) != entry["size"]:
                return f"err {args[0]} takes exactly {entry['size']} bytes"
            self.stored[args[0]] = data
            return "ok"
        if kind == "run":
            entry = api["mac"].get(args[0] if args else "")
            if entry is None:
                return f"err unknown name '{args[0] if args else ''}'"
            if entry["kind"] == "read":
                return f"err {args[0]}: it returns data; use mac"
            return "ok"
        name = args[0] if args else ""
        if name not in api["call"]:
            return f"err unknown name '{name}'"
        if name == "temperature_fahrenheit":
            return f"ok {name}={self.temp * 9 / 5 + 32:.2f}F"
        sizes = {"read_security_keys": 8, "read_authentication_key": 16, "read_mfg_info": 33, "read_mfg_info_c": 32}
        if name in sizes:
            return f"ok {name}=" + bytes(sizes[name]).hex()
        if name == "read_charging_voltage_override":
            return f"ok {name}=4000/4200/4200/4100/4200mV"
        return "ok"

    def command(self, cmd):
        with self.lock:
            time.sleep(0.05)
            self._step()
            words = cmd.split()
            if not words:
                return "err empty command"
            if words[0] == "ping":
                return "ok pong"
            if words[0] == "help":
                return ("ok commands: ping, help, fields, read, get <field>..., reg <name>..., regw <name> <hex>, "
                        "mac <name>..., run <name>, buf <name>..., bufw <name> <hex>, call <function> [args], bootsel")
            if words[0] == "fields":
                return "ok fields: " + " ".join(self.FIELDS)
            if words[0] in self.RAW_COMMANDS:
                return self._raw(words)
            if words[0] == "read":
                words = ["get", *self.READ]
            if words[0] != "get":
                return f"err unknown command '{words[0]}' (try help)"
            if len(words) == 1:
                return "err get needs at least one field"
            if len(words) > 65:
                return "err too many fields"
            values = self._values()
            for name in words[1:]:
                if name not in values:
                    return f"err unknown field '{name}' (try fields)"
            return "ok " + " ".join(f"{name}={values[name]}" for name in words[1:])


FIELD_RE = re.compile(r'(\w+)=("[^"]*"|\S+)')


def parse_reply(line):
    """`ok temp=25.10C name="bq41z50" soh=!` -> {"temp": "25.10C", "name": "bq41z50", "soh": None}.

    Values stay as the firmware wrote them, units included; the page knows how to show each one.
    `!` means the gauge refused that field.
    """
    return {name: None if raw == "!" else raw.strip('"') for name, raw in FIELD_RE.findall(line)}


def make_handler(pico, demo):
    class Handler(BaseHTTPRequestHandler):
        # Keep-alive: the page polls every second, and a new connection per request costs ~220 ms
        # from a Windows browser that opened the page as localhost (see main()).
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def do_GET(self):
            if self.path in ("/", "/index.html"):
                self.send_body(200, "text/html; charset=utf-8", PAGE.read_bytes())
            elif self.path == "/api/driver":
                self.send_body(200, "application/json", DRIVER_API.read_bytes())
            elif self.path == "/api/info":
                self.send_json({"mode": "demo" if demo else "live"})
            else:
                self.send_error(404)

        def do_POST(self):
            if self.path != "/api/command":
                self.send_error(404)
                return
            # Requiring JSON means other websites can't post here without a CORS preflight,
            # which this server never answers.
            if self.headers.get("Content-Type", "").split(";")[0] != "application/json":
                self.send_error(415)
                return
            try:
                length = int(self.headers.get("Content-Length", 0))
                if length > 4096:
                    self.close_connection = True  # the unread rest of the body would end up in the next request
                cmd = json.loads(self.rfile.read(min(length, 4096)))["cmd"].strip()
            except (ValueError, KeyError, TypeError, AttributeError):
                self.send_json({"ok": False, "error": 'Expected JSON like {"cmd": "get soc"}'}, 400)
                return
            if not cmd or len(cmd) > MAX_COMMAND_LEN or not cmd.isascii() or not cmd.isprintable():
                self.send_json({"ok": False, "error": f"Commands are 1-{MAX_COMMAND_LEN} printable ASCII characters"}, 400)
                return

            started = time.monotonic()
            try:
                reply = pico.command(cmd)
            except PicoError as e:
                self.send_json({"ok": False, "connected": False, "error": str(e)})
                return
            result = {"ok": reply.startswith("ok"), "connected": True, "reply": reply,
                      "ms": round((time.monotonic() - started) * 1000)}
            if result["ok"]:
                result["values"] = parse_reply(reply)
            else:
                result["error"] = reply.removeprefix("err").strip()
            self.send_json(result)

        def send_json(self, obj, status=200):
            self.send_body(status, "application/json", json.dumps(obj).encode())

        def send_body(self, status, content_type, body):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", help="serial port (default: auto-detect the Pico 2)")
    parser.add_argument("--demo", action="store_true", help="answer commands from a simulated board")
    parser.add_argument("--host", default="127.0.0.1", help="address to serve on (default: 127.0.0.1)")
    parser.add_argument("--http-port", type=int, default=8000, help="web server port (default: 8000)")
    args = parser.parse_args()

    pico = FakePico() if args.demo else Pico(args.port)
    server = ThreadingHTTPServer((args.host, args.http_port), make_handler(pico, args.demo))
    server.daemon_threads = True
    # Not "localhost": a Windows browser resolves that to ::1 first, which WSL doesn't serve, and
    # waits ~220 ms per new connection before falling back to 127.0.0.1.
    print(f"Dashboard at http://{args.host}:{args.http_port}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
