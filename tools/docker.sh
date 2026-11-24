#!/usr/bin/env bash
# Builds the Sculk image and runs the newest build in the cluster of compose.yaml (AnvilDB, IO, workers running
# the vanilla game).
#
#   tools/docker.sh up [--fast] [--workers N] [--flat]   build, then start or update the cluster on that build
#   tools/docker.sh build [--fast]                       build and tag the image only
#   tools/docker.sh down [--volumes]                     stop the cluster (--volumes also deletes the world)
#   tools/docker.sh logs [service...]                    follow the logs
#   tools/docker.sh ps                                   the containers and their health
#
#   --fast       build with the fast profile instead of release
#   --workers N  how many workers (engines)
#   --flat       the built-in flat test world instead of the game: development only
#
# Settings come from .env (see .env.example). Each build is tagged <git commit>-<time> and `latest`; `up` runs the
# tag it just built, so every container is recreated on the new image.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

if [[ -f .env ]]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi
IMAGE=${SCULK_IMAGE:-sculk}
PROFILE=${SCULK_PROFILE:-release}
PROFILES=()

die() {
    echo "docker.sh: $*" >&2
    exit 1
}

compose() {
    local args=()
    for p in "${PROFILES[@]+"${PROFILES[@]}"}"; do
        args+=(--profile "$p")
    done
    docker compose "${args[@]+"${args[@]}"}" "$@"
}

check() {
    command -v docker >/dev/null || die "docker is not installed"
    docker info >/dev/null 2>&1 || die "the Docker daemon is not running (start Docker Desktop or OrbStack)"
    docker compose version >/dev/null 2>&1 || die "docker compose v2 is needed"
}

build() {
    [[ -f external/horizon/Cargo.toml ]] || die "external/horizon is missing: run tools/horizon.sh setup"
    local base tag
    base=$(git rev-parse --short HEAD 2>/dev/null || echo dev)
    git diff --quiet HEAD 2>/dev/null || base+="-dirty"
    tag="$base-$(date +%Y%m%d%H%M%S)"
    echo "Building $IMAGE:$tag ($PROFILE profile)..."
    DOCKER_BUILDKIT=1 docker build \
        --pull \
        --build-arg "PROFILE=$PROFILE" \
        --label "org.opencontainers.image.revision=$base" \
        --label "org.opencontainers.image.created=$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --tag "$IMAGE:$tag" \
        --tag "$IMAGE:latest" \
        .
    SCULK_TAG=$tag
    export SCULK_TAG
}

command=${1:-up}
shift || true
if [[ "$command" == -h || "$command" == --help ]]; then
    sed -n '2,17p' "$0"
    exit 0
fi
while [[ $# -gt 0 ]]; do
    case "$1" in
    --flat) FLAT=1 ;;
    # Kept for old habits: the game is what `up` starts anyway.
    --engine) ;;
    --fast) PROFILE=fast ;;
    --workers)
        [[ $# -ge 2 ]] || die "--workers needs a number"
        export SCULK_WORKERS=$2
        shift
        ;;
    --volumes) DOWN_VOLUMES=1 ;;
    -h | --help)
        sed -n '2,17p' "$0"
        exit 0
        ;;
    *) break ;;
    esac
    shift
done

case "$command" in
build)
    check
    build
    echo "Built $IMAGE:$SCULK_TAG (also tagged latest)"
    ;;
up)
    check
    if [[ -n "${FLAT:-}" ]]; then
        # Only the flat test workers: no engine, no EULA, no resources.
        PROFILES+=(flat)
        UP_ARGS=(--scale worker=0)
    else
        [[ "${SCULK_EULA:-false}" == "true" ]] \
            || die "set SCULK_EULA=true in .env if you accept Minecraft's EULA (https://aka.ms/MinecraftEULA)"
        resources=${HORIZON_RESOURCES:-./external/horizon/.local/horizon-client/26.3}
        [[ -f "$resources/horizon-resources.json" ]] \
            || die "no Horizon resources at $resources: set HORIZON_RESOURCES in .env (horizon-prepare's output)"
        UP_ARGS=()
    fi
    mkdir -p "${SCULK_CONFIG:-./config}"
    build
    # --wait returns once every service is running and healthy; prepare must have finished.
    compose up --detach --remove-orphans --wait --wait-timeout 600 "${UP_ARGS[@]+"${UP_ARGS[@]}"}"
    compose ps
    echo "Sculk is up on port ${SCULK_PORT:-25565} with $IMAGE:$SCULK_TAG (web console: http://127.0.0.1:${SCULK_CONSOLE_PORT:-7180})"
    ;;
down)
    if [[ -n "${DOWN_VOLUMES:-}" ]]; then
        PROFILES=(flat)
        compose down --remove-orphans --volumes
    else
        PROFILES=(flat)
        compose down --remove-orphans
    fi
    ;;
logs)
    PROFILES=(flat)
    compose logs --follow --tail 100 "$@"
    ;;
ps)
    PROFILES=(flat)
    compose ps
    ;;
*)
    die "unknown command $command (up, build, down, logs, ps)"
    ;;
esac
