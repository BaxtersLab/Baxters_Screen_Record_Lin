#!/usr/bin/env bash
# Install (or remove) the Baxter's Screen Record launcher for the current user.
#
# User-scope only: everything lands under $XDG_DATA_HOME (default ~/.local/share),
# nothing needs sudo, and `--uninstall` removes exactly what this installed.
set -euo pipefail

HERE="$(dirname "$(readlink -f "$0")")"
TREE="$(dirname "$HERE")"
APP_ID="baxters-screen-record"
# Snap contamination, and this installer was caught by it on its first run: the VS
# Code snap exports XDG_DATA_HOME=$HOME/snap/code/<rev>/.local/share, so the launcher
# installed into the snap's PRIVATE data directory, where GNOME never looks. The entry
# appeared to install cleanly and then simply did not exist as far as the Shell was
# concerned. Same durable rule as run.sh: distrust any XDG path pointing into /snap/.
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
case "$DATA" in
    */snap/*)
        echo "install: ignoring snap-provided XDG_DATA_HOME ($DATA)" >&2
        DATA="$HOME/.local/share"
        ;;
esac
APPS="$DATA/applications"
ICONS="$DATA/icons/hicolor"
SRC_ICON="$TREE/assets/bsr 512x512.png"
SIZES="48 64 128 256 512"

refresh() {
    # Best-effort: a stale cache only delays the icon appearing, it is not fatal.
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APPS" || true
    command -v gtk-update-icon-cache   >/dev/null 2>&1 && gtk-update-icon-cache -f -t "$ICONS" >/dev/null 2>&1 || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$APPS/$APP_ID.desktop"
    for s in $SIZES; do rm -f "$ICONS/${s}x${s}/apps/$APP_ID.png"; done
    refresh
    echo "Removed the $APP_ID launcher and icons."
    exit 0
fi

[ -x "$TREE/run.sh" ] || { echo "install: $TREE/run.sh is missing or not executable" >&2; exit 1; }
[ -f "$SRC_ICON" ]    || { echo "install: icon not found at $SRC_ICON" >&2; exit 1; }

# Icons at each size the hicolor theme looks in. GNOME can scale a single large PNG,
# but it picks a same-size icon first and the scaled result is visibly softer in the
# dock, so the sizes are generated rather than symlinked.
mkdir -p "$APPS"
python3 - "$SRC_ICON" "$ICONS" "$APP_ID" $SIZES <<'PY'
import os, sys
from PIL import Image
src, icons, app_id, *sizes = sys.argv[1:]
img = Image.open(src).convert("RGBA")
for s in (int(x) for x in sizes):
    d = os.path.join(icons, f"{s}x{s}", "apps")
    os.makedirs(d, exist_ok=True)
    img.resize((s, s), Image.LANCZOS).save(os.path.join(d, f"{app_id}.png"))
    print(f"  icon {s}x{s}")
PY

# @TREE@ is substituted rather than hardcoded: Article XI, no machine paths in source.
sed "s|@TREE@|$TREE|g" "$HERE/$APP_ID.desktop" > "$APPS/$APP_ID.desktop"
chmod 644 "$APPS/$APP_ID.desktop"

if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$APPS/$APP_ID.desktop" || {
        echo "install: the generated desktop entry is invalid (left in place for inspection)" >&2
        exit 1
    }
fi

refresh
echo "Installed: $APPS/$APP_ID.desktop -> $TREE/run.sh"
echo "Search for \"Baxter's Screen Record\" in Activities. Remove with: $0 --uninstall"
