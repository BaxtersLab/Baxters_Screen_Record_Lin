#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
set -euo pipefail

HERE="$(dirname "$(readlink -f "$0")")"
TREE="$(dirname "$HERE")"
APP_ID="baxters-screen-record"
TEMPLATE="$HERE/$APP_ID.desktop"
GENERATED="$(mktemp --suffix=.desktop)"
trap 'rm -f "$GENERATED"' EXIT

sed "s|@TREE@|$TREE|g" "$TEMPLATE" > "$GENERATED"
desktop-file-validate "$GENERATED"

value() {
    sed -n "s/^$1=//p" "$GENERATED"
}

test "$(basename "$TEMPLATE")" = "$APP_ID.desktop"
grep -Fq ".with_app_id(\"$APP_ID\")" "$TREE/crates/bsr-ui/main.rs"
test "$(value Exec)" = "$TREE/run.sh"
test "$(value TryExec)" = "$TREE/run.sh"
test "$(value Path)" = "$TREE"
test "$(value StartupWMClass)" = "bsr-ui"
grep -Fq "'snap.code.code '*)" "$TREE/run.sh"
grep -Fq -- '--setenv=BSR_OWN_APP_UNIT=1' "$TREE/run.sh"
if grep -Fq -- 'systemd-run --user --scope' "$TREE/run.sh"; then
    echo "FAIL: a transient scope cannot shed inherited Snap confinement" >&2
    exit 1
fi
grep -Fq -- '--unit="app-gnome-baxters-screen-record-$$"' "$TREE/run.sh"
grep -Fq 'export GIO_LAUNCHED_DESKTOP_FILE="$_desktop_file"' "$TREE/run.sh"

echo "PASS: desktop identity and application-scope launcher contracts agree."