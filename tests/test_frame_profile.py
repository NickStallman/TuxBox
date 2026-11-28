from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from tools.check_frame_profile import inspect


class FrameProfileTests(unittest.TestCase):
    def test_other_executables_are_refused_by_hash(self):
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "game.exe"
            path.write_bytes(b"MZ" + bytes(3 << 20))
            with self.assertRaisesRegex(ValueError, "Unknown executable hash"):
                inspect(path, "game.exe")


if __name__ == "__main__":
    unittest.main()
