#!/usr/bin/env python3
"""Run real Vita code in an isolated Vita3K instance; save screenshots and JSON results."""
import argparse
import atexit
import contextlib
import hashlib
import json
import os
import re
from pathlib import Path
import selectors
import signal
import shutil
import subprocess
import sys
import time
import unittest
from datetime import datetime, timezone

from PIL import Image, ImageGrab, ImageOps

from fixtures import Fixtures


class App:
    def __init__(self, options, name, authenticated=True, kind="plain", frontend="sdl"):
        self.options = options
        self.directory = options.output / name
        self.directory.mkdir(parents=True)
        self.fixtures = Fixtures(self.directory)
        self.fixtures.kind = kind
        self.fs = self.directory / "fs"
        self.fs.mkdir()
        for mount in ("os0", "sa0", "vs0"):
            # Registry files can change on boot, so each case gets real copies.
            # copytree also dereferences symlinks rather than retaining a path
            # back into the user's installation.
            shutil.copytree((options.firmware / mount).resolve(), self.fs / mount)
        self.data = self.fs / "ux0/data/BSKY00001"
        self.data.mkdir(parents=True)
        if authenticated:
            (self.data / "emulator-test.json").write_text(json.dumps({"endpoint": self.fixtures.endpoint}))
        self.process = None
        self.xvfb = None
        self.window = None
        self.closed = False
        self.frontend = frontend
        self.env = os.environ.copy()
        self.env.update({"XDG_CONFIG_HOME": str(self.directory / "config"),
            "XDG_DATA_HOME": str(self.directory / "data"), "XDG_CACHE_HOME": str(self.directory / "cache"),
            "TMPDIR": str(self.directory / "tmp"), "SDL_VIDEODRIVER": "x11", "SDL_AUDIODRIVER": "dummy",
            "GDK_BACKEND": "x11", "QT_QPA_PLATFORM": "xcb"})
        self.env.update(QT_QPA_PLATFORMTHEME="generic", QT_STYLE_OVERRIDE="Fusion", GTK_USE_PORTAL="0")
        self.env.pop("WAYLAND_DISPLAY", None)
        Path(self.env["TMPDIR"]).mkdir()
        authority = self.directory / "Xauthority"
        authority.touch()
        self.env["XAUTHORITY"] = str(authority)
        user = self.fs / "ux0/user/00"
        user.mkdir(parents=True)
        (user / "user.xml").write_text('<user id="00" name="Emulator Test"/>')
        if not options.hardware:
            self.env.update(LIBGL_ALWAYS_SOFTWARE="1", USE_HOST_MESA_DRIVERS="1")
            if options.backend == "Vulkan":
                candidates = list(Path("/usr/share/vulkan/icd.d").glob("*lvp*.json"))
                if not candidates:
                    raise RuntimeError("Vulkan software tests require Mesa lavapipe (lvp ICD)")
                self.env.update(SHARUN_ALLOW_SYS_VKICD="1", VK_DRIVER_FILES=str(candidates[0]),
                    VK_LOADER_DRIVERS_SELECT="*lvp*")
            # The AppImage bundles newer Wayland libraries than Debian. Its generic
            # host-Mesa hook replaces those too, breaking SDL before launch. Expose
            # only the system graphics driver libraries inside this test's TMPDIR.
            mesa = Path(self.env["TMPDIR"]) / ".HOST_MESA/lib"
            mesa.mkdir(parents=True)
            for base in (Path("/usr/lib/x86_64-linux-gnu"), Path("/usr/lib64"), Path("/usr/lib")):
                if not (base / "dri").is_dir():
                    continue
                for pattern in ("dri", "libGL*.so*", "libEGL*.so*", "libglapi*.so*", "libgallium*.so*", "libgbm*.so*"):
                    for library in base.glob(pattern):
                        target = mesa / library.name
                        if not target.exists():
                            target.symlink_to(library)
                break
        self.config = self.directory / "config.yml"
        self.config.write_text(f'''initial-setup: true
pref-path: {json.dumps(str(self.fs))}
backend-renderer: {options.backend}
user-id: "00"
user-auto-connect: true
show-welcome: false
check-for-updates-mode: 0
discord-rich-presence: false
disable-motion: true
performance-overlay: false
memory-mapping: double-buffer
disable-surface-sync: false
cpu-opt: true
high-accuracy: false
texture-cache: true
log-level: 2
''')

    def start(self):
        r, w = os.pipe()
        self.xlog = open(self.directory / "xvfb.log", "w")
        try:
            self.xvfb = subprocess.Popen(["Xvfb", "-displayfd", str(w), "-screen", "0", "1280x720x24",
                "-nolisten", "tcp", "-noreset", "-ac"], pass_fds=(w,), stdout=self.xlog, stderr=self.xlog)
        finally:
            os.close(w)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(r, selectors.EVENT_READ)
                if not selector.select(10):
                    raise AssertionError("Xvfb did not start; see xvfb.log")
                number = os.read(r, 32).decode().strip()
                if not number.isdigit():
                    raise AssertionError("Xvfb returned no display number")
                self.env["DISPLAY"] = ":" + number
                (self.directory / "display.txt").write_text(self.env["DISPLAY"])
        finally:
            os.close(r)
        atexit.register(self.close)
        # Verify that X11 accepts clients before the AppImage's GTK/SDL probes.
        subprocess.run(["xwininfo", "-root"], env=self.env, capture_output=True, check=True, timeout=5)
        self.log = open(self.directory / "emulator.log", "w")
        self.process = subprocess.Popen(["dbus-run-session", "--", str(self.options.vita3k), "--frontend", self.frontend, "-c", str(self.config),
            "--keep-config", "-l", "2", "-B", self.options.backend, str(self.options.vpk)],
            env=self.env, cwd=self.directory, stdout=self.log, stderr=self.log, start_new_session=True)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            self.alive()
            search = subprocess.run(["xwininfo", "-root", "-tree"], env=self.env,
                capture_output=True, text=True, timeout=5)
            (self.directory / "window-tree.txt").write_text(search.stdout)
            windows = re.findall(r'(0x[0-9a-f]+).*?\s(\d+)x(\d+)[+-]\d+[+-]\d+', search.stdout)
            for window, width, height in sorted(windows, key=lambda w: int(w[1]) * int(w[2]), reverse=True):
                if int(width) * int(height) < 100000:
                    continue
                self.window = window
                self.command("windowsize", self.window, "960", "544")
                self.command("windowmove", self.window, "0", "0")
                if self.command("windowfocus", self.window, check=False).returncode:
                    continue
                return
            time.sleep(0.2)
        self.image("launch-failure")
        raise AssertionError("Vita3K did not create a named window")

    def close(self):
        if self.closed:
            return
        self.closed = True
        if self.process:
            # Include any frontend/DBus helper children in cleanup.
            with contextlib.suppress(ProcessLookupError):
                os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=5)
        if self.xvfb:
            self.xvfb.terminate()
            self.xvfb.wait(timeout=5)
        for name in ("log", "xlog"):
            handle = getattr(self, name, None)
            if handle:
                handle.close()
        self.fixtures.close()
        if self.fixtures.unexpected:
            raise AssertionError(f"Unexpected fixture requests: {self.fixtures.unexpected}")

    def alive(self):
        if self.process and self.process.poll() is not None:
            raise AssertionError(f"Vita3K exited ({self.process.returncode}); see {self.directory / 'emulator.log'}")
        path = self.directory / "emulator.log"
        if path.exists():
            with path.open("rb") as log:
                log.seek(max(0, path.stat().st_size - 8192))
                if b"Unhandled SIGSEGV" in log.read():
                    raise AssertionError(f"Vita3K reported a native SIGSEGV; see {path}")

    def command(self, *args, check=True):
        return subprocess.run(["xdotool", *map(str, args)], env=self.env, capture_output=True,
            text=True, timeout=10, check=check)

    def tap(self, x, y):
        self.command("mousemove", x, y, "mousedown", "1")
        time.sleep(0.3)
        self.command("mouseup", "1")
        time.sleep(0.2)

    def key(self, key, duration=0.2):
        self.command("keydown", key)
        time.sleep(duration)
        self.command("keyup", key)
        time.sleep(0.2)

    def type(self, text):
        self.command("type", "--clearmodifiers", "--delay", "35", "--", text)

    def image(self, label=None):
        self.alive()
        image = ImageGrab.grab(xdisplay=self.env["DISPLAY"]).crop((0, 0, 960, 544)).convert("RGB")
        image.save(self.directory / "latest.png")
        if label:
            image.save(self.directory / f"{label}.png")
        return image

    def text(self, image=None, box=None):
        image = image if image is not None else self.image()
        if box:
            image = image.crop(box)
        # Grayscale preserves thin red error glyphs. A minimum-channel version
        # gives white text on saturated blue buttons enough contrast. Both are
        # derived from the same captured frame, without thresholding away edges.
        minimum = Image.new("L", image.size)
        minimum.putdata([min(pixel) for pixel in image.convert("RGB").getdata()])
        texts = []
        for name, plane in (("gray", image.convert("L")), ("minimum", minimum)):
            path = self.directory / f"ocr-{name}.png"
            ImageOps.invert(plane).resize((image.width * 3, image.height * 3)).save(path)
            result = subprocess.run([str(self.options.tesseract), str(path), "stdout", "--psm", "6" if box else "11"],
                capture_output=True, text=True, timeout=30, check=True,
                env={**os.environ, "OMP_THREAD_LIMIT": "1"})
            texts.append(result.stdout)
        text = "\n".join(texts)
        (self.directory / "latest.txt").write_text(text)
        return " ".join(text.split())

    def wait(self, predicate, message, timeout=25):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.alive()
            if predicate():
                return
            time.sleep(0.25)
        self.image("failure")
        raise AssertionError(message() if callable(message) else message)

    def wait_text(self, *texts, timeout=25, box=None):
        last = [""]
        def matches():
            last[0] = self.text(box=box)
            return all(text.lower() in last[0].lower() for text in texts)
        self.wait(matches, lambda: f"Screen did not contain {texts}; last OCR: {last[0]!r}", timeout)
        self.image("passed-screen")

    def colored(self, color, box=(0, 90, 960, 480)):
        pixels = self.image().crop(box).getdata()
        if color == "red":
            return sum(r > 170 and g < 100 and b < 100 for r, g, b in pixels)
        if color == "green":
            return sum(g > 150 and r < 110 and b < 120 for r, g, b in pixels)
        return sum(max(r, g, b) - min(r, g, b) > 80 for r, g, b in pixels)


