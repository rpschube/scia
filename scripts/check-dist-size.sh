#!/usr/bin/env bash
# Release size guard: fail if a shipped scia binary exceeds the size budget.
#
# US-DIST-1 promises a single-file binary "< 10 MB, no runtime dependencies
# beyond OS/system audio libraries". A regression that bloats the binary (an
# accidental heavy dependency, a debug build slipping into the release profile,
# LTO/strip disabled) is exactly the kind of silent failure a gate must catch,
# so this runs against the real dist/release-profile binary in CI.
#
# Usage:   check-dist-size.sh <binary-path> [max-bytes]
# Default max: 10 MiB (10 * 1024 * 1024 = 10485760 bytes).
#
# Exit: 0 if the file is at/under the limit, 1 if it exceeds it, 2 on usage or
# a missing file. The message names the actual and allowed sizes either way so
# a passing run still shows the current headroom.
set -euo pipefail

MAX_DEFAULT=$((10 * 1024 * 1024))

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 <binary-path> [max-bytes]" >&2
  exit 2
fi

bin="$1"
max="${2:-$MAX_DEFAULT}"

if [[ ! -f "$bin" ]]; then
  echo "check-dist-size: not a file: $bin" >&2
  exit 2
fi

# Portable size read: GNU stat (-c) first, then BSD/macOS stat (-f).
size=$(stat -c %s "$bin" 2>/dev/null || stat -f %z "$bin")

human() { awk -v b="$1" 'BEGIN { printf "%.2f MiB (%d bytes)", b / 1048576, b }'; }

if (( size > max )); then
  echo "check-dist-size: FAIL — $bin is $(human "$size"), over the limit of $(human "$max")" >&2
  exit 1
fi

echo "check-dist-size: OK — $bin is $(human "$size"), within the limit of $(human "$max")"
