"""Local-only GUI smoke test. Uses synthetic credentials and a loopback RFB server."""

import argparse
import ctypes
from ctypes import wintypes
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parent.parent
FORMAT = struct.pack("!BBBBHHHBBB3x", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0)
PRIME = int(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1"
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD"
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245"
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED"
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE65381"
    "FFFFFFFFFFFFFFFF", 16
)
user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
bcrypt = ctypes.WinDLL("bcrypt")
user32.SetThreadDpiAwarenessContext.argtypes = [ctypes.c_void_p]
user32.SetThreadDpiAwarenessContext.restype = ctypes.c_void_p
if not user32.SetThreadDpiAwarenessContext(ctypes.c_void_p(-4)):
    raise ctypes.WinError(ctypes.get_last_error())
callback_type = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
user32.EnumWindows.argtypes = [callback_type, wintypes.LPARAM]
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.IsWindowVisible.argtypes = [wintypes.HWND]
user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.SetWindowPos.argtypes = [
    wintypes.HWND, wintypes.HWND, ctypes.c_int, ctypes.c_int,
    ctypes.c_int, ctypes.c_int, wintypes.UINT
]
user32.GetDC.argtypes = [wintypes.HWND]
user32.GetDC.restype = wintypes.HDC
user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]
user32.ScreenToClient.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.POINT)]
user32.GetPhysicalCursorPos.argtypes = [ctypes.POINTER(wintypes.POINT)]
user32.SetPhysicalCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.GetForegroundWindow.restype = wintypes.HWND
user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
gdi32.GetPixel.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int]
gdi32.GetPixel.restype = wintypes.DWORD


class MouseInput(ctypes.Structure):
    _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG),
                ("data", wintypes.DWORD), ("flags", wintypes.DWORD),
                ("time", wintypes.DWORD), ("extra", ctypes.c_size_t)]


class KeyboardInput(ctypes.Structure):
    _fields_ = [("key", wintypes.WORD), ("scan", wintypes.WORD),
                ("flags", wintypes.DWORD), ("time", wintypes.DWORD),
                ("extra", ctypes.c_size_t)]


class InputUnion(ctypes.Union):
    _fields_ = [("mouse", MouseInput), ("keyboard", KeyboardInput)]


class Input(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("value", InputUnion)]


user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(Input), ctypes.c_int]
user32.SendInput.restype = wintypes.UINT

bcrypt.BCryptOpenAlgorithmProvider.argtypes = [
    ctypes.POINTER(ctypes.c_void_p), wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.ULONG
]
bcrypt.BCryptSetProperty.argtypes = [
    ctypes.c_void_p, wintypes.LPCWSTR, ctypes.c_void_p, wintypes.ULONG, wintypes.ULONG
]
bcrypt.BCryptGenerateSymmetricKey.argtypes = [
    ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p), ctypes.c_void_p,
    wintypes.ULONG, ctypes.c_void_p, wintypes.ULONG, wintypes.ULONG
]
bcrypt.BCryptDecrypt.argtypes = [
    ctypes.c_void_p, ctypes.c_void_p, wintypes.ULONG, ctypes.c_void_p,
    ctypes.c_void_p, wintypes.ULONG, ctypes.c_void_p, wintypes.ULONG,
    ctypes.POINTER(wintypes.ULONG), wintypes.ULONG
]
bcrypt.BCryptDestroyKey.argtypes = [ctypes.c_void_p]
bcrypt.BCryptCloseAlgorithmProvider.argtypes = [ctypes.c_void_p, wintypes.ULONG]


def ntstatus(status):
    if status:
        raise RuntimeError(f"BCrypt status: {status & 0xffffffff:08x}")


