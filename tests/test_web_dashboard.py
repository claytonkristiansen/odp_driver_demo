#!/usr/bin/env python3
"""Tests for tools/web_dashboard.py against its simulated board (FakePico). No hardware needed.

    python3 -m unittest discover -s tests
"""

import http.client
import json
import sys
import threading
import unittest
from http.server import ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import web_dashboard  # noqa: E402
from web_dashboard import FakePico, make_handler, parse_reply  # noqa: E402

API = json.loads(web_dashboard.DRIVER_API.read_text())
SIGNATURE_MACS = {"mac_instruction_flash_signature", "mac_static_df_signature", "mac_all_df_signature"}
POLLED_HELPERS = ["temperature_fahrenheit", "read_security_keys", "read_authentication_key",
                  "read_mfg_info", "read_mfg_info_c", "read_charging_voltage_override"]


def fake_pico():
    pico = FakePico()
    pico.LATENCY_S = 0
    return pico


class ParseReplyTest(unittest.TestCase):
    def test_values_keep_their_units(self):
        self.assertEqual(parse_reply("ok temp=25.10C soc=87%"), {"temp": "25.10C", "soc": "87%"})

    def test_quoted_values_lose_their_quotes(self):
        self.assertEqual(parse_reply('ok device_name="bq41z50" chemistry="LION"'),
                         {"device_name": "bq41z50", "chemistry": "LION"})

    def test_refused_field_is_none(self):
        self.assertEqual(parse_reply("ok soh=! soc=50%"), {"soh": None, "soc": "50%"})

    def test_reply_without_fields(self):
        self.assertEqual(parse_reply("ok pong"), {})


class FakePicoProtocolTest(unittest.TestCase):
    def setUp(self):
        self.pico = fake_pico()

    def test_ping(self):
        self.assertEqual(self.pico.command("ping"), "ok pong")

    def test_fields_lists_every_get_field(self):
        self.assertEqual(self.pico.command("fields"), "ok fields: " + " ".join(FakePico.FIELDS))

    def test_get_every_field(self):
        values = parse_reply(self.pico.command("get " + " ".join(FakePico.FIELDS)))
        self.assertEqual(set(values), set(FakePico.FIELDS))
        self.assertNotIn(None, values.values())

    def test_read_is_get_of_the_plot_fields(self):
        self.assertEqual(set(parse_reply(self.pico.command("read"))), set(FakePico.READ))

    def test_get_errors(self):
        self.assertEqual(self.pico.command("get"), "err get needs at least one field")
        self.assertEqual(self.pico.command("get nope"), "err unknown field 'nope' (try fields)")
        self.assertEqual(self.pico.command("get " + "soc " * 66), "err too many fields")
        self.assertTrue(self.pico.command("frobnicate").startswith("err unknown command"))

    def test_simulated_pack_is_plausible(self):
        v = parse_reply(self.pico.command("get voltage cells soc"))
        cells = [int(c) for c in v["cells"].removesuffix("mV").split("/")]
        self.assertEqual(len(cells), 4)
        self.assertEqual(int(v["voltage"].removesuffix("mV")), sum(cells))
        self.assertTrue(0 <= int(v["soc"].removesuffix("%")) <= 100)

    def test_register_reads_return_their_size_in_bytes(self):
        for reg in API["registers"]:
            with self.subTest(reg["name"]):
                raw = parse_reply(self.pico.command(f"reg {reg['name']}"))[reg["name"]]
                self.assertEqual(len(bytes.fromhex(raw)), reg["size"])

    def test_register_write_then_read_back(self):
        writable = next(r for r in API["registers"] if r["access"] == "RW")
        data = bytes(range(1, writable["size"] + 1)).hex()
        self.assertEqual(self.pico.command(f"regw {writable['name']} {data}"), "ok")
        self.assertEqual(parse_reply(self.pico.command(f"reg {writable['name']}"))[writable["name"]], data)

    def test_register_write_errors(self):
        read_only = next(r for r in API["registers"] if r["access"] == "RO")
        writable = next(r for r in API["registers"] if r["access"] == "RW")
        self.assertEqual(self.pico.command(f"regw {read_only['name']} 0000"), f"err {read_only['name']} is read-only")
        one_byte_too_many = "00" * (writable["size"] + 1)
        self.assertEqual(self.pico.command(f"regw {writable['name']} {one_byte_too_many}"),
                         f"err {writable['name']} takes exactly {writable['size']} bytes")
        self.assertEqual(self.pico.command("regw nope 00"), "err unknown name 'nope'")

    def test_mac_reads_and_actions_are_not_interchangeable(self):
        read = next(c for c in API["commands"] if c["kind"] == "read")
        action = next(c for c in API["commands"] if c["kind"] == "action")
        self.assertEqual(self.pico.command(f"mac {action['name']}"), f"err {action['name']}: it is an action; use run")
        self.assertEqual(self.pico.command(f"run {read['name']}"), f"err {read['name']}: it returns data; use mac")
        self.assertEqual(self.pico.command(f"run {action['name']}"), "ok")

    def test_buffers_are_length_byte_then_data(self):
        raw = bytes.fromhex(parse_reply(self.pico.command("buf device_name"))["device_name"])
        self.assertEqual(raw[1:1 + raw[0]], b"bq41z50")

    def test_every_dashboard_poll_command_succeeds(self):
        """Everything the page polls, in its batches of 10, answers without a refusal."""
        regs = [r["name"] for r in API["registers"]]
        macs = [c["name"] for c in API["commands"]
                if c["kind"] == "read" and not c.get("sideEffects") and c["name"] not in SIGNATURE_MACS]
        bufs = [b["name"] for b in API["buffers"]]
        for kind, names in (("reg", regs), ("mac", macs), ("buf", bufs)):
            for i in range(0, len(names), 10):
                batch = names[i:i + 10]
                reply = self.pico.command(f"{kind} {' '.join(batch)}")
                with self.subTest(f"{kind} {batch[0]}..."):
                    self.assertTrue(reply.startswith("ok "), reply)
                    values = parse_reply(reply)
                    self.assertEqual(set(values), set(batch))
                    self.assertNotIn(None, values.values())
        for helper in POLLED_HELPERS:
            with self.subTest(helper):
                self.assertIn(helper, parse_reply(self.pico.command(f"call {helper}")))
        self.assertEqual(len(regs) + len(macs) + len(bufs) + len(POLLED_HELPERS), 124)


