"""Compile Xbox shaders and link raster stages requested by a running Proton game."""
from contextlib import contextmanager, nullcontext
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait as wait_futures
import errno
from functools import lru_cache
import hashlib
import os
from pathlib import Path
import re
import shutil
import stat
import struct
import subprocess
from tempfile import TemporaryDirectory
from threading import Event, Lock, Thread
from time import monotonic, sleep

from .dxil_store import COMPUTE_FLAGS, VARIANTS, DxilStore, write_identity
from .inspect import digest
from . import persistent_dxc
from .shader_diagnostics import refusal_line
from .shader_blob import MAX_SHADER_BYTES, bounded_shader, exact_marker
from .shader_prep import SOURCE_CACHE, ToolFailure, compiler_tools, run_tool


XSHADER_UNSUPPORTED_HLSL = 65
# The emitter's one-line refusal reason kept beside an .unsupported marker.
REASON_BYTES = 512
# Each runtime cache is named after the compiler fingerprint, so every branch
# build gets its own and switching back is a warm hit. Request keys carry no
# translator version, so one build's output must never answer another's
# request: the caches stay apart on purpose rather than being merged.
#
# Nothing removed a cache whose build was gone, so they accumulated (two branch
# builds held 2.1 GB). Keep the running build's cache plus the two most
# recently used others, and record in `last-used` when each one ran, because a
# directory's own time also moves when a file inside it is rewritten.
USED_FILE = "last-used"
# Held open with an flock by the launcher using that cache, so retention cannot
# remove a directory that is still compiling on another branch.
IN_USE_FILE = "in-use"
RUNTIME_CACHES_KEPT = 3
SOURCE_KEY = r"[a-f0-9]{64}"
STENCIL_KEY = SOURCE_KEY + r"_rt[0-7]"
LINK_KEY = "_".join([SOURCE_KEY] * 3)
# A compute shader again, for a host whose waves are too narrow for its ordinary
# translation. It has its own cache entry, so other hosts keep theirs.
WAVE32_KEY = SOURCE_KEY + r"_w32"
REQUEST_PATTERNS = {".link": LINK_KEY, ".stencil": STENCIL_KEY, ".wave32": WAVE32_KEY, ".dxbc": SOURCE_KEY}


class ShaderRefused(ValueError):
    def __init__(self, message, *, cached, remembered=False):
        super().__init__(message)
        self.cached = cached
        # A cached refusal whose emitter reason was kept beside its marker.
        self.remembered = remembered

    @property
    def reason_known(self):
        return not self.cached or self.remembered


def verified_input(path, expected):
    data = bounded_shader(path)
    if hashlib.sha256(data).hexdigest() != expected:
        raise ValueError(f"{path.name} contents differ from the requested digest")
    return data


STAGE_IDS = {"ps": 0, "vs": 1, "gs": 2, "cs": 5, "lib": 6, "ms": 13, "as": 14}


def container_stage(data, *, portable):
    """Match the runtime's container/stage gate, not just LLVM magic."""
    if len(data) < 32 or len(data) > MAX_SHADER_BYTES or data[:4] != b"DXBC":
        return None
    size, count = struct.unpack_from("<II", data, 24)
    end = 32 + count * 4
    if size != len(data) or end > size:
        return None
    program = None
    accc = psv = 0
    for offset, in struct.iter_unpack("<I", memoryview(data)[32:end]):
        if offset < end or offset + 8 > size:
            return None
        tag, length = struct.unpack_from("<4sI", data, offset)
        start = offset + 8
        if start + length > size:
            return None
        accc += tag == b"ACCC"
        psv += tag == b"PSV0"
        if tag == b"DXIL":
            if program is not None:
                return None
            program = memoryview(data)[start:start + length]
    if program is None:
        # ACCC-only console compute. Full extraction still belongs to xshader.
        return 5 if not portable and accc == 1 and psv == 1 else None
    if len(program) < 24:
        return None
    version, words, magic, _, offset, length = struct.unpack_from("<II4sIII", program)
    stage = version >> 16
    start = 8 + offset
    if (stage not in STAGE_IDS.values() or version & 0xf0 != 0x60
            or words * 4 != len(program) or magic != b"DXIL"
            or start < 24 or start + length > len(program)):
        return None
    bits = program[start:start + length]
    if portable:
        return stage if bits[:4] == b"BC\xc0\xde" else None
    return stage if bits[:4] == b"BC\xc0\xde" or (program[24:28] == b"XDAT" and bits == b"NOBC") else None


