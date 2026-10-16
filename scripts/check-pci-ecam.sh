#!/usr/bin/env bash
# Verify read-only ECAM on Q35 and retain the legacy-only i440FX path.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
mkdir -p build

for machine in q35 pc; do
  log="build/pci-${machine}.serial.log"
  set +e
  timeout 30 qemu-system-x86_64 \
    -machine "$machine" -kernel build/nolx.elf \
    -m 512M -nographic -no-reboot >"$log" 2>&1
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
    grep -qE 'pci: ECAM [1-9][0-9]* identity match\(es\), 0 refused; legacy configuration retained' "$log"
    grep -qE 'pci: ECAM [0-9a-f]{2}:[0-9a-f]{2}\.[0-7] identity matched, extended header 0x[0-9a-f]{8}' "$log"
  else
    grep -qF 'pci: no usable MCFG; legacy configuration retained' "$log"
  fi
done

echo 'PASS: Q35 ECAM identities match and legacy-only PCI still boots'
