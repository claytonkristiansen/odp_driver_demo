// Runs tools/dashboard.html in jsdom against tools/web_dashboard.py --demo (the simulated board).
//
//     cd tests && npm install && npm test
//
// The page's own script runs unchanged; fetch is passed through to the server, logging every
// command sent and optionally rewriting replies to simulate refused readings or a lost board.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { after, before, test } from "node:test";
import { fileURLToPath } from "node:url";
import { JSDOM } from "jsdom";

const TOOLS = fileURLToPath(new URL("../tools/", import.meta.url));
const SIGNATURE_MACS = ["mac_instruction_flash_signature", "mac_static_df_signature", "mac_all_df_signature"];
const CALIBRATION_MACS = ["mac_output_ccadc_cal", "mac_output_shorted_ccadc_cal"];
const POLL_TOTAL = 182;   // 58 gauge fields + 44 registers + 69 MAC reads + 5 buffers + 6 functions
const POLL_TIMEOUT = 15000;

let server, base;

const freePort = () => new Promise(resolve => {
  const s = createServer().listen(0, "127.0.0.1", () => { const { port } = s.address(); s.close(() => resolve(port)); });
});

before(async () => {
  const port = await freePort();
  base = `http://127.0.0.1:${port}/`;
  server = spawn("python3", ["-u", `${TOOLS}web_dashboard.py`, "--demo", "--http-port", String(port)], { stdio: ["ignore", "pipe", "inherit"] });
  await new Promise((resolve, reject) => {
    server.stdout.on("data", d => { if (String(d).includes("Dashboard at")) resolve(); });
    server.on("exit", code => reject(new Error(`web_dashboard.py exited with ${code}`)));
  });
});

after(() => server?.kill());

// Opens the page. `reply(cmd, body)` may rewrite a /api/command reply before the page sees it.
async function openPage({ reply } = {}) {
  const sent = [];
  let closed = false;
  const never = new Promise(() => {});   // after close(), requests never answer, so the page goes quiet
  const dom = await JSDOM.fromURL(base, {
    runScripts: "dangerously",
    pretendToBeVisual: true,
    beforeParse(window) {
      window.fetch = async (url, options) => {
        if (closed) return never;
        let res, body;
        try {
          res = await fetch(new URL(url, base), options);
          body = await res.json();
        } catch (e) {
          if (closed) return never;   // e.g. the server stopping after the last test
          throw e;
        }
        if (closed) return never;
        if (options?.body) {
          const { cmd } = JSON.parse(options.body);
          sent.push({ cmd, t: Date.now() });
          if (reply) body = reply(cmd, body) ?? body;
        }
        return { json: async () => body };
      };
      window.matchMedia = () => ({ matches: false, addEventListener() {} });
      window.confirm = () => false;
    },
  });
  const document = dom.window.document;
  const $ = id => document.getElementById(id);
  // Every status line shown. A demo poll takes ~1.3 s, so the next one starts at once and a
  // finished poll's "182 readings" is on screen only briefly; sampling would miss it.
  const history = [];
  new dom.window.MutationObserver(() => history.push($("status").textContent))
    .observe($("status"), { childList: true, characterData: true, subtree: true });
  const page = {
    dom, document, $, sent, history,
    status: () => $("status").textContent,
    tabs: () => [...document.querySelectorAll("#api-tabs button")],
    openTab: label => page.tabs().find(b => b.textContent.startsWith(label)).click(),
    groups: () => [...document.querySelectorAll("#api-rows tr.group-head .title")].map(e => e.textContent),
    async waitFor(check, what, timeout = POLL_TIMEOUT) {
      const end = Date.now() + timeout;
      while (Date.now() < end) {
        if (check()) return;
        await new Promise(r => setTimeout(r, 25));
      }
      assert.fail(`timed out waiting for ${what}; status: "${page.status()}"`);
    },
    // Resolves with the first status line matching `re` shown from now on.
    async waitForStatus(re, what) {
      const from = history.length;
      let found;
      await page.waitFor(() => (found = history.slice(from).find(line => re.test(line))) !== undefined, what);
      return found;
    },
    // Resolves with the status line of the next poll that finished all its readings.
    fullPoll: () => page.waitForStatus(new RegExp(`· ${POLL_TOTAL} readings:`), "a full poll"),
    close: () => { closed = true; dom.window.close(); },
  };
  return page;
}

// Marks `name` as refused by the gauge in any reply that carries it.
const refuse = name => (cmd, body) => {
  if (body.values && name in body.values) body.values[name] = null;
  return body;
};

test("every poll reads all 182 values and they pass", async () => {
  const page = await openPage();
  try {
    const status = await page.fullPoll();
    assert.match(status, new RegExp(`^Simulated board — polling every second · ${POLL_TOTAL} readings: ${POLL_TOTAL} passed, 0 failed$`));
    assert.match(page.$("api-note").textContent, /^124 readings: 124 passed, 0 failed/);
    assert.match(page.$("all-updated").textContent, /58 fields: 58 passed, 0 failed/);
    assert.equal(page.$("read-fail").hidden, true);
  } finally { page.close(); }
});

