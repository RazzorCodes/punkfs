#!/usr/bin/env bash
# Manual playground: serve a scratch folder over grpc and mount it.
#
#   ./scripts/playground.sh          # mount at ./.playground/mnt, drop into a shell
#   ./scripts/playground.sh --keep   # same, but leave the mount up afterwards
#
# The backing store is ./.playground/backing, so what you see through the
# mount is a normal directory you can inspect from a second terminal.
set -uo pipefail

cd "$(dirname "$0")/.."

PLAY=.playground
BACKING="$PLAY/backing"
MNT="$PLAY/mnt"
LOG="$PLAY/server.log"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

cleanup() {
    fusermount3 -u "$MNT" 2>/dev/null || fusermount3 -uz "$MNT" 2>/dev/null
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT

echo "==> building"
cargo build --quiet --bins --examples || exit 1

mkdir -p "$BACKING" "$MNT"

# A small fixture worth poking at.
printf 'hello, punkfs\n'      > "$BACKING/greeting.txt"
printf 'line1\nline2\nline3\n' > "$BACKING/lines.txt"
head -c 100000 /dev/urandom   > "$BACKING/random.bin"
mkdir -p "$BACKING/nested/deep"
printf 'buried\n'             > "$BACKING/nested/deep/t.txt"
ln -sf greeting.txt             "$BACKING/a-symlink"
: > "$BACKING/empty.txt"

# Find a free port.
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
ADDR="127.0.0.1:$PORT"

echo "==> serving $BACKING on $ADDR"
./target/debug/punkfs-server "$BACKING" "$ADDR" playground > "$LOG" 2>&1 &
SERVER_PID=$!

for _ in $(seq 50); do
    (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break
    sleep 0.1
done

echo "==> mounting at $MNT"
./target/debug/examples/mount "http://$ADDR" "$MNT" > "$PLAY/mount.log" 2>&1 &
MOUNT_PID=$!

for _ in $(seq 50); do
    ls "$MNT" >/dev/null 2>&1 && break
    sleep 0.1
done

cat <<EOF

  mount:   $MNT
  backing: $BACKING
  server:  $LOG

  Try these.  WORKS is what the suite says passes today; GAP is a known bug
  listed in tests/CONFORMANCE.md.

  WORKS
    ls -la $MNT
    cat $MNT/greeting.txt
    cat $MNT/lines.txt
    head -2 $MNT/lines.txt
    wc -c $MNT/random.bin
    cmp $BACKING/random.bin $MNT/random.bin
    grep line2 $MNT/lines.txt
    stat -c '%n %s %a %U' $MNT/greeting.txt
    find $MNT -type f
    df -h $MNT
    du -b $MNT/random.bin
    tar -cf /tmp/p.tar -C $MNT lines.txt random.bin   # archive regular files
    cp $MNT/random.bin /tmp/copy.bin
    printf 'APPENDED\n' >> $MNT/lines.txt
    mv $MNT/greeting.txt $MNT/renamed.txt
    rm $MNT/empty.txt
    rmdir $MNT/nested/deep && echo removed

  GAPS (these fail, on purpose)
    echo hi > $MNT/greeting.txt            # O_TRUNC needs setattr (exit 1)
    truncate -s 2 $MNT/lines.txt           # setattr
    chmod 600 $MNT/lines.txt               # setattr
    cp /etc/hostname $MNT/greeting.txt     # cp truncates first
    cat $MNT/a-symlink                     # readlink refused
    tar -cf /tmp/p.tar -C $MNT .           # fails: it tries to readlink a-symlink
    touch $MNT/new.txt                     # creation refused by design
    mkdir $MNT/newdir                      # creation refused by design

  Watch the same change from the backing side:
    ls -la $BACKING

EOF

if [ "$KEEP" = 1 ]; then
    echo "Leaving mount up. Unmount with: fusermount3 -u $MNT"
    trap - EXIT
    wait
else
    echo "Press Enter to unmount and exit."
    read -r _
fi