def portable_stage(data):
    return container_stage(data, portable=True)


def remembered_reason(path):
    """The refusal reason saved with a marker, or None for caches that predate it."""
    flags = os.O_RDONLY | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        with os.fdopen(os.open(path, flags), "rb") as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_size > REASON_BYTES:
                return None
            data = stream.read(REASON_BYTES + 1)
    except OSError:
        return None
    if len(data) > REASON_BYTES:
        return None
    return data.decode("utf-8", errors="replace").strip() or None


@contextmanager
def compile_lock(cache, key, timeout=90):
    """Serialize an exact cold cache key across independent launcher processes.

    Keep the lock inode for this compiler fingerprint: unlinking it on
    release could let a later process lock a different inode while another
    waiter still holds the original. No locks are acquired for warm hits.
    """
    import fcntl
    if not any(re.fullmatch(pattern, key) for pattern in REQUEST_PATTERNS.values()):
        raise ValueError("invalid shader cache key")
    directory = cache / ".locks"
    directory.mkdir(exist_ok=True)
    flags = os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    with os.fdopen(os.open(directory / key, flags, 0o600), "r+b") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("shader cache lock is not a regular file")
        deadline = monotonic() + timeout
        while True:
            try:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if monotonic() >= deadline:
                    raise TimeoutError("shader cache lock wait timed out")
                sleep(min(.025, max(0, deadline - monotonic())))
        try:
            yield
        finally:
            fcntl.flock(stream, fcntl.LOCK_UN)


def cache_snapshot(dest, checksum, stages):
    """Return only bounded, stage-correct bytes with their matching checksum."""
    try:
        data = bounded_shader(dest)
        if portable_stage(data) in stages and exact_marker(checksum, hashlib.sha256(data).hexdigest().encode("ascii")):
            return data
    except (OSError, ValueError):
        pass
    return None


def portable_pixel(data):
    return portable_stage(data) == 0  # D3D12_SHVER_PIXEL_SHADER


def archive_request(archive, source, inputs):
    """Publish the exact verified inputs for a later launch's compiler."""
    dest = archive / source.name
    if dest.exists():
        return
    try:
        if source.suffix == ".link":
            staged = inputs[0].parent / "record"
            staged.mkdir()
            for path, name in zip(inputs, ("producer.dxbc", "pixel.dxbc", "pixel.cso")):
                os.link(path, staged / name)
            staged.rename(dest)
        else:
            os.link(inputs[0], dest)
    except OSError as exc:
        if exc.errno not in (errno.EEXIST, errno.ENOTEMPTY):
            raise


# Unoptimised DXC is the last resort for a shader. The largest ProjectScarlett
# compute program (CS_Emit_Dynamic_Model_Edge, 7,813 instructions) needs
# about 18 s at -O0, so a 15 s cap refused it on every boot.
O0_TIMEOUT = 90
# The raytracing resolve kernels translate to about 2.5 MB of HLSL each. One
# needs about 77 s at -O0 while the title runs, with up to four compiling at
# once, so a fixed 90 s cap refused them on every boot.
O0_SECONDS_PER_MB = 90
O0_TIMEOUT_MAX = 600


def o0_timeout(hlsl):
    """Seconds allowed for an unoptimised compile of `hlsl`."""
    megabytes = hlsl.stat().st_size / (1 << 20)
    return min(O0_TIMEOUT_MAX, max(O0_TIMEOUT, round(O0_SECONDS_PER_MB * megabytes)))
# The longest optimised cs_6_6 compile in a ProjectScarlett sample took 40 s
# with twelve compiles sharing the processor.
COMPUTE_TIMEOUT = 60
# The loop the shader compiler puts a program's blocks in when its loops cannot be nested.
DISPATCHER = b"while (tb_pc != "


# Promotes each function's locals before inlining it, which saves DXC about a
# third of its time on large generated programs. DXC calls the option experimental.
INLINING_FLAG = "-fnew-inlining-behavior"


@lru_cache(maxsize=None)
def inlining_flags(dxc):
    """The flag for a `dxc` that lists it; older releases reject unknown options."""
    try:
        listed = subprocess.run([dxc, "-help"], capture_output=True, timeout=30)
    except (OSError, subprocess.SubprocessError):
        return ()
    return (INLINING_FLAG,) if INLINING_FLAG.encode("ascii") in listed.stdout else ()


