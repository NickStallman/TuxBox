"""Display first-launch preparation in the terminal and optional desktop window."""
from contextlib import contextmanager
from html import escape
import os
import shutil
import subprocess
import sys
from time import monotonic


def report_progress(progress, message, completed=0, total=0):
    print(message, file=sys.stderr, flush=True)
    if progress:
        progress(message, completed, total)


class PrepPhase:
    """Report completed work, even on a warm cache, without logging each file."""

    def __init__(self, progress, label, total):
        self.progress = progress
        self.label = label
        self.total = total
        self.last_report = 0
        self.update(0, 0)

    def update(self, completed, compiled, *, unsupported=0, failed=0):
        now = monotonic()
        if completed not in (0, self.total) and now - self.last_report < 1:
            return
        self.last_report = now
        report_progress(self.progress, self.message(completed, compiled, unsupported, failed),
                        completed, self.total)

    def message(self, completed, compiled, unsupported, failed):
        skipped = unsupported + failed
        reused = completed - compiled - skipped
        status = "checked" if skipped else "ready"
        message = f"{self.label} · {completed}/{self.total} {status} · {compiled} compiled · {reused} reused"
        if skipped:
            message += f" · {unsupported} unsupported · {failed} failed"
        return message


class ProgramPhase(PrepPhase):
    """Preparation counted in shader programs. One program can take many
    requests, so the requests only measure the work left."""

    def __init__(self, progress, label, total, programs=0):
        self.programs = programs
        self.compiled_programs = set()
        self.refused_programs = set()
        self.failed_programs = set()
        super().__init__(progress, label, total)

    def count(self, program, category):
        """Note one request's result: compiled, cached, unsupported or failed."""
        group = {"unsupported": self.refused_programs, "failed": self.failed_programs}.get(
            category, self.compiled_programs)
        group.add(program)

    def outcome(self):
        """Programs compiled, then those with no compiled request that were
        refused, then the rest that failed."""
        refused = self.refused_programs - self.compiled_programs
        failed = self.failed_programs - self.compiled_programs - refused
        return len(self.compiled_programs), len(refused), len(failed)

    def summary(self):
        compiled, refused, failed = self.outcome()
        of = f" of {self.programs:,}" if self.programs else ""
        text = f"{compiled:,}{of} shaders compiled"
        if refused:
            text += f" · {refused:,} unsupported"
        if failed:
            text += f" · {failed:,} failed"
        return text

    def message(self, completed, compiled, unsupported, failed):
        return f"{self.label} · {self.summary()} · {completed:,}/{self.total:,} requests"


@contextmanager
def desktop_progress(title, env):
    """Show progress within the current phase. Console output works independently."""
    zenity = None
    if env.get("TUXBOX_SHADER_PREP_UI") == "1" and (env.get("DISPLAY") or env.get("WAYLAND_DISPLAY")):
        zenity = shutil.which("zenity", path=env.get("PATH"))
    if not zenity:
        yield None
        return

    try:
        dialog = subprocess.Popen(
            [zenity, "--progress", "--no-cancel", "--ok-label=Hide", "--percentage=0", "--width=560",
             f"--title=Preparing {title}", "--text=Checking the shader cache…"],
            env=env, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except OSError:
        yield None
        return

    def update(message, completed=0, total=0):
        if dialog.poll() is None:
            percent = completed * 100 // total if total else 0
            label = escape(message).replace("\n", " ")
            try:
                os.write(dialog.stdin.fileno(), f"# {label}\n{percent}\n".encode())
            except OSError:
                # A busy or closed window must never hold up preparation.
                pass

    try:
        os.set_blocking(dialog.stdin.fileno(), False)
        yield update
    finally:
        dialog.stdin.close()
        if dialog.poll() is None:
            dialog.terminate()
            try:
                dialog.wait(timeout=0.5)
            except subprocess.TimeoutExpired:
                dialog.kill()
                dialog.wait()
