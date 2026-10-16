#!/usr/bin/env bash
# Verify real driver ECAM reads/writes on Q35 and retain the legacy-only i440FX path.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
mkdir -p build

for machine in q35 pc; do
  log="build/pci-${machine}.serial.log"
  drives=()
  if [ "$machine" = q35 ]; then
    python3 tools/make-test-disk.py build/ecam-disk.img
    drives=(-drive file=build/ecam-disk.img,if=none,id=vd0,format=raw -device virtio-blk-pci,drive=vd0)
  fi
  set +e
  timeout 30 qemu-system-x86_64 \
    -machine "$machine" -kernel build/nolx.elf \
    -m 512M -nographic -no-reboot "${drives[@]}" >"$log" 2>&1
  status=$?
  set -e
  cat "$log"
  if [ "$status" -ne 124 ]; then
    echo "QEMU $machine exited with status $status (expected timeout)"
    exit 1
  fi
  if grep -qE 'PANIC|BUGCHECK|CPU exception' "$log"; then
    echo "PCI $machine boot reported a fault"
    exit 1
  fi
  grep -qF 'NOLX: init sequence complete' "$log"
  grep -qF 'NOLX: timer reached 100 ticks; nominal 100 Hz' "$log"
  if [ "$machine" = q35 ]; then
    grep -qF 'pci: configuration transport=ECAM segment 0' "$log"
    grep -qE 'pci: ECAM driver accesses: [1-9][0-9]* reads, [1-9][0-9]* writes' "$log"
    grep -qF 'virtio-blk: bound and started through nolx-df' "$log"
    grep -qE 'virtio-blk: wrote 4 sectors at 1600, read back, crc32 0x[0-9a-f]{8} matches' "$log"
    python3 - <<'CHECK'
import zlib
from pathlib import Path
log = Path('build/pci-q35.serial.log').read_text()
data = Path('build/ecam-disk.img').read_bytes()[1600*512:1604*512]
checksum = f'crc32 0x{zlib.crc32(data):08x} matches'
assert checksum in log, 'ECAM-backed disk image does not match the reported checksum'
CHECK
    grep -qE 'pci: ECAM [1-9][0-9]* identity match\(es\), 0 refused; legacy fallback available' "$log"
    grep -qE 'pci: ECAM [0-9a-f]{2}:[0-9a-f]{2}\.[0-7] identity matched, extended header 0x[0-9a-f]{8}' "$log"
  else
    grep -qF 'pci: configuration transport=legacy' "$log"
    grep -qF 'pci: no usable MCFG; legacy configuration retained' "$log"
  fi
done

echo 'PASS: Q35 ECAM identities match and legacy-only PCI still boots'
