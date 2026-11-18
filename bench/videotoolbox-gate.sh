#!/usr/bin/env bash
# videotoolbox-gate.sh: this Mac's VideoToolbox VP9 decoder against the daemon's stream,
# whatever a viewer lost (see videotoolbox-gate/src/main.rs). Run on the viewing Mac before
# a deploy; needs libvpx (pkg-config) and FFmpeg on PATH. Exits nonzero if it fails.
set -euo pipefail
cd "$(dirname "$0")/videotoolbox-gate"
exec cargo run --release -j 3 --target-dir ../../target/videotoolbox-gate
