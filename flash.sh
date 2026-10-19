#!/bin/bash
# Build and flash everything to the device in one go:
#   1. firmware + partition table   (cargo espflash flash --release)
#   2. voice bank                   (tools/voice/voice.bin -> `voice` partition at 0x700000)
#   3. serial monitor
#
#   ./flash.sh                  firmware + voice bank + monitor
#   ./flash.sh --voice-only     only the voice bank (e.g. after regenerating voice.bin)
#   ./flash.sh --no-voice       firmware only
#   ./flash.sh --no-monitor     don't open the monitor at the end
#   ./flash.sh --sandbox        build with --features quake-sandbox (P2P test data: fires an EEW at boot!)
#   ./flash.sh --port /dev/ttyACM1
#
# Generate the voice bank first (see README, "Spoken announcements"):
#   GOOGLE_TTS_API_KEY=... python3 tools/voice/gen_voice.py
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

VOICE_ADDR=0x700000
VOICE_BIN="$SCRIPT_DIR/tools/voice/voice.bin"

do_firmware=1; do_voice=1; do_monitor=1; features=(); port=()
while [ $# -gt 0 ]; do
    case "$1" in
        --voice-only) do_firmware=0 ;;
        --no-voice)   do_voice=0 ;;
        --no-monitor) do_monitor=0 ;;
        --sandbox)    features=(--features quake-sandbox) ;;
        --port)       shift; port=(--port "$1") ;;
        -h|--help)    sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "unknown option: $1 (see --help)" >&2; exit 1 ;;
    esac
    shift
done

# ESP toolchain environment (espup writes export-esp.sh to the home directory).
for f in "$SCRIPT_DIR/../export-esp.sh" "$HOME/export-esp.sh"; do
    if [ -f "$f" ]; then source "$f" >/dev/null 2>&1; break; fi
done

if [ "$do_voice" = 1 ] && [ ! -f "$VOICE_BIN" ]; then
    echo "NOTE: $VOICE_BIN not found — the firmware will run without spoken announcements."
    echo "      Generate it with tools/voice/gen_voice.py (README: Spoken announcements)."
    do_voice=0
fi

if [ "$do_firmware" = 1 ]; then
    echo "==> Flashing firmware + partition table"
    cargo espflash flash --release "${features[@]}" "${port[@]}"
fi

if [ "$do_voice" = 1 ]; then
    echo "==> Flashing voice bank ($(du -h "$VOICE_BIN" | cut -f1)) to $VOICE_ADDR"
    cargo espflash write-bin "${port[@]}" "$VOICE_ADDR" "$VOICE_BIN"
fi

if [ "$do_monitor" = 1 ]; then
    echo "==> Serial monitor (Ctrl+C to quit)"
    cargo espflash monitor "${port[@]}"
fi
