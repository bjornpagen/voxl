#!/usr/bin/env bash
# term.sh FIFO: a terminal on the benchmark display that runs each line written to FIFO, for
# linkviewer.py's scenarios that print to the screen (`cold` fills it with dense text before
# its viewer connects). xterm, maximized; XTERM to use another.
D=${BENCH_DISPLAY:-:2}; FIFO=$1
[ -p "$FIFO" ] || mkfifo "$FIFO"
DISPLAY=$D exec "${XTERM:-xterm}" -maximized -e bash -c 'while :; do read -r l < "$0" && eval "$l"; done' "$FIFO"