class Results(unittest.TextTestResult):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.records = []
    def startTest(self, test):
        self.started = time.monotonic()
        super().startTest(test)
    def record(self, test, status, detail=""):
        self.records.append({"test": test.id(), "status": status,
            "seconds": round(time.monotonic() - self.started, 3), "detail": detail})
        if detail:
            directory = test.options.output / test._testMethodName
            if directory.is_dir():
                (directory / "failure.txt").write_text(detail)
    def addSuccess(self, test):
        super().addSuccess(test)
        self.record(test, "passed")
    def addFailure(self, test, err):
        super().addFailure(test, err)
        self.record(test, "failed", self._exc_info_to_string(err, test))
    def addError(self, test, err):
        super().addError(test, err)
        self.record(test, "error", self._exc_info_to_string(err, test))
    def addSkip(self, test, reason):
        super().addSkip(test, reason)
        self.record(test, "skipped", reason)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vpk", type=Path, required=True, help="VPK built with --features emulator-tests")
    parser.add_argument("--firmware", type=Path, default=Path.home() / ".local/share/Vita3K/Vita3K")
    parser.add_argument("--vita3k", type=Path, default=shutil.which("vita3k"))
    parser.add_argument("--tesseract", type=Path, default=shutil.which("tesseract"))
    parser.add_argument("--output", type=Path, default=Path(__file__).resolve().parents[2] / "target/vita3k-tests" /
        datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ"))
    parser.add_argument("--backend", choices=["OpenGL", "Vulkan"], default="OpenGL")
    parser.add_argument("--hardware", action="store_true", help="Do not force a software Mesa driver")
    parser.add_argument("--test", action="append", help="Run only tests whose names contain this substring")
    options = parser.parse_args()
    for command in ("Xvfb", "xdotool", "xwininfo", "dbus-run-session", "ffmpeg"):
        if not shutil.which(command):
            parser.error(f"missing dependency: {command}")
    for name in ("vita3k", "tesseract", "vpk"):
        value = getattr(options, name)
        if not value or not value.is_file():
            parser.error(f"{name} not found; pass --{name}")
        setattr(options, name, value.resolve())
    options.firmware = options.firmware.resolve()
    for mount in ("os0", "sa0", "vs0"):
        if not (options.firmware / mount).is_dir():
            parser.error(f"firmware mount missing: {options.firmware / mount}")
    options.output = options.output.resolve()
    options.output.mkdir(parents=True, exist_ok=False)
    from tests import EmulatorTests
    EmulatorTests.options = options
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(EmulatorTests)
    if options.test:
        suite = unittest.TestSuite(test for test in suite if any(s in test.id() for s in options.test))
    if not suite.countTestCases():
        parser.error("no tests selected")
    version = subprocess.run([str(options.vita3k), "--version"], capture_output=True, text=True, timeout=20)
    result = unittest.TextTestRunner(verbosity=2, resultclass=Results).run(suite)
    report = {"emulator": version.stdout.strip(), "backend": options.backend,
        "software_rendering": not options.hardware, "vpk": str(options.vpk),
        "vpk_sha256": hashlib.sha256(options.vpk.read_bytes()).hexdigest(),
        "tests": result.records, "success": result.wasSuccessful()}
    (options.output / "results.json").write_text(json.dumps(report, indent=2))
    print(f"Artifacts: {options.output}", flush=True)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
