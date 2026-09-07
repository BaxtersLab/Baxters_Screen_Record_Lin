#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# Purge gate for the A1 packaging contract, §7.
#
# Usage:
#   ./packaging/purge_check.sh before     # run while the package is INSTALLED
#   sudo apt purge baxters-screen-record
#   ./packaging/purge_check.sh after      # run once it is gone
#
# What this can and cannot prove:
#
#   * On a box with sibling baxters-* packages, /opt/baxters is SHARED, so clause 4
#     ("/opt/baxters is gone") is NOT testable here and is reported as SKIPPED.
#     That clause needs the A2 VM harness with only this package installed.
#   * Clause 1's "no network access" is a property of the VM harness, not of this
#     script.
#
# Contract, §7: "A purge test that has never been watched fail does not count."
# Break a removal rule deliberately and confirm this script goes red.

set -uo pipefail
PKG="${PKG:-baxters-screen-record}"
STATE="${STATE:-/tmp/purge_check_${PKG}}"
MODE="${1:-}"

case "$MODE" in
before)
    if ! dpkg -s "$PKG" >/dev/null 2>&1; then
        echo "FAIL: $PKG is not installed; nothing to snapshot." >&2
        exit 1
    fi
    dpkg -L "$PKG" | sort > "$STATE.files"
    dpkg -s "$PKG" | awk '/^Version:/{print $2}' > "$STATE.version"
    echo "snapshot: $(wc -l < "$STATE.files") paths from $PKG $(cat "$STATE.version")"
    echo "written to $STATE.files"
    ;;
after)
    if [ ! -f "$STATE.files" ]; then
        echo "FAIL: no snapshot at $STATE.files — run '$0 before' first." >&2
        exit 1
    fi
    rc=0

    if dpkg -s "$PKG" >/dev/null 2>&1 && \
       [ "$(dpkg -s "$PKG" | awk '/^Status:/{print $4}')" != "not-installed" ]; then
        echo "FAIL  package $PKG is still installed"
        rc=1
    else
        echo "PASS  package $PKG is removed"
    fi

    # Clause 3: every path it shipped is gone, unless another package also owns it.
    leaked=0
    shared=0
    while IFS= read -r p; do
        # `dpkg -L` lists the archive's `./` entry as `/.` — the filesystem root, which
        # no package owns and which obviously survives. Counting it as a leak turned a
        # clean purge into a false RED, which is worse than no gate: it trains you to
        # ignore the result.
        case "$p" in /.|/) continue ;; esac
        [ -e "$p" ] || continue
        if owner=$(dpkg -S "$p" 2>/dev/null); then
            # Still owned by someone else — legitimately shared.
            shared=$((shared+1))
            continue
        fi
        echo "LEAK  $p  (exists, owned by no package)"
        leaked=$((leaked+1))
    done < "$STATE.files"

    if [ "$leaked" -eq 0 ]; then
        echo "PASS  no dpkg-orphaned files left behind ($shared shared path(s) survive, each still owned)"
    else
        echo "FAIL  $leaked orphaned path(s) left behind"
        rc=1
    fi

    # Clause 3, explicitly: the app's own directory must be gone.
    appdir="/opt/baxters/${PKG#baxters-}"
    if [ -e "$appdir" ]; then
        echo "FAIL  $appdir still exists"
        rc=1
    else
        echo "PASS  $appdir removed"
    fi

    # Clause 4: only meaningful when no sibling occupies /opt/baxters.
    siblings=$(dpkg-query -W -f='${Package} ${Status}\n' 'baxters-*' 2>/dev/null \
               | awk '$2=="install"{print $1}' | grep -v "^$PKG$" | tr '\n' ' ')
    if [ -n "${siblings// /}" ]; then
        echo "SKIP  /opt/baxters survives — still occupied by:${siblings% }"
        echo "      Clause 4 needs the A2 VM with only $PKG installed."
    elif [ -e /opt/baxters ]; then
        echo "FAIL  /opt/baxters still exists with no baxters-* package occupying it"
        rc=1
    else
        echo "PASS  /opt/baxters removed"
    fi

    [ "$rc" -eq 0 ] && echo "== PURGE GATE GREEN ==" || echo "== PURGE GATE RED =="
    exit "$rc"
    ;;
*)
    echo "usage: $0 before|after" >&2
    exit 2
    ;;
esac
