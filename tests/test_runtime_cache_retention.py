"""Runtime caches of several branch builds: keep them, evict the stale ones.

Every build of the shader compiler names its runtime cache after the compiler
fingerprint, so switching branches used to leave the previous cache behind
forever. Retention keeps the caches worth hopping back to and removes the rest.
"""
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
from time import time
import unittest

from project_tuxbox.runtime_shaders import (
    IN_USE_FILE, USED_FILE, locked_by_another_launcher, mark_in_use,
    retain_runtime_caches, runtime_caches,
)

import tools.rank_pipeline_failures as ranking


@unittest.skipUnless(sys.platform == "linux", "Runtime shader caches are claimed with flock, on Proton on Linux")
class RuntimeCacheRetention(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.runtime = self.root / "runtime"
        self.runtime.mkdir()

    def cache(self, name, used):
        """A fingerprint directory last used at `used` (seconds).

        Both the marker and the directory are backdated: the directory alone
        is not trusted, and a test that only set one would not prove which one
        retention ordered by.
        """
        folder = self.runtime / name
        folder.mkdir()
        (folder / "aa.cso").write_text("translated")
        (folder / "aa.sha256").write_text("digest")
        (folder / USED_FILE).write_text("")
        os.utime(folder / USED_FILE, (used, used))
        os.utime(folder, (used, used))
        return folder

    def names(self):
        return sorted(path.name for path in self.runtime.iterdir())

    def test_keep_three_most_recently_used(self):
        now = time()
        self.cache("oldest", now - 400)
        self.cache("older", now - 300)
        self.cache("old", now - 200)
        self.cache("recent", now - 100)
        current = self.cache("current", now)

        removed = retain_runtime_caches(self.runtime, current)

        self.assertEqual(sorted(removed), ["older", "oldest"])
        self.assertEqual(self.names(), ["current", "old", "recent"])

    def test_current_is_kept_even_when_it_is_the_oldest(self):
        now = time()
        current = self.cache("current", now - 900)
        for age, name in enumerate(["a", "b", "c", "d"]):
            self.cache(name, now - age)

        retain_runtime_caches(self.runtime, current)

        self.assertIn("current", self.names())

    def test_running_launcher_keeps_its_cache(self):
        now = time()
        running = self.cache("running", now - 400)
        self.cache("stale", now - 300)
        self.cache("recent", now - 100)
        current = self.cache("current", now)
        # Stand in for the other launcher: flock conflicts hold between two
        # descriptors of one process too, so no second process is needed.
        self.addCleanup(os.close, mark_in_use(running))

        retain_runtime_caches(self.runtime, current, keep=2)

        self.assertEqual(self.names(), ["current", "recent", "running"])

    def test_prelaunch_and_live_worker_share_one_claim(self):
        now = time()
        current = self.cache("current", now)
        first = mark_in_use(current)
        self.addCleanup(os.close, first)

        self.assertIs(mark_in_use(current), first)
        self.assertTrue(locked_by_another_launcher(current))

    def test_two_launchers_of_one_build_share_its_cache(self):
        current = self.cache("current", time())
        self.addCleanup(os.close, mark_in_use(current))
        other = ("import sys; from project_tuxbox.runtime_shaders import mark_in_use; "
                 "mark_in_use(sys.argv[1])")

        run = subprocess.run([sys.executable, "-c", other, str(current)], capture_output=True,
                             text=True, timeout=30, cwd=Path(__file__).resolve().parents[1])

        self.assertEqual(run.returncode, 0, run.stderr)

    def test_directory_time_orders_caches_without_a_marker(self):
        now = time()
        fixtures = self.runtime / "unmarked"
        fixtures.mkdir()
        os.utime(fixtures, (now - 100, now - 100))
        marked = self.cache("marked", now - 500)
        current = self.cache("current", now)

        self.assertEqual([path.name for path in runtime_caches(self.runtime)],
                         ["current", "unmarked", "marked"])

        retain_runtime_caches(self.runtime, current, keep=1)

        self.assertEqual(self.names(), ["current"])

    def test_last_used_marker_moves_a_cache_ahead(self):
        now = time()
        moved = self.cache("revived", now - 900)
        self.cache("idle", now - 500)
        current = self.cache("current", now)
        os.utime(moved / USED_FILE, (now - 50, now - 50))

        retain_runtime_caches(self.runtime, current, keep=2)

        self.assertEqual(self.names(), ["current", "revived"])

    def test_files_leave_the_directory_alone(self):
        now = time()
        (self.runtime / "loose.txt").write_text("not a cache")
        current = self.cache("current", now)

        self.assertEqual(runtime_caches(self.runtime), [current])
        retain_runtime_caches(self.runtime, current, keep=1)
        self.assertTrue((self.runtime / "loose.txt").is_file())

    def test_keep_must_be_positive(self):
        with self.assertRaises(ValueError):
            retain_runtime_caches(self.runtime, self.cache("current", time()), keep=0)

    def test_ranking_reads_the_same_marker_the_runtime_writes(self):
        # The two modules are independent (the ranking tool imports nothing
        # from project_tuxbox), so nothing but this test ties the names together.
        self.assertEqual(ranking.LAST_USED, USED_FILE)
        self.assertEqual(IN_USE_FILE, "in-use")


if __name__ == "__main__":
    unittest.main()
