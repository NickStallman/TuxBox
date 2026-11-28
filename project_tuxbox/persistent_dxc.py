"""The Linux DXC helper: its build, and a session-scoped pool of running copies."""

from contextlib import contextmanager
import hashlib
import os
from pathlib import Path
from queue import Queue
import select
import shutil
import subprocess
import sys
from tempfile import TemporaryDirectory
from time import monotonic

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "tools/dxc_worker.cpp"
# Releases whose helper output was checked on the GPU. The helper runs DXC's
# stages itself, which another release may arrange differently.
VERIFIED_DXC = "(1.9.2609."


def session_helper(env, report):
    """The helper a session compiles with, or None for the dxc command line.

    TUXBOX_PERSISTENT_DXC names one, or turns the helper off with `0`. Without
    it the launcher builds its own.
    """
    value = env.get("TUXBOX_PERSISTENT_DXC")
    if value == "0":
        return None
    return value or build_helper(report)


def build_helper(report, dxc=None, output=ROOT / "build/dxc-helper"):
    """Build the helper for `dxc` once and return its path, or None with the reason reported."""
    if sys.platform != "linux":
        return None

    def unused(reason):
        report(f"Not using the DXC helper, so shaders compile more slowly: {reason}")

    try:
        if dxc is None:
            from build import find_dxc
            dxc = find_dxc(None)
        release = Path(dxc).resolve().parent.parent
        include, library = release / "include", release / "lib"
        if not (include / "dxc/dxcapi.h").is_file() or not (library / "libdxcompiler.so").is_file():
            return unused("it needs a whole Microsoft DXC release, with its headers and library")
        version = subprocess.check_output([dxc, "--version"], timeout=10)
        if VERIFIED_DXC not in version.decode("utf-8", "replace"):
            return unused("it is checked only with DXC 1.9.2609")
        # A changed source or compiler release gets a helper of its own.
        key = hashlib.sha256(SOURCE.read_bytes() + version + os.fsencode(release)).hexdigest()[:16]
        helper = Path(output) / key / "dxc-worker"
        if helper.is_file():
            return str(helper)
        compiler = shutil.which("c++") or shutil.which("g++") or shutil.which("clang++")
        if not compiler:
            return unused("building it needs a C++ compiler")
        helper.parent.mkdir(parents=True, exist_ok=True)
        with TemporaryDirectory(dir=helper.parent) as work:
            built = Path(work) / helper.name
            run = subprocess.run([compiler, "-std=c++17", "-O2", "-isystem", include, SOURCE, "-L", library,
                                  "-ldxcompiler", "-o", built], capture_output=True, text=True,
                                 errors="replace", timeout=300)
            if run.returncode:
                errors = [line for line in run.stderr.splitlines() if "error" in line]
                return unused(f"it did not build: {(errors or ['unknown error'])[0][:300]}")
            os.replace(built, helper)
        return str(helper)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        return unused(error)


class CompilerExited(ConnectionError):
    pass


class Compiler:
    def __init__(self, helper, library):
        env = dict(os.environ, LD_LIBRARY_PATH=str(library))
        self.process = subprocess.Popen(
            [helper], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, bufsize=0, env=env,
        )

    def compile(self, hlsl, output, target, flags, deadline):
        """False when this DXC cannot build the program; the helper stays usable."""
        lines = (os.fsencode(hlsl), os.fsencode(output), target.encode("ascii"), " ".join(flags).encode("ascii"))
        self.process.stdin.write(b"\n".join(lines) + b"\n")
        response = b""
        while len(response) < 3:
            if not select.select([self.process.stdout], [], [], max(0, deadline - monotonic()))[0]:
                raise TimeoutError("persistent DXC timed out")
            data = os.read(self.process.stdout.fileno(), 3 - len(response))
            if not data:
                raise CompilerExited("persistent DXC exited")
            response += data
        if response not in (b"ok\n", b"no\n"):
            raise ValueError("invalid persistent DXC response")
        return response == b"ok\n"

    def close(self, *, kill=False):
        if kill:
            self.process.kill()
        self.process.stdin.close()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        finally:
            self.process.stdout.close()


class PersistentDxc:
    def __init__(self, helper, jobs, report):
        self.helper = helper
        self.report = report
        self.slots = Queue(jobs)
        for _ in range(jobs):
            self.slots.put(None)

    def compile(self, dxc, hlsl, output, target, flags=(), *, timeout=15):
        """Compile with the helper. False leaves the work to the dxc command line;
        a compile that runs out of time raises, as dxc itself would."""
        # The helper uses newline-delimited ASCII paths and the same Linux
        # library as the Microsoft DXC installation selected for this session.
        dxc = Path(dxc).resolve()
        library = dxc.parent.parent / "lib"
        if not (library / "libdxcompiler.so").is_file() or any(
            not str(path).isascii() or "\n" in str(path) for path in (hlsl, output)
        ):
            return False
        compiler = self.slots.get()
        deadline = monotonic() + timeout
        try:
            for attempt in range(2):
                try:
                    if compiler is None:
                        compiler = Compiler(self.helper, library)
                    if compiler.compile(hlsl, output, target, flags, deadline):
                        return True
                    output.unlink(missing_ok=True)
                    self.report(f"Persistent DXC could not compile a {target} shader, using the CLI")
                    return False
                except TimeoutError as error:
                    compiler.close(kill=True)
                    compiler = None
                    output.unlink(missing_ok=True)
                    raise subprocess.TimeoutExpired(str(self.helper), timeout) from error
                except (OSError, ValueError) as error:
                    if compiler is not None:
                        compiler.close(kill=True)
                        compiler = None
                    output.unlink(missing_ok=True)
                    if attempt or not isinstance(error, (CompilerExited, BrokenPipeError)) or monotonic() >= deadline:
                        self.report(f"Persistent DXC unavailable, using the CLI: {error}")
                        return False
        finally:
            self.slots.put(compiler)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        for _ in range(self.slots.maxsize):
            compiler = self.slots.get_nowait()
            if compiler is not None:
                compiler.close()


@contextmanager
def staged(helper, directory, jobs, report):
    """A pool running a private copy of `helper`, or None without a usable one.

    The copy keeps one build for the whole session, and its digest names the
    session's shader cache.
    """
    pinned = None
    if helper:
        try:
            pinned = Path(directory) / "dxc-worker"
            shutil.copyfile(Path(helper).expanduser(), pinned)
            pinned.chmod(0o700)
        except OSError as error:
            report(f"Could not stage persistent DXC, using the CLI: {error}")
            pinned = None
    if pinned is None:
        yield None
        return
    with PersistentDxc(pinned, jobs, report) as pool:
        yield pool
