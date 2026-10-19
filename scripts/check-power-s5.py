#!/usr/bin/env python3
"""Observe actual ACPI S5 through QMP, plus refusal with an active DMA-capable device.

Build with: make kernel KERNEL_FEATURES=hal-power-test-s5
This is not a claim that S3 resume or production-wide device quiescence is implemented.
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


def boot(qemu, kernel, machine, active=False):
    name = f'power-s5-{machine}' + ('-active-dma' if active else '')
    log = ROOT / 'build' / f'{name}.log'
    qmp = ROOT / 'build' / f'{name}.sock'
    qmp.unlink(missing_ok=True)
    # The Q35 SATA controller would otherwise be probed/enabled even without disks.
    # Positive tests deliberately supply no DMA-capable storage/network controller.
    platform = 'q35,sata=off' if machine == 'q35' else machine
    command = [qemu, '-machine', platform, '-kernel', str(kernel), '-m', '512M',
               '-nographic', '-no-reboot', '-qmp', f'unix:{qmp},server=on,wait=off']
    if active:
        command += ['-netdev', 'user,id=net0,ipv6=off', '-device', 'virtio-net-pci,netdev=net0']
    else:
        command += ['-net', 'none']
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    events = []
    pending = b''
    with log.open('w') as out:
        process = subprocess.Popen(command, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 30
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
            refused_at = None
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
                except socket.timeout:
                    pass
                if active and 'power: S5 refused: active PCI bus master' in text:
                    if refused_at is None:
                        refused_at = time.monotonic()
                    if time.monotonic() - refused_at >= .5:
                        assert process.poll() is None, 'refusal must not shut the guest down'
                        assert not any(e['event'] == 'SHUTDOWN' for e in events), events
                        connection.sendall(b'{"execute":"quit"}\r\n')
                        break
            code = process.wait(timeout=5)
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
    if active:
        assert 'power: S5 refused: active PCI bus master' in text, text
        assert 'power: requesting ACPI S5' not in text, text
    else:
        assert 'power: requesting ACPI S5' in text, text
        assert any(e['event'] == 'SHUTDOWN' and e.get('data', {}).get('guest') is True
                   and e['data'].get('reason') == 'guest-shutdown' for e in events), events
    print(f'PASS: {name}; ' + ('active-DMA refusal' if active else 'QMP guest-shutdown and QEMU exit 0'))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build/nolx.elf')
    args = parser.parse_args()
    qemu = os.environ.get('QEMU', 'qemu-system-x86_64')
    for machine in ['pc', 'q35']:
        boot(qemu, args.kernel.resolve(), machine)
    boot(qemu, args.kernel.resolve(), 'pc', active=True)


if __name__ == '__main__':
    main()
