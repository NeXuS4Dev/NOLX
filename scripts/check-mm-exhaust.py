#!/usr/bin/env python3
"""Allocator exhaustion: the frame allocator and the kernel heap must refuse, not break.

Runs the image built with `hal-mm-exhaust-test`. The fixture takes every free frame the machine
has, proves the allocator refuses the next frame and an order-1 block while its own free count
is zero, returns every frame, and then fills the heap to its 16-region ceiling, checks that
every 1 MiB payload is still intact, refuses an 8 MiB request, and releases everything with the
in-use accounting returning to exactly where it started.

Two real defects this fixture found, both in the heap's over-aligned path:

  * the two bookkeeping words that record a block's region and offset were written *at* the
    returned pointer instead of in front of it, so the caller's first write destroyed them and
    the block could never be freed (`HEAP CORRUPTION: the payload offset is not inside the
    region`);
  * the block reservation omitted those words, so the caller's last eight bytes landed on the
    next block's header.

Usage:
    make kernel KERNEL_FEATURES=hal-mm-exhaust-test
    python3 scripts/check-mm-exhaust.py [--machine q35]
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]

# The fixture prints this once, and only when every assertion in it passed. The wait below
# requires the whole line plus its newline: terminating QEMU the instant the prefix appears
# truncates the line mid-write, which is exactly how one run of this script failed with the
# fixture itself passing.
DONE = re.compile(r'mm-exhaust: PASS[^\n]*audit ok[^\n]*\n')

PASS = re.compile(
    r'mm-exhaust: PASS (\d+) frame\(s\) exhausted and returned, free frames exact at (\d+); '
    r'heap refused after (\d+) block\(s\) of (\d+) KiB with (\d+) region\(s\), in use '
    r'(\d+) -> (\d+) bytes, audit ok'
)
EXHAUSTED = re.compile(r'mm-exhaust: (\d+) frame\(s\) taken to exhaustion')
FRAMES_BACK = re.compile(r'mm-exhaust: (\d+) frame\(s\) returned, free frames (\d+) -> (\d+)')
HEAP_FULL = re.compile(
    r'mm-exhaust: heap served (\d+) block\(s\) of (\d+) KiB across (\d+) region\(s\), '
    r'(\d+) bytes in use, then refused an 8 MiB request'
)
HEAP_BACK = re.compile(
    r'mm-exhaust: heap released (\d+) block\(s\), in use (\d+) -> (\d+) bytes '
    r'\(started at (\d+)\), (\d+) region\(s\) retained'
)

# Any of these in the log is a failure, whatever else the log says.
FAILURES = re.compile(r'PANIC|BUGCHECK|CPU exception|panicked at|HEAP CORRUPTION')


def boot(qemu, kernel, machine):
    label = f'mm-exhaust-{machine}'
    log = ROOT / 'build' / f'{label}.log'
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot',
               '-net', 'none']
    print(f'booting {label}: {" ".join(command)}', flush=True)
    with log.open('w') as output:
        process = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
        try:
            # The fixture deliberately takes every frame on the machine and then fills the
            # heap: on a 512 MiB guest that is a few seconds of work, and a QEMU on a loaded
            # CI runner is slower, so the deadline is generous but finite.
            started = time.monotonic()
            deadline = started + 300
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                assert not FAILURES.search(text), text
                if DONE.search(text):
                    break
                assert process.poll() is None, text
                # A kernel built without the feature never prints the fixture's first line;
                # say so instead of waiting out the whole deadline.
                assert not (
                    time.monotonic() - started > 45
                    and 'mm-exhaust: starting' not in text
                    and 'NOLX: init sequence complete' in text
                ), (
                    f'{label}: this kernel has no mm-exhaust fixture; '
                    'build with KERNEL_FEATURES=hal-mm-exhaust-test'
                )
                time.sleep(.05)
            else:
                raise AssertionError(f'{label}: the fixture did not finish within 300 s')
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()

    text = log.read_text(errors='replace')
    assert not FAILURES.search(text), text
    assert 'NOLX: init sequence complete' in text, f'{log}\n{text}'

    match = PASS.search(text)
    assert match is not None, text
    taken, free_exact, blocks, block_kib, regions, in_use_from, in_use_to = (
        int(group) for group in match.groups())

    exhausted = EXHAUSTED.search(text)
    assert exhausted is not None, text
    assert int(exhausted.group(1)) == taken, text

    returned = FRAMES_BACK.search(text)
    assert returned is not None, text
    released, free_before, free_after = (int(group) for group in returned.groups())
    assert released == taken, ('the fixture returned fewer frames than it took', text)
    assert free_before == free_after == free_exact, (
        'the free-frame count did not return to the value the fixture found', text)

    full = HEAP_FULL.search(text)
    assert full is not None, text
    served, served_kib, full_regions, full_in_use = (int(group) for group in full.groups())
    assert served == blocks, ('the heap served a different number of blocks', text)
    assert served_kib == block_kib, ('the heap block size changed', text)
    assert served > 0, ('the heap refused its first 1 MiB block', text)
    assert full_regions == regions, ('the region count changed between reports', text)
    assert full_regions <= 16, (f'the heap reported {full_regions} regions', text)
    assert full_in_use > 0, ('the heap reported no bytes in use while holding blocks', text)

    back = HEAP_BACK.search(text)
    assert back is not None, text
    released_blocks, released_from, released_to, started_at, retained = (
        int(group) for group in back.groups())
    assert released_blocks == blocks, ('the heap released a different number of blocks', text)
    assert released_from == full_in_use, ('the release line disagrees about bytes in use', text)
    assert released_to == started_at, (
        'the heap did not return to the bytes-in-use it started with', text)
    assert in_use_to == started_at, ('the summary disagrees about bytes in use', text)
    assert retained == full_regions, ('the heap lost regions while releasing blocks', text)
    assert in_use_from == started_at, ('the summary starts from different bytes in use', text)

    print(f'PASS: {label}: {taken} frame(s) exhausted and returned with the free count exact at '
          f'{free_exact}; the heap served {blocks} block(s) of {block_kib} KiB across {regions} '
          f'region(s) with every payload intact, refused an 8 MiB request, and returned to '
          f'{started_at} bytes in use', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build' / 'nolx.elf')
    parser.add_argument('--machine', action='append', choices=('pc', 'q35'),
                        help='machine(s) to boot; defaults to both')
    parser.add_argument('--qemu', default=os.environ.get('QEMU') or 'qemu-system-x86_64',
                        help='QEMU binary (defaults to $QEMU, then qemu-system-x86_64)')
    args = parser.parse_args()
    kernel = args.kernel.resolve()
    if not kernel.is_file():
        print(f'error: {kernel} does not exist; build with hal-mm-exhaust-test', file=sys.stderr)
        return 2
    for machine in args.machine or ['pc', 'q35']:
        boot(args.qemu, kernel, machine)
    return 0


if __name__ == '__main__':
    sys.exit(main())
