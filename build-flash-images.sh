#!/bin/bash
# Build a merged flash image from the cfg.toml.tmp defaults (Wi-Fi credentials
# and Maps API key left unset), suitable for distribution:
#   flash_images/flash_eew-display.bin
#
# When tools/voice/voice.bin exists it is folded into the same image at the
# `voice` partition offset (0x700000), so the one file also carries the spoken
# announcements — flash it alone to 0x0 and nothing else is needed. Generate
# voice.bin first with tools/voice/gen_voice.py (see README, Spoken announcements).
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$SCRIPT_DIR"
OUTPUT_DIR="$SCRIPT_DIR/flash_images"
OUTPUT="$OUTPUT_DIR/flash_eew-display.bin"
MERGED="$OUTPUT_DIR/.merged.bin"   # espflash output, before the voice bank is folded in
VOICE_BIN="$SCRIPT_DIR/tools/voice/voice.bin"
VOICE_ADDR=7340032   # 0x700000, the `voice` partition offset (partitions.csv)

# Source ESP toolchain environment
source "$SCRIPT_DIR/../export-esp.sh"

mkdir -p "$OUTPUT_DIR"

# The build reads cfg.toml, so the template is copied over it for the build.
# Keep the user's own cfg.toml (credentials, API key) and put it back afterwards.
CFG_BACKUP=""
if [ -f "$PROJECT_DIR/cfg.toml" ]; then
    CFG_BACKUP="$(mktemp)"
    cp "$PROJECT_DIR/cfg.toml" "$CFG_BACKUP"
fi
restore_cfg() {
    if [ -n "$CFG_BACKUP" ]; then
        cp "$CFG_BACKUP" "$PROJECT_DIR/cfg.toml"
        rm -f "$CFG_BACKUP"
    else
        rm -f "$PROJECT_DIR/cfg.toml"
    fi
}
trap restore_cfg EXIT

echo "========================================"
echo " Building from cfg.toml.tmp"
echo "========================================"

cp "$PROJECT_DIR/cfg.toml.tmp" "$PROJECT_DIR/cfg.toml"

# cfg.toml is read by a macro at compile time and cargo does not track it,
# so force the crate to be recompiled.
touch "$PROJECT_DIR/src/main.rs"

FLASH_SIZE=16777216   # 16 MB — the whole flash (partitions.csv ends at 0x1000000)

(cd "$PROJECT_DIR" && cargo build --release)
(cd "$PROJECT_DIR" && cargo espflash save-image --chip esp32s3 --merge "$MERGED")

# Lay the pieces onto a full-size 0xFF (erased) canvas so the gaps — and the
# voice region before it is written — are 0xFF, not 0x00, no matter how espflash
# padded its own output. Then overlay the merged firmware at 0x0 and, if present,
# the voice bank at the `voice` partition offset. The result is flashed to 0x0.
head -c "$FLASH_SIZE" /dev/zero | tr '\0' '\377' > "$OUTPUT"
dd if="$MERGED" of="$OUTPUT" bs=1M conv=notrunc status=none

if [ -f "$VOICE_BIN" ]; then
    voice_end=$((VOICE_ADDR + $(stat -c%s "$VOICE_BIN")))
    if [ "$voice_end" -gt "$FLASH_SIZE" ]; then
        echo "ERROR: voice.bin does not fit at $(printf 0x%X $VOICE_ADDR) within the flash" >&2
        exit 1
    fi
    dd if="$VOICE_BIN" of="$OUTPUT" bs=1M seek="$VOICE_ADDR" oflag=seek_bytes conv=notrunc status=none
    echo "Included voice bank: $(du -h "$VOICE_BIN" | cut -f1) at $(printf 0x%X $VOICE_ADDR)"
else
    echo "NOTE: $VOICE_BIN not found — image built WITHOUT spoken announcements."
    echo "      Generate it with tools/voice/gen_voice.py (README: Spoken announcements)."
fi
rm -f "$MERGED"

echo "========================================"
echo " Build complete"
ls -lh "$OUTPUT"
echo "========================================"
