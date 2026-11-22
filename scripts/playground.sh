#!/usr/bin/env bash
# Merge three gRPC backing stores and the local covered directory into one mount.
# ./scripts/playground.sh          # interactive, clean up on exit
# ./scripts/playground.sh --keep   # leave processes up; prints cleanup commands
set -euo pipefail
cd "$(dirname "$0")/.."

PLAY=${PUNKFS_PLAYGROUND_DIR:-.playground}
MNT="$PLAY/mnt"
KEEP=0
case "${1:-}" in "") ;; --keep) KEEP=1 ;; *) echo "usage: $0 [--keep]" >&2; exit 2 ;; esac
PIDS=()
MOUNT_PID=
MOUNTED=0

cleanup() {
    if (( MOUNTED )); then
        fusermount3 -u "$MNT" 2>/dev/null || fusermount3 -uz "$MNT" 2>/dev/null || true
    fi
    [[ -z "$MOUNT_PID" ]] || kill "$MOUNT_PID" 2>/dev/null || true
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    [[ -z "$MOUNT_PID" ]] || wait "$MOUNT_PID" 2>/dev/null || true
    for pid in "${PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if mountpoint -q "$MNT" 2>/dev/null; then
    echo "error: $MNT is already mounted; unmount it before starting" >&2
    exit 1
fi

echo '==> building'
cargo build --quiet --bin punkfs --bin punkfs-server
mkdir -p "$MNT" "$PLAY/backing-1" "$PLAY/backing-2" "$PLAY/backing-3"

# Use unique playground-only names. Do not overwrite data from earlier runs.
[[ -e "$MNT/local-example.txt" ]] || printf 'hello from local backing\n' > "$MNT/local-example.txt"
for i in 1 2 3; do
    path="$PLAY/backing-$i/node-$i-example.txt"
    [[ -e "$path" ]] || printf 'hello from remote node %s\n' "$i" > "$path"
    mkdir -p "$PLAY/backing-$i/shared"
    path="$PLAY/backing-$i/shared/node-$i.txt"
    [[ -e "$path" ]] || printf 'shared directory contribution from node %s\n' "$i" > "$path"
done

nodes=()
for i in 1 2 3; do
    port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
    addr="127.0.0.1:$port"
    echo "==> starting node-$i: $PLAY/backing-$i on $addr"
    PUNKFS_LOG_FILE="$PLAY/node-$i-telemetry.jsonl" ./target/debug/punkfs-server "$PLAY/backing-$i" "$addr" "node-$i" >"$PLAY/node-$i.log" 2>&1 &
    pid=$!
    PIDS+=("$pid")
    ready=0
    for _ in $(seq 1 100); do
        if ! kill -0 "$pid" 2>/dev/null; then echo "node-$i exited; see $PLAY/node-$i.log" >&2; exit 1; fi
        if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then ready=1; break; fi
        sleep 0.1
    done
    if (( ! ready )); then echo "node-$i failed to start; see $PLAY/node-$i.log" >&2; exit 1; fi
    nodes+=("node-$i=http://$addr")
done

echo "==> mounting merged filesystem at $MNT"
# Console route events are explicitly enabled for this playground. The logger
# writes structured records to a file and mirrors selected messages to stderr.
PUNKFS_LOG_ROUTES=1 PUNKFS_LOG_LEVEL=info PUNKFS_LOG_DESTINATION=file \
    PUNKFS_LOG_FILE="$PLAY/mount-telemetry.jsonl" \
    ./target/debug/punkfs "$MNT" local-node "${nodes[@]}" >"$PLAY/mount.log" &
MOUNT_PID=$!
for _ in $(seq 1 100); do
    if mountpoint -q "$MNT"; then MOUNTED=1; break; fi
    if ! kill -0 "$MOUNT_PID" 2>/dev/null; then
        # The process may have mounted between the two checks.
        if mountpoint -q "$MNT"; then MOUNTED=1; fi
        echo "mount process exited; see $PLAY/mount.log" >&2; exit 1
    fi
    sleep 0.1
done
if (( ! MOUNTED )); then
    if mountpoint -q "$MNT"; then MOUNTED=1; else echo "mount failed; see $PLAY/mount.log" >&2; exit 1; fi
fi

cat <<EOF

  mount and local backing: $MNT (the original directory is visible after unmount)
  remote backing stores:   $PLAY/backing-{1,2,3}
  server logs:             $PLAY/node-{1,2,3}.log
  structured mount log:    $PLAY/mount-telemetry.jsonl

  Route messages are printed here: operation, path, node ID, and local/remote.
  Directories can merge contributions from multiple nodes and are marked merged;
  their individual entries show the selected local or remote owner.

  Try:
    ls -la $MNT
    cat $MNT/local-example.txt
    cat $MNT/node-{1,2,3}-example.txt
    ls $MNT/shared && cat $MNT/shared/node-{1,2,3}.txt
    printf 'new local file\n' > $MNT/new-local.txt
    mkdir $MNT/new-local-dir
    printf 'appended\n' >> $MNT/node-1-example.txt

  Remote truncate/chmod and remote unlink remain unsupported. Avoid cp -f.
EOF

if (( KEEP )); then
    echo "Leaving mount and servers running. To stop:"
    printf '  fusermount3 -u %q\n' "$MNT"
    printf '  kill %q' "$MOUNT_PID"; printf ' %q' "${PIDS[@]}"; printf '\n'
    trap - EXIT
else
    echo 'Press Enter to unmount and exit.'
    read -r _ || true
fi
