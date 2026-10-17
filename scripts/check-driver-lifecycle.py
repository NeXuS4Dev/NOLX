#!/usr/bin/env python3
"""Exercise persistent native driver stop/restart/removal and exact DMA ownership.

Build with make kernel KERNEL_FEATURES=hal-driver-lifecycle-test.
Healthy paths only: failed-start recovery and MMIO unmapping are separate gates.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
DRIVERS = {'virtio-blk': 2, 'ahci': 2, 'nvme': 6, 'e1000': 11, 'virtio-net': 4}


def boot(qemu, kernel, names, machine="pc"):
    fixture = 'all' if len(names) > 1 else names[0]
    if machine != 'pc':
        fixture += '-' + machine
    log = ROOT / 'build' / f'lifecycle-{fixture}.log'
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else machine, '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot']
    disks = []
    for name in names:
        if name in ('virtio-blk', 'ahci', 'nvme'):
            disk = ROOT / 'build' / f'lifecycle-{fixture}-{name}.img'
            subprocess.run(['python3', 'tools/make-test-disk.py', str(disk)], cwd=ROOT,
                           check=True, stdout=subprocess.DEVNULL)
            disks.append(disk)
            command += ['-drive', f'file={disk},if=none,id={name},format=raw']
            if name == 'virtio-blk':
                command += ['-device', 'virtio-blk-pci,drive=virtio-blk']
            elif name == 'ahci':
                command += ['-device', 'ahci,id=ahci', '-device', 'ide-hd,drive=ahci,bus=ahci.0']
            else:
                command += ['-device', 'nvme,drive=nvme,serial=nolx0']
        else:
            command += ['-netdev', f'user,id={name},ipv6=off', '-device', f'{name if name == "e1000" else "virtio-net-pci"},netdev={name}']
    command += ['-net', 'none']  # No implicit default NIC; explicit -netdevs remain active.
    complete = f'driver: lifecycle selftest complete, {len(names)} device(s), DMA pages pinned 0'
    with log.open('w') as out:
        process = subprocess.Popen(command, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                if complete in text or re.search(r'PANIC|BUGCHECK|CPU exception', text) or process.poll() is not None:
                    break
                time.sleep(.1)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    text = log.read_text(errors='replace')
    assert complete in text, f'{log}\n{text}'
    if machine == 'q35':
        assert 'pci: configuration transport=ECAM segment 0' in text, text
        assert re.search(r'pci: ECAM driver accesses: [1-9][0-9]* reads, [1-9][0-9]* writes', text), text
    assert not re.search(r'PANIC|BUGCHECK|CPU exception', text), text
    assert 'in use 0 -> 0 bytes, integrity ok' in text, text
    assert 'NOLX: address space selftest:' in text, text
    assert 'driver: recursive framework and public I/O callbacks refused with Busy' in text, text
    assert 'nvme: poll deadline selftest: ready predicate accepted, false predicate timed out after two HPET seconds' in text, text
    assert 'mm: MMIO lifecycle: stale access refused, VA reused, partial-map OOM rolled back, shared root retained' in text, text
    for name in names:
        assert f'driver: {name} MMIO mappings released and reused across all three cycles' in text, text
        for cycle in range(1, 4):
            assert f'driver: {name} cycle {cycle}: Started -> Stopped -> Started, {DRIVERS[name]} DMA pages reclaimed and reacquired, I/O verified' in text, text
        assert f'driver: {name} Removed, restart refused' in text, text
    for disk in disks:
        with disk.open('rb') as image:
            image.seek(1700 * 512)
            assert image.read(512) == bytes([0xc3]) * 512, disk
    detail = 'disk writes persisted' if disks else 'fresh ARP RX/TX'
    print(f'PASS: lifecycle-{fixture}: three cycles/device, DMA reclaimed, removal refused, {detail}', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build' / 'nolx.elf')
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    for name in DRIVERS:
        boot(args.qemu, args.kernel.resolve(), [name])
    boot(args.qemu, args.kernel.resolve(), list(DRIVERS))
    boot(args.qemu, args.kernel.resolve(), list(DRIVERS), "q35")


if __name__ == '__main__':
    main()
