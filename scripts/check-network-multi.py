#!/usr/bin/env python3
"""Check exact-adapter public RX/TX, shared INTx and independent hardware lifecycle.

Build with make kernel KERNEL_FEATURES=hal-driver-net-multi-test.
Each adapter has its own user-network backend and packet capture. Segment zero only;
Firmware routing comes from evaluated ACPI, not Interrupt Line; optional native BAR
allocation is exercised by the separate resource fixtures.
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
    platform = 'q35,sata=off' if machine == 'q35' else machine
    if machine == 'q35' and memory > 2048:
        platform += ',max-ram-below-4g=2G'
    command = [qemu, '-machine', platform,
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
    instances = re.findall(
        r'(e1000|virtio-net): instance (\d+), PCI (\S+), mac \[([^]]+)\], '
        r'(?:GSI (\d+), )?vectors? (0x[0-9a-f]+)(?:/(0x[0-9a-f]+))?',
        text,
    )
    assert len(instances) == len(names), text
    assert len({r[1] for r in instances}) == len(names), instances
    assert len({r[2] for r in instances}) == len(names), instances
    mac_for_handle = {r[1]: bytes.fromhex(r[3].replace(',', '')) for r in instances}
    assert set(mac_for_handle.values()) == set(captures), instances
    all_vectors = [int(vector, 16) for row in instances for vector in row[5:7] if vector]
    assert all(0x50 <= vector <= 0xdf for vector in all_vectors), instances
    e1000_routes = [(int(row[4]), row[5]) for row in instances if row[0] == 'e1000']
    for index, (gsi, vector) in enumerate(e1000_routes):
        for other_gsi, other_vector in e1000_routes[index + 1:]:
            assert (gsi == other_gsi) == (vector == other_vector), instances
    msix = [vector for row in instances if row[0] == 'virtio-net' for vector in row[5:7] if vector]
    assert len(set(msix)) == 2 * names.count('virtio-net'), instances
    assert not (set(msix) & {vector for _, vector in e1000_routes}), instances
    if shared:
        vectors = {vector for _, vector in e1000_routes}
        gsis = {gsi for gsi, _ in e1000_routes}
        assert len(vectors) == len(gsis) == 1, f'fixture did not create shared INTx: {instances}'
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


def intx_refusal(qemu, kernel, machine, mode):
    """Injected advertised routes must not downgrade to the valid firmware line byte."""
    aml_tools = runpy.run_path(str(ROOT / 'scripts' / 'check-aml.py'))
    package, method, device, table = [aml_tools[k] for k in ['package', 'method', 'device', 'table']]
    if mode == 'unsupported-pic':
        payload = method(b'_PIC', bytes([0xa4, 0]), 1)
    else:
        value = bytes([0]) if mode == 'malformed-prt' else package([0x12], bytes([1]) + package([0x12], bytes([4, 0x0c, 0xff, 0xff, 0, 0, 0, 0, 0x0a, 11])))
        child = device(b'NLXI', bytes([8]) + b'_ADR' + bytes([0x0c, 0, 0, 28, 0]) + method(b'_PRT', bytes([0xa4]) + value))
        payload = package([0x10], b'\\' + bytes([0x2e]) + b'_SB_PCI0' + child)
    label = f'intx-refuse-{machine}-{mode}'
    aml = ROOT / 'build' / f'{label}.aml'
    aml.write_bytes(table(payload))
    log = ROOT / 'build' / f'{label}.log'
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot',
               '-net', 'none', '-netdev', 'user,id=n0,ipv6=off', '-device', 'e1000,netdev=n0',
               '-acpitable', f'file={aml}']
    with log.open('w') as output:
        process = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                if 'kernel heap selftest:' in text or process.poll() is not None:
                    break
                time.sleep(.1)
        finally:
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=5)
    text = log.read_text(errors='replace')
    assert 'kernel heap selftest:' in text, text
    assert not re.search(r'PANIC|BUGCHECK|CPU exception', text), text
    assert 'e1000: not brought up: Interrupt' in text, text
    assert 'e1000: bound and started' not in text and 'e1000: instance' not in text, text
    assert ('ACPI INTx evaluation refused:' in text or 'ACPI INTx scope mapping refused:' in text
            or 'SSDT refused atomically:' in text), text
    print(f'PASS: {label}: advertised-but-refused routes never fall back to Interrupt Line', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build' / 'nolx.elf')
    parser.add_argument('--intx-routing-only', action='store_true', help='require hal-pci-intx-poison-test; verify direct/shared/bridged/nested PC and Q35')
    parser.add_argument('--link-srs-only', action='store_true', help='require hal-pci-link-srs-test (and poisoned lines); all PIRQ links start disabled and must recover through _SRS')
    parser.add_argument('--storage-fixture-only', action='store_true', help='require both multi-test features; six storage controllers plus four NICs on Q35')
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    if args.link_srs_only:
        for machine in ['pc', 'q35']:
            for options in [{}, {'shared': True}, {'bridge': True}]:
                text = boot(args.qemu, args.kernel.resolve(), ['e1000'] * 2, machine, **options)
                assert re.search(r'link test: disabled [1-9]\d* PIRQ route\(s\)', text), \
                    'fixture did not disable the firmware PIRQ routes first:\n' + text
                assert text.count('INTx line poisoned to ff') == 2, text
                preflight = re.findall(r'ACPI link preflight: (\d+) link\(s\), (\d+) require _SRS', text)
                assert len(preflight) == 1, f'preflight summary missing or repeated: {preflight}\n{text}'
                assert 'evaluated ACPI INTx routing authorized:' in text, text
                assert 'ACPI INTx evaluation refused:' not in text, text
                routes = re.findall(
                    r'e1000: instance \d+, PCI \S+, mac \[[^]]+\], GSI (\d+), vector (0x[0-9a-f]+)',
                    text,
                )
                assert len(routes) == 2, text
                if machine == 'q35':
                    # ICH9 in APIC mode routes through GSIA..GSIH link devices whose
                    # _CRS reports hardwired GSIs 16..23, as the retained-link lines in
                    # CI run 39461714857 show. The LPC PIRQ bytes this fixture disabled
                    # only steer PIC-mode delivery, so the audited preflight must
                    # retain every current route and perform no firmware write at all.
                    assert preflight[0] == ('8', '0'), f'Q35 preflight changed shape: {preflight}\n{text}'
                    retained = re.findall(r'link test: retained (\\_SB_\.GSI[A-H]) at GSI (\d+)', text)
                    assert len(retained) == 8, f'Q35 retained links incomplete: {retained}\n{text}'
                    assert {gsi for _, gsi in retained} == {str(gsi) for gsi in range(16, 24)}, retained
                    assert 'reprogrammed via _SRS' not in text, \
                        'Q35 fixed APIC routes were reprogrammed unnecessarily:\n' + text
                    assert {gsi for gsi, _ in routes} <= {gsi for _, gsi in retained}, \
                        f'e1000 traffic used a GSI outside the retained routes: routes={routes} retained={retained}'
                else:
                    # PIIX links are steerable: with every PIRQ route disabled, the
                    # preflight must select _PRS alternatives and the kernel must
                    # apply exactly that many _SRS transactions before authorizing
                    # routes, and the captured traffic must ride the selected GSIs.
                    assert int(preflight[0][1]) >= 1, \
                        f'no link needed _SRS after every PIRQ route was disabled: {preflight}\n{text}'
                    applied = re.findall(r'ACPI link (\S+) reprogrammed via _SRS: GSI (\d+)', text)
                    assert len(applied) == int(preflight[0][1]), \
                        f'preflight/applied _SRS counts disagree: {preflight} vs {applied}\n{text}'
                    assert {gsi for gsi, _ in routes} <= {gsi for _, gsi in applied}, \
                        f'e1000 traffic used a GSI no _SRS reprogramming selected: routes={routes} applied={applied}'
        return
    if args.intx_routing_only:
        for machine in ['pc', 'q35']:
            for options in [{}, {'shared': True}, {'bridge': True}, {'bridge': True, 'nested': True}]:
                text = boot(args.qemu, args.kernel.resolve(), ['e1000'] * 2, machine, **options)
                assert text.count('INTx line poisoned to ff') == 2, text
                assert 'evaluated ACPI INTx routing authorized:' in text, text
                routes = re.findall(
                    r'e1000: instance \d+, PCI \S+, mac \[[^]]+\], GSI (\d+), vector (0x[0-9a-f]+)',
                    text,
                )
                assert len(routes) == 2, text
                assert all(0x50 <= int(vector, 16) <= 0xdf for _, vector in routes), routes
                if options.get('shared'):
                    assert len(set(routes)) == 1, f'fixture did not share one GSI/vector: {routes}'
                if machine == 'q35':
                    assert 'interrupt model=APIC, _PIC notified=true' in text, text
                    assert re.search(r'INTx vector 0x[0-9a-f]+, interrupts [1-9]', text), text
            for mode in ['malformed-prt', 'unmapped-scope', 'unsupported-pic']:
                intx_refusal(args.qemu, args.kernel.resolve(), machine, mode)
        return
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
