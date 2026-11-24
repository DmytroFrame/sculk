#!/bin/sh
# Sculk builds on Horizon (https://github.com/horizon-rust/horizon): upstream at the commit in
# patches/horizon/BASE, plus Sculk's own changes as patches in patches/horizon/*.patch.
# The checkout lives in external/horizon on the branch `sculk`; cargo uses it through path dependencies.
#
#   tools/horizon.sh setup    clone upstream, check out BASE and apply the patches (refuses to touch an existing checkout)
#   tools/horizon.sh export   write the commits on `sculk` since BASE back to patches/horizon/*.patch
#   tools/horizon.sh status   show BASE, the patches and the checkout's state
#
# Updating Horizon: in external/horizon, `git fetch origin && git rebase --onto <new> $(cat ../../patches/horizon/BASE) sculk`,
# fix conflicts, write the new commit to patches/horizon/BASE, then `tools/horizon.sh export`.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DIR="$ROOT/external/horizon"
PATCHES="$ROOT/patches/horizon"
UPSTREAM=https://github.com/horizon-rust/horizon.git
BASE=$(cat "$PATCHES/BASE")

case "${1:-status}" in
setup)
    if [ -e "$DIR" ]; then
        echo "$DIR already exists; move it away first, it may hold unexported commits" >&2
        exit 1
    fi
    git clone "$UPSTREAM" "$DIR"
    # rustfmt reads the nearest rustfmt.toml going up: without this one, Sculk's (120 columns) reformats Horizon.
    printf '# Horizon uses rustfmt defaults.\n' > "$ROOT/external/rustfmt.toml"
    git -C "$DIR" checkout -b sculk "$BASE"
    set -- "$PATCHES"/*.patch
    if [ -e "$1" ]; then
        git -C "$DIR" am --3way "$@"
    fi
    ;;
export)
    rm -f "$PATCHES"/*.patch
    git -C "$DIR" format-patch --quiet --no-numbered --zero-commit -o "$PATCHES" "$BASE..sculk"
    ls "$PATCHES"
    ;;
status)
    echo "BASE: $BASE"
    ls "$PATCHES"/*.patch 2>/dev/null || echo "no patches"
    if [ -d "$DIR/.git" ]; then
        echo "checkout: $(git -C "$DIR" branch --show-current), $(git -C "$DIR" rev-list --count "$BASE..HEAD") commits over BASE"
    else
        echo "no checkout: run tools/horizon.sh setup"
    fi
    ;;
*)
    echo "usage: $0 setup|export|status" >&2
    exit 2
    ;;
esac
