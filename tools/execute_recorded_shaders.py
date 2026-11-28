"""Translate a sample of recorded compute shaders with two builds, run both on a GPU and compare.

Used by `tools/shader_harness.py execute` and by its `run` command. Each
request's two translations are bound to the same generated resources, so equal
outputs mean the change didn't alter what that shader computes for those
inputs. See project_tuxbox/shader_execute.py and docs/SHADER_HARNESS.md.
"""
from concurrent.futures import ThreadPoolExecutor
import hashlib
from pathlib import Path
import random
import shutil
import subprocess
import sys
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))
from project_tuxbox import shader_execute as execute  # noqa: E402
from project_tuxbox import shader_survey as survey  # noqa: E402
from project_tuxbox.dxil_store import COMPUTE_FLAGS  # noqa: E402
from project_tuxbox.runtime_shaders import XSHADER_UNSUPPORTED_HLSL  # noqa: E402

GUEST = ROOT / "tools/reference/execute-shaders/guest.cpp"
SIDES = ("before", "after")
BATCH = 48


def compute_requests(requests):
    """Recorded compute programs: the ones the survey also translates for 32-lane hosts."""
    return sorted(source for source, host in survey.plan(requests)
                  if host == "wave32" and source.suffix == ".dxbc")


def choose(requests, sample, seed):
    computes = compute_requests(requests)
    return sorted(random.Random(seed).sample(computes, min(sample, len(computes))))


def compile_dxil(dxc, profile, hlsl, output):
    """Compile as the runtime does for the profile, falling back to -O0 when that is too slow."""
    flags = COMPUTE_FLAGS if profile == "cs_6_6" else ("-disable-lifetime-markers",)
    base = [dxc, "-T", profile, "-E", "main", "-rootsig-define", "TB_EXECUTE_ROOT", "-Fo", output, hlsl]
    try:
        run = subprocess.run([*base[:5], *flags, *base[5:]], capture_output=True, timeout=120)
    except subprocess.TimeoutExpired:
        run = subprocess.run([*base[:5], "-O0", *base[5:]], capture_output=True, timeout=600)
    if run.returncode or not output.is_file():
        lines = [line for line in run.stderr.decode("utf-8", "replace").splitlines() if "error" in line]
        raise RuntimeError("DXC: " + (lines or ["failed"])[0].split("error: ", 1)[-1][:160])
    check = subprocess.run([Path(dxc).with_name("dxv"), output], capture_output=True, timeout=60)
    if check.returncode:
        raise RuntimeError("DXV rejected the program")


def prepare(xshader, dxc, source, host, name, out, max_hlsl):
    """Translate and compile one request; write its case files into `out`.

    Returns the outcome so far: `prepared` with the binding list, or why not.
    """
    with TemporaryDirectory(prefix="tuxbox-execute-") as tmp:
        work = Path(tmp)
        try:
            run, hlsl, profile, _ = survey.translate(xshader, source, host, work, 120)
        except subprocess.TimeoutExpired:
            return {"status": "failed", "reason": "translation timed out"}
        if run.returncode == XSHADER_UNSUPPORTED_HLSL:
            return {"status": "refused", "reason": "translator refused"}
        if run.returncode or not hlsl.is_file():
            return {"status": "failed", "reason": f"translator exited {run.returncode}"}
        data = hlsl.read_bytes()
        outcome = {"hlsl": hashlib.sha256(data).hexdigest()}
        if len(data) > max_hlsl:
            return {**outcome, "status": "skipped", "reason": f"translation over {max_hlsl >> 10} KiB"}
        text = data.decode("utf-8", "replace")
        try:
            declared = execute.bindings(text)
        except execute.Unsupported as error:
            return {**outcome, "status": "skipped", "reason": str(error)}
        rooted = work / "rooted.hlsl"
        rooted.write_text(execute.with_root_signature(text, declared))
        try:
            compile_dxil(dxc, profile.read_text().strip(), rooted, out / f"{name}.cso")
        except (RuntimeError, subprocess.TimeoutExpired) as error:
            return {**outcome, "status": "failed", "reason": str(error)[:160]}
    for file, content in execute.case_files(name, source.name, declared, minwave=execute.wave_size(text)).items():
        (out / file).write_bytes(content)
    return {**outcome, "status": "prepared", "bindings": declared}


def run_guest(names, staged, work, host, timeout_per_case=20):
    """Run cases on the GPU, restarting after a case that stops the runner. Returns outcomes and fields."""
    from frosty_gpu_guest import run_gpu_guest
    outcomes, fields, pending, attempt = {}, {}, list(names), 0
    retried = set()
    while pending:
        batch, pending = pending[:BATCH], pending[BATCH:]
        attempt += 1
        trial = work / f"batch-{attempt:04d}"
        trial.mkdir(parents=True)
        for name in batch:
            for file in staged.glob(f"{name}.*"):
                shutil.copyfile(file, trial / file.name)
        (trial / "cases.txt").write_text("".join(f"{name}\n" for name in batch))
        _, status, text = run_gpu_guest(GUEST, "execute-shaders.exe", trial, trial, host["proton"], host["prefix"],
                                        None, host["compiler_dir"], timeout=timeout_per_case * len(batch) + 60)
        # Each launch copies the prefix; only the outputs are kept.
        shutil.rmtree(trial / "prefix", ignore_errors=True)
        raw = (trial / "result.txt").read_text(errors="replace") if (trial / "result.txt").exists() else ""
        done, rest, observed = execute.receipt(raw, batch)
        fields.update({k: v for k, v in observed.items() if k in ("adapter", "wave_lanes_min", "wave_lanes_max", "error")})
        if not done and batch:
            # The runner made no progress: report every case in the batch rather than loop.
            reason = observed.get("error") or f"runner exited {status} before the first case"
            done = {name: ("failed", reason) for name in batch}
            rest = []
        for name, (state, reason) in done.items():
            if state == "hang" and name not in retried:
                # A loaded host can make one driver compile miss the limit; try once more.
                retried.add(name)
                rest.append(name)
                continue
            outcome = {"status": state, "reason": reason}
            if state == "ran":
                outcome["read"] = trial
            outcomes[name] = outcome
        pending = rest + pending
    return outcomes, fields


