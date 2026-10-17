#!/usr/bin/env python3
"""Verify frozen six-driver shutdown, real held input retirement and fail-closed S5 refusal.

Build hal-power-quiesce-test (or its flush-fault/stop-fault variants). BSP PC/Q35 only.
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


def boot(qemu, kernel, machine, mode):
    name = f'power-quiesce-{machine}-{mode}'
    active = mode != 'normal'
    marker = 'shutdown admission remains closed'
    disks = []
    log = ROOT / 'build' / f'{name}.log'
    qmp = ROOT / 'build' / f'{name}.sock'
    qmp.unlink(missing_ok=True)
    # The Q35 SATA controller would otherwise be probed/enabled even without disks.
    # Each test supplies all six native driver types, unlike the primitive-only S5 fixture.
    platform = 'q35,sata=off' if machine == 'q35' else machine
    command = [qemu, '-machine', platform, '-kernel', str(kernel), '-m', '512M',
               '-nographic', '-no-reboot', '-qmp', f'unix:{qmp},server=on,wait=off']
    for backend in ('virtio', 'ahci', 'nvme'):
        disk = ROOT / 'build' / f'{name}-{backend}.img'
        subprocess.run(['python3', 'tools/make-test-disk.py', str(disk)], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
        disks.append(disk)
        command += ['-drive', f'file={disk},if=none,id={backend},format=raw']
        if backend == 'virtio': command += ['-device', 'virtio-blk-pci,drive=virtio']
        elif backend == 'ahci': command += ['-device', 'ahci,id=ahci', '-device', 'ide-hd,drive=ahci,bus=ahci.0']
        else: command += ['-device', 'nvme,drive=nvme,serial=shutdown0']
    command += ['-netdev', 'user,id=vnet,ipv6=off', '-device', 'virtio-net-pci,netdev=vnet',
                '-netdev', 'user,id=enet,ipv6=off', '-device', 'e1000,netdev=enet',
                '-device', 'qemu-xhci,id=usb', '-device', 'usb-kbd,bus=usb.0',
                '-device', 'usb-mouse,bus=usb.0', '-net', 'none']
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    events = []
    pending = b''
    with log.open('w') as out:
        process = subprocess.Popen(command, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 90
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
            injected = False
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                if not injected and 'power-test: armed, require held USB key and mouse button' in text:
                    injected = True
                    connection.sendall((json.dumps({'execute': 'input-send-event', 'arguments': {'events': [
                        {'type': 'key', 'data': {'down': True, 'key': {'type':'qcode', 'data':'a'}}},
                        {'type': 'btn', 'data': {'down': True, 'button': 'left'}}]}}) + '\n').encode())
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
                if active and marker in text:
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
        assert marker in text, text
        assert f'power-test: PASS {mode.split("-")[0]} refusal,' in text, text
        assert 'power: requesting ACPI S5' not in text, text
    else:
        assert 'quiesced 6 controller(s), DMA pages 0' in text, text
        for driver in ('virtio-blk', 'ahci', 'nvme', 'e1000', 'virtio-net', 'xhci'):
            assert f'driver={driver} durable=true' in text, text
        assert 'new I/O/start refused, DMA 38 -> 0; entering S5' in text, text
        assert 'power: requesting ACPI S5' in text, text
        assert any(e['event'] == 'SHUTDOWN' and e.get('data', {}).get('guest') is True
                   and e['data'].get('reason') == 'guest-shutdown' for e in events), events
    if not active:
        import importlib
        storage = importlib.import_module('check-storage-files')
        for index, disk in enumerate(disks):
            with disk.open('rb') as media:
                media.seek(1780 * 512)
                assert media.read(512) == bytes([0xd1 + index]) * 512, disk
            storage.host_read(disk, disk.with_suffix('.partition.img'), ROOT / 'target/debug/nolxfs-dump')
    print(f'PASS: {name}; ' + ('fail-closed refusal, no QMP shutdown' if active else 'six native barriers/stops, input EOF, DMA 38 -> 0, QMP guest-shutdown'))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build/nolx.elf')
    parser.add_argument('--mode', choices=['normal','flush-fault','stop-fault','unowned'], default='normal')
    args = parser.parse_args()
    qemu = os.environ.get('QEMU', 'qemu-system-x86_64')
    for machine in ['pc', 'q35']:
        boot(qemu, args.kernel.resolve(), machine, args.mode)



if __name__ == '__main__':
    main()
