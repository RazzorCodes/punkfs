#!/usr/bin/env bash
# Three identical bidirectional nodes; each mount covers its own backing folder.
# ./scripts/playground.sh [--keep]
set -euo pipefail
cd "$(dirname "$0")/.."
PLAY=${PUNKFS_PLAYGROUND_DIR:-.playground}
KEEP=0
case "${1:-}" in "") ;; --keep) KEEP=1 ;; *) echo "usage: $0 [--keep]" >&2; exit 2 ;; esac
pids=()
started=()
cleanup() {
    for pid in "${pids[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
    for i in "${started[@]}"; do
        mountpoint -q "$PLAY/node-$i" && fusermount3 -uz "$PLAY/node-$i" 2>/dev/null || true
    done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for i in 1 2 3; do
    if mountpoint -q "$PLAY/node-$i" 2>/dev/null; then
        echo "error: $PLAY/node-$i is already mounted; unmount before starting" >&2; exit 1
    fi
done
cargo build --quiet --bin punkfs
ports=()
# Reserve three distinct ports simultaneously. They are released before the
# nodes bind, so a conflicting process can still win; startup then fails.
mapfile -t ports < <(python3 -c 'import socket; s=[socket.socket() for _ in range(3)]; [x.bind(("127.0.0.1",0)) for x in s]; [print(x.getsockname()[1]) for x in s]')
for i in 1 2 3; do
    mkdir -p "$PLAY/node-$i/shared"
    file="$PLAY/node-$i/node-$i-example.txt"
    [[ -e "$file" ]] || printf 'hello from node %s\n' "$i" > "$file"
    file="$PLAY/node-$i/shared/node-$i.txt"
    [[ -e "$file" ]] || printf 'shared contribution from node %s\n' "$i" > "$file"
done
for i in 1 2 3; do
    peers=()
    for j in 1 2 3; do
        (( i == j )) || peers+=("node-$j=http://127.0.0.1:${ports[j-1]}")
    done
    echo "==> starting node-$i on 127.0.0.1:${ports[i-1]}, mount/backing $PLAY/node-$i"
    PUNKFS_LOG_ROUTES=1 PUNKFS_LOG_LEVEL=info PUNKFS_LOG_DESTINATION=file \
        PUNKFS_LOG_FILE="$PLAY/node-$i-telemetry.jsonl" \
        ./target/debug/punkfs "$PLAY/node-$i" "node-$i" --listen "127.0.0.1:${ports[i-1]}" "${peers[@]}" >"$PLAY/node-$i.log" &
    pids+=("$!")
    started+=("$i")
done
for i in 1 2 3; do
    ready=0
    for _ in $(seq 1 100); do
        if ! kill -0 "${pids[i-1]}" 2>/dev/null; then echo "node-$i exited; see $PLAY/node-$i.log" >&2; exit 1; fi
        if mountpoint -q "$PLAY/node-$i" && (exec 3<>"/dev/tcp/127.0.0.1/${ports[i-1]}") 2>/dev/null; then ready=1; break; fi
        sleep 0.1
    done
    if (( ! ready )); then echo "node-$i failed to start; see $PLAY/node-$i.log" >&2; exit 1; fi
done
cat <<EOF

  Each mount covers its original backing directory: $PLAY/node-{1,2,3}
  Each process serves only its own backing directory, not its merged view.
  Structured logs: $PLAY/node-{1,2,3}-telemetry.jsonl
  Route events from all nodes appear here; directories are marked merged.

  Try from another terminal:
    ls $PLAY/node-1
    cat $PLAY/node-1/node-{1,2,3}-example.txt
    ls $PLAY/node-2/shared
    cat $PLAY/node-3/shared/node-{1,2,3}.txt
    printf 'created on node 2\n' > $PLAY/node-2/new-local.txt
    cat $PLAY/node-1/new-local.txt

  Remote truncate/chmod and remote unlink remain unsupported. Avoid cp -f.
EOF
if (( KEEP )); then
    echo 'Leaving nodes running. To stop:'
    printf '  kill'; printf ' %q' "${pids[@]}"; printf '\n'
    trap - EXIT
else
    echo 'Press Enter to stop nodes and unmount.'
    read -r _ || true
fi
