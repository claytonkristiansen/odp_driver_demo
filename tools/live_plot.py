#!/usr/bin/env python3
"""Live dashboard for the BQ41Z50 demo: battery temperature, voltage and current.

Opens the Pico 2's USB serial port, parses the once-a-second readings printed by the
firmware, and plots the last few minutes as three stacked charts with a large readout
next to each, sized for a projector.

    pip install pyserial matplotlib
    python3 tools/live_plot.py              # auto-detects the Pico 2
    python3 tools/live_plot.py --port COM5  # or name the port explicitly
    python3 tools/live_plot.py --demo       # simulated data, no hardware needed
"""

import argparse
import errno
import math
import random
import re
import threading
import time
from collections import deque

import matplotlib.pyplot as plt
import serial
from matplotlib.animation import FuncAnimation
from serial.tools import list_ports

# USB IDs set in src/main.rs.
USB_VID = 0xC0DE
USB_PID = 0xCAFE

LINE_RE = re.compile(
    r"temp=(?P<temp>-?[\d.]+)C voltage=(?P<mv>\d+)mV current=(?P<ma>-?\d+)mA soc=(?P<soc>\d+)%"
)

HISTORY_SECONDS = 180

SURFACE = "#fcfcfb"
TEXT_PRIMARY = "#0b0b0b"
TEXT_SECONDARY = "#52514e"
GRID = "#e4e3df"
# First three categorical slots; validated as a set for small multiples.
BLUE, ORANGE, AQUA = "#2a78d6", "#eb6834", "#1baf7a"


class Readings:
    """Thread-safe rolling history shared between the reader thread and the plot."""

    def __init__(self):
        self.lock = threading.Lock()
        self.t = deque()
        self.temp_c = deque()
        self.volts = deque()
        self.amps = deque()
        self.soc = None
        self.status = "Connecting to the Pico 2..."
        self.start = time.monotonic()

    def add(self, temp_c, volts, amps, soc):
        now = time.monotonic() - self.start
        with self.lock:
            for series, value in ((self.t, now), (self.temp_c, temp_c), (self.volts, volts), (self.amps, amps)):
                series.append(value)
            while self.t and self.t[0] < now - HISTORY_SECONDS:
                for series in (self.t, self.temp_c, self.volts, self.amps):
                    series.popleft()
            self.soc = soc
            self.status = ""

    def snapshot(self):
        with self.lock:
            return list(self.t), list(self.temp_c), list(self.volts), list(self.amps), self.soc, self.status

    def set_status(self, status):
        with self.lock:
            self.status = status


def find_port():
    for port in list_ports.comports():
        if port.vid == USB_VID and port.pid == USB_PID:
            return port.device
    return None


def serial_reader(readings, port_name):
    """Reads lines forever, reconnecting if the board is unplugged."""
    while True:
        port = port_name or find_port()
        if port is None:
            readings.set_status("Plug in the Pico 2...")
            time.sleep(1)
            continue
        try:
            # The firmware only streams while DTR is asserted, which pyserial does on open.
            with serial.Serial(port, 115200, timeout=3) as ser:
                readings.set_status(f"Connected on {port}, waiting for data...")
                while True:
                    line = ser.readline().decode("ascii", errors="replace").strip()
                    if not line:
                        continue
                    m = LINE_RE.search(line)
                    if m:
                        readings.add(
                            float(m["temp"]),
                            int(m["mv"]) / 1000,
                            int(m["ma"]) / 1000,
                            int(m["soc"]),
                        )
                    elif line.startswith("read error"):
                        readings.set_status("Can't reach the battery - check the wiring")
        except (serial.SerialException, OSError) as e:
            if getattr(e, "errno", None) == errno.EACCES:
                readings.set_status(f"No permission to open {port} - run: sudo usermod -aG dialout $USER")
            else:
                readings.set_status("Pico 2 disconnected - reconnecting...")
            time.sleep(1)


def demo_reader(readings):
    """Simulated pack that alternates between charging and discharging."""
    temp, soc = 25.0, 80.0
    while True:
        t = time.monotonic() - readings.start
        amps = 1.5 * math.copysign(1, math.sin(t / 30)) + random.uniform(-0.05, 0.05)
        soc = min(100, max(0, soc + amps / 20))
        temp += (25 + abs(amps) * 3 - temp) * 0.05 + random.uniform(-0.03, 0.03)
        volts = 4 * (3.4 + 0.8 * soc / 100) + amps * 0.1
        readings.add(temp, volts, amps, round(soc))
        time.sleep(1)