def validated_dxil(dxv, output, stage):
    """Return bounded, stage-correct output bytes after DXV accepts the file."""
    run_tool([dxv, output], timeout=10)
    data = bounded_shader(output)
    if portable_stage(data) != stage:
        raise ValueError("compiler returned invalid DXIL or a different shader stage")
    return data


def compile_dxil(dxc, dxv, target, hlsl, output, store, request, stage, *, persistent=None):
    """Compile HLSL for the requested stage and return fully validated DXIL bytes."""
    entry = [] if target == "lib_6_3" else ["-E", "main"]
    unoptimised = [dxc, "-T", target, *entry, "-O0", "-Fo", output, hlsl]

    def optimised(flags, timeout):
        """One optimised compile: by the session's DXC helper when it has one,
        and by the command line when it hasn't or the helper's output fails."""
        if persistent is not None and persistent.compile(dxc, hlsl, output, target, flags, timeout=timeout):
            try:
                return validated_dxil(dxv, output, stage)
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                persistent.report(f"Persistent DXC output rejected, using the CLI: {error}")
                output.unlink(missing_ok=True)
        run_tool([dxc, "-T", target, *entry, *flags, "-Fo", output, hlsl], timeout=timeout)
        if not output.exists():
            raise ValueError("compiler returned no DXIL output")
        return validated_dxil(dxv, output, stage)

    # With lifetime markers, DXC can spend most of a compile in Jump Threading on
    # generated programs. Every profile but cs_6_6, which has its own variant
    # below, therefore starts without them at the default optimisation level,
    # before consulting the slow marker. A timeout goes straight to -O0, since
    # the markers would only make the compile slower. Output that fails, as when
    # an older DXC rejects the flag, falls back to the default compile, except
    # for programs of the shader compiler's block dispatcher: with the markers
    # some cost RADV minutes and more than 20 GB, so those use -O0.
    dispatched = target != "cs_6_6" and DISPATCHER in hlsl.read_bytes()
    if target != "cs_6_6":
        # A timeout without the markers has its own marker: older slow markers
        # came from compiles that kept them, so they don't rule this one out.
        timed_out = f"{target}.unmarked"
        if store and not dispatched and store.is_slow(request, timed_out):
            run_tool(unoptimised, timeout=o0_timeout(hlsl))
            return validated_dxil(dxv, output, stage)
        try:
            faster = () if target == "lib_6_3" else inlining_flags(dxc)
            return optimised(("-disable-lifetime-markers", *faster), 15)
        except subprocess.TimeoutExpired:
            if store and not dispatched:
                store.mark_slow(request, target)
                store.mark_slow(request, timed_out)
            output.unlink(missing_ok=True)
            run_tool(unoptimised, timeout=o0_timeout(hlsl))
            return validated_dxil(dxv, output, stage)
        except ValueError:
            output.unlink(missing_ok=True)
    # A dispatched program whose candidate failed, or a request that timed out
    # before, goes straight to -O0 rather than spending the cap again.
    if dispatched or (store and store.is_slow(request, target)):
        run_tool(unoptimised, timeout=o0_timeout(hlsl))
        return validated_dxil(dxv, output, stage)
    # See `COMPUTE_FLAGS`. -O0 remains the fallback: the largest programs still
    # compile that way when the optimiser fails or runs out of time.
    if target == "cs_6_6":
        faster = inlining_flags(dxc)
        # Output that fails with the experimental option is compiled without it.
        for extra in (faster, ()) if faster else ((),):
            try:
                return optimised((*COMPUTE_FLAGS, *extra), COMPUTE_TIMEOUT)
            except subprocess.TimeoutExpired:
                if store:
                    store.mark_slow(request, target)
                break
            except ValueError:
                output.unlink(missing_ok=True)
        output.unlink(missing_ok=True)
        run_tool(unoptimised, timeout=o0_timeout(hlsl))
        return validated_dxil(dxv, output, stage)
    try:
        return optimised((), 15)
    except subprocess.TimeoutExpired:
        if store:
            store.mark_slow(request, target)
        output.unlink(missing_ok=True)
        run_tool(unoptimised, timeout=o0_timeout(hlsl))
    return validated_dxil(dxv, output, stage)


