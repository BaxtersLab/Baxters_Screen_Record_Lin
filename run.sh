#!/usr/bin/env bash
# Baxter's Screen Record — supported entry point on Linux (CLAUDE.md; packaging
# contract A1 §3). Launch the app through this script, never by execing the binary.
set -euo pipefail
HERE="$(dirname "$(readlink -f "$0")")"

# GNOME 50 files a window under its sandbox application before considering the
# Wayland app_id. Children of VS Code's Snap terminal retain the kernel label
# `snap.code.code`, even after setsid or a move into another systemd scope, so
# Shell assigns BSR to code_code.desktop. A transient service is spawned by the
# unconfined user manager and therefore sheds that inherited sandbox identity.
if [ "${BSR_OWN_APP_UNIT:-0}" != 1 ] && [ -r /proc/self/attr/current ]; then
    case "$(cat /proc/self/attr/current 2>/dev/null)" in
        'snap.code.code '*)
            if command -v systemd-run >/dev/null 2>&1; then
                _systemd_env=(--setenv=BSR_OWN_APP_UNIT=1)
                for _v in BSR_BUILD BSR_CAPTURE_CURSOR BSR_CROP BSR_DEBUG \
                          BSR_NO_PORTAL BSR_PROOF_DIR BSR_TELEMETRY_LOG \
                          BSR_TELEMETRY_PIPE RUST_LOG; do
                    if [[ -v $_v ]]; then
                        _systemd_env+=(--setenv="$_v=${!_v}")
                    fi
                done
                echo "run.sh: relaunching outside VS Code's Snap confinement." >&2
                exec systemd-run --user --quiet --collect --property=Type=exec \
                    --unit="app-gnome-baxters-screen-record-$$" \
                    "${_systemd_env[@]}" "$HERE/run.sh" "$@"
            fi
            ;;
    esac
fi

# --- Snap environment scrub -------------------------------------------------
# Inherited Snap paths are the single biggest time sink on this box. Note that
# XDG_DATA_DIRS is FILTERED, not dropped: glib walks it in order, and a snap entry
# leading it wins, so unsetting GSETTINGS_SCHEMA_DIR alone is not enough — but
# dropping the variable outright would lose the legitimate system entries with it.
if [ -r /opt/baxters/runtime/baxters-env.sh ]; then
    . /opt/baxters/runtime/baxters-env.sh
else
    for _v in $(env | grep -o '^[A-Za-z_][A-Za-z0-9_]*=/snap/[^:]*' | cut -d= -f1); do
        [ "$_v" = XDG_DATA_DIRS ] && continue
        unset "$_v"
    done
    for _v in GSETTINGS_SCHEMA_DIR GTK_PATH GIO_MODULE_DIR LOCPATH \
              XDG_DATA_HOME XDG_CONFIG_HOME XDG_CACHE_HOME; do
        eval "_val=\${$_v:-}"; case "$_val" in */snap/*) unset "$_v" ;; esac
    done
    unset _v _val ELECTRON_RUN_AS_NODE
    case "${XDG_DATA_DIRS:-}" in */snap/*)
        _c=""; _o="$IFS"; IFS=':'
        for _p in $XDG_DATA_DIRS; do
            [ -z "$_p" ] && continue
            case "$_p" in */snap/*) continue ;; esac
            _c="${_c:+$_c:}$_p"
        done
        IFS="$_o"; export XDG_DATA_DIRS="${_c:-/usr/local/share:/usr/share}"
        unset _c _o _p ;;
    esac
fi

# --- Launcher attribution -----------------------------------------------------
# MEASURED 2026-09-06. GNOME's window tracker attributes a window to the app that
# LAUNCHED it, via these variables, and that beats the window's own app_id. Launched
# from a VS Code terminal, BSR inherits
#   GIO_LAUNCHED_DESKTOP_FILE=/var/lib/snapd/desktop/applications/code_code.desktop
# so the Shell files BSR's window under VS Code: no icon of its own, no separate dock
# entry, and it disappears behind VS Code's icon. The window was setting its app_id
# correctly the whole time — confirmed in the Wayland protocol trace:
#   -> xdg_toplevel#38.set_app_id("baxters-screen-record")
# The inherited attribution simply outranked it.
#
# This affects EVERY app launched from a VS Code terminal on this box, not just BSR.
# ...but only when it is WRONG. Launched properly from the desktop entry, this variable
# names OUR entry and is exactly how GNOME gives the window its icon — a blanket unset
# threw away the correct attribution along with the bad one, which is why no icon
# appeared even from Activities.
case "${GIO_LAUNCHED_DESKTOP_FILE:-}" in
    ""|*/baxters-screen-record.desktop) : ;;   # absent, or correctly ours: leave it
    *) unset GIO_LAUNCHED_DESKTOP_FILE GIO_LAUNCHED_DESKTOP_FILE_PID ;;
esac
if [ -z "${GIO_LAUNCHED_DESKTOP_FILE:-}" ]; then
    _desktop_file="${XDG_DATA_HOME:-$HOME/.local/share}/applications/baxters-screen-record.desktop"
    if [ -f "$_desktop_file" ]; then
        export GIO_LAUNCHED_DESKTOP_FILE="$_desktop_file"
        export GIO_LAUNCHED_DESKTOP_FILE_PID="$$"
    fi
    unset _desktop_file
