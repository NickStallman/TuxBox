"""The dev container's display script must leave a desktop's X servers alone."""
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / ".devcontainer/start-display.sh"
IN_CONTAINER = any(Path(marker).exists() for marker in ("/.dockerenv", "/run/.containerenv"))


class DisplayScriptTests(unittest.TestCase):
    def test_kills_no_process_by_name(self):
        code = [line.split("#")[0] for line in SCRIPT.read_text().splitlines()]
        self.assertFalse([line for line in code if "pkill" in line or "killall" in line])

    @unittest.skipUnless(sys.platform.startswith("linux") and shutil.which("bash"), "needs bash on Linux")
    @unittest.skipIf(IN_CONTAINER, "inside a container the script starts a display")
    def test_refuses_to_run_outside_a_container(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            tools, runtime = root / "bin", root / "runtime"
            tools.mkdir()
            runtime.mkdir()
            # Only these are on PATH. Every program that could reach the desktop
            # is a stand-in that records its name.
            for name in ("cat", "chmod", "grep", "mkdir", "rm", "seq", "tr"):
                (tools / name).symlink_to(shutil.which(name))
            for name, status in (("pgrep", 0), ("pkill", 0), ("setsid", 0), ("sleep", 0), ("xdpyinfo", 1)):
                stub = tools / name
                stub.write_text(f"#!/bin/sh\necho {name} >>'{root}/calls'\nexit {status}\n")
                stub.chmod(0o755)
            with socket.socket(socket.AF_UNIX) as compositor:
                compositor.bind(str(runtime / "wayland-0"))
                result = subprocess.run(
                    [shutil.which("bash"), SCRIPT], capture_output=True, text=True, timeout=60,
                    env={"PATH": str(tools), "DISPLAY": ":99", "XDG_RUNTIME_DIR": str(runtime),
                         "WAYLAND_DISPLAY": "wayland-0", "HOME": tmp})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not a container", result.stderr)
            self.assertFalse((root / "calls").exists(), "the script ran a program before refusing")


if __name__ == "__main__":
    unittest.main()
