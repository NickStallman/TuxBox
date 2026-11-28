# TuxBox dev container

An Ubuntu 24.04 container with the [Lanes build environment](../examples/lanes/README.md#build-requirements)
already in place, so a change can be built and run on a Linux GPU host without
touching the host's toolchain. Ubuntu because umu-launcher publishes an official
`noble` package and apt resolves its dependencies there.

## What is inside

| Part | Owned by | Notes |
| --- | --- | --- |
| Rust 1.95.0 + `x86_64-pc-windows-msvc` | [`ghcr.io/devcontainers/features/rust`](https://github.com/devcontainers/features/tree/main/src/rust) | matches CI |
| `clang-cl`, `lld-link`, `llvm-mc` | Dockerfile | LLVM 21 from apt.llvm.org: Ubuntu's clang 18 is rejected by the MSVC headers (STL1000) and ships no `clang-cl` |
| MSVC CRT/SDK (cargo-xwin) | `post-create.sh` | staged in `~/.cache/cargo-xwin/xwin`, where `build.py` looks |
| DXC `v1.9.2609` + `dxv` | Dockerfile | Microsoft's Linux build; distro packages abort on every shader |
| `umu-launcher 1.4.4` | Dockerfile | upstream `.deb`, includes the 32-bit mesa bridge |
| Vulkan loader, RADV, `vulkan-tools` | Dockerfile | AMD and Intel; see NVIDIA below |
| headless display | Dockerfile + `start-display.sh` | Xwayland on a headless Weston, started by `postStartCommand` |
| `.venv` + `requirements.txt` | `post-create.sh` | `pefile` for the launcher tools |

## The display the guests need

`postStartCommand` runs `.devcontainer/start-display.sh`, which brings up a
headless Weston (gl renderer) and a rootful Xwayland on `:99`; `containerEnv`
exports `DISPLAY=:99`. Nothing is visible. Two things need it:

* Wine's Vulkan driver asks the display driver for a desktop window *before* it
  creates an instance. Without a display that request never returns and the
  guest hangs before its first line of output, which looks like a broken GPU;
  `eu-stack` shows the wait in `loaderdrv_VulkanInit -> wait_message_reply`.
* The present suite presents on the GPU and samples the X root window, and those
  writes go through DRI3. Xvfb has none (`vulkan: No DRI3 support detected`) and
  neither has Xwayland behind a software Weston, hence `--renderer=gl`.

The script is idempotent and fails loudly when the display comes up without
DRI3, so broken passthrough is reported instead of silently degrading. It
refuses to run outside a container, and when it retries it stops only the
Xwayland it started.

## Starting it

```sh
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . bash
```

VS Code: *Dev Containers: Reopen in Container*. `postCreateCommand` downloads
the MSVC SDK (~1 GB, once per container) and creates `.venv`.

## Running on a GPU

`/dev/dri` is bind-mounted, which is what Mesa needs; no `--device` flags and no
privileged mode. Pick the adapter explicitly on a multi-GPU host, the id comes
from `vulkaninfo --summary`:

```sh
python tuxbox.py verify --only lanes-selftest --adapter 1002:73df
```

Warm the two downloads a gate cannot afford first, once per container:

```sh
bash .devcontainer/warm-gpu.sh
```

It installs GE-Proton11-14 (~600 MB, the version `host.py` pins) and fetches the
umu runtime (~700 MB). `verify` gives every guest 15 seconds, so a container
that still has to download either of them fails all 67 gates with
`timed out after 15 seconds` and looks like a broken GPU.

NVIDIA needs the proprietary driver's userspace inside the image plus
`NVIDIA_DRIVER_CAPABILITIES=all`; add a second Dockerfile stage for that rather
than growing this one, so the common case stays small.

Two suites `tuxbox.py check` runs on Linux do **not** work in this container:
`D3D12 regression controls` and `Shader translator on the GPU`. Both spawn
their guests under `bwrap --unshare-pid --proc`, and mounting a fresh `/proc`
for a new PID namespace needs `CAP_SYS_ADMIN` in the container's user
namespace. Without it `bwrap` fails with
`Can't mount proc on /newroot/proc: Operation not permitted`, and granting the
capability is not a fix either (`bwrap: Unexpected capabilities but not setuid`,
and the setuid alternative dies on `capset failed: Operation not permitted`).
Run those two on a host, or give the container `--privileged` (`bwrap` itself
works then). The six code suites and every Lanes gate run in the container as
configured.

## Without a GPU

The build and the CI checks need no GPU and no Proton:

```sh
python tuxbox.py check --no-gpu
python build.py lanes
```

On a host without `/dev/dri`, drop the `mounts` entry so the engine does not
have to create it.

## Notes for other hosts

* `--security-opt=label=disable` exists for SELinux hosts (Fedora, Bazzite):
  without it every file in the workspace mount is `Permission denied`. It is a
  no-op elsewhere.
* The container runs as `dev` (uid 1000), not root, because Proton refuses to
  start as root. Podman's `keep-id` maps that onto your host user, so `build/`
  stays yours.
* The workspace is mounted at `/tuxbox`, not under `/workspaces`: `clang-cl`
  reads an absolute source path such as `/workspaces/TuxBox/...` as the `/W`
  option with a joined value and exits with `no input files`. `/opt` and `/work`
  collide the same way.
