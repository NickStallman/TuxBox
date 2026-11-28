#!/usr/bin/env bash
# Headless display for the guests, run by the devcontainer postStartCommand.
#
# Two things need a display even though nothing is shown:
#   * Wine's Vulkan driver asks the display driver for a desktop window before
#     it creates an instance (winevulkan's loaderdrv_VulkanInit ->
#     get_desktop_window). With no display that request never returns and the
#     guest hangs before its first line of output, which looks exactly like a
#     broken GPU.
#   * The present suite renders on the GPU and writes to that window through
#     DRI3. Xvfb has no DRI3 and a software Weston leaves Xwayland without it,
#     so the display is Xwayland on a headless Weston running on the gl
#     renderer, and Xwayland is rootful so Wine can sample the root window.
#
# Re-running this script is safe: a display already answering with DRI3 is left
# alone. It runs only inside a container, where /tmp/.X11-unix and the process
# list are the container's own: on a desktop it could take over that session's
# display. It stops only the Xwayland it started itself.
set -euo pipefail

# Docker and Podman each leave one of these in every container.
if [[ ! -e /.dockerenv && ! -e /run/.containerenv ]]; then
  echo "start-display: this is not a container; refusing to touch the host's X servers" >&2
  exit 1
fi

display="${DISPLAY:-:99}"
runtime="${XDG_RUNTIME_DIR:-/tmp/xdg-runtime}"
socket="${WAYLAND_DISPLAY:-wayland-0}"
geometry="2560x1440"

mkdir -p "${runtime}"
chmod 700 "${runtime}"

has_dri3() {
  xdpyinfo -display "${display}" 2>/dev/null | grep -qw DRI3
}

if has_dri3; then
  echo "start-display: ${display} is already up with DRI3"
  exit 0
fi

# --- Weston: the Wayland compositor Xwayland draws into -----------------------
if ! pgrep -x weston >/dev/null 2>&1; then
  setsid nohup weston --backend=headless-backend.so --socket="${socket}" \
    --width="${geometry%x*}" --height="${geometry#*x}" --idle-time=0 --renderer=gl \
    >"${runtime}/weston.log" 2>&1 &
  for _ in $(seq 1 60); do
    [[ -S "${runtime}/${socket}" ]] && break
    sleep 0.25
  done
fi
if [[ ! -S "${runtime}/${socket}" ]]; then
  echo "start-display: weston did not create ${runtime}/${socket}; see ${runtime}/weston.log" >&2
  exit 1
fi

# --- Xwayland: the X server the guests talk to --------------------------------
# DRI3 is only offered once glamor came up on the compositor's GPU path, and an
# Xwayland started a moment too early gives up on that and stays software-only.
# Starting it again after the compositor has settled is cheap, so retry instead
# of reporting a GPU failure that is really a startup race.
pidfile="${runtime}/xwayland${display//:/-}.pid"

# The process an earlier start recorded, if it is still that Xwayland.
stop_xwayland() {
  local pid
  pid="$(cat "${pidfile}" 2>/dev/null)" || return 0
  if [[ "${pid}" =~ ^[0-9]+$ ]] &&
     tr '\0' ' ' 2>/dev/null <"/proc/${pid}/cmdline" | grep -qF "Xwayland ${display} "; then
    kill "${pid}" 2>/dev/null || true
  fi
  rm -f "${pidfile}"
}

start_xwayland() {
  WAYLAND_DISPLAY="${socket}" setsid nohup Xwayland "${display}" \
    -geometry "${geometry}" -noreset >"${runtime}/xwayland.log" 2>&1 &
  echo "$!" >"${pidfile}"
  for _ in $(seq 1 60); do
    has_dri3 && return 0
    sleep 0.25
  done
  return 1
}

for attempt in 1 2 3; do
  stop_xwayland
  sleep 1
  if start_xwayland; then
    echo "start-display: ${display} ready - Xwayland on headless weston, DRI3 and Present enabled (${geometry})"
    exit 0
  fi
  echo "start-display: ${display} came up without DRI3 (attempt ${attempt}), retrying" >&2
done

echo "start-display: ${display} has no DRI3; the present suite will fail" >&2
echo "start-display: weston needs a working GL stack (see ${runtime}/weston.log)" >&2
exit 1
