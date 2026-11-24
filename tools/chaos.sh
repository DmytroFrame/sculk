#!/usr/bin/env bash
# Chaos test (PLAN.md, block I): a cluster of separate processes under bots, its workers killed (SIGKILL) and
# started again while they play; then a fresh crowd of bots must get in and play with no errors.
#
#   tools/chaos.sh [--workers N] [--bots N] [--seconds N] [--kill-every N]
#
# Needs: cargo build --profile fast -p sculk -p sculk-engine; external/horizon's horizon-bots (release); Horizon's
# prepared resources in HORIZON_RESOURCES (or .local/horizon-client/26.3 of the Horizon checkout); the EULA accepted
# with SCULK_EULA=true. Everything runs in a fresh folder under $TMPDIR and is stopped at the end.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORKERS=3 BOTS=12 SECONDS_=90 KILL_EVERY=20
while [[ $# -gt 0 ]]; do
    case "$1" in
    --workers) WORKERS=$2; shift ;;
    --bots) BOTS=$2; shift ;;
    --seconds) SECONDS_=$2; shift ;;
    --kill-every) KILL_EVERY=$2; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
    esac
    shift
done
[[ "${SCULK_EULA:-}" == true ]] || { echo "set SCULK_EULA=true if you accept Minecraft's EULA" >&2; exit 2; }
RES=${HORIZON_RESOURCES:-$ROOT/external/horizon/.local/horizon-client/26.3}
SCULK=$ROOT/target/fast/sculk
ENGINE=$ROOT/target/fast/sculk-engine
BOTS_BIN=$ROOT/external/horizon/target/release/horizon-bots
for f in "$SCULK" "$ENGINE" "$BOTS_BIN"; do [[ -x "$f" ]] || { echo "missing $f" >&2; exit 2; }; done
[[ -d "$RES" ]] || { echo "no Horizon resources at $RES" >&2; exit 2; }

DIR=$(mktemp -d "${TMPDIR:-/tmp}/sculk-chaos.XXXXXX")
PORT=25590
cd "$DIR"
printf 'gamemode=creative\nforce-gamemode=true\nallow-flight=true\n' > bots.properties
export SCULK_GAME_DATA=$ROOT/game-data RUST_LOG=info
PIDS=()
cleanup() { { kill -9 "${PIDS[@]}" "${WORKER_PIDS[@]}"; pkill -9 -f "$DIR"; } 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT

"$SCULK" anvildb --anvildb-listen 127.0.0.1:7290 --data-dir "$DIR/anvildb" > anvildb.log 2>&1 & PIDS+=($!)
"$SCULK" io --listen 127.0.0.1:$PORT --io-control-listen 127.0.0.1:7190 --anvildb http://127.0.0.1:7290 \
    --console-listen 127.0.0.1:7191 > io.log 2>&1 & PIDS+=($!)
sleep 1
declare -a WORKER_PIDS
start_worker() {
    local i=$1
    "$SCULK" worker --io http://127.0.0.1:7190 --anvildb http://127.0.0.1:7290 --engine "$ENGINE" --resources "$RES" \
        --eula --engine-dir "$DIR/engine-$i" --engine-port $((25690 + i)) --properties "$DIR/bots.properties" \
        < /dev/null >> "worker-$i.log" 2>&1 &
    WORKER_PIDS[$i]=$!
}
for i in $(seq 1 "$WORKERS"); do start_worker "$i"; done
sleep 4

echo "chaos: $WORKERS workers, $BOTS bots for ${SECONDS_}s, a worker killed every ${KILL_EVERY}s ($DIR)"
"$BOTS_BIN" --server 127.0.0.1:$PORT --bots "$BOTS" --join-every 0 --behaviour mixed --seconds "$SECONDS_" \
    --report chaos.json > bots.log 2>&1 &
BOTS_PID=$!
KILLS=0
END=$((SECONDS + SECONDS_))
while (( SECONDS + KILL_EVERY < END )); do
    sleep "$KILL_EVERY"
    # The worker with the most players (the web console says), whose loss hurts most; any when nobody plays.
    victim=$(curl -s 127.0.0.1:7191/api/state | python3 -c '
import json, sys
workers = [w for w in json.load(sys.stdin)["workers"] if w["engine"]]
w = max(workers, key=lambda w: w["players"], default=None)
print(int(w["engine"].rsplit(":", 1)[1]) - 25690 if w and w["players"] else 0)' 2>/dev/null || echo 0)
    (( victim >= 1 && victim <= WORKERS )) || victim=$((RANDOM % WORKERS + 1))
    echo "chaos: killing worker $victim"
    kill -9 "${WORKER_PIDS[$victim]}" 2>/dev/null || true
    KILLS=$((KILLS + 1))
    sleep 3
    start_worker "$victim"
done
wait "$BOTS_PID" || true

echo "chaos: a fresh crowd after $KILLS kills"
sleep 15
"$BOTS_BIN" --server 127.0.0.1:$PORT --bots "$BOTS" --join-every 0 --behaviour mixed --seconds 30 \
    --report after.json > after.log 2>&1 || true

python3 - "$DIR" <<'EOF'
import glob, json, sys
d = sys.argv[1]
during, after = json.load(open(f"{d}/chaos.json")), json.load(open(f"{d}/after.json"))
errors = lambda r: [b["error"] for b in r["per_bot"] if b["error"]]
crashes = sum(open(f, errors="replace").read().count("Horizon crashed") for f in glob.glob(f"{d}/engine-*/logs/*.log"))
refused = sum(open(f, errors="replace").read().count("holds no lease") for f in glob.glob(f"{d}/engine-*/logs/*.log"))
print(f"during chaos: {during['joined']} joined, {during['chunks']} chunks, {len(errors(during))} bots cut off (their worker was killed)")
for e in sorted(set(errors(during))):
    print(f"  {e}")
print(f"after chaos:  {after['joined']} joined, {after['chunks']} chunks, errors: {errors(after) or 'none'}")
print(f"engine crashes: {crashes}, writes refused by AnvilDB: {refused}")
ok = after["joined"] == len(after["per_bot"]) and not errors(after) and crashes == 0
print("chaos: PASS" if ok else "chaos: FAIL")
sys.exit(0 if ok else 1)
EOF
