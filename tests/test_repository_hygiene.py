import os
import re
import subprocess
import sys
import unittest
from pathlib import Path

from project_tuxbox.verify import status


ROOT = Path(__file__).resolve().parents[1]
RUST_LINE_LIMIT = 1200


def rust_sources():
    """Tracked and new Rust files, by repository path."""
    listed = subprocess.run(["git", "ls-files", "-co", "--exclude-standard", "*.rs"],
                            cwd=ROOT, check=True, capture_output=True, text=True).stdout.splitlines()
    return {name: (ROOT / name).read_text(encoding="utf-8") for name in listed if (ROOT / name).is_file()}


class RepositoryHygieneTests(unittest.TestCase):
    def test_results_table_matches_recorded_evidence(self):
        saved = (ROOT / "docs/RESULTS.md").read_text(encoding="utf-8")
        generated = status() + "\n"
        self.assertEqual(saved, generated,
                         "Regenerate with python tuxbox.py status > docs/RESULTS.md")

    def test_results_table_is_written_as_utf8_whatever_the_console_encoding(self):
        # A Windows console writes redirected output in its code page, such as cp1252.
        env = {**os.environ, "PYTHONIOENCODING": "cp1252", "PYTHONUTF8": "0"}
        written = subprocess.run([sys.executable, str(ROOT / "tuxbox.py"), "status"], cwd=ROOT, env=env,
                                 check=True, capture_output=True).stdout
        self.assertEqual(written.replace(b"\r\n", b"\n").decode("utf-8"), status() + "\n")

    def test_no_tracked_files_match_gitignore(self):
        result = subprocess.run(
            ["git", "ls-files", "-c", "--ignored", "--exclude-standard"],
            cwd=ROOT, check=True, capture_output=True, text=True)
        tracked = [line for line in result.stdout.splitlines() if line]
        self.assertEqual(tracked, [],
                         "These tracked files match .gitignore. Remove them from the index.")

    def test_no_rust_file_outgrows_the_limit(self):
        long = {name: len(text.splitlines()) for name, text in rust_sources().items()
                if len(text.splitlines()) > RUST_LINE_LIMIT}
        self.assertEqual(long, {}, "Split these by subject. See docs/SOURCE_LAYOUT.md")

    def test_paths_are_chosen_by_host_capability_not_vendor(self):
        vendor = re.compile(r"\b0x(?:10de|1002|8086)\b|\.VendorId\b|\bvendor_id\b", re.I)
        # Where the adapter is identified so the host profile can report it.
        reporting = ("runtime/d3d12/src/device/dxgi.rs", "runtime/d3d12/src/device/host_profile")
        found = sorted(name for name, text in rust_sources().items()
                       if name.startswith(("runtime/", "tools/shader-compiler/src/"))
                       and "/src/tests/" not in name and not name.startswith(reporting) and vendor.search(text))
        self.assertEqual(found, [], "Choose from the host profile instead. See docs/USAGE.md")

    def test_rust_tests_live_in_the_tests_folder(self):
        inline = re.compile(r"^\s*#\[cfg\(test\)\]\s*\n\s*(?:pub(?:\([^)]*\))? )?mod \w+\s*\{", re.M)
        # The reference controls are single files compiled on their own.
        found = sorted(name for name, text in rust_sources().items()
                       if not name.startswith("tools/reference/") and "/src/tests/" not in name
                       and inline.search(text))
        self.assertEqual(found, [], "Move these into src/tests. See docs/SOURCE_LAYOUT.md")
