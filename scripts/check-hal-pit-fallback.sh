#!/usr/bin/env bash
# Boot the kernel with the architectural local-APIC feature hidden and prove that the
# retained 8259/PIT path still delivers the same periodic tick.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
mkdir -p build
log=build/hal-pit-fallback.serial.log

set +e
timeout 20 qemu-system-x86_64 \
  -kernel build/nolx.elf \
  -cpu qemu64,-apic \
  -m 512M -nographic -no-reboot \
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