def probe_waves(work, host):
    """The adapter's PCI ID and widest wave, from a runner launch with no cases."""
    trial = work / "probe"
    trial.mkdir(parents=True)
    (trial / "cases.txt").write_text("")
    from frosty_gpu_guest import run_gpu_guest
    _, status, fields = run_gpu_guest(GUEST, "execute-shaders.exe", trial, trial, host["proton"], host["prefix"],
                                      None, host["compiler_dir"], timeout=60)
    shutil.rmtree(trial / "prefix", ignore_errors=True)
    if "wave_lanes_max" not in fields:
        raise SystemExit(f"the shader runner did not start (exit {status}); see {trial / 'gpu.log'}")
    return fields.get("adapter", "?"), int(fields["wave_lanes_max"])


def execute_stage(builds, requests, dxc, host, folder, *, sample=100, seed=0, max_hlsl=1 << 20, jobs=0):
    """Both builds over a sample of recorded compute requests on one adapter.

    `host` holds proton, prefix and compiler_dir. Returns the stage's report.
    """
    folder.mkdir(parents=True, exist_ok=True)
    adapter, widest = probe_waves(folder, host)
    wave = "wave64" if widest >= 64 else "wave32"
    chosen = choose(requests, sample, seed)
    staged = folder / "staged"
    staged.mkdir()
    names = {}
    for number, source in enumerate(chosen):
        for side in SIDES:
            names[source.name, side] = f"c{number:05d}{side[0]}"

    def one(item):
        source, side = item
        return item, prepare(Path(builds[side]).resolve(), dxc, source, wave, names[source.name, side], staged, max_hlsl)

    with ThreadPoolExecutor(survey.jobs_for(jobs)) as pool:
        prepared = dict(pool.map(one, [(source, side) for source in chosen for side in SIDES]))
    runnable = [names[source.name, side] for (source, side), outcome in prepared.items() if outcome["status"] == "prepared"]
    # The baseline runs twice, so a program that races on these inputs shows as unstable.
    for source in chosen:
        if prepared[source, "before"]["status"] == "prepared":
            first, again = names[source.name, "before"], names[source.name, "before"][:-1] + "r"
            for file in staged.glob(f"{first}.*"):
                shutil.copyfile(file, staged / (again + file.name[len(first):]))
            runnable.append(again)
    ran, fields = run_guest(sorted(runnable), staged, folder / "runs", host)
    results = {side: {} for side in (*SIDES, "repeat")}
    for (source, side), outcome in prepared.items():
        name = names[source.name, side]
        final = {key: value for key, value in outcome.items() if key not in ("bindings",)}
        if outcome["status"] == "prepared":
            got = ran.get(name, {"status": "failed", "reason": "not run"})
            final.update(status=got["status"], reason=got.get("reason", ""))
            if got["status"] == "ran":
                final["digest"] = execute.output_digest(lambda file, at=got["read"]: (at / file).read_bytes(),
                                                        name, outcome["bindings"])
                final["wrote"] = execute.wrote(lambda file, at=got["read"]: (at / file).read_bytes(),
                                               lambda file: (staged / file).read_bytes(), name, outcome["bindings"])
        results[side][source.name] = final
        if side == "before" and outcome["status"] == "prepared":
            again = ran.get(name[:-1] + "r", {"status": "failed"})
            results["repeat"][source.name] = {"status": again["status"]}
            if again["status"] == "ran":
                results["repeat"][source.name]["digest"] = execute.output_digest(
                    lambda file, at=again["read"]: (at / file.replace(name, name[:-1] + "r", 1)).read_bytes(),
                    name, outcome["bindings"])
    kinds = execute.compare(results["before"], results["after"], results["repeat"])
    report = {"adapter": fields.get("adapter", adapter), "wave_lanes_max": widest, "host": wave,
              "sample": len(chosen), "seed": seed,
              "ran": {side: sum(o["status"] == "ran" for o in results[side].values()) for side in SIDES},
              # Programs that left every writable resource as seeded: they ran, but show little.
              "wrote_nothing": sum(o["status"] == "ran" and not o["wrote"] for o in results["after"].values()),
              "kinds": {kind: len(names_) for kind, names_ in kinds.items()},
              "names": kinds, "reasons": execute.reasons(results["after"]),
              "problems": execute.blocking(kinds),
              "digests": {request: o.get("digest") for request, o in results["after"].items()}}
    return report


def cross_adapter(reports):
    """Requests whose candidate output differs between cards. Reported, not blocking."""
    if len(reports) < 2:
        return []
    first, *others = reports
    differ = []
    for request, digest in first["digests"].items():
        if digest and any(other["digests"].get(request) not in (None, digest) for other in others):
            differ.append(request)
    return differ