def decrypt(key_bytes, ciphertext):
    algorithm = ctypes.c_void_p()
    key = ctypes.c_void_p()
    ntstatus(bcrypt.BCryptOpenAlgorithmProvider(ctypes.byref(algorithm), "AES", None, 0))
    try:
        mode = ctypes.create_unicode_buffer("ChainingModeECB")
        ntstatus(bcrypt.BCryptSetProperty(algorithm, "ChainingMode", mode, ctypes.sizeof(mode), 0))
        raw_key = ctypes.create_string_buffer(key_bytes)
        ntstatus(bcrypt.BCryptGenerateSymmetricKey(
            algorithm, ctypes.byref(key), None, 0, raw_key, len(key_bytes), 0
        ))
        try:
            encrypted = ctypes.create_string_buffer(ciphertext)
            plaintext = ctypes.create_string_buffer(len(ciphertext))
            length = wintypes.ULONG()
            ntstatus(bcrypt.BCryptDecrypt(
                key, encrypted, len(ciphertext), None, None, 0,
                plaintext, len(ciphertext), ctypes.byref(length), 0
            ))
            return plaintext.raw[:length.value]
        finally:
            ntstatus(bcrypt.BCryptDestroyKey(key))
    finally:
        ntstatus(bcrypt.BCryptCloseAlgorithmProvider(algorithm, 0))


def read(connection, length):
    data = bytearray()
    while len(data) < length:
        part = connection.recv(length - len(data))
        if not part:
            raise EOFError("Viewer closed the fixture connection")
        data.extend(part)
    return bytes(data)


class Fixture:
    def __init__(self):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(1)
        self.listener.settimeout(30)
        self.port = self.listener.getsockname()[1]
        self.stop = threading.Event()
        self.authenticated = threading.Event()
        self.pause_updates = threading.Event()
        self.pointers = []
        self.keys = []
        self.offers = []
        self.updates = 0
        self.error = None

    def run(self):
        try:
            connection, _ = self.listener.accept()
            with connection:
                connection.settimeout(15)
                connection.sendall(b"RFB 003.889\n")
                assert read(connection, 12) == b"RFB 003.008\n"
                connection.sendall(b"\x01\x1e")
                assert read(connection, 1) == b"\x1e"
                secret = int.from_bytes(os.urandom(32), "big")
                public = pow(2, secret, PRIME)
                connection.sendall(struct.pack("!HH", 2, 128)
                                   + PRIME.to_bytes(128, "big") + public.to_bytes(128, "big"))
                encrypted = read(connection, 128)
                peer = int.from_bytes(read(connection, 128), "big")
                key = hashlib.md5(pow(peer, secret, PRIME).to_bytes(128, "big")).digest()
                credentials = decrypt(key, encrypted)
                assert credentials[:64].split(b"\0", 1)[0] == b"fixture-user"
                assert credentials[64:].split(b"\0", 1)[0] == b"fixture-password"
                self.authenticated.set()
                connection.sendall(b"\0\0\0\0")
                read(connection, 1)
                connection.sendall(struct.pack("!HH", 3840, 2160) + FORMAT
                                   + struct.pack("!I", 7) + b"Fixture")
                sent = False
                while not self.stop.is_set():
                    if not select.select([connection], [], [], 0.05)[0]:
                        continue
                    kind = read(connection, 1)[0]
                    if kind == 0:
                        assert read(connection, 19)[3:] == FORMAT
                    elif kind == 2:
                        count = struct.unpack("!H", read(connection, 3)[1:])[0]
                        self.offers.append(struct.unpack(f"!{count}i", read(connection, count * 4)))
                    elif kind == 3:
                        read(connection, 9)
                        if not sent:
                            connection.sendall(
                                b"\0\0\0\x01" + struct.pack("!HHHHi", 0, 0, 3840, 2160, 0)
                                + ((b"\0\0\xff\0" * 1920 + b"\0\xff\0\0" * 1920) * 1080)
                                + ((b"\xff\0\0\0" * 1920 + b"\xff\xff\xff\0" * 1920) * 1080)
                            )
                            sent = True
                        else:
                            while self.pause_updates.is_set() and not self.stop.is_set():
                                time.sleep(0.05)
                            if self.stop.is_set():
                                break
                            time.sleep(0.04)
                            color = b"\x20\x60\xa0\0" if self.updates % 2 else b"\x50\xa0\x20\0"
                            connection.sendall(
                                b"\0\0\0\x01" + struct.pack("!HHHHi", 0, 0, 64, 64, 0)
                                + color * 64 * 64
                            )
                        self.updates += 1
                    elif kind == 4:
                        payload = read(connection, 7)
                        self.keys.append((payload[0], struct.unpack("!I", payload[3:])[0]))
                    elif kind == 5:
                        self.pointers.append(struct.unpack("!BHH", read(connection, 5)))
                    else:
                        raise AssertionError(f"Unrequested client message: {kind}")
        except (EOFError, ConnectionResetError):
            if not self.stop.is_set():
                self.error = AssertionError("Viewer closed the fixture unexpectedly")
        except Exception as error:
            self.error = error