def style_axes(ax, title, color):
    ax.set_facecolor(SURFACE)
    ax.set_title(title, loc="left", fontsize=18, fontweight="bold", color=TEXT_PRIMARY, pad=10)
    ax.grid(axis="y", color=GRID, linewidth=1)
    ax.tick_params(colors=TEXT_SECONDARY, labelsize=12, length=0)
    for side in ("top", "right", "left"):
        ax.spines[side].set_visible(False)
    ax.spines["bottom"].set_color(GRID)
    (line,) = ax.plot([], [], color=color, linewidth=2.5, solid_capstyle="round")
    (dot,) = ax.plot([], [], "o", color=color, markersize=10, markeredgecolor=SURFACE, markeredgewidth=2)
    return line, dot


def autoscale(ax, values, min_span):
    lo, hi = min(values), max(values)
    pad = max((hi - lo) * 0.15, min_span / 2)
    mid = (lo + hi) / 2
    ax.set_ylim(min(lo - pad, mid - min_span / 2), max(hi + pad, mid + min_span / 2))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", help="serial port (default: auto-detect the Pico 2)")
    parser.add_argument("--demo", action="store_true", help="plot simulated data instead of reading the board")
    args = parser.parse_args()

    readings = Readings()
    target = demo_reader if args.demo else serial_reader
    target_args = (readings,) if args.demo else (readings, args.port)
    threading.Thread(target=target, args=target_args, daemon=True).start()

    plt.rcParams["font.family"] = "sans-serif"
    fig = plt.figure(figsize=(14, 9), facecolor=SURFACE)
    fig.canvas.manager.set_window_title("Battery Monitor")
    grid = fig.add_gridspec(3, 2, width_ratios=[1.2, 3.2], hspace=0.55, wspace=0.05,
                            left=0.03, right=0.97, top=0.86, bottom=0.07)
    fig.text(0.03, 0.95, "Live Battery Monitor", fontsize=28, fontweight="bold", color=TEXT_PRIMARY)
    subtitle = fig.text(0.03, 0.905, "", fontsize=15, color=TEXT_SECONDARY)

    panels = []
    for row, (title, color, unit, fmt, min_span) in enumerate((
        ("Temperature", ORANGE, "°C", "{:.1f}", 2.0),
        ("Voltage", BLUE, "V", "{:.2f}", 0.2),
        ("Current", AQUA, "A", "{:.2f}", 0.2),
    )):
        tile = fig.add_subplot(grid[row, 0])
        tile.axis("off")
        value = tile.text(0.05, 0.45, "--", fontsize=44, fontweight="bold", color=TEXT_PRIMARY,
                          transform=tile.transAxes, va="center")
        caption = tile.text(0.05, 0.05, "", fontsize=14, color=TEXT_SECONDARY, transform=tile.transAxes)
        ax = fig.add_subplot(grid[row, 1])
        line, dot = style_axes(ax, title, color)
        ax.set_ylabel(unit, color=TEXT_SECONDARY, fontsize=13)
        panels.append((ax, line, dot, value, caption, unit, fmt, min_span))
    panels[-1][0].set_xlabel("seconds ago", color=TEXT_SECONDARY, fontsize=13)
    current_ax = panels[2][0]
    current_ax.axhline(0, color=TEXT_SECONDARY, linewidth=1, linestyle="--", alpha=0.6)

    def update(_frame):
        t, temp, volts, amps, soc, status = readings.snapshot()
        if not t:
            subtitle.set_text(status)
            return
        now = t[-1]
        ago = [x - now for x in t]
        for (ax, line, dot, value, caption, unit, fmt, min_span), series in zip(panels, (temp, volts, amps)):
            line.set_data(ago, series)
            dot.set_data([0], [series[-1]])
            ax.set_xlim(-HISTORY_SECONDS, 3)
            autoscale(ax, series + ([0] if ax is current_ax else []), min_span)
            value.set_text(f"{fmt.format(series[-1])} {unit}")

        a = amps[-1]
        panels[2][4].set_text("Charging" if a > 0.01 else "Discharging" if a < -0.01 else "Idle")
        panels[0][4].set_text(f"{temp[-1] * 9 / 5 + 32:.0f} °F")
        panels[1][4].set_text(f"Battery {soc}% full" if soc is not None else "")
        subtitle.set_text(status or ("Simulated data" if args.demo else "Reading from the BQ41Z50 every second"))

    # Keep a reference so the animation isn't garbage-collected.
    _anim = FuncAnimation(fig, update, interval=250, cache_frame_data=False)
    plt.show()


if __name__ == "__main__":
    main()