test("polls never read a signature MAC read or a calibration-output read", async () => {
  const page = await openPage();
  try {
    await page.fullPoll();
    const names = new Set(page.sent.flatMap(s => s.cmd.split(" ").slice(1)));
    for (const name of [...SIGNATURE_MACS, ...CALIBRATION_MACS]) assert.ok(!names.has(name), `${name} was polled`);
    assert.ok(names.has("mac_static_chem_df_sig"), "the static chemistry signature should still be polled");
  } finally { page.close(); }
});

test("the reading count climbs from 0 to 182 as replies arrive", async () => {
  const page = await openPage();
  try {
    await page.fullPoll();               // the first poll starts from "Connecting...", not 0
    await page.fullPoll();
    const counts = page.history.map(line => Number(line.match(/· (\d+) readings:/)?.[1]));
    const start = counts.indexOf(0);     // the second poll's first status line
    const second = counts.slice(start, counts.indexOf(POLL_TOTAL, start) + 1);
    assert.equal(second[0], 0);
    assert.equal(second.at(-1), POLL_TOTAL);
    assert.ok(second.every((n, i) => i === 0 || n > second[i - 1]), `not increasing: ${second.join(" ")}`);
    assert.ok(second.length > 10, `only ${second.length} steps: ${second.join(" ")}`);
  } finally { page.close(); }
});

test("every Driver API item sits in a named group", async () => {
  const page = await openPage();
  try {
    await page.waitFor(() => page.tabs().length === 5, "the Driver API tabs");
    for (const button of page.tabs()) {
      const [, label, count] = button.textContent.match(/^(.+?) \((\d+)\)/);
      button.click();
      const rows = page.document.querySelectorAll("#api-rows tr:not(.group-head)").length;
      assert.equal(rows, Number(count), `${label}: rows shown`);
      assert.ok(!page.groups().includes("Other"), `${label} has ungrouped items`);
      assert.ok(page.groups().length > 1, `${label} has only ${page.groups().length} group`);
    }
  } finally { page.close(); }
});

test("a refused reading is counted, popped up, dismissable, and clears when it passes", async () => {
  let failing = "temperature";
  const page = await openPage({ reply: (cmd, body) => (failing ? refuse(failing)(cmd, body) : body) });
  try {
    assert.match(await page.fullPoll(), new RegExp(`${POLL_TOTAL} readings: ${POLL_TOTAL - 1} passed, 1 failed`));
    assert.equal(page.$("read-fail").hidden, false);
    assert.match(page.$("read-fail-title").textContent, /^1 reading failed at /);
    assert.equal(page.$("read-fail-list").textContent, "temperature (register: refused by the gauge)");
    page.openTab("Registers");
    assert.match(page.document.querySelector("#api-tabs .counts").textContent, /✓ 43\s+1 failed/);

    page.$("read-fail-close").click();
    assert.equal(page.$("read-fail").hidden, true);
    await page.fullPoll();
    await page.fullPoll();
    assert.equal(page.$("read-fail").hidden, true, "the same failure should stay dismissed");

    failing = null;
    await page.waitForStatus(new RegExp(`${POLL_TOTAL} readings: ${POLL_TOTAL} passed, 0 failed$`), "a clean poll");
    assert.equal(page.$("read-fail").hidden, true);
  } finally { page.close(); }
});

test("the poll interval dropdown sets the time between polls", async () => {
  const page = await openPage();
  try {
    await page.fullPoll();
    const select = page.$("poll-rate");
    select.value = "2000";
    select.dispatchEvent(new page.dom.window.Event("change"));
    const pollStarts = () => page.sent.filter(s => s.cmd.startsWith("get temp ")).map(s => s.t);
    const before = pollStarts().length;
    await page.waitFor(() => pollStarts().length >= before + 2, "two polls at 2 s", 10000);
    const [a, b] = pollStarts().slice(-2);
    assert.ok(b - a >= 1900, `polls ${b - a} ms apart`);
    assert.match(page.status(), /polling every 2 s/);
  } finally { page.close(); }
});

test("polling refreshes values without wiping a Write field being typed in", async () => {
  const page = await openPage();
  try {
    await page.waitFor(() => page.document.querySelector("#api-rows input.arg"), "a writable register row");
    const input = page.document.querySelector("#api-rows input.arg");
    input.value = "abcd";
    await page.fullPoll();
    await page.fullPoll();
    assert.equal(page.document.querySelector("#api-rows input.arg"), input);
    assert.equal(input.value, "abcd");
  } finally { page.close(); }
});

test("a lost board shows the banner and keeps the last values", async () => {
  let unplugged = false;
  const page = await openPage({
    reply: (cmd, body) => (unplugged ? { ok: false, connected: false, error: "Plug in the Pico 2..." } : body),
  });
  try {
    await page.fullPoll();
    const temp = page.$("v-temp").textContent;
    assert.match(temp, /°C$/);
    unplugged = true;
    await page.waitForStatus(/^Plug in the Pico 2\.\.\.$/, "the disconnected status");
    assert.equal(page.$("banner").hidden, false);
    assert.equal(page.$("banner-title").textContent, "The Pico 2 is not connected.");
    assert.ok(page.document.body.classList.contains("stale"));
    assert.equal(page.$("v-temp").textContent, temp);

    unplugged = false;
    await page.fullPoll();
    assert.equal(page.$("banner").hidden, true);
    assert.ok(!page.document.body.classList.contains("stale"));
  } finally { page.close(); }
});
