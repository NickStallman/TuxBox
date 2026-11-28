"""Launch through the host PE loader, or through a selected userspace Wine provider."""
import json
import os
from pathlib import Path
import platform
import re
import shutil
import struct
import subprocess
import sys
from contextlib import nullcontext

from .backend_log import PROTON_APP_ID, game_diagnostics
from .console_cbv import configure_console_cbv
from .mangohud import performance_capture
from .pipeline_profile import pipeline_capture
from .runtime_shaders import runtime_shader_compiler
from .shader_prep import SOURCE_CACHE
from .vkd3d_cache import merge_pipeline_cache


# The Proton distribution used when none is selected. Its dxil-spirv converts
# large translated shaders in seconds, where older releases take minutes.
PINNED_PROTON = "GE-Proton11-14"


# Titles that are only ever launched without network access. ProjectScarlett's
# crash handler uploads minidumps to an online service.
OFFLINE_EXECUTABLES = ("game_scarlett_master.exe",)

# Room for a title's compiled GPU pipelines. With 32 GiB, ProjectScarlett's
# cache file reached 13.7 GB and the driver stopped updating its index after
# about 5 GB, so nothing written later was found again. Keep each file small
# enough to stay indexed.
NVIDIA_CACHE_BYTES = 4 << 30

# A launch starts a new cache file once the current one holds this much, which
# leaves one launch 3 GiB before the file's 32-bit index is full.
NVIDIA_CACHE_ROTATE_BYTES = 1 << 30
NVIDIA_CACHE_FILE = re.compile(r"tuxbox([0-9]+)[0-9]")

# Room for the driver's compiled shaders on Mesa (RADV, ANV, NVK). One
# ProjectScarlett boot on RADV writes about 1.2 GB, over Mesa's 1 GB default.
MESA_CACHE_SIZE = "4G"


def requires_offline(executable):
    return executable.name.casefold() in OFFLINE_EXECUTABLES


def _descendant_pids(pid):
    """All descendants of pid, depth-first, via /proc (Linux)."""
    children = {}
    try:
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            try:
                with open(f"/proc/{entry}/stat", "r") as proc:
                    raw = proc.read()
                # /proc/<pid>/stat: "pid (comm) state ppid ..." -- comm may contain spaces.
                close = raw.rfind(")")
                if close == -1:
                    continue
                fields = raw[close + 2:].split()
                parent = int(fields[1])
                children.setdefault(parent, []).append(int(entry))
            except (OSError, ValueError):
                continue
    except OSError:
        return []
    collected, stack = [], [pid]
    while stack:
        current = stack.pop()
        for child in children.get(current, ()):
            collected.append(child)
            stack.append(child)
    return collected


def proton_installation(selection):
    if selection:
        path = Path(selection).expanduser()
        resolved = path.resolve()
        if resolved.is_file():
            if resolved.name == "proton":
                return resolved.parent
            if path.name == "proton":
                return path.parent.resolve()
        return resolved
    steam = Path(os.environ.get("STEAM_COMPAT_CLIENT_INSTALL_PATH") or Path.home() / ".steam/root")
    libraries = [steam, Path.home() / ".local/share/Steam",
                 Path.home() / ".var/app/com.valvesoftware.Steam/.local/share/Steam"]
    for installation in libraries.copy():
        vdf = installation / "steamapps/libraryfolders.vdf"
        if vdf.is_file():
            libraries += [Path(p) for p in re.findall(r'"path"\s*"([^"]+)"', vdf.read_text(errors="replace"))]
    for library in libraries:
        for parent in ("compatibilitytools.d", "steamapps/common"):
            # The release archive adds the architecture; Proton managers may not.
            for name in (f"{PINNED_PROTON}-x86_64", PINNED_PROTON):
                candidate = library / parent / name
                if (candidate / "proton").is_file():
                    return candidate.resolve()
    raise ValueError(f"Install {PINNED_PROTON} in {steam / 'compatibilitytools.d'} from "
                     f"https://github.com/GloriousEggroll/proton-ge-custom/releases/tag/{PINNED_PROTON}, "
                     "or select another distribution with --proton or PROTONPATH")