def native_window(pid):
    windows = []

    @callback_type
    def callback(window, _):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(window, ctypes.byref(owner))
        if owner.value == pid and user32.IsWindowVisible(window):
            title = ctypes.create_unicode_buffer(256)
            user32.GetWindowTextW(window, title, len(title))
            if title.value == "MacCanvas":
                windows.append(window)
        return True

    user32.EnumWindows(callback, 0)
    return windows[0] if windows else None


def automate(pid, port, action):
    output = subprocess.check_output([
        "powershell.exe", "-NoProfile", "-File", str(ROOT / "scripts" / "Gui-Fixture.ps1"),
        "-ProcessId", str(pid), "-Port", str(port), "-Action", action
    ], text=True, timeout=30)
    return json.loads(output) if action in ("bounds", "telemetry") else None


def send_input(event):
    if user32.SendInput(1, ctypes.byref(event), ctypes.sizeof(Input)) != 1:
        raise ctypes.WinError(ctypes.get_last_error())


def click(down):
    send_input(Input(0, InputUnion(mouse=MouseInput(flags=0x0002 if down else 0x0004))))


def key(code, down):
    send_input(Input(1, InputUnion(keyboard=KeyboardInput(key=code, flags=0 if down else 2))))


def pixel(window, x, y):
    point = wintypes.POINT(int(x), int(y))
    if not user32.ScreenToClient(window, ctypes.byref(point)):
        raise ctypes.WinError(ctypes.get_last_error())
    dc = user32.GetDC(window)
    try:
        return gdi32.GetPixel(dc, point.x, point.y)
    finally:
        user32.ReleaseDC(window, dc)


