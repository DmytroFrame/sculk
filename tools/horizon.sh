#!/bin/sh
# Sculk builds on Horizon (https://github.com/horizon-rust/horizon): upstream at the commit in
# patches/horizon/BASE, plus Sculk's own changes as patches in patches/horizon/*.patch.
# Horizon is the git submodule external/horizon, recorded at BASE (upstream's own commit, so any clone can fetch it);
# setup applies the patches on the branch `sculk` there. Cargo uses it through path dependencies.
#
#   tools/horizon.sh setup    fetch the submodule at BASE and apply the patches on `sculk` (refuses to redo an existing `sculk`)
#   tools/horizon.sh export   write the commits on `sculk` since BASE back to patches/horizon/*.patch
#   tools/horizon.sh status   show BASE, the patches and the checkout's state
#
# Updating Horizon: in external/horizon, `git fetch origin && git rebase --onto <new> $(cat ../../patches/horizon/BASE) sculk`,
# fix conflicts, write the new commit to patches/horizon/BASE, then `tools/horizon.sh export` and record the submodule
# at it: `git update-index --cacheinfo 160000,$(cat patches/horizon/BASE),external/horizon`.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DIR="$ROOT/external/horizon"
PATCHES="$ROOT/patches/horizon"
BASE=$(cat "$PATCHES/BASE")

case "${1:-status}" in
setup)
    # The submodule at BASE (a fresh clone with --recursive has it already).
    if [ ! -e "$DIR/.git" ]; then
        git -C "$ROOT" submodule update --init external/horizon
    fi
    if git -C "$DIR" rev-parse --verify --quiet sculk >/dev/null; then
        echo "external/horizon already has the branch sculk, which may hold unexported commits; move it away first" >&2
        exit 1
    fi
    git -C "$DIR" fetch --quiet origin "$BASE" 2>/dev/null || true
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
