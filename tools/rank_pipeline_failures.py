#!/usr/bin/env python3
"""Rank why ProjectScarlett pipelines failed, by how many pipelines each reason cost.

The provider writes tuxbox_shader_cache/pipeline-failures/<run>.tsv under the
prefix's temporary folder. Shader preparation and live translation keep a
refusal reason beside each unsupported shader in tuxbox_shader_cache/runtime.
Both stay on your machine; the printed summary holds no paths or shader data.

Each compiler build keeps its own runtime cache, so the newest one is ranked by
default. Request keys do not name the translator version: the same key can be
refused for different reasons by two builds, which is not a conflict. Merge
every cache with --all-runtime-caches only when the version does not matter.
"""
import argparse
from collections import Counter
from pathlib import Path
import re


NO_REASON = "no translator refusal recorded"
HOST_REJECTED = "translated, then rejected by the driver or runtime"
NO_ANSWER = "translation missing with no recorded reason"
ROOTLESS = "root signature missing"
MIXED = "mixed translation lookups (causal refusal unknown)"
STAND_IN = "stand-in after translation (cause not recorded)"
NOT_LISTED = "unlisted pipelines (record limit reached)"
CONFLICT = "conflicting refusal reasons across runtime caches"
# Written by project_tuxbox/runtime_shaders.py beside each compiler build's cache.
LAST_USED = "last-used"


def newest_runtime_cache(cache):
    """The runtime cache the latest launch used, or None when there is none.

    Only the newest build's cache is ranked by default. Request keys do not
    name the translator version, so an older build's reason for the same key
    says nothing about this build and must not be merged into its counts.
    """
    def used(folder):
        for path in (folder / LAST_USED, folder):
            try:
                return path.stat().st_mtime_ns
            except OSError:
                continue
        return 0
    folders = [path for path in (cache / "runtime").glob("*") if path.is_dir()]
    return max(folders, key=used, default=None)


def normalize(reason):
    """Group reasons that differ only in offsets, registers or encoded words."""
    reason = reason.split("\n", 1)[0].strip()
    reason = re.sub(r" words \[.*", "", reason)
    reason = re.sub(r"0x[0-9a-f]+", "0x_", reason)
    reason = re.sub(r"\bs\d+\b", "sN", reason)
    return re.sub(r"InlineWord\(\d+\)", "InlineWord(_)", reason)


def load_reasons(caches):
    """Collect reasons without guessing which compiler version produced a run."""
    reasons = {}
    for cache in sorted(caches):
        for path in sorted(cache.glob("*.reason")):
            try:
                reason = normalize(path.read_text(encoding="utf-8", errors="replace"))
            except FileNotFoundError:
                # A live runtime can remove an obsolete refusal marker.
                continue
            if not reason:
                continue
            previous = reasons.setdefault(path.stem, reason)
            if previous != reason:
                reasons[path.stem] = CONFLICT
    return reasons


def rank(lines, reasons):
    """Return pipeline counts, distinct attributed requests and outcome counts."""
    pipelines, shaders, outcomes = Counter(), {}, Counter()
    for line in lines:
        fields = line.rstrip("\n").split("\t")
        if len(fields) != 4 or not fields[0].isascii() or not fields[0].isdigit():
            continue
        count, outcome, _, stages = int(fields[0]), *fields[1:]
        if count <= 0:
            continue
        outcomes[outcome] += count
        # A refusal may contain spaces. The next name=value starts a new field.
        items = [(name, value) for name, sep, value in
                 (item.partition("=") for item in re.split(r"\s+(?=[\w.]+=)", stages.strip()))
                 if sep and name and value]
        used = {(name[:-5], digest) for name, digest in items if name.endswith(".used")}
        refusal = next(((name.removesuffix(".refused"), value) for name, value in items
                        if name.endswith(".refused")), None)
        if refusal and outcome != "not listed":
            stage, reason = refusal
            pipelines[reason] += count
            keys = {key for name, key in used if name == stage}
            if not keys:
                keys = {digest for name, digest in items if name == stage and digest != "unknown"}
            distinct = shaders.setdefault(reason, set())
            if len(keys) == 1:
                distinct.update(keys)
            continue
        missing = [(name[:-8], digest) for name, digest in items if name.endswith(".missing")]
        used_stages = {stage for stage, _ in used}
        # A raw digest is not the rooted request key. In modern rows, attribute
        # only explicitly missing requests from stages with no recorded success.
        unresolved = [(stage, digest) for stage, digest in missing
                      if (stage, digest) not in used]
        definite = [(stage, digest) for stage, digest in unresolved
                    if stage not in used_stages]
        mixed = bool(unresolved and len(definite) != len(unresolved))
        candidates = definite if used or missing else [(stage, digest) for stage, digest in items if digest != "unknown"]
        found = [(reasons[digest], digest) for _, digest in candidates if digest in reasons]
        found.sort(key=lambda entry: (entry[0] == CONFLICT, ROOTLESS in entry[0]))
        if outcome == "not listed":
            reason = NOT_LISTED
            found = []
        elif found:
            reason = found[0][0]
        elif definite:
            reason = NO_ANSWER
        elif mixed:
            reason = MIXED
        elif used:
            reason = STAND_IN if outcome == "stand-in" else HOST_REJECTED
        else:
            reason = NO_REASON
        pipelines[reason] += count
        distinct = shaders.setdefault(reason, set())
        if found:
            distinct.add(found[0][1])
    return pipelines, {reason: len(found) for reason, found in shaders.items()}, outcomes


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("cache", type=Path, help="the prefix's tuxbox_shader_cache folder")
    parser.add_argument("--run", type=Path, help="a failure file (default: the newest)")
    parser.add_argument("--top", type=int, default=15)
    parser.add_argument("--runtime-cache", type=Path,
                        help="restrict refusal reasons to a known runtime version directory")
    parser.add_argument("--all-runtime-caches", action="store_true",
                        help="merge every runtime cache, including other compiler builds")
    args = parser.parse_args(argv)
    run = args.run or max((args.cache / "pipeline-failures").glob("*.tsv"),
                          key=lambda path: path.stat().st_mtime, default=None)
    if run is None or not run.is_file():
        parser.error("no pipeline failure file found; run the game first")
    if args.runtime_cache is not None and not args.runtime_cache.is_dir():
        parser.error("--runtime-cache must be a directory")
    if args.runtime_cache is not None and args.all_runtime_caches:
        parser.error("choose one of --runtime-cache and --all-runtime-caches")
    if args.runtime_cache is not None:
        caches = [args.runtime_cache]
    elif args.all_runtime_caches:
        caches = [path for path in (args.cache / "runtime").glob("*") if path.is_dir()]
    else:
        newest = newest_runtime_cache(args.cache)
        caches = [newest] if newest is not None else []
    reasons = load_reasons(caches)
    pipelines, shaders, outcomes = rank(run.read_text(encoding="utf-8").splitlines(), reasons)
    total = sum(pipelines.values())
    print(f"{total} pipelines not delivered: "
          + ", ".join(f"{count} {outcome}" for outcome, count in outcomes.most_common()))
    print("pipelines   share  shaders  reason")
    for reason, count in pipelines.most_common(args.top):
        print(f"{count:9d}  {100 * count / total:5.1f}%  {shaders[reason]:7d}  {reason}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
