#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Build baxters-screen-record_<version>_amd64.deb per the A1 packaging contract.
#
# Contract points this implements, so the next reader does not have to re-derive them:
#   §1  payload lives in /opt/baxters/screen-record — never /usr or /usr/local
#   §3  run.sh is the ONLY entry point; the .desktop Exec points at it
#   §4  dependencies that fail silently are declared, not assumed
#   §5  package name baxters-<app>, so `apt purge 'baxters-*'` works — load-bearing
#   §5b every rebuild that leaves this box bumps the Debian revision
#   §7  purge must leave nothing behind
set -euo pipefail
HERE="$(dirname "$(readlink -f "$0")")"
TREE="$(cd "$HERE/../.." && pwd)"

PKG=baxters-screen-record
VERSION="${BSR_DEB_VERSION:-1.0.0-1}"
ARCH=amd64
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

BIN="$TREE/target/release/bsr-ui"
[ -x "$BIN" ] || { echo "build_deb: $BIN missing — run: cargo build --release -p bsr-ui" >&2; exit 1; }

APPDIR="$STAGE/opt/baxters/screen-record"
mkdir -p "$APPDIR" "$STAGE/DEBIAN" \
         "$STAGE/usr/share/applications" \
         "$STAGE/usr/share/icons/hicolor"

install -m 0755 "$BIN"            "$APPDIR/bsr-ui"
install -m 0755 "$TREE/run.sh"    "$APPDIR/run.sh"
install -m 0644 "$TREE/LICENSE"   "$APPDIR/LICENSE"
install -m 0644 "$TREE/THIRD_PARTY_LICENSES" "$APPDIR/THIRD_PARTY_LICENSES"

# Icons at the sizes the hicolor theme looks in. GNOME can scale one large PNG, but it
# prefers an exact size and the scaled result is visibly softer in the dock.
python3 - "$TREE/assets/bsr 512x512.png" "$STAGE/usr/share/icons/hicolor" "$PKG" <<'PY'
import os, sys
from PIL import Image
src, icons, app_id = sys.argv[1:4]
img = Image.open(src).convert("RGBA")
for s in (48, 64, 128, 256, 512):
    d = os.path.join(icons, f"{s}x{s}", "apps")
    os.makedirs(d, exist_ok=True)
    img.resize((s, s), Image.LANCZOS).save(os.path.join(d, f"{app_id}.png"))
PY

# The desktop entry. Exec is run.sh (contract §3) and the app_id/StartupWMClass must match
# the entry's basename or GNOME cannot pair the window with its icon.
sed 's|@TREE@|/opt/baxters/screen-record|g' "$TREE/packaging/baxters-screen-record.desktop" \
    > "$STAGE/usr/share/applications/$PKG.desktop"
chmod 0644 "$STAGE/usr/share/applications/$PKG.desktop"

# Normalise modes: the build umask otherwise ships 775 directories and 664 files, and
# the mktemp root arrives as 0700 — which dpkg would apply to the extracted tree.
chmod 0755 "$STAGE"
find "$STAGE" -type d -exec chmod 0755 {} +
find "$STAGE/usr/share/icons" -type f -exec chmod 0644 {} +

INSTALLED_KB=$(du -sk "$STAGE" | cut -f1)

# ── Depends ──────────────────────────────────────────────────────────────────
#
# The linked-library half is DERIVED from the binary at build time. It used to be a
# hardcoded list, snapshotted once from a running process, and it drifted: dropping
# tray-icon's `libxdo` feature removed libxdo.so.3 from the binary while the package
# went on declaring `libxdo3`. A frozen list can also UNDER-declare, which is the
# failure that actually hurts — the package installs and then the app will not start.
SHLIB_TMP="$(mktemp -d)"
mkdir -p "$SHLIB_TMP/debian"
printf 'Source: %s\n\nPackage: %s\nArchitecture: %s\n' "$PKG" "$PKG" "$ARCH" \
    > "$SHLIB_TMP/debian/control"
SHLIB_DEPS="$(cd "$SHLIB_TMP" && dpkg-shlibdeps -O --ignore-missing-info "$BIN" 2>/dev/null \
    | sed 's/^shlibs:Depends=//')"
rm -rf "$SHLIB_TMP"
if [ -z "$SHLIB_DEPS" ]; then
    echo "build_deb: dpkg-shlibdeps produced no dependencies for $BIN" >&2
    exit 1
fi

# The other half CANNOT be derived, and each entry states why. `ldd` and
# `dpkg-shlibdeps` see only what the linker recorded; these are opened at run time
# (dlopen) or are services and plugins with no ELF link at all. Measured by reading
# /proc/<pid>/maps of a running BSR, which is the only way to catch them.
RUNTIME_DEPS="gstreamer1.0-pipewire, gstreamer1.0-plugins-base, xdg-desktop-portal,
 libayatana-appindicator3-1, libegl1, libwayland-client0, libwayland-egl1,
 libwayland-cursor0, libxkbcommon0, libx11-6"

cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: $PKG
Version: $VERSION
Section: video
Priority: optional
Architecture: $ARCH
Maintainer: Baxter <165230507+BaxtersLab@users.noreply.github.com>
Installed-Size: $INSTALLED_KB
Depends: $SHLIB_DEPS, $RUNTIME_DEPS
Recommends: xdg-desktop-portal-gnome
Description: Baxter's Screen Record - screen recorder with a croppable record space
 Records the screen to H.264/MP4 through the XDG desktop portal and PipeWire, which is
 the only sanctioned capture path under Wayland: an X11 grab of the root window returns
 solid black with no error.
 .
 The recorded area can be trimmed to any rectangle, set numerically, by clicking corners
 on a live preview, or over IPC by an automation agent.
CONTROL

cat > "$STAGE/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
# Offline, idempotent, and fails loudly (contract §4.4). No network, no downloads.
set -e
if [ "$1" = configure ]; then
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q /usr/share/applications || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -f -t /usr/share/icons/hicolor || true
    fi
fi
exit 0
POSTINST

cat > "$STAGE/DEBIAN/postrm" <<'POSTRM'
#!/bin/sh
set -e
if [ "$1" = remove ] || [ "$1" = purge ]; then
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q /usr/share/applications || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -f -t /usr/share/icons/hicolor || true
    fi
fi
exit 0
POSTRM

chmod 0755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/postrm"

OUT="$TREE/dist"
mkdir -p "$OUT"
dpkg-deb --build --root-owner-group "$STAGE" "$OUT/${PKG}_${VERSION}_${ARCH}.deb" >/dev/null
echo "built $OUT/${PKG}_${VERSION}_${ARCH}.deb"