class ServerTest(unittest.TestCase):
    """The HTTP API the page uses, served by the real handler in a background thread."""

    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(fake_pico(), demo=True))
        cls.server.daemon_threads = True
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.port = cls.server.server_address[1]

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        self.conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)

    def tearDown(self):
        self.conn.close()

    def request(self, method, path, body=None, content_type="application/json"):
        headers = {"Content-Type": content_type} if body is not None else {}
        self.conn.request(method, path, body=body, headers=headers)
        resp = self.conn.getresponse()
        return resp, resp.read()

    def command(self, cmd):
        resp, body = self.request("POST", "/api/command", json.dumps({"cmd": cmd}))
        self.assertEqual(resp.status, 200)
        return json.loads(body)

    def test_serves_the_page_uncached(self):
        resp, body = self.request("GET", "/")
        self.assertEqual(resp.status, 200)
        self.assertEqual(resp.getheader("Cache-Control"), "no-store")
        self.assertIn(b"<title>", body)

    def test_serves_driver_metadata_and_mode(self):
        resp, body = self.request("GET", "/api/driver")
        self.assertEqual(json.loads(body)["registers"], API["registers"])
        resp, body = self.request("GET", "/api/info")
        self.assertEqual(json.loads(body), {"mode": "demo"})

    def test_command_reply_is_parsed(self):
        r = self.command("get soc temp")
        self.assertTrue(r["ok"])
        self.assertTrue(r["connected"])
        self.assertEqual(set(r["values"]), {"soc", "temp"})
        self.assertIn("ms", r)

    def test_command_error_is_reported(self):
        r = self.command("get nope")
        self.assertFalse(r["ok"])
        self.assertEqual(r["error"], "unknown field 'nope' (try fields)")

    def test_rejects_bad_requests(self):
        resp, _ = self.request("POST", "/api/command", "cmd=ping", content_type="text/plain")
        self.assertEqual(resp.status, 415)
        self.conn.close()  # 415 closes the connection
        resp, _ = self.request("POST", "/api/command", "{not json")
        self.assertEqual(resp.status, 400)
        for cmd in ["", "x" * (web_dashboard.MAX_COMMAND_LEN + 1), "get soc\x00", "get sö"]:
            with self.subTest(cmd=cmd[:20]):
                resp, body = self.request("POST", "/api/command", json.dumps({"cmd": cmd}))
                self.assertEqual(resp.status, 400)
                self.assertFalse(json.loads(body)["ok"])

    def test_unknown_paths(self):
        resp, _ = self.request("GET", "/nope")
        self.assertEqual(resp.status, 404)
        self.conn.close()
        resp, _ = self.request("POST", "/api/nope", "{}")
        self.assertEqual(resp.status, 404)

    def test_keeps_the_connection_alive(self):
        """A new connection per request cost a Windows browser ~220 ms each (see main())."""
        self.command("ping")
        sock = self.conn.sock
        for _ in range(3):
            self.command("ping")
        self.assertIs(self.conn.sock, sock)

    def test_oversized_body_closes_the_connection(self):
        body = json.dumps({"cmd": "ping", "pad": "x" * 5000})
        resp, _ = self.request("POST", "/api/command", body)
        self.assertTrue(resp.will_close)


if __name__ == "__main__":
    unittest.main()
