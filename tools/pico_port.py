"""Opens the Pico 2's USB serial port, wherever it is.

Under WSL the board stays on Windows (e.g. as COM4), where WSL can't open it, so the port is
reached through tools/bridge.ps1 run with powershell.exe; its stdin/stdout carry the serial data.
No usbipd needed. A board attached to WSL with usbipd, or any port on native Linux or Windows,
is opened with pyserial.

    with pico_port.open_port(None, timeout=2) as ser:   # or "COM4", "/dev/ttyACM0"
        ser.write(b"ping\n")
        ser.readline()

Both kinds of port raise OSError when the board goes away, and NotFound when it isn't plugged in.
"""

import queue
import shutil
import subprocess
import threading
import time
from pathlib import Path

# USB IDs set in src/main.rs.
USB_VID = 0xC0DE
USB_PID = 0xCAFE

BRIDGE = Path(__file__).with_name("bridge.ps1")
# PowerShell takes a second or two to start and look up the COM port.
BRIDGE_START_TIMEOUT = 10


class NotFound(OSError):
    """No Pico 2 is plugged in."""


def open_port(port=None, timeout=2.0):
    """Opens `port`, or finds the Pico 2 by its USB IDs. `readline` gives up after `timeout` s."""
    if port is None:
        port = local_port()
    in_wsl = shutil.which("wslpath") and shutil.which("powershell.exe")
    if in_wsl and (port is None or port.upper().startswith("COM")):
        return BridgePort(port, timeout)
    if port is None:
        raise NotFound("Plug in the Pico 2...")
    import serial

    return serial.Serial(port, 115200, timeout=timeout, write_timeout=timeout)


def local_port():
    """The Pico 2's port as this OS sees it, or None (also when pyserial isn't installed)."""
    try:
        from serial.tools import list_ports
    except ImportError:
        return None
    return next((p.device for p in list_ports.comports() if p.vid == USB_VID and p.pid == USB_PID), None)


class BridgePort:
    """The subset of serial.Serial the tools use, over a tools/bridge.ps1 subprocess."""

    def __init__(self, port, timeout):
        self.timeout = timeout
        self.port = port
        self.lines = queue.Queue()
        self.errors = []
        win_bridge = subprocess.run(["wslpath", "-w", str(BRIDGE)], capture_output=True, text=True, check=True).stdout.strip()
        cmd = ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", win_bridge]
        if port:
            cmd += ["-Port", port]
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        self.connected = threading.Event()
        threading.Thread(target=self._read_stdout, daemon=True).start()
        stderr_reader = threading.Thread(target=self._read_stderr, daemon=True)
        stderr_reader.start()

        deadline = time.monotonic() + BRIDGE_START_TIMEOUT
        while not self.connected.wait(0.05):
            if self.proc.poll() is not None:
                stderr_reader.join(1)
                # bridge.ps1 exits with 2 when no COM port has the Pico 2's USB IDs.
                if self.proc.returncode == 2:
                    raise NotFound("Plug in the Pico 2...")
                raise OSError(self.errors[-1] if self.errors else "bridge exited")
            if time.monotonic() > deadline:
                self.close()
                raise OSError("bridge did not connect")

    def _read_stdout(self):
        for raw in self.proc.stdout:
            self.lines.put(raw)
        self.lines.put(None)  # stdout closed: the bridge has exited

    def _read_stderr(self):
        for raw in self.proc.stderr:
            text = raw.decode("utf-8", "replace").strip().removeprefix("bridge: ")
            if text.startswith("connected to "):
                self.port = text.removeprefix("connected to ")
                self.connected.set()
            elif text:
                self.errors.append(text)

    def _gone(self):
        return OSError(self.errors[-1] if self.errors else f"{self.port} closed")

    def write(self, data):
        if self.proc.poll() is not None:
            raise self._gone()
        self.proc.stdin.write(data)
        self.proc.stdin.flush()

    def readline(self):
        """One line including its newline, or b"" after `timeout` seconds, like pyserial."""
        try:
            line = self.lines.get(timeout=self.timeout)
        except queue.Empty:
            return b""
        if line is None:
            self.lines.put(None)
            raise self._gone()
        return line

    def reset_input_buffer(self):
        while True:
            try:
                line = self.lines.get_nowait()
            except queue.Empty:
                return
            if line is None:
                self.lines.put(None)
                return

    def close(self):
        if self.proc.poll() is None:
            # Closing stdin ends the bridge's copy loop, so it closes the COM port itself.
            try:
                self.proc.stdin.close()
                self.proc.wait(2)
            except (OSError, subprocess.TimeoutExpired):
                self.proc.terminate()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
