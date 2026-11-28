"""Fold vkd3d-proton's last write archive into its pipeline cache.

A launch appends new pipelines to <cache>.write, and vkd3d-proton merges that
into <cache> when the next one starts. With GE-Proton11-14 it reports "Write
cache is invalid ... nuking it" at every ProjectScarlett launch, although both
files carry the same header, so each launch's pipelines were thrown away. The
launcher merges them first.
"""
import os
from pathlib import Path
import struct

MAGIC = b"VKS"
HEADER = 48
# Entry hash, checksum, payload size and type; the payload is padded to 8 bytes.
ENTRY = struct.Struct("<QQII")


def _entries(data):
    """Each complete entry as (key, start, end), stopping at a torn one."""
    offset = HEADER
    while offset + ENTRY.size <= len(data):
        entry, _, size, kind = ENTRY.unpack_from(data, offset)
        end = offset + ENTRY.size + (size + 7 & ~7)
        if not size or end > len(data):
            return
        yield (entry, kind), offset, end
        offset = end


def merge_pipeline_cache(env, executable, report=None):
    """Merge the write archive beside this executable's cache, if both can be read."""
    directory = env.get("VKD3D_SHADER_CACHE_PATH")
    # Another spelling is a Windows path, a file name, or 0 to disable the cache.
    if not directory or not Path(directory).is_absolute():
        return
    cache = Path(directory) / f"vkd3d-proton.{executable.name}.cache"
    written = cache.with_name(cache.name + ".write")
    try:
        new = written.read_bytes()
        if len(new) < HEADER or not new.startswith(MAGIC):
            return
        try:
            old = cache.read_bytes()
        except FileNotFoundError:
            old = b""
        # A different header means another driver or vkd3d build wrote the
        # older cache. The write archive is from the launch that came after.
        if old[:HEADER] != new[:HEADER]:
            old = new[:HEADER]
        known, end = set(), HEADER
        for key, _, end in _entries(old):
            known.add(key)
        added = []
        for key, start, stop in _entries(new):
            if key not in known:
                known.add(key)
                added.append(new[start:stop])
        if not added:
            return
        pending = cache.with_name(cache.name + ".tuxbox")
        # Appending after a torn entry would hide everything added.
        pending.write_bytes(old[:end] + b"".join(added))
        os.replace(pending, cache)
        written.unlink()
    except FileNotFoundError:
        return
    except OSError as exc:
        if report:
            report(f"Could not merge the vkd3d-proton pipeline cache: {exc}")
        return
    if report:
        report(f"vkd3d-proton pipeline cache: added {len(added):,} pipelines from the last launch")
