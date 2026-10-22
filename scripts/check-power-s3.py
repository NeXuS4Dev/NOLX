#!/usr/bin/env python3
"""Observe a real ACPI S3 suspend and the platform wake path through QMP.

Build with: make kernel KERNEL_FEATURES=hal-power-test-s3
This fixture proves that the kernel stores the firmware's S3 sleep type with SLP_EN and that
the platform parks the VM (QMP SUSPEND). Before suspending, the kernel installs a real-mode
hand-off in low memory, writes it into the FACS waking vector and sets CMOS shutdown status
0xfe. A QMP system_wakeup then produces WAKEUP, SeaBIOS calls the waking vector, and the
hand-off re-enters long mode and prints a tag stored in kernel RAM before the sleep. Device
state is not restored, so the resumed kernel has no driver or ACPI PM path to shut down
cleanly; the fixture quits QEMU after observing the marker.
"""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def boot(qemu, kernel, machine):
    name = f'power-s3-{machine}'
    log = ROOT / 'build' / f'{name}.log'
    qmp = ROOT / 'build' / f'{name}.sock'
    qmp.unlink(missing_ok=True)
    # Q35's SATA controller would be probed and enabled even without disks. The S3 fixture
    # deliberately supplies neither storage nor network controllers, so no DMA bus master can
    # write memory while the sleep word is stored.
    platform = 'q35,sata=off' if machine == 'q35' else machine
    command = [qemu, '-machine', platform, '-kernel', str(kernel), '-m', '512M',
               '-nographic', '-no-reboot', '-qmp', f'unix:{qmp},server=on,wait=off',
               '-net', 'none']
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    events = []
    pending = b''
    with log.open('w') as out:
        process = subprocess.Popen(command, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 60
            while True:
                try:
                    connection.connect(str(qmp))
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    if time.monotonic() >= deadline or process.poll() is not None:
                        raise AssertionError(log.read_text(errors='replace'))
                    time.sleep(.05)
            connection.settimeout(.1)
            connection.sendall(b'{"execute":"qmp_capabilities"}\r\n')
            suspended_at = None
            woke_at = None
            requested = False
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                assert not re.search(r'PANIC|BUGCHECK|CPU exception', text), text
                try:
                    data = connection.recv(65536)
                    pending += data
                    while b'\n' in pending:
                        line, pending = pending.split(b'\n', 1)
                        if line.strip():
                            message = json.loads(line)
                            if 'event' in message:
                                events.append(message)
                    if not data:
                        break
                except (socket.timeout, ValueError):
                    pass
                if 'power: requesting ACPI S3' in text and not requested:
                    requested = True
                    assert any(e['event'] == 'SUSPEND' for e in events), events
                    suspended_at = time.monotonic()
                    connection.sendall(b'{"execute":"query-status"}\r\n')
                if requested and suspended_at and not woke_at:
                    # The machine is parked; ask the platform to wake it. QEMU requires
                    # RUN_STATE_SUSPENDED or the request is refused.
                    connection.sendall(b'{"execute":"system_wakeup"}\r\n')
                    woke_at = time.monotonic()
                if woke_at and 'power: resumed from S3 with tag' in text:
                    connection.sendall(b'{"execute":"quit"}\r\n')
                    break
                if process.poll() is not None:
                    break
                time.sleep(.05)
            code = process.wait(timeout=10)
        finally:
            connection.close()
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            qmp.unlink(missing_ok=True)
    text = log.read_text(errors='replace')
    assert code == 0, (code, text)
    assert 'NOLX: address space selftest:' in text, text
    assert 'in use 0 -> 0 bytes, integrity ok' in text, text
    assert text.count('power: requesting ACPI S3') == 1, text
    assert 'power: S3 refused' not in text, text
    assert 'power: resumed from S3 with tag 0x534c33524553554d' in text, text
    word = int(re.search(r'power: requesting ACPI S3 via PM1a 0x[0-9a-f]+, control (0x[0-9a-f]+)', text).group(1), 16)
    # SLP_TYP comes from the firmware's `_S3_` (1 here); SLP_EN must be set and the unrelated
    # SCI_EN bit preserved from the current control word.
    assert word & 0x3C00 == 0x2400, hex(word)
    assert word & 0x2000, hex(word)
    assert any(e['event'] == 'SUSPEND' for e in events), events
    assert any(e['event'] == 'WAKEUP' for e in events), events
    print(f'PASS: {name}; QMP SUSPEND + WAKEUP, control {hex(word)}, S3 resume tag verified')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build/nolx.elf')
    args = parser.parse_args()
    qemu = os.environ.get('QEMU', 'qemu-system-x86_64')
    for machine in ['pc', 'q35']:
        boot(qemu, args.kernel.resolve(), machine)


if __name__ == '__main__':
    main()
