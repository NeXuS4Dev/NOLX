#!/usr/bin/env python3
"""Verify native same-kind controller binding, I/O isolation and independent lifecycle.

Build: make kernel KERNEL_FEATURES=hal-driver-multi-test
The fixtures cover segment zero, one SATA port / NVMe namespace per controller.
Firmware still assigns BARs/bridge windows; this does not test native resource allocation.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
KINDS = ['virtio-blk', 'ahci', 'nvme']


def boot(qemu, kernel, names, machine, bridge=False, nics=False):
    label = ('all' if len(set(names)) > 1 else names[0]) + '-' + machine + ('-bridge' if bridge else '') + ('-net' if nics else '')
    log = ROOT / 'build' / f'multi-{label}.log'
    platform = 'q35,sata=off' if machine == 'q35' else machine
    command = [qemu, '-machine', platform, '-kernel', str(kernel), '-m', '512M',
               '-nographic', '-no-reboot', '-net', 'none']
    if bridge:
        command += ['-device', 'pci-bridge,id=storagebridge,chassis_nr=1']
    disks = []
    for i, name in enumerate(names):
        disk = ROOT / 'build' / f'multi-{label}-{i}.img'
        subprocess.run(['python3', 'tools/make-test-disk.py', str(disk)], cwd=ROOT,
                       check=True, stdout=subprocess.DEVNULL)
        tag = i + 1
        with disk.open('r+b') as image:
            image.seek(1740 * 512)
            image.write(bytes([tag]) * 512)
        disks.append((disk, tag))
        command += ['-drive', f'file={disk},if=none,id=disk{i},format=raw']
        bus = ',bus=storagebridge' if bridge else ''
        if name == 'virtio-blk':
            modern = ',disable-legacy=on' if i % 2 else ''
            command += ['-device', f'virtio-blk-pci,drive=disk{i}{modern}{bus}']
        elif name == 'ahci':
            command += ['-device', f'ahci,id=ahci{i}{bus}', '-device', f'ide-hd,drive=disk{i},bus=ahci{i}.0']
        else:
            command += ['-device', f'nvme,drive=disk{i},serial=nolx{i}{bus}']
    if nics:
        command += ['-netdev', 'user,id=net0,ipv6=off', '-device', 'e1000,netdev=net0',
                    '-netdev', 'user,id=net1,ipv6=off', '-device', 'virtio-net-pci,netdev=net1']
    complete = f'driver: lifecycle selftest complete, {len(names) + (2 if nics else 0)} device(s), DMA pages pinned 0'
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
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    text = log.read_text(errors='replace')
    assert complete in text, f'{log}\n{text}'
    assert not re.search(r'PANIC|BUGCHECK|CPU exception', text), text
    assert 'driver: duplicate storage enumeration preserved handles and resources' in text, text
    assert f'driver: multi-controller isolation verified: {len(names)} storage devices' in text, text
    rows = re.findall(r'driver: multi instance (\d+) (\S+), block (\d+), media tag (\d+)', text)
    assert len(rows) == len(names), text
    assert len({r[0] for r in rows}) == len(names), rows
    assert len({r[2] for r in rows}) == len(names), rows
    assert {int(r[3]) for r in rows} == set(range(1, len(names) + 1)), rows
    instances = re.findall(r'(virtio-blk|ahci|nvme): instance (\d+), block (\d+), vector (0x[0-9a-f]+), PCI (\S+)', text)
    assert len(instances) == len(names), text
    assert len({r[3] for r in instances}) == len(names), instances
    assert len({r[4] for r in instances}) == len(names), instances
    for name in set(names):
        count = names.count(name)
        assert text.count(f'driver: {name} Removed, restart refused') == count, text
        for cycle in range(1, 4):
            assert text.count(f'driver: {name} cycle {cycle}:') == count, text
    if nics:
        assert 'driver: e1000 Removed, restart refused' in text, text
        assert 'driver: virtio-net Removed, restart refused' in text, text
    if machine == 'q35':
        assert 'pci: configuration transport=ECAM segment 0' in text, text
        assert re.search(r'pci: ECAM driver accesses: [1-9][0-9]* reads, [1-9][0-9]* writes', text), text
    if bridge:
        assert all(not r[4].startswith('00:') for r in instances), instances
    assert 'in use 0 -> 0 bytes, integrity ok' in text, text
    for disk, tag in disks:
        with disk.open('rb') as image:
            image.seek(1740 * 512)
            assert image.read(512) == bytes([tag]) * 512, disk
            image.seek(1750 * 512)
            assert image.read(512) == bytes([tag ^ 0xa0]) * 512, disk
            image.seek(1700 * 512)
            assert image.read(512) == bytes([0xc3]) * 512, disk
    print(f'PASS: {label}: {len(names)} distinct media/BDFs/queues/vectors, independent stop/restart, zero pinned DMA', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build' / 'nolx.elf')
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    for machine in ['pc', 'q35']:
        for kind in KINDS:
            boot(args.qemu, args.kernel.resolve(), [kind, kind], machine)
        boot(args.qemu, args.kernel.resolve(), KINDS * 2, machine)
    boot(args.qemu, args.kernel.resolve(), ['virtio-blk', 'virtio-blk'], 'pc', bridge=True)
    boot(args.qemu, args.kernel.resolve(), KINDS * 2, 'q35', nics=True)


if __name__ == '__main__':
    main()