# Translated shaders, compiled DXIL and recorded requests, inside the prefix. Every
# entry is keyed by the content that produced it, so a prefix for another Proton
# can reuse them as they are.
SHADER_CACHE = SOURCE_CACHE.parent


def seed_shader_cache(prefix, report):
    """Give a new default prefix the shader cache of the most recent other one.

    A new Proton release gets a new prefix, and without this the first launch
    translates and compiles every shader again, which takes hours for a large
    title. Copies share disk blocks where the file system allows it.
    """
    target = prefix / SHADER_CACHE
    if target.exists():
        return
    others = [p for p in prefix.parent.iterdir()
              if p != prefix and p.is_dir() and (p / SHADER_CACHE).is_dir()]
    if not others:
        return
    donor = max(others, key=lambda other: (other / SHADER_CACHE).stat().st_mtime)
    source = donor / SHADER_CACHE
    target.parent.mkdir(parents=True, exist_ok=True)
    pending = target.with_name(target.name + ".seeding")
    shutil.rmtree(pending, ignore_errors=True)
    try:
        if shutil.which("cp") and sys.platform == "linux":
            subprocess.run(["cp", "-a", "--reflink=auto", str(source), str(pending)],
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        else:
            shutil.copytree(source, pending, symlinks=True)
        pending.rename(target)
    except (OSError, subprocess.CalledProcessError) as exc:
        shutil.rmtree(pending, ignore_errors=True)
        report(f"Could not reuse the shader cache from {donor.name}: {exc}")
        return
    report(f"Reusing the shader cache from the {donor.name} prefix")


def warn_scarlett_dxc(executable, report):
    """Check app-local Windows DXC files, without installing or changing them.

    A system-provided compiler can still work, so this is advisory only.
    The Linux dxc used for shader preparation is a separate dependency.
    """
    if executable.name.casefold() != "game_scarlett_master.exe":
        return
    import pefile

    problems = []
    for name in ("dxcompiler.dll", "dxil.dll"):
        path = executable.with_name(name)
        if not path.is_file():
            problems.append(f"{name} missing")
            continue
        try:
            pe = pefile.PE(str(path), fast_load=True)
            try:
                if (pe.FILE_HEADER.Machine != 0x8664 or pe.OPTIONAL_HEADER.Magic != 0x20b
                        or not pe.FILE_HEADER.Characteristics & 0x2000):
                    problems.append(f"{name} is not an x64 Windows DLL")
            finally:
                pe.close()
        except (OSError, pefile.PEFormatError):
            problems.append(f"{name} is unreadable or not a valid PE")
    if problems:
        report("ProjectScarlett Windows DXC: " + "; ".join(problems)
               + ". For rectangle pipelines, stage both DLLs from the same Microsoft x64 DXC release"
               " beside the executable (see docs/PROJECTSCARLETT.md). A system installation may also work.")


PACKAGE_DRIVE_RECORD = "tuxbox-package-drive.json"


def claim_package_drive(prefix, package):
    """Record the prefix's G: links before the GDK provider replaces them with the package.

    Returns False, and records nothing, when G: is not a drive link TuxBox can put back.
    """
    links = {}
    for name in ("g:", "g::"):
        link = prefix / "dosdevices" / name
        if link.is_symlink():
            links[name] = os.readlink(link)
        elif os.path.lexists(link):
            return False
        else:
            links[name] = None
    (prefix / PACKAGE_DRIVE_RECORD).write_text(json.dumps({"package": str(package), "links": links}))
    return True


def restore_package_drive(prefix, report):
    """Put back the G: links a launch replaced, including after an interrupted launch."""
    record_path = prefix / PACKAGE_DRIVE_RECORD
    try:
        record = json.loads(record_path.read_text())
        package, links = record["package"], record["links"]
    except FileNotFoundError:
        return
    except (OSError, ValueError, KeyError, TypeError) as exc:
        report(f"Ignoring an unreadable G: record {record_path}: {exc}")
        record_path.unlink(missing_ok=True)
        return
    try:
        for name, target in links.items():
            link = prefix / "dosdevices" / name
            # Leave a link that something other than the provider changed meanwhile.
            if link.is_symlink() and os.readlink(link) != package:
                continue
            if os.path.lexists(link) and not link.is_symlink():
                continue
            link.unlink(missing_ok=True)
            if target is not None:
                link.symlink_to(target)
        record_path.unlink()
    except OSError as exc:
        report(f"Could not restore the prefix's G: drive from {record_path}: {exc}")


def _driver_cache_name(name):
    return len(name) == 32 and all(character in "0123456789abcdef" for character in name)


def adopt_nvidia_glcache(cache):
    """Fill <path>/GLCache from a device bucket that was copied beside it.

    __GL_SHADER_DISK_CACHE_PATH replaces ~/.cache/nvidia. The driver then
    reads <path>/GLCache/<device>/<app>, so a copy at <path>/<device> is never
    opened. An existing GLCache file stays: it belongs to this path, and
    substituting a blob from the default cache makes the driver drop both.
    """
    try:
        entries = list(cache.iterdir())
    except OSError:
        return
    for device in entries:
        if not device.is_dir() or device.name == "GLCache" or not _driver_cache_name(device.name):
            continue
        try:
            apps = [app for app in device.iterdir() if app.is_dir()]
        except OSError:
            return
        for app in apps:
            destination = cache / "GLCache" / device.name / app.name
            try:
                destination.mkdir(parents=True, exist_ok=True)
                sources = [path for path in app.iterdir() if path.is_file()]
            except OSError:
                return
            for source in sources:
                target = destination / source.name
                try:
                    # A file already under GLCache was written for this path.
                    # Replacing it with a larger copy from the default cache
                    # makes the driver discard both and compile again.
                    if target.exists():
                        continue
                    temporary = destination / f".{source.name}.adopting"
                    shutil.copy2(source, temporary)
                    temporary.replace(target)
                except OSError:
                    temporary = destination / f".{source.name}.adopting"
                    temporary.unlink(missing_ok=True)
                    return
        shutil.rmtree(device, ignore_errors=True)


# The driver keeps each cache as a .bin of records and a .toc that indexes
# them. Both start with the same 32-byte header. A .toc entry is a 16-byte
# hash, then the record's offset and payload size as 32-bit values, and each
# record in the .bin carries 32 bytes of its own before the payload.
NVIDIA_CACHE_MAGIC = b"CDVN"
NVIDIA_CACHE_HEADER = 32
NVIDIA_TOC_ENTRY = struct.Struct("<16sII")


def _nvidia_indexed_bytes(toc):
    """How much of the .bin beside toc its index covers, or None if unreadable."""
    try:
        with open(toc, "rb") as index:
            if index.read(len(NVIDIA_CACHE_MAGIC)) != NVIDIA_CACHE_MAGIC:
                return None
            entries = os.fstat(index.fileno()).st_size - NVIDIA_CACHE_HEADER
            if entries < 0 or entries % NVIDIA_TOC_ENTRY.size:
                return None
            if not entries:
                return NVIDIA_CACHE_HEADER
            # Entries are appended in file order, so the last one ends the index.
            index.seek(-NVIDIA_TOC_ENTRY.size, os.SEEK_END)
            _, offset, size = NVIDIA_TOC_ENTRY.unpack(index.read(NVIDIA_TOC_ENTRY.size))
    except (OSError, struct.error):
        return None
    return offset + NVIDIA_CACHE_HEADER + size


def report_unindexed_nvidia_cache(cache, report):
    """Say when a driver cache file holds pipelines its index cannot find.

    The index addresses the file with 32-bit offsets. The driver still appends
    past 4 GiB, and it compiles those pipelines again on every launch. Nothing
    is changed here: moving a cache aside is the person's decision.
    """
    try:
        indexes = sorted((Path(cache) / "GLCache").glob("*/*/*.toc"))
    except OSError:
        return
    for toc in indexes:
        blob = toc.with_suffix(".bin")
        indexed = _nvidia_indexed_bytes(toc)
        try:
            stored = blob.stat().st_size
        except OSError:
            continue
        if indexed is None or stored <= indexed and stored < 1 << 32:
            continue
        if stored > indexed:
            fault = (f"{(stored - indexed) / (1 << 20):,.0f} MiB of its {stored / (1 << 30):.1f} GiB is past the "
                     "end of its index and is compiled again on every launch")
        else:
            fault = "it has reached 4 GiB, the most its index can address, so nothing added to it is found again"
        report(f"NVIDIA shader cache {blob} has stopped being indexed: {fault}. Move the .bin and .toc aside "
               "while no game is running to let the driver start a new cache.")


def rotate_nvidia_shader_cache(env, cache):
    """Write this launch's pipelines to a file with room left, and read the others.

    The driver's index addresses a cache file with 32-bit offsets, so a file
    stops taking new pipelines at 4 GiB and every later launch compiles them
    again. Each launch writes to a numbered file of TuxBox's own, starts the
    next once that one holds NVIDIA_CACHE_ROTATE_BYTES, and has the driver read
    every other file. Nothing is moved or deleted. Returns the name written and
    how many files are only read, or None when the environment names its own.
    """
    if env.get("__GL_SHADER_DISK_CACHE_APP_NAME") or env.get("__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME"):
        return None
    sizes = {}
    try:
        for blob in (Path(cache) / "GLCache").glob("*/*/*.bin"):
            size = blob.stat().st_size
            if size > NVIDIA_CACHE_HEADER and ";" not in blob.stem:
                sizes[blob.stem] = max(size, sizes.get(blob.stem, 0))
    except OSError:
        return None
    # The driver appends one digit to the name it is given, and can keep
    # several such files: a ProjectScarlett launch filled the one ending in 2.
    largest = {}
    for name, size in sizes.items():
        if match := NVIDIA_CACHE_FILE.fullmatch(name):
            largest[int(match[1])] = max(size, largest.get(int(match[1]), 0))
    number = max(largest, default=1)
    if largest.get(number, 0) >= NVIDIA_CACHE_ROTATE_BYTES:
        number += 1
    older = sorted(name for name in sizes
                   if not ((match := NVIDIA_CACHE_FILE.fullmatch(name)) and int(match[1]) == number))
    env["__GL_SHADER_DISK_CACHE_APP_NAME"] = f"tuxbox{number}"
    if older:
        env["__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME"] = ";".join(older)
    return f"tuxbox{number}", len(older)


def keep_nvidia_shader_cache(env):
    """Stop NVIDIA's driver deleting an exported cache folder.

    Without SKIP_CLEANUP, any process given the path checks the whole folder
    against the default cap at startup. Past it, the driver deletes every file
    that no running process holds, including the title's pipelines.
    """
    if env.get("__GL_SHADER_DISK_CACHE_PATH") and env.get("__GL_SHADER_DISK_CACHE") != "0":
        env.setdefault("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP", "1")
        env.setdefault("__GL_SHADER_DISK_CACHE_SIZE", str(NVIDIA_CACHE_BYTES))


# glibc gives each busy thread its own arena, up to 8 per CPU, and a title
# compiling pipelines on dozens of threads leaves free memory spread across all
# of them. A ProjectScarlett launch with the defaults held 22.75 GiB in 371
# arena heaps by 200 s; one with these reached its loading screen.
GLIBC_MALLOC_LIMITS = {
    "MALLOC_ARENA_MAX": "8",
    "MALLOC_MMAP_THRESHOLD_": str(1 << 20),
    "MALLOC_TRIM_THRESHOLD_": str(8 << 20),
}


def limit_malloc_growth(env):
    """Bound what the C library keeps beyond what the title and driver use."""
    for name, value in GLIBC_MALLOC_LIMITS.items():
        env.setdefault(name, value)


def configure_nvidia_shader_cache(env, report=None):
    """Keep this prefix's compiled GPU pipelines for the next launch.

    NVIDIA stores them under <path>/GLCache. With no path set, that is
    ~/.cache/nvidia/GLCache: every Wine process shares one application bucket,
    and the driver drops that bucket once it passes a small size. vkd3d can
    reuse its SPIR-V while the driver compiles the same pipelines again. A
    prefix-local directory is the cache Steam gives each title. A disabled
    disk cache is left alone. An explicit path is kept, and still gets room:
    a session that exports the path without a size keeps the small default
    cap, and the driver then drops the whole bucket part-way through a boot.
    """
    if env.get("__GL_SHADER_DISK_CACHE") == "0":
        return
    explicit = env.get("__GL_SHADER_DISK_CACHE_PATH")
    if explicit:
        keep_nvidia_shader_cache(env)
        if report:
            report(f"NVIDIA shader cache: {explicit} (path set by the environment)")
            report_unindexed_nvidia_cache(explicit, report)
        return
    prefix = env.get("WINEPREFIX")
    if not prefix:
        return
    cache = Path(prefix) / "shadercache"
    try:
        cache.mkdir(parents=True, exist_ok=True)
        adopt_nvidia_glcache(cache)
    except OSError as exc:
        if report:
            report(f"NVIDIA shader cache stays in the default directory: {exc}")
        return
    env["__GL_SHADER_DISK_CACHE"] = "1"
    env["__GL_SHADER_DISK_CACHE_PATH"] = str(cache)
    env["__GL_SHADER_DISK_CACHE_SKIP_CLEANUP"] = "1"
    # The driver's default cap is far smaller than what one Scarlett boot
    # writes, so give this directory room of its own.
    env.setdefault("__GL_SHADER_DISK_CACHE_SIZE", str(NVIDIA_CACHE_BYTES))
    rotated = rotate_nvidia_shader_cache(env, cache)
    if report:
        if rotated:
            report(f"NVIDIA shader cache: {cache} (writing {rotated[0]}, also reading {rotated[1]} earlier files)")
        else:
            report(f"NVIDIA shader cache: {cache}")
            report_unindexed_nvidia_cache(cache, report)


def configure_mesa_shader_cache(env, report=None):
    """Keep this prefix's compiled Mesa shaders for the next launch.

    Mesa stores them in ~/.cache/mesa_shader_cache, shared by every program on
    the desktop and capped at 1 GB. A desktop session's own GPU work then
    evicts the title's shaders, and the next launch compiles them again, which
    takes about half an hour for ProjectScarlett. A prefix-local directory is
    the cache Steam gives each title. A disabled cache and an explicit
    directory are kept; an explicit directory still gets room unless a size is
    set.
    """
    if env.get("MESA_SHADER_CACHE_DISABLE", "").lower() in ("1", "true", "yes"):
        return
    explicit = env.get("MESA_SHADER_CACHE_DIR")
    if explicit:
        env.setdefault("MESA_SHADER_CACHE_MAX_SIZE", MESA_CACHE_SIZE)
        if report:
            report(f"Mesa shader cache: {explicit} (path set by the environment)")
        return
    prefix = env.get("WINEPREFIX")
    if not prefix:
        return
    cache = Path(prefix) / "shadercache"
    try:
        cache.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        if report:
            report(f"Mesa shader cache stays in the default directory: {exc}")
        return
    env["MESA_SHADER_CACHE_DIR"] = str(cache)
    env.setdefault("MESA_SHADER_CACHE_MAX_SIZE", MESA_CACHE_SIZE)
    if report:
        report(f"Mesa shader cache: {cache}")


def configure_pipeline_cache(env, prefix):
    """Keep vkd3d-proton's pipeline cache in the prefix, beside the driver's.

    Without a path it is written into the game's folder. A path already set,
    including 0 to disable the cache, is kept.
    """
    if "VKD3D_SHADER_CACHE_PATH" in env:
        return
    cache = prefix / "shadercache"
    try:
        cache.mkdir(parents=True, exist_ok=True)
    except OSError:
        return
    env["VKD3D_SHADER_CACHE_PATH"] = str(cache)


class Host:
    def __init__(self, backend, proton, prefix, offline=False):
        if platform.machine().lower() not in {"amd64", "x86_64"}:
            raise ValueError("This native CPU baseline requires an x86-64 host")
        self.backend = backend or ("native" if os.name == "nt" else "proton")
        self.env = os.environ.copy()
        self.command = []
        self.version = platform.platform()
        self.offline = False
        if self.backend == "native":
            if offline:
                raise ValueError("--offline requires the Wine or Proton backend on Linux")
            if os.name != "nt" or proton or prefix:
                raise ValueError("Native execution requires Windows and takes no Proton or prefix arguments")
            return
        if sys.platform != "linux":
            raise ValueError("The Wine and Proton backends require Linux")
        if self.backend == "proton":
            installation = proton_installation(proton or os.getenv("PROTONPATH"))
            if not (installation / "proton").is_file():
                raise ValueError(f"Missing Proton distribution launcher in {installation}")
            tool = "umu-run"
            self.env.update(PROTONPATH=str(installation), GAMEID=f"umu-{PROTON_APP_ID}")
            version = installation / "version"
            self.version = version.read_text().strip() if version.is_file() else installation.name
            prefix_name = installation.name
        else:
            if proton:
                raise ValueError("--proton selects a distribution for the proton backend")
            tool = "wine"
            prefix_name = "wine"
        executable = shutil.which(tool)
        if not executable:
            raise ValueError(f"Install {tool} to use the {self.backend} backend")
        if self.backend == "wine":
            self.version = subprocess.check_output([executable, "--version"], text=True).strip()
        data_home = Path(os.getenv("XDG_DATA_HOME") or Path.home() / ".local/share")
        default = prefix is None
        prefix = (prefix or data_home / "project-tuxbox/prefixes" / prefix_name).expanduser().resolve()
        fresh = not prefix.exists()
        prefix.mkdir(parents=True, exist_ok=True)
        if default and fresh:
            seed_shader_cache(prefix, lambda line: print(f"tuxbox: {line}", file=sys.stderr))
        self.env["WINEPREFIX"] = str(prefix)
        configure_pipeline_cache(self.env, prefix)
        # Before any GPU work, so Frosty's pipeline preparation fills the cache the title reads.
        configure_mesa_shader_cache(self.env)
        self.command = [executable]
        if offline:
            self.isolate_network()

    def isolate_network(self):
        # A new network namespace has only a loopback device that is down, so the
        # game and its crash handler cannot upload minidumps or reach any server.
        unshare = shutil.which("unshare")
        if not unshare:
            raise ValueError("Install util-linux unshare to launch without network access")
        wrapper = [unshare, "--user", "--map-current-user", "--net", "--"]
        probe = subprocess.run([*wrapper, "true"], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        if probe.returncode:
            reason = probe.stderr.strip() or f"exit code {probe.returncode}"
            raise ValueError(f"Cannot create an offline network namespace: {reason}")
        self.command = [*wrapper, *self.command]
        # umu-run otherwise tries to update the Steam Runtime before starting.
        self.env["UMU_RUNTIME_UPDATE"] = "0"
        self.offline = True

    def run(self, executable, arguments, *, timeout=None, capture=False):
        options = {"stdout": subprocess.PIPE, "stderr": subprocess.STDOUT} if capture else {}
        # umu-run/proton forks wineserver, winedevice and the guest; “test” the whole
        # tree so a timed-out check cannot leave processes behind that poison later runs.
        process = subprocess.Popen([*self.command, str(executable), *arguments], cwd=executable.parent,
                                   env=self.env, start_new_session=True, **options)
        try:
            stdout, _ = process.communicate(timeout=timeout) if options else process.communicate(timeout=timeout)
            return subprocess.CompletedProcess([*self.command, str(executable), *arguments], process.returncode, stdout)
        except subprocess.TimeoutExpired:
            import signal
            for descendant in _descendant_pids(process.pid):
                try:
                    os.kill(descendant, signal.SIGKILL)
                except OSError:
                    pass
            try:
                process.kill()
            except OSError:
                pass
            process.wait()
            raise subprocess.TimeoutExpired([*self.command, str(executable), *arguments], timeout)

    def run_game(self, executable, arguments, log, *, package=None, mangohud=False, profile_pipelines=False):
        env = self.env.copy()
        env.setdefault("TUXBOX_LOG", "stderr" if self.backend == "native" else "1")
        if "-nullrhi" not in (arg.lower() for arg in arguments):
            warn_scarlett_dxc(executable, log.status)
        env.pop("TUXBOX_WINE_TITLE_TEMP", None)
        env.pop("TUXBOX_WINE_PACKAGE_DRIVE", None)
        drive_prefix = None
        if self.backend in ("proton", "wine") and env.get("WINEPREFIX"):
            prefix = Path(env["WINEPREFIX"])
            drive = prefix / "dosdevices/t:"
            # Proton can remove a user-configured T: during startup. Give the GDK
            # provider its original target so it can reattach it after that point.
            if drive.is_dir():
                env["TUXBOX_WINE_TITLE_TEMP"] = str(drive.resolve())
            elif not os.path.lexists(drive) and not os.path.lexists(drive.with_name("t::")):
                storage = prefix / "drive_t"
                try:
                    storage.mkdir(mode=0o700, exist_ok=True)
                except OSError as exc:
                    log.status(f"Could not prepare the title scratch directory {storage}: {exc}")
                else:
                    env["TUXBOX_WINE_TITLE_TEMP"] = str(storage)
            # Xbox titles open their package files through G:, as on the console. The GDK
            # provider replaces the prefix's G: with the package, so record the existing
            # links and put them back when the title exits.
            restore_package_drive(prefix, log.status)
            if package is not None:
                package = Path(package).resolve()
                try:
                    claimed = claim_package_drive(prefix, package)
                except OSError as exc:
                    log.status(f"Could not record the prefix's G: drive: {exc}")
                    claimed = False
                if claimed:
                    env["TUXBOX_WINE_PACKAGE_DRIVE"] = str(package)
                    drive_prefix = prefix
                else:
                    drive = prefix / "dosdevices/g:"
                    log.status(f"The package is not mapped to G: because {drive} is not a drive link")
        try:
            if self.backend in ("proton", "wine"):
                limit_malloc_growth(env)
                configure_nvidia_shader_cache(env, log.status)
                merge_pipeline_cache(env, executable, log.status)
            if "-nullrhi" not in (arg.lower() for arg in arguments):
                configure_console_cbv(self.backend, env, executable, log.status)
            capture = performance_capture(self.backend, env, executable, log) if mangohud else nullcontext()
            pipelines = pipeline_capture(self.backend, env, log) if profile_pipelines else nullcontext()
            with capture, pipelines, runtime_shader_compiler(self.backend, env, executable, log.status), \
                 game_diagnostics(self.backend, env, executable.parent, log.stderr_bytes,
                                  log_path=log.path) as stderr:
                return log.run([*self.command, str(executable), *arguments], cwd=executable.parent, env=env,
                               stderr_bytes=stderr)
        finally:
            if drive_prefix is not None:
                restore_package_drive(drive_prefix, log.status)
