#!/usr/bin/env bash
# Headless screenshot of Lector under Xvfb + Mesa lavapipe (software Vulkan).
#
#   scripts/screenshot.sh <path-to-open> <out.png> [delay_secs] [-- extra lector args]
#
# Env:
#   LECTOR_BIN   binary to run (default: target/release/lector, else target/debug/lector)
#   DISPLAY_NUM  X display number (default 99)
#   SCREEN       Xvfb screen geometry (default 1600x1000x24)
#   INPUT        scripted input run before the capture, e.g.
#                INPUT="key ctrl+shift+p; type split" (see scripts/x11_input.py)
#   POKE_XY      where to park the pointer (default "1500 600")
#
# GPUI's Linux renderer (blade) needs Vulkan; under Xvfb there is no GPU, so we
# force Mesa's lavapipe ICD. WAYLAND_DISPLAY is unset so GPUI picks X11.
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 <path-to-open> <out.png> [delay_secs] [-- extra args]" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_PATH="$1"
OUT="$2"
DELAY="${3:-4}"
shift $(( $# >= 3 ? 3 : 2 ))
if [[ "${1:-}" == "--" ]]; then shift; fi
EXTRA_ARGS=("$@")

BIN="${LECTOR_BIN:-}"
if [[ -z "$BIN" ]]; then
  if [[ -x "$ROOT/target/release/lector" && "$ROOT/target/release/lector" -nt "$ROOT/target/debug/lector" ]]; then
    BIN="$ROOT/target/release/lector"
  else
    BIN="$ROOT/target/debug/lector"
  fi
fi
[[ -x "$BIN" ]] || { echo "lector binary not found: $BIN (run cargo build -p lector)" >&2; exit 1; }

DISPLAY_NUM="${DISPLAY_NUM:-99}"
SCREEN="${SCREEN:-1600x1000x24}"
export DISPLAY=":$DISPLAY_NUM"
unset WAYLAND_DISPLAY

if ! xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then
  Xvfb "$DISPLAY" -screen 0 "$SCREEN" -nolisten tcp >/dev/null 2>&1 &
  for _ in $(seq 1 50); do
    xdpyinfo -display "$DISPLAY" >/dev/null 2>&1 && break
    sleep 0.1
  done
fi

# Software Vulkan (Mesa lavapipe).
for icd in /usr/share/vulkan/icd.d/lvp_icd.x86_64.json /usr/share/vulkan/icd.d/lvp_icd.json; do
  if [[ -f "$icd" ]]; then
    export VK_ICD_FILENAMES="$icd"
    export VK_DRIVER_FILES="$icd"
    break
  fi
done
export LIBGL_ALWAYS_SOFTWARE=1
# Deterministic window placement/size for screenshots.
export LECTOR_SCREENSHOT=1

LOG="${OUT%.*}.log"
"$BIN" "$TARGET_PATH" "${EXTRA_ARGS[@]}" >"$LOG" 2>&1 &
PID=$!
# Wake GPUI's X11 event loop (see scripts/x11_poke.py), then let it settle.
POKE_XY="${POKE_XY:-1500 600}"
sleep 1.5
python3 -I "$ROOT/scripts/x11_poke.py" "$DISPLAY" 1 1 || true
python3 -I "$ROOT/scripts/x11_poke.py" "$DISPLAY" $POKE_XY || true
sleep "$DELAY"
if [[ -n "${INPUT:-}" ]]; then
  python3 -I "$ROOT/scripts/x11_input.py" "$DISPLAY" "$INPUT"
  sleep 1
fi
if ! kill -0 "$PID" 2>/dev/null; then
  echo "lector exited early; log: $LOG" >&2
  tail -n 30 "$LOG" >&2 || true
  exit 1
fi
import -display "$DISPLAY" -window root "$OUT"
kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true
echo "wrote $OUT"
