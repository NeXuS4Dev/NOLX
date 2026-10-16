#!/usr/bin/env bash
# Boot the kernel with the architectural local-APIC feature hidden and prove that the
# retained 8259/PIT path still delivers the same periodic tick.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
mkdir -p build
log=build/hal-pit-fallback.serial.log

python3 tools/make-test-disk.py build/pit-ahci.img
set +e
timeout 30 qemu-system-x86_64 \
  -kernel build/nolx.elf \
  -cpu qemu64,-apic \
  -m 512M -nographic -no-reboot \
  -net none \
  -drive file=build/pit-ahci.img,if=none,id=sata0,format=raw \
  -device ahci,id=ahci -device ide-hd,drive=sata0,bus=ahci.0 \
  >"$log" 2>&1
status=$?
set -e
cat "$log"

# The kernel deliberately halts forever after the boot smoke marker, so timeout is expected.
if [ "$status" -ne 124 ]; then
  echo "QEMU fallback boot exited with status $status (expected timeout after a clean boot)"
  exit 1
fi

for marker in \
  "ahci: polled completion fallback; no local APIC" \
  "ahci: bound and started through nolx-df" \
  "apic: unavailable (Unsupported); retaining PIC fallback" \
  "hal: active clock source=PIT/PIC nominal 100 Hz" \
  "NOLX: timer reached 100 ticks; nominal 100 Hz"
do
  if ! grep -qF "$marker" "$log"; then
    echo "missing PIC/PIT fallback marker: $marker"
    exit 1
  fi
done

if grep -qE "PANIC|BUGCHECK|CPU exception" "$log"; then
  echo "the PIC/PIT fallback reported a fault during boot"
  exit 1
fi

echo "PASS: the PIC/PIT fallback delivered at least 100 timer interrupts"

# The no-APIC fallback must preserve real storage, not only timer ticks.
python3 - <<'PYTEST'
from pathlib import Path
import zlib
log = Path('build/hal-pit-fallback.serial.log').read_text()
image = Path('build/pit-ahci.img').read_bytes()[1600*512:1604*512]
assert f'crc32 0x{zlib.crc32(image):08x} matches' in log, 'fallback disk checksum mismatch'
PYTEST
