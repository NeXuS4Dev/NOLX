#!/usr/bin/env bash
#
# T2 checks for the M2 fatal-exception path. Each feature builds an isolated kernel image,
# triggers one documented CPU exception after the full IDT is installed, and expects QEMU's
# serial log to contain the vector/register report before the kernel's deliberate HLT.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
QEMU="${QEMU:-qemu-system-x86_64}"
TIMEOUT="${TIMEOUT:-8}"

command -v "$QEMU" >/dev/null 2>&1 || {
    echo "FAIL: $QEMU not found (install qemu-system-x86)" >&2
    exit 1
}

run_case() {
    local feature="$1"
    local vector="$2"
    local log="$REPO/build/hal-$feature.serial.log"
    local status=0

    echo "== M2 exception test: $feature (vector $vector)"
    make -C "$REPO" kernel KERNEL_FEATURES="$feature"

    timeout "$TIMEOUT" "$QEMU" \
        -kernel "$REPO/build/nolx.elf" \
        -m 512M -nographic -no-reboot \
        >"$log" 2>&1 || status=$?

    if [[ "$status" -ne 124 ]]; then
        echo "FAIL: QEMU should remain halted until timeout (exit $status)" >&2
        cat "$log" >&2
        return 1
    fi
    if ! grep -qF "NOLX: CPU exception vector=$vector " "$log"; then
        echo "FAIL: missing exception vector $vector in $log" >&2
        cat "$log" >&2
        return 1
    fi
    if ! grep -qF " rsp=0x" "$log" || ! grep -qF " ss=0x" "$log"; then
        echo "FAIL: missing saved RSP/SS in $log" >&2
        cat "$log" >&2
        return 1
    fi
    if ! grep -qF "NOLX: registers rax=" "$log"; then
        echo "FAIL: missing register dump in $log" >&2
        cat "$log" >&2
        return 1
    fi
    if ! grep -qF "NOLX: fatal exception; processor halted" "$log"; then
        echo "FAIL: exception handler did not reach its deliberate halt" >&2
        cat "$log" >&2
        return 1
    fi
    if [[ "$vector" == 14 ]] && ! grep -qF "cr2=0x0000000080000000" "$log"; then
        echo "FAIL: page-fault report has the wrong CR2 address" >&2
        cat "$log" >&2
        return 1
    fi

    echo "PASS: vector $vector register report and fatal halt"
}

mkdir -p "$REPO/build"
run_case hal-fault-test-pf 14
run_case hal-fault-test-gp 13
