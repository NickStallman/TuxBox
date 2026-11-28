# Contributing

TuxBox is licensed under the Apache License 2.0. A contribution in a pull request is licensed under the same terms. Third-party materials keep their own terms. [License](LICENSE) · [Third-party notices](docs/REFERENCES.md#shader-compiler-attribution)

Open pull requests against `main`. [Source layout](docs/SOURCE_LAYOUT.md) explains where code goes. If you use an AI coding agent, point it at [AGENTS.md](AGENTS.md).

## Prepare a checkout

Use Python 3.10 or newer and Rust 1.95 or newer. CI uses Python 3.12 and Rust 1.95.0. From the repository root, install the Python dependency and the Windows target used by the D3D12 typecheck:

```sh
python -m pip install -r requirements.txt
rustup target add x86_64-pc-windows-msvc
```

Building Lanes or a provider needs the desktop compiler and Proton setup in the [Lanes build requirements](examples/lanes/README.md#build-requirements).

## Test what your change touches

A change is merged when the checks relevant to it pass. You don't have to run suites your change can't affect. Every change that touches code runs the [code checks](#code-checks):

```sh
python tuxbox.py check --no-gpu
```

A change to anything that runs on the GPU also needs the matching GPU checks, with [Lanes](examples/lanes/README.md), the project's own test game, where it applies:

| What changed | GPU checks to run | Cards |
| --- | --- | --- |
| Documentation or evidence only | None | |
| `tools/shader-compiler/` | [Translator checks](tools/check_translator_gpu.py) and the `lanes-shader-cache`, `lanes-raster-tables`, `lanes-compute-root`, `lanes-compute-root-recovery` and `lanes-bindless-compute` suites | AMD and NVIDIA |
| Launcher shader translation (`runtime_shaders.py`, `shader_prep.py`, `dxil_store.py`, `persistent_dxc.py`) | The same Lanes suites | AMD and NVIDIA |
| `runtime/d3d12/` | The `lanes`, `lanes-*`, `d3d12-*`, `guest-frame` and `present` suites for what you changed, and the [D3D12 regression controls](tools/check_d3d12_regressions.py) | AMD and NVIDIA |
| `runtime/dxbc/` | Both of the rows for the translator and `runtime/d3d12/` | AMD and NVIDIA |
| `runtime/xmem/` | `xmem`, `lanes-selftest` and the buffer cases of the D3D12 controls | AMD and NVIDIA |
| `runtime/xg*/` | `xg`, `xg-provider` and `d3d12-placement` | AMD and NVIDIA |
| Another provider under `runtime/` | That provider's suite, such as `xapu`, `xgameruntime` or `xcrdapi` | Any |
| `examples/lanes/` | The `lanes` and `lanes-*` suites you changed | AMD and NVIDIA |
| The check runners (`build.py`, `verify.py`, `check.py`, `tools/check_*.py` shared by suites) | The full check | AMD and NVIDIA |

One tool's own check script needs only that check. If you can't tell what a change affects, or it spans most of the table, run the full check. A reviewer may ask for more.

Build, then name the suites. `python tuxbox.py verify --help` lists them, and `--only` can be repeated:

```sh
python build.py
python tuxbox.py verify --only lanes-shader-cache --only lanes-raster-tables --output build/verify.json
```

The translator checks and the D3D12 controls are scripts of their own. Run them with `--help` for their options; `--only` and `--case` select single checks.

### The full check

```sh
python tuxbox.py check
```

It runs the code checks below, builds every provider and guest, and then runs Lanes and the other guest fixtures on your GPU. The Lanes part covers:

| Group | What runs |
| --- | --- |
| Replay | The game itself: course generation, input, presentation and board readback |
| [Self-test](examples/lanes/README.md#self-test) | Xbox graphics behaviour retail titles depend on, each scenario failing if the GPU does not finish in time |
| Shader cache | A console shader loaded from a cache prepared before launch, translated while running, then reused without compiling again |
| [Benchmark](examples/lanes/README.md#benchmark) | Submission, fence handover, copy and pipeline timings. Add `--baseline` with an earlier result to compare. |

On Linux it also runs the [D3D12 regression controls](tools/check_d3d12_regressions.py), which cover buffer imports, page remapping, descriptor tables and other provider behaviour in isolated copies of the prefix. They need `--proton`, `--prefix`, `--windows-dxc` pointing at Microsoft's x64 `dxcompiler.dll` and `dxil.dll`, plus Bubblewrap and `xvfb-run`. The `root-cbv-offsets` control runs with the launcher's [constant-buffer settings](docs/PROJECTSCARLETT.md), so it also needs a C compiler and the Vulkan headers:

```sh
python tuxbox.py check --proton PROTON --prefix PREFIX --windows-dxc DXC/bin/x64
```

A change to what the translator emits for 64-lane programs also shows in a [recorded set of outputs](tools/shader-compiler/src/tests/hlsl/host_profiles.txt) for each [host profile](docs/USAGE.md#host-profile). The test names the profile affected. Include the updated record, and say in the pull request why that profile changed.

With the same options it runs the [shader translator checks](tools/check_translator_gpu.py): synthetic console programs translated, run on the GPU and compared word for word with a reference model. They also need `llvm-mc` from LLVM. When you add or fix an instruction, add it to one of these checks. The [shader harness](docs/SHADER_HARNESS.md) compares a translator change with `main` over recorded shaders, runs these checks on each card and lists instructions that no check covers.

Put the commands you ran, their results, and your GPU, driver and backend in the pull request. Pass `--backend`, `--proton`, `--prefix` or `--adapter` as for `tuxbox.py run`. Results are saved in `build/check/results.json`.

If your change adds or fixes behaviour, add or extend a Lanes scenario that fails without it. If it cannot be exercised from Lanes, say why in the pull request.

CI has no GPU, so it cannot run Lanes. A reviewer should not merge a pull request that needs GPU checks without a passing run of them, from the author or from someone who has the hardware.

### AMD and NVIDIA

A change that fixes one vendor must not break the other.

- **Shared code needs both.** Where the table above says AMD and NVIDIA, the relevant checks pass on one card of each before the merge. The two runs can come from different people.
- **Choose paths by capability.** Code that differs between hosts reads the [host profile](docs/USAGE.md#host-profile). The unit tests refuse vendor checks elsewhere in the provider and the translator.
- **Try the other path.** `TUXBOX_HOST_PROFILE` runs another profile's code on your card, but only that driver's own hardware shows its behaviour.
- **Failures that `main` also has** don't block a change. Name them in the pull request.

Pass `--adapter` to choose a card on a machine with more than one.

## Code checks

GitHub runs these on every pull request and on every push to `main`. [Workflow](.github/workflows/ci.yml)

```sh
python -m unittest discover -s tests -q
cargo test --locked --target x86_64-unknown-linux-gnu --manifest-path tools/shader-compiler/Cargo.toml
cargo test --locked --target x86_64-unknown-linux-gnu -p tuxbox-dxbc --features native-bindings
cargo test --locked --target x86_64-unknown-linux-gnu -p tuxbox-xg-layout
cargo test --locked --target x86_64-unknown-linux-gnu -p tuxbox-xcrdapi-codec
cargo check --locked --target x86_64-pc-windows-msvc -p tuxbox-d3d12
```

The D3D12 command typechecks the Windows provider. The Python tests check that `docs/progress.svg` and `docs/RESULTS.md` match the recorded evidence, and that [`docs/INSTRUCTIONS.md`](docs/SHADER_HARNESS.md#the-instruction-inventory) matches the translator's record of each instruction.

## Evidence

When a change adds or edits a receipt under `evidence/`, regenerate both views and include them in the pull request:

```sh
python tuxbox.py status > docs/RESULTS.md
python tuxbox.py status --svg docs/progress.svg
```

The results table and the progress map are generated. Editing either file by hand makes the unit tests fail.

## Files that stay on your machine

Retail games, Proton prefixes, build output, crash dumps, and packaged binaries stay outside the repository. `.gitignore` excludes `build/`, `games/`, `prefixes/`, `target/`, and the usual package and binary suffixes. The unit tests fail when a tracked file matches that ignore list.

Measured homebrew receipts in `evidence/` belong in the repository. Keep title dumps, captures, and native libraries on your machine.