fi
# Startup-notification tokens are single-use and belong to whoever was launched with
# them; inheriting one mis-attributes the window and can steal focus.
unset DESKTOP_STARTUP_ID XDG_ACTIVATION_TOKEN

# --- GDK_BACKEND ------------------------------------------------------------
# GDK_BACKEND IS DELIBERATELY NOT SET, AND MUST NEVER BE SET TO x11 FOR THIS APP.
#
# BSR captures the screen. Under a native Wayland session the X11 root window
# contains nothing, so an X11 grab returns solid black with no error and nothing in
# the log. The sibling SOC Ultralight suite hit exactly this on this box. The rule
# (MAIN_HANDOFF §5 item 10b): captures the screen or uses portals -> never set it.
#
# If a decorated window's ✕ is ever unclickable here, the fix is `decorations: false`
# plus a custom titlebar — NOT this variable.
# MEASURED ON THIS BOX 2026-09-04, and the reason this block scrubs rather than refuses:
# the VS Code snap exports GDK_BACKEND=x11 into every integrated terminal (confirmed in
# /proc/<vscode>/environ, alongside its GDK_PIXBUF_* pointing into /snap/code/259/). It is
# in no dotfile. So BSR launched from the operator's normal working terminal would have
# gone to X11 and recorded solid black. Note the generic snap scrub above CANNOT catch
# this one: its value is "x11", not a /snap/ path.
#
# This is inherited contamination, not an operator choice, so it is scrubbed loudly rather
# than treated as a fatal error — refusing would make BSR unlaunchable from the very
# terminal the operator works in. There is deliberately no opt-out: for an app that
# captures the screen, X11 under Wayland is never a valid configuration.
if [ -n "${GDK_BACKEND:-}" ]; then
    echo "run.sh: scrubbing inherited GDK_BACKEND=$GDK_BACKEND — BSR captures the screen," >&2
    echo "        and an X11 grab under Wayland records solid black while logging nothing." >&2
fi
unset GDK_BACKEND

# --- Runtime prerequisites --------------------------------------------------
# Fail loudly and early rather than at the first Record click. Capture goes through
# xdg-desktop-portal ScreenCast -> PipeWire -> GStreamer's pipewiresrc.
_missing=""
command -v gst-inspect-1.0 >/dev/null 2>&1 || _missing="$_missing gstreamer1.0-tools"
if command -v gst-inspect-1.0 >/dev/null 2>&1; then
    gst-inspect-1.0 pipewiresrc >/dev/null 2>&1 || _missing="$_missing gstreamer1.0-pipewire"
fi
if [ -n "$_missing" ]; then
    echo "run.sh: screen capture cannot work without:$_missing" >&2
    echo "        install with: sudo apt install$_missing" >&2
    exit 1
fi
if [ -z "${WAYLAND_DISPLAY:-}" ] && [ -z "${DISPLAY:-}" ]; then
    echo "run.sh: no graphical session (WAYLAND_DISPLAY and DISPLAY are both unset)." >&2
    exit 1
fi
# Hard fail on Xorg. BSR captures the screen through the XDG portal + PipeWire, which is a
# Wayland path; under X11 a root-window grab returns solid black with no error. Refusing to
# start is strictly better than recording an empty file and reporting success.
if [ "${XDG_SESSION_TYPE:-}" = "x11" ]; then
    echo "run.sh: this is an Xorg (X11) session. BSR requires Wayland — under X11 the" >&2
    echo "        desktop cannot be captured and a recording comes out solid black." >&2
    echo "        Log in with a Wayland session (Ubuntu 26.04 and later default to it)." >&2
    exit 1
fi
unset _missing

# --- Logging ---
# Without this, tracing's EnvFilter lets only ERROR through, so warnings from the portal
# and capture paths vanish and a failure is indistinguishable from a no-op. Respects an
# explicit RUST_LOG if the caller set one.
export RUST_LOG="${RUST_LOG:-info}"

# --- Binary selection -------------------------------------------------------
# Which binary actually ran is printed, with its build time. A stale target/release
# binary silently faking a live run of freshly changed code has bitten this estate
# before; BSR_BUILD=1 rebuilds first.
# BSR_BUILD only makes sense in a source tree; an installed package has no Cargo.toml.
if [ "${BSR_BUILD:-0}" = "1" ] && [ -f "$HERE/Cargo.toml" ]; then
    ( cd "$HERE" && cargo build --release -p bsr-ui )
fi
BIN=""
# "$HERE/bsr-ui" FIRST: that is the installed layout (/opt/baxters/screen-record/bsr-ui),
# which has no target/ directory at all. Looking only under target/ made the packaged
# launcher unable to find its own binary — the package built and installed cleanly and
# then refused to start.
for _cand in "$HERE/bsr-ui" "$HERE/target/release/bsr-ui" "$HERE/target/debug/bsr-ui"; do
    [ -x "$_cand" ] && { BIN="$_cand"; break; }
done
if [ -z "$BIN" ]; then
    echo "run.sh: no bsr-ui binary found. Build it with:" >&2
    echo "        cd \"$HERE\" && cargo build --release -p bsr-ui" >&2
    echo "        (needs the FFmpeg dev headers: libavcodec-dev libavformat-dev" >&2
    echo "         libavutil-dev libswscale-dev libswresample-dev)" >&2
    exit 1
fi
echo "run.sh: launching $BIN (built $(date -r "$BIN" '+%Y-%m-%d %H:%M:%S'))"

cd "$HERE"
exec "$BIN" "$@"