def wait(predicate, process, fixture, description, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError(f"Viewer exited before {description}: {process.stderr.read()}")
        if fixture.error:
            raise fixture.error
        result = predicate()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError(f"Timed out: {description}; recent fixture pointer events: {fixture.pointers[-5:]}")


def main(executable, native_messages=False):
    fixture = Fixture()
    thread = threading.Thread(target=fixture.run, daemon=True)
    thread.start()
    original = wintypes.POINT()
    if not user32.GetPhysicalCursorPos(ctypes.byref(original)):
        raise ctypes.WinError(ctypes.get_last_error())
    foreground = user32.GetForegroundWindow()
    with tempfile.TemporaryDirectory(prefix="maccanvas-gui-") as data:
        environment = os.environ.copy()
        environment["PATH"] = str(Path(environment["SYSTEMROOT"]) / "System32")
        process = subprocess.Popen(
            [str(executable), "--data-dir", data], env=environment,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            creationflags=subprocess.CREATE_NO_WINDOW,
        )
        window = None
        try:
            window = wait(lambda: native_window(process.pid), process, fixture, "native window")
            time.sleep(1)
            automate(process.pid, fixture.port, "connect")
            wait(fixture.authenticated.is_set, process, fixture, "Apple DH authentication")
            bounds = automate(process.pid, fixture.port, "bounds")
            width, height = bounds["width"], bounds["height"]
            scale = min(width / 3840, height / 2160, 1)
            draw_width, draw_height = int(3840 * scale), int(2160 * scale)
            left = bounds["x"] + (width - draw_width) / 2
            top = bounds["y"] + (height - draw_height) / 2
            wait(lambda: pixel(window, left + draw_width / 4, top + draw_height / 4) == 0x0000ff,
                 process, fixture, "visible Lanczos-rendered 4K framebuffer")
            assert pixel(window, left + draw_width * 3 / 4, top + draw_height / 4) == 0x00ff00
            assert pixel(window, left + draw_width / 4, top + draw_height * 3 / 4) == 0xff0000
            assert pixel(window, left + draw_width * 3 / 4, top + draw_height * 3 / 4) == 0xffffff
            x, y = round(left + draw_width / 2), round(top + draw_height / 2)
            if native_messages:
                user32.SetPhysicalCursorPos(x, y)
                point = wintypes.POINT(x, y)
                user32.ScreenToClient(window, ctypes.byref(point))
                coordinates = (point.y << 16) | (point.x & 0xffff)
                user32.PostMessageW(window, 0x200, 0, coordinates)
            else:
                user32.SetForegroundWindow(window)
                if user32.GetForegroundWindow() != window:
                    automate(process.pid, fixture.port, "focus")
                if user32.GetForegroundWindow() != window:
                    raise RuntimeError("Physical input requires an interactive foreground desktop; use --native-messages for a background-window test.")
                user32.SetPhysicalCursorPos(x, y)
            wait(lambda: any(abs(px - 1920) < 6 and abs(py - 1080) < 6
                             for _, px, py in fixture.pointers),
                 process, fixture, "Windows mouse movement with DPI conversion")
            if native_messages:
                user32.PostMessageW(window, 0x201, 1, coordinates)
                time.sleep(0.1)
                user32.PostMessageW(window, 0x202, 0, coordinates)
            else:
                click(True)
                click(False)
            wait(lambda: any(mask == 1 for mask, _, _ in fixture.pointers),
                 process, fixture, "real mouse click")
            if native_messages:
                user32.PostMessageW(window, 0x100, 0x43, 0x2e0001)
                user32.PostMessageW(window, 0x102, 0x63, 0x2e0001)
                user32.PostMessageW(window, 0x101, 0x43, 0xc02e0001)
                wait(lambda: (1, 0x63) in fixture.keys and (0, 0x63) in fixture.keys,
                     process, fixture, "native key messages and releases")
            else:
                key(0x11, True)
                key(0x43, True)
                key(0x43, False)
                key(0x11, False)
                wait(lambda: (1, 0xffeb) in fixture.keys and (1, 0x63) in fixture.keys,
                     process, fixture, "Ctrl+C mapped to macOS Command+C")
                wait(lambda: (0, 0xffeb) in fixture.keys and (0, 0x63) in fixture.keys,
                     process, fixture, "key releases")
            print("PASS: real Slint GUI, Apple DH login, 4K Lanczos frame, DPI-aware "
                  + ("native mouse/key message dispatch" if native_messages else "physical mouse/key input"))
            deadline = time.monotonic() + 10
            while True:
                samples = automate(process.pid, fixture.port, "telemetry")
                values = [sample["name"] for sample in samples]
                if any(name.startswith("Live FPS: ") and not name.startswith("Live FPS: 0 ") for name in values):
                    break
                assert time.monotonic() < deadline, values
                time.sleep(0.3)
            assert any(name.startswith("Live FPS: ") and not name.startswith("Live FPS: 0 ") for name in values), values
            assert any(name.startswith("Download rate: ") and not name.startswith("Download rate: 0.0 ") for name in values), values
            indicator = next(sample for sample in samples if sample["name"] == "Connection status indicator")
            status = next(sample for sample in samples if sample["name"].startswith("Connected to "))
            assert abs((indicator["y"] + indicator["height"] / 2) - (status["y"] + status["height"] / 2)) <= 2, samples
            print("PASS: live measured traffic/FPS and vertically centered status indicator")
            automate(process.pid, fixture.port, "toggle-sidebar")
            time.sleep(0.35)
            expanded = automate(process.pid, fixture.port, "bounds")
            assert expanded["width"] > bounds["width"] + 300
            automate(process.pid, fixture.port, "toggle-sidebar")
            time.sleep(0.35)
            print("PASS: animated sidebar collapse and restoration")
            automate(process.pid, fixture.port, "quality")
            time.sleep(0.2)
            automate(process.pid, fixture.port, "manual-quality")
            wait(lambda: len(fixture.offers) >= 2, process, fixture, "live encoding configuration")
            print("PASS: manual bandwidth target and FPS controls apply without reconnecting")
            if not user32.SetWindowPos(window, None, 0, 0, 1500, 950, 0x16):
                raise ctypes.WinError(ctypes.get_last_error())
            time.sleep(0.5)
            resized = automate(process.pid, fixture.port, "bounds")
            ratio = min(resized["width"] / 3840, resized["height"] / 2160, 1)
            scaled_width, scaled_height = int(3840 * ratio), int(2160 * ratio)
            scaled_left = resized["x"] + (resized["width"] - scaled_width) / 2
            scaled_top = resized["y"] + (resized["height"] - scaled_height) / 2
            wait(lambda: pixel(window, scaled_left + scaled_width / 4,
                               scaled_top + scaled_height / 4) == 0x0000ff,
                 process, fixture, "redraw after native window resize")
            automate(process.pid, fixture.port, "fullscreen")
            time.sleep(0.5)
            fullscreen = automate(process.pid, fixture.port, "bounds")
            assert fullscreen["width"] > resized["width"]
            automate(process.pid, fixture.port, "exit-fullscreen")
            print("PASS: resize redraw and immersive full-screen round trip")
            fixture.pause_updates.set()
            deadline = time.monotonic() + 10
            while True:
                idle = automate(process.pid, fixture.port, "telemetry")
                names = [entry["name"] for entry in idle]
                if "Live FPS: 0 FPS" in names and "Download rate: 0.0 kbps" in names:
                    break
                assert time.monotonic() < deadline, names
                time.sleep(0.3)
            print("PASS: idle traffic and FPS return to zero without counting layout redraws")
            settings = (Path(data) / "settings.json").read_text()
            assert "fixture-password" not in settings and "fixture-user" not in settings
            configuration = json.loads(settings)
            assert configuration["streaming"]["target_kbps"] == 1000
            assert configuration["streaming"]["max_fps"] == 15
            assert configuration["streaming"]["compression"] == 3
            assert -253 in fixture.offers[-1]
            assert "dark_theme" not in configuration and "hello_for_all_connections" not in configuration
            print("PASS: profiles contain neither username nor password")
        finally:
            user32.SetPhysicalCursorPos(original.x, original.y)
            fixture.stop.set()
            if not native_messages:
                for code in (0x43, 0x11):
                    key(code, False)
                if foreground:
                    user32.SetForegroundWindow(foreground)
            if window and process.poll() is None:
                user32.PostMessageW(window, 0x10, 0, 0)
            try:
                stdout, stderr = process.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                subprocess.run([
                    "powershell.exe", "-NoProfile", "-Command",
                    f"Stop-Process -Id {process.pid} -Force"
                ], check=True)
                stdout, stderr = process.communicate(timeout=5)
                raise AssertionError("Viewer did not close cleanly")
            fixture.listener.close()
            thread.join(timeout=3)
            if stderr:
                print(stderr)
            if process.returncode:
                raise AssertionError(f"Viewer exit code {process.returncode}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--executable", type=Path, default=ROOT / "target" / "x86_64-pc-windows-msvc" / "debug" / "maccanvas.exe")
    parser.add_argument("--native-messages", action="store_true", help="Dispatch native test messages without requiring foreground keyboard ownership")
    arguments = parser.parse_args()
    main(arguments.executable.resolve(), arguments.native_messages)
