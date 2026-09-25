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
# The version lives in a FILE in the tree, not in a default. It used to read
#     VERSION="${BSR_DEB_VERSION:-1.0.0-1}"
# so anyone building without that variable set produced 1.0.0-1 -- a version
# that goes BACKWARDS from the 1.0.0-14 already shipped. apt then refuses the
# "upgrade", and a train picks whichever file sorts highest rather than the one
# just built. Done accidentally on 2026-09-25 while adding a copyright file.
VERSION="${BSR_DEB_VERSION:-$(cat "$(dirname "$(readlink -f "$0")")/VERSION")}"
[ -n "$VERSION" ] || { echo "FATAL: packaging/deb/VERSION is empty" >&2; exit 1; }
# Never regress against something already built.
for _existing in "$(dirname "$(readlink -f "$0")")"/../../dist/baxters-screen-record_*.deb; do
    [ -e "$_existing" ] || continue
    _ev="$(dpkg-deb -f "$_existing" Version 2>/dev/null)" || continue
    if dpkg --compare-versions "$_ev" gt "$VERSION"; then
        echo "FATAL: $VERSION is older than $_ev, which is already built" >&2
        echo "       bump packaging/deb/VERSION -- a lower version is not an upgrade" >&2
        exit 1
    fi
done
ARCH=amd64
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

BIN="$TREE/target/release/bsr-ui"
[ -x "$BIN" ] || { echo "build_deb: $BIN missing — run: cargo build --release -p bsr-ui" >&2; exit 1; }

# REFUSE A STALE BINARY.
#
# This check only tested that the binary EXISTS. On 2026-09-08 that shipped
# baxters-screen-record 1.0.0-12 carrying a bsr-ui byte-identical to 1.0.0-11: two fixes
# had been written and tested in DEBUG, `cargo build --release` was never run, and the
# release binary was 14 hours older than the source. The test box caught it by hashing
# the shipped binary against the previous round's -- the package looked new because only
# the Version: line had changed.
#
# A version bump is not evidence. Compare timestamps against every tracked source file
# and refuse to package a binary older than the code it claims to contain.
newer=$(find "$TREE/crates" "$TREE/Cargo.toml" "$TREE/Cargo.lock" \
          -name target -prune -o -type f \( -name '*.rs' -o -name 'Cargo.toml' -o -name 'Cargo.lock' \) \
          -newer "$BIN" -print 2>/dev/null | head -5)
if [ -n "$newer" ]; then
    echo "build_deb: REFUSING — $BIN is older than source that changed since it was built:" >&2
    echo "$newer" | sed 's|^|  |' >&2
    echo "  run: cargo build --release -p bsr-ui" >&2
    exit 1
fi
echo "build_deb: release binary is newer than all tracked source (stale-binary check passed)"

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

# Debian policy requires every package to carry a copyright file. An audit
# of the built .deb files on 2026-09-24 found five packages shipping
# without one.
install -d -m 0755 "$STAGE/usr/share/doc/$(awk '/^Package: /{print $2; exit}' "$STAGE/DEBIAN/control")"
install -m 0644 "$(dirname "$(readlink -f "$0")")/copyright" "$STAGE/usr/share/doc/$(awk '/^Package: /{print $2; exit}' "$STAGE/DEBIAN/control")/copyright"

# --- final hygiene sweep ---------------------------------------------------
# `install -d -m 0755 <dir>` sets the mode on the LAST component only; the
# parents it creates on the way (usr, usr/share, usr/share/doc) take the build
# user's umask instead. Three packages shipped a group-writable
# ./usr/share/doc/ that way, because the copyright step ran AFTER the hygiene
# step. Hygiene therefore gets the last word, immediately before the build.
chmod -R go-w "$STAGE"
if [ -n "$(find "$STAGE" \( -type f -o -type d \) -perm -g+w -print -quit 2>/dev/null)" ]; then
    echo "FATAL: group-writable entries remain at build time" >&2
    exit 1
fi
dpkg-deb --build --root-owner-group "$STAGE" "$OUT/${PKG}_${VERSION}_${ARCH}.deb" >/dev/null
echo "built $OUT/${PKG}_${VERSION}_${ARCH}.deb"
