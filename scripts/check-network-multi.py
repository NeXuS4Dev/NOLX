#!/usr/bin/env python3
"""Check exact-adapter public RX/TX, shared INTx and independent hardware lifecycle.

Build with make kernel KERNEL_FEATURES=hal-driver-net-multi-test.
Each adapter has its own user-network backend and packet capture. Segment zero only;
BAR/bridge assignments still come from firmware, not native resource allocation.
"""
import argparse
import os
from pathlib import Path
import re
import runpy
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
WIRE = runpy.run_path(str(ROOT / 'scripts' / 'check-virtio-net.py'))
packets = WIRE['packets']
GATEWAY = WIRE['GATEWAY']


def boot(qemu, kernel, names, machine, bridge=False, shared=False, storage=False, nested=False, memory=512):
    label = ('mixed' if len(set(names)) > 1 else names[0]) + '-' + machine
    label += '-bridge' if bridge else '-shared' if shared else ''
    label += '-nested' if nested else ''
    label += '-storage' if storage else ''
    if memory != 512:
        label += f'-ram{memory}'
    log = ROOT / 'build' / f'net-multi-{label}.log'
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else machine,
               '-kernel', str(kernel), '-m', f'{memory}M', '-nographic', '-no-reboot', '-net', 'none']
    if bridge:
        model = 'pcie-pci-bridge' if machine == 'q35' else 'pci-bridge'
        command += ['-device', model + ',id=nics' + (',chassis_nr=1' if machine == 'pc' else '')]
        if nested:
            command += ['-device', 'pci-bridge,id=inner,bus=nics,chassis_nr=2']
    captures = {}
    for i, name in enumerate(names):
        mac = bytes([2, 0, 0, 0, 0, 0x20 + i])
        capture = ROOT / 'build' / f'net-multi-{label}-{i}.pcap'
        captures[mac] = capture
        bus = (',bus=inner' if nested else ',bus=nics') if bridge else ''
        address = f',addr={4 + i * 4:x}' if shared else ''
        modern = ',disable-legacy=on' if name == 'virtio-net' and i % 2 else ''
        model = 'virtio-net-pci' if name == 'virtio-net' else name
        command += ['-netdev', f'user,id=net{i},ipv6=off', '-device',
                    f'{model},netdev=net{i},mac={mac.hex(":")}{bus}{address}{modern}',
                    '-object', f'filter-dump,id=capture{i},netdev=net{i},file={capture}']
    disks = []
    if storage:
        for i, kind in enumerate(['virtio-blk', 'ahci', 'nvme'] * 2):
            disk = ROOT / 'build' / f'net-multi-{label}-{kind}-{i}.img'
            subprocess.run(['python3', 'tools/make-test-disk.py', str(disk)], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
            tag = i + 1
            with disk.open('r+b') as image:
                image.seek(1740 * 512)
                image.write(bytes([tag]) * 512)
            disks.append((disk, tag))
            command += ['-drive', f'file={disk},if=none,id=disk{i},format=raw']
            if kind == 'virtio-blk':
                command += ['-device', f'virtio-blk-pci,drive=disk{i}' + (bus if bridge else '')]
            elif kind == 'ahci':
                command += ['-device', f'ahci,id=ahci{i}' + (bus if bridge else ''), '-device', f'ide-hd,drive=disk{i},bus=ahci{i}.0']
            else:
                command += ['-device', f'nvme,drive=disk{i},serial=nolx{i}' + (bus if bridge else '')]
    complete = f'driver: lifecycle selftest complete, {len(names) + len(disks)} device(s), DMA pages pinned 0'
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
    assert f'driver: multi-adapter isolation verified: {len(names)} NICs' in text, text
    assert 'driver: network burst: every adapter queued before public RX drain' in text, text
    assert 'in use 0 -> 0 bytes, integrity ok' in text, text
    if machine == 'q35':
        assert 'pci: configuration transport=ECAM segment 0' in text, text
    instances = re.findall(r'(e1000|virtio-net): instance (\d+), PCI (\S+), mac \[([^]]+)\], vectors? (0x[0-9a-f]+)(?:/(0x[0-9a-f]+))?', text)
    assert len(instances) == len(names), text
    assert len({r[1] for r in instances}) == len(names), instances
    assert len({r[2] for r in instances}) == len(names), instances
    mac_for_handle = {r[1]: bytes.fromhex(r[3].replace(',', '')) for r in instances}
    assert set(mac_for_handle.values()) == set(captures), instances
    msix = [v for r in instances if r[0] == 'virtio-net' for v in r[4:6]]
    assert len(set(msix)) == 2 * names.count('virtio-net'), instances
    if shared:
        vectors = [r[4] for r in instances]
        assert len(set(vectors)) == 1, f'fixture did not create shared INTx: {instances}'
    if bridge:
        assert all(not r[2].startswith('00:') for r in instances), instances
    for name in set(names):
        assert text.count(f'driver: {name} Removed, restart refused') == names.count(name), text
        for cycle in range(1, 4):
            assert text.count(f'driver: {name} cycle {cycle}:') == names.count(name), text
    exchanges = re.findall(r'driver: net exchange (\d+), handle (\d+), (\S+), mac \[([^]]+)\], local (\d+), selected IRQ progressed', text)
    assert len(exchanges) == len(names) * (len(names) + 5), exchanges
    assert len({row[0] for row in exchanges}) == len(exchanges), exchanges
    observed = {mac: packets(path) for mac, path in captures.items()}
    for _, handle, _, encoded_mac, local in exchanges:
        mac = bytes.fromhex(encoded_mac.replace(',', ''))
        assert mac_for_handle[handle] == mac, exchanges
        source_ip = bytes([10, 0, 2, int(local)])
        request = (b'\xff' * 6 + mac + bytes.fromhex('08060001080006040001')
                   + mac + source_ip + bytes(6) + bytes([10, 0, 2, 2]) + bytes(18))
        assert observed[mac].count(request) == 1, f'{label}: selected adapter did not send unique request {local}'
        replies = [frame for frame in observed[mac] if len(frame) >= 42 and frame[:6] == mac
                   and frame[6:12] == GATEWAY and frame[22:28] == GATEWAY
                   and frame[12:22] == bytes.fromhex('08060001080006040002')
                   and frame[28:32] == bytes([10, 0, 2, 2]) and frame[32:38] == mac and frame[38:42] == source_ip]
        assert len(replies) == 1, f'{label}: no unique host reply for {mac.hex()} local {local}'
        for other, frames in observed.items():
            if other != mac:
                assert request not in frames, f'{label}: request escaped to another adapter'
    if storage:
        assert 'driver: multi-controller isolation verified: 6 storage devices' in text, text
        for disk, tag in disks:
            with disk.open('rb') as image:
                for sector, value in [(1740, tag), (1750, tag ^ 0xa0), (1700, 0xc3)]:
                    image.seek(sector * 512)
                    assert image.read(512) == bytes([value]) * 512, disk
    print(f'PASS: {label}: {len(names)} independent adapters, {len(exchanges)} captured public ARP exchanges, own IRQ progress, zero pinned DMA', flush=True)
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build' / 'nolx.elf')
    parser.add_argument('--storage-fixture-only', action='store_true', help='require both multi-test features; six storage controllers plus four NICs on Q35')
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    if args.storage_fixture_only:
        boot(args.qemu, args.kernel.resolve(), ['e1000', 'e1000', 'virtio-net', 'virtio-net'], 'q35', storage=True)
        return
    for machine in ['pc', 'q35']:
        for name in ['e1000', 'virtio-net']:
            boot(args.qemu, args.kernel.resolve(), [name] * 2, machine)
        boot(args.qemu, args.kernel.resolve(), ['e1000', 'e1000', 'virtio-net', 'virtio-net'], machine)
    boot(args.qemu, args.kernel.resolve(), ['e1000'] * 2, 'pc', shared=True)
    boot(args.qemu, args.kernel.resolve(), ['e1000', 'e1000', 'virtio-net', 'virtio-net'], 'pc', bridge=True)


if __name__ == '__main__':
    main()