def stride_table_retry(xshader, cooked, flags, hlsl, profile):
    """Mirror the runtime's heap-stride retry: translate the embedded root with the
    stride table appended. Returns the variant's digest, or None on failure.

    The runtime retries a heap-indexed IDXEN access on the same root with the
    stride table appended (see the compute_table merge). The embedded root is
    the only root available before launch, so the variant is built from it.
    """
    try:
        result = subprocess.run(
            [xshader, cooked, *flags, "--stride-table", "--hlsl-out", hlsl, "--profile-out", profile],
            capture_output=True, text=True, errors="replace", timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    match = re.search(r"^stride-digest=([0-9a-f]{64})$", result.stdout, re.M)
    return match.group(1) if match else None


def compile_request(source, key, tools, cache, session, *, remember=None, persistent=None):
    linked = source.suffix == ".link"
    xshader, dxc, dxv = tools
    with TemporaryDirectory(dir=cache) as tmp:
        work = Path(tmp)
        flags = []
        if linked:
            if not stat.S_ISDIR(source.lstat().st_mode):
                raise ValueError("linked shader request must be a directory")
            producer_hash, pixel_hash, dxil_hash = key.split("_")
            cooked = work / "producer.dxbc"
            source_data = verified_input(source / "producer.dxbc", producer_hash)
            cooked.write_bytes(source_data)
            pixel = work / "pixel.dxbc"
            pixel.write_bytes(verified_input(source / "pixel.dxbc", pixel_hash))
            pixel_code = verified_input(source / "pixel.cso", dxil_hash)
            if not portable_pixel(pixel_code):
                raise ValueError("pixel order requires a portable pixel DXIL container")
            order = work / "pixel.cso"
            order.write_bytes(pixel_code)
            flags = ["--link-pixel", pixel, "--pixel-order", order]
            inputs = (cooked, pixel, order)
        else:
            cooked = work / "input.dxbc"
            if source.suffix == ".stencil":
                source_hash, slot = key.split("_rt")
                flags = ["--stencil-target", slot]
            elif source.suffix == ".wave32":
                source_hash = key.removesuffix("_w32")
                flags = ["--wave32-host"]
            else:
                source_hash = key
            source_data = verified_input(source, source_hash)
            cooked.write_bytes(source_data)
            inputs = (cooked,)

        if remember:
            remember(source, inputs)

        dest = cache / (key + ".cso")
        checksum = dest.with_suffix(".sha256")
        refusal = dest.with_suffix(".unsupported")
        reply = work / "reply"
        stages = ({1, 2, 13} if linked else {0} if source.suffix == ".stencil"
                  else {STAGE_IDS["cs"]} if source.suffix == ".wave32" else set(STAGE_IDS.values()))
        expected_stage = container_stage(source_data, portable=False)
        if expected_stage is not None:
            stages &= {expected_stage}
        del source_data
        def reuse():
            cached = cache_snapshot(dest, checksum, stages)
            if cached is not None:
                # Prelaunch hits need no temporary DXIL copy.
                if session is not None:
                    reply.write_bytes(cached)
                    os.replace(reply, session / dest.name)
                return True
            if exact_marker(refusal, key.encode("ascii")):
                reason = remembered_reason(dest.with_suffix(".reason"))
                if reason:
                    raise ShaderRefused(reason, cached=True, remembered=True)
                raise ShaderRefused("cached unsupported HLSL translation", cached=True)
            return False

        if reuse():
            return False
        with compile_lock(cache, key):
            # The first process may have published while we waited.
            if reuse():
                return False
            original_dest = dest
            original_refusal = refusal
            hlsl, output, profile = (work / name for name in ("shader.hlsl", "out.cso", "profile"))
            if linked:
                run_tool([dxv, order], timeout=10)
            try:
                run_tool([xshader, cooked, *flags, "--hlsl-out", hlsl, "--profile-out", profile], timeout=30)
                variant_digest = None
            except ToolFailure as error:
                # xshader reserves this status for the in-process HLSL emitter. DXC, DXV,
                # input parsing, tool startup and filesystem failures remain retryable.
                if error.returncode != XSHADER_UNSUPPORTED_HLSL:
                    raise
                reason = refusal_line(str(error))
                variant_digest = None
                if session is None and "raw buffer IDXEN" in reason and not any(
                        flag.startswith("--link-pixel") or flag == "--stencil-target" for flag in flags):
                    # The runtime retries a heap-indexed IDXEN access on the same
                    # root with the stride table appended (see compute_table).
                    # Mirror the retry offline: the embedded root is the only root
                    # available before launch. A successful variant is published
                    # under its own digest; the refusal markers for the original
                    # key stay in place so the runtime's own retry can see them.
                    variant_digest = stride_table_retry(xshader, cooked, flags, hlsl, profile)
                if variant_digest is None:
                    # Publish the reason first: a visible marker may already be read.
                    reason_path = work / "reason"
                    reason_path.write_bytes(reason.encode("utf-8")[:REASON_BYTES])
                    os.replace(reason_path, dest.with_suffix(".reason"))
                    marker = work / "unsupported"
                    marker.write_text(key, encoding="ascii")
                    os.replace(marker, refusal)
                    raise ShaderRefused(str(error), cached=False) from error
                # Keep the original refusal markers: the runtime's own retry
                # reads them to know this translation refused only for the
                # missing heap stride table.
                reason_path = work / "reason"
                reason_path.write_bytes(reason.encode("utf-8")[:REASON_BYTES])
                os.replace(reason_path, original_dest.with_suffix(".reason"))
                marker = work / "unsupported"
                marker.write_text(key, encoding="ascii")
                os.replace(marker, original_refusal)
                dest = cache / (variant_digest + ".cso")
                checksum = dest.with_suffix(".sha256")
                refusal = dest.with_suffix(".unsupported")
            target = profile.read_text().strip()
            # Mesh and amplification stages start at shader model 6.5. Linked
            # requests currently repack the outputs of vertex and geometry shaders.
            profiles = r"(vs|gs)_6_[016]|ms_6_[56]" if linked else r"((cs|ps|vs|gs)_6_[016]|cs_6_[45]|(ms|as)_6_[56]|lib_6_3)"
            if not re.fullmatch(profiles, target):
                raise ValueError(f"unsupported compiler profile {target!r}")
            target_stage = STAGE_IDS[target.split("_", 1)[0]]
            if target_stage not in stages:
                raise ValueError("compiler profile differs from requested shader stage")
            # An earlier translator may have produced this exact HLSL. Reuse any
            # validated equivalent output, including the candidate and -O0 paths;
            # compiler policy changes deliberately leave warm cache entries valid.
            store = DxilStore.for_cache(cache)
            content = store.key_file(hlsl, target) if store else None
            def reuse_content():
                data = store.fetch(content, output, accept=lambda blob: portable_stage(blob) == target_stage) if store else None
                if data is not None and portable_stage(data) == target_stage:
                    return data
                output.unlink(missing_ok=True)
                return None

            data = reuse_content()
            if data is None:
                # The exact-source lock above cannot deduplicate different
                # requests (or translator builds) that emit identical HLSL.
                # Acquire the shared toolchain/content lock only after a miss.
                # Always acquire source before content; never the reverse.
                # Allow time for the optimised attempt, fallback and validation.
                lock = (compile_lock(store.root, content, timeout=O0_TIMEOUT_MAX + COMPUTE_TIMEOUT + 60)
                        if store else nullcontext())
                with lock:
                    data = reuse_content() if store else None
                    if data is None:
                        data = compile_dxil(dxc, dxv, target, hlsl, output, store, key, target_stage,
                                            persistent=persistent)
                        if store:
                            store.save(content, data)
            stamp = work / "sha256"
            stamp.write_text(hashlib.sha256(data).hexdigest())
            if session is not None:
                reply.write_bytes(data)
            os.replace(output, dest)
            os.replace(stamp, checksum)
            if session is not None:
                os.replace(reply, session / dest.name)
    return True


def remove_request(path):
    if not path.is_symlink() and path.is_dir():
        shutil.rmtree(path)
    else:
        path.unlink(missing_ok=True)


def serve(session, stop, report, archive=None, jobs=1, *, dxc_helper=None):
    # Cargo can replace the release executable while another title is running.
    # Keep the executable we fingerprint private to this host worker.
    with TemporaryDirectory(prefix="tuxbox-xshader-") as tmp, request_notifications(session / "requests", stop) as (wait, wake):
        with persistent_dxc.staged(dxc_helper, tmp, jobs, report) as persistent:
            _serve_requests(session, stop, report, Path(tmp) / "xshader", wait, archive, jobs, persistent=persistent, wake=wake)


@contextmanager
def request_notifications(directory, stop):
    # The Proton guest publishes both shader files and complete link directories
    # by renaming them into this directory. Install the watch before scanning.
    import ctypes
    import select
    libc = ctypes.CDLL(None)
    init = getattr(libc, "inotify_init1", None)
    fd = init(os.O_NONBLOCK | os.O_CLOEXEC) if init else -1
    if fd < 0:
        yield (lambda: stop.wait(0.05)), None
        return
    try:
        moved_to = 0x00000080
        if libc.inotify_add_watch(fd, os.fsencode(directory), moved_to) < 0:
            yield (lambda: stop.wait(0.05)), None
            return
        changes = select.poll()
        changes.register(fd, select.POLLIN)
        # A finished job wakes the same wait as a new request, so the worker
        # can listen for requests while jobs run.
        finished, finish = os.pipe2(os.O_NONBLOCK | os.O_CLOEXEC)
        changes.register(finished, select.POLLIN)

        def wait():
            # A bounded fallback scan also handles filesystems without events
            # and lets shutdown finish when there is no outstanding work.
            for ready, _ in changes.poll(50):
                try:
                    os.read(ready, 65536)
                except BlockingIOError:
                    pass

        def wake():
            try:
                os.write(finish, b"\0")
            except BlockingIOError:
                pass  # A full pipe wakes the wait already.

        try:
            yield wait, wake
        finally:
            os.close(finished)
            os.close(finish)
    finally:
        os.close(fd)


# Claims this process holds, by cache path; the handles stay open until it exits.
_IN_USE = {}


def mark_in_use(cache):
    """Hold `cache` against retention until this process closes the handle.

    Returns the handle. The launcher keeps it for the life of the worker, so a
    second branch cannot evict a cache that is still compiling. The claim is
    shared: launchers of one build use its cache together, and only retention
    asks for it exclusively. Prelaunch preparation and the live worker run in
    one process and ask twice, so reuse the claim.
    """
    import fcntl
    directory = Path(cache)
    key = str(directory.resolve())
    if key in _IN_USE:
        return _IN_USE[key]
    handle = os.open(directory / IN_USE_FILE,
                     os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
    try:
        # Blocks only while another launcher's retention probes this cache.
        fcntl.flock(handle, fcntl.LOCK_SH)
    except OSError:
        os.close(handle)
        raise
    _IN_USE[key] = handle
    return handle


def locked_by_another_launcher(cache):
    """Whether another process holds `cache` open, or it cannot be told apart.

    Probing with an flock rather than a marker file means a launcher that
    crashed or was killed releases its claim when the kernel closes the file;
    no stale claim outlives the process. This process's own claim also reads as
    held, which is what retention wants: never remove a cache in use.
    """
    import fcntl
    try:
        handle = os.open(Path(cache) / IN_USE_FILE,
                         os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
    except OSError:
        return True
    try:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        return True
    finally:
        os.close(handle)
    return False


def runtime_caches(runtime_root):
    """The fingerprint directories under `runtime_root`, most recently used first.

    A cache without a `last-used` marker is ordered by its directory time, so
    caches written before the marker existed still sort sensibly. `last-used`
    wins where it exists because rewriting files inside a directory moves that
    directory's own timestamp whether the build ran or not.
    """
    def used(folder):
        for path in (folder / USED_FILE, folder):
            try:
                return path.stat().st_mtime_ns
            except OSError:
                continue
        return 0
    folders = [path for path in Path(runtime_root).iterdir() if path.is_dir()]
    return sorted(folders, key=used, reverse=True)


def retain_runtime_caches(runtime_root, current, *, keep=RUNTIME_CACHES_KEPT):
    """Keep `current` and the `keep - 1` most recently used sibling caches.

    `current` is kept even when it is the oldest, so a long session never loses
    the cache it is writing to. Returns the names of the directories removed. A
    cache another launcher holds is skipped rather than evicted, so this can
    keep more than `keep` while two branches compile at the same time; the next
    launch with nothing running trims the remainder.
    """
    if keep < 1:
        raise ValueError("keep must be at least 1")
    current = Path(current)
    # Record this build as the one that just ran. Its in-use claim is already
    # held by the caller, so evicting the rest cannot touch it either way.
    (current / USED_FILE).touch()
    ordered = [path for path in runtime_caches(runtime_root) if path.name != current.name]
    removed = []
    for folder in ordered[keep - 1:]:
        if locked_by_another_launcher(folder):
            continue
        try:
            shutil.rmtree(folder)
        except OSError:
            continue  # A cache this run cannot remove is not worth a failed launch.
        removed.append(folder.name)
    return removed


def snapshot_compiler_cache(xshader, cache_root, *, dxc_helper=None):
    """Pin the executable used for all requests under this toolchain fingerprint."""
    compiler, dxc, dxv = compiler_tools()
    shutil.copyfile(compiler, xshader)
    xshader.chmod(0o700)
    tools = (xshader, dxc, dxv)
    version = subprocess.check_output([dxc, "--version"], timeout=10)
    if dxc_helper is not None:
        version += b"\npersistent-dxc:" + digest(dxc_helper).encode("ascii")
    # Outputs built with other compiler flags must not be reused.
    fingerprint = hashlib.sha256(version + repr(sorted(VARIANTS.items())).encode("ascii"))
    for tool in tools:
        fingerprint.update(bytes.fromhex(digest(Path(tool))))
    cache = cache_root / "runtime" / fingerprint.hexdigest()
    cache.mkdir(parents=True, exist_ok=True)
    write_identity(cache, version, dxc, dxv)
    # Claim this build's cache before evicting, so a concurrent launch on
    # another branch cannot remove it while it compiles. Both steps also run
    # during prelaunch preparation; the claim is shared with the live worker.
    mark_in_use(cache)
    retain_runtime_caches(cache.parent, cache)
    return tools, cache


RUNTIME_JOBS_LIMIT = 32


def runtime_jobs(env, report):
    """Bound live compiler subprocesses; honor the launcher's CPU affinity.

    The game runs at the same time, so by default half the available cores
    compile, and at least two where there are two.
    """
    try:
        cpus = len(os.sched_getaffinity(0))
    except (AttributeError, OSError):
        cpus = os.cpu_count() or 1
    cap = max(1, min(RUNTIME_JOBS_LIMIT, cpus))
    default = min(cap, max(2, cpus // 2))
    value = env.get("TUXBOX_RUNTIME_SHADER_JOBS")
    if value is None:
        return default
    try:
        jobs = int(value)
        if not 1 <= jobs <= RUNTIME_JOBS_LIMIT:
            raise ValueError
    except ValueError:
        report(f"Invalid TUXBOX_RUNTIME_SHADER_JOBS={value!r}; using {default}")
        return default
    return min(jobs, cap)


def ordered_requests(directory):
    """Mix ordinary, linked and stencil requests in publication order."""
    entries = []
    for source in directory.iterdir():
        if source.suffix not in REQUEST_PATTERNS:
            continue
        try:
            timestamp = source.lstat().st_mtime_ns
        except FileNotFoundError:
            continue
        entries.append((timestamp, source.name, source))
    return [source for _, _, source in sorted(entries)]


def _serve_requests(session, stop, report, xshader, wait, archive, jobs=1, *, persistent=None, wake=None):
    def bind_cache():
        options = {"dxc_helper": persistent.helper} if persistent is not None else {}
        tools, cache = snapshot_compiler_cache(xshader, session.parent, **options)
        # Publish only after the exact compiler used by this worker is pinned.
        pending = session / "compiler-id.tmp"
        pending.write_text(cache.name, encoding="ascii")
        pending.replace(session / "compiler-id")
        return tools, cache

    tools = cache = None
    if (session.parent / "runtime").is_dir():
        try:
            tools, cache = bind_cache()
        except (OSError, ValueError, subprocess.SubprocessError):
            pass  # Retry and report the error if this game needs a shader.
    refused = cached_refusals = 0
    record_requests = archive is not None
    archive_lock = Lock()
    archive_errors = []

    def remember(source, inputs):
        nonlocal archive
        with archive_lock:
            if archive is not None:
                try:
                    archive_request(archive, source, inputs)
                except OSError as exc:
                    archive_errors.append(f"Could not save ProjectScarlett shader requests for the next launch: {exc}")
                    archive = None

    def compile_one(source, key, bound_tools, bound_cache):
        # Only this job owns the source. Archive publication is separately
        # serialized; compiler temporary directories are private per request.
        options = {"remember": remember} if record_requests else {}
        if persistent is not None:
            options["persistent"] = persistent
        return compile_request(source, key, bound_tools, bound_cache, session, **options)

    def complete(source, key, future=None, error=None):
        nonlocal refused, cached_refusals
        failed = False
        try:
            if error is not None:
                raise error
            if future.result():
                report(f"Compiled Xbox shader {key}")
        except ShaderRefused as exc:
            failed = True
            if exc.cached:
                cached_refusals += 1
            else:
                refused += 1
                report(f"Xbox shader {key}: {exc}")
        except Exception as exc:
            # A worker failure must release its guest waiter. Never turn a
            # transient tool or programming error into a persistent refusal.
            failed = True
            report(f"Xbox shader {key}: {exc}")
        finally:
            # Publish the terminal result before releasing the source name.
            # Once it disappears, the guest may clear the old failure and
            # publish a new generation of the same request.
            if failed:
                (session / (key + ".failed")).touch()
            remove_request(source)

    directory = session / "requests"
    active = {}
    report(f"Xbox shader worker: up to {jobs} concurrent requests")
    with ThreadPoolExecutor(max_workers=jobs, thread_name_prefix="xshader") as pool:
        while active or not stop.is_set():
            for key, (source, future) in list(active.items()):
                if future.done():
                    complete(source, key, future)
                    del active[key]
            with archive_lock:
                messages = archive_errors[:]
                archive_errors.clear()
            for message in messages:
                report(message)
            # No unbounded executor queue: on shutdown only running jobs
            # finish, before the session's files and pinned compiler disappear.
            if not stop.is_set() and len(active) < jobs:
                for source in ordered_requests(directory):
                    if stop.is_set() or len(active) >= jobs:
                        break
                    key = source.stem
                    if not re.fullmatch(REQUEST_PATTERNS[source.suffix], key):
                        remove_request(source)
                        continue
                    if key in active:
                        continue
                    if tools is None:
                        try:
                            tools, cache = bind_cache()
                        except (OSError, ValueError, subprocess.SubprocessError) as exc:
                            complete(source, key, error=exc)
                            continue
                    future = pool.submit(compile_one, source, key, tools, cache)
                    if wake:
                        # The pool joins its threads before the caller closes
                        # the notifications, so no callback outlives them.
                        future.add_done_callback(lambda _: wake())
                    active[key] = (source, future)
            if active and wake:
                # Wake immediately when any worker completes or a request is
                # published. Do not spin during stop.
                wait()
            elif active:
                # Without notifications, wake when any worker completes, with
                # a bounded rescan interval for new requests.
                wait_futures([future for _, future in active.values()],
                             timeout=0.05, return_when=FIRST_COMPLETED)
            elif not stop.is_set():
                wait()
    if refused or cached_refusals:
        report(f"Xbox shader worker: {refused} new unsupported translations, {cached_refusals} cached refusals")


@contextmanager
def runtime_shader_compiler(backend, env, executable, report):
    if backend != "proton" or env.get("TUXBOX_RUNTIME_SHADERS") == "0" or not executable.with_name("d3d12_xs.dll").is_file():
        yield
        return
    cache = Path(env["WINEPREFIX"]) / SOURCE_CACHE.parent
    from .scarlett_prep import scarlett_requests

    archive = None
    try:
        archive = scarlett_requests(executable, cache)
        if archive is not None:
            archive.mkdir(parents=True, mode=0o700, exist_ok=True)
    except OSError as exc:
        report(f"Could not open ProjectScarlett's shader inputs: {exc}")
        archive = None
    with shader_session(cache, env, report, archive=archive):
        yield


@contextmanager
def shader_session(cache, env, report, *, archive=None):
    cache.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(prefix="run-", dir=cache) as tmp:
        session = Path(tmp)
        (session / "requests").mkdir()
        env["TUXBOX_SHADER_SESSION"] = session.name
        stop = Event()
        helper = persistent_dxc.session_helper(env, report)
        options = {"dxc_helper": helper} if helper else {}
        worker = Thread(target=serve, args=(session, stop, report, archive, runtime_jobs(env, report)),
                        kwargs=options, name="shader-compiler", daemon=True)
        worker.start()
        try:
            yield
        finally:
            stop.set()
            worker.join()
            env.pop("TUXBOX_SHADER_SESSION", None)
