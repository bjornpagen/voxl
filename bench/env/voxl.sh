#!/usr/bin/env bash
# (Re)start the voxl daemon on the benchmark display, its runtime folder (sockets and
# microphone FIFO) $VOXL_BENCH_RUN (default /tmp/voxl-bench), this user's alone. Extra args are passed through. Viewers reach it through
# `voxl relay` on port 6902 (`--ssh` to this machine, `--lanes` naming a proxy that reaches
# the lane sockets), as they reach a real desktop.
cd "$(dirname "$(readlink -f "$0")")"
BIN=${VOXL_BIN:-../../target/release/voxl}
RUN=${VOXL_BENCH_RUN:-/tmp/voxl-bench}
./killpat.sh "--runtime=$RUN" >/dev/null 2>&1
sleep 1
mkdir -p -m 0700 "$RUN"
DISPLAY=${BENCH_DISPLAY:-:2} RUST_LOG=${RUST_LOG:-info} setsid nohup "$BIN" --runtime="$RUN" --audio-enabled=false "$@" > /tmp/voxl-bench.log 2>&1 < /dev/null &
for i in $(seq 1 50); do [ -S "$RUN/control" ] && break; sleep 0.2; done
