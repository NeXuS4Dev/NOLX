#!/usr/bin/env python3
"""Real virtio-net failure recovery on two MSI-X adapters, with independent wire captures.

Requires hal-virtio-net-recovery-test. Faults refuse cleanup or withhold proof; they do
not claim QEMU actually fails reset/PCI writes. Packet bytes and IRQs are real.
"""
import argparse
import os
from pathlib import Path
import re
import runpy
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
WIRE = runpy.run_path(str(ROOT / 'scripts/check-virtio-net.py'))


def boot(qemu, kernel, machine, modern):
    label = f'virtio-net-recovery-{machine}-' + ('modern' if modern else 'transitional')
    log = ROOT / 'build' / f'{label}.log'
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot', '-net', 'none']
    captures = {}
    for i in range(2):
        mac = bytes([2, 0, 0, 0, 0, 0x20 + i])
        capture = ROOT / 'build' / f'{label}-{i}.pcap'
        captures[mac] = capture
        command += ['-netdev', f'user,id=net{i},ipv6=off', '-device',
                    f'virtio-net-pci,netdev=net{i},mac={mac.hex(":")},addr={4 + i*4:x},disable-legacy={"on" if (modern if i==0 else not modern) else "off"}',
                    '-object', f'filter-dump,id=capture{i},netdev=net{i},file={capture}']
    complete = 'virtio-net-recovery: PASS eleven fresh wire exchanges, checked failure recovery, independent MSI-X peer, stale mappings, freeze refusal, final DMA 0\n'
    with log.open('w') as output:
        process = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 100
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                if complete in text or re.search(r'PANIC|BUGCHECK|CPU exception', text) or process.poll() is not None:
                    break
                time.sleep(.05)
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
    assert 'in use 0 -> 0 bytes, integrity ok' in text, text
    for marker in ['post-ARP startup quarantine recovered', 'failed stop/repeated refusal retained 8 pages',
                   'partial three-page allocation rollback reclaimed before exposure',
                   'RX-queue-only partial startup retained all four pages until checked reset',
                   'deliberately withheld PCI exclusion proof pinned DMA despite actual bus-master disable']:
        assert f'virtio-net-recovery: {marker}' in text, text

    # Interrupt-vector exhaustion across both queue kinds: every candidate held, the start
    # refused as NoVector without a leak, and both vectors reclaimed once the pools are free.
    exhaustion = re.search(
        r'virtio-net-recovery: exhausted (\d+)/(\d+) RX/TX vector\(s\) refused start as '
        r'NoVector with no DMA/MMIO/vector leak; released pools let it start again', text)
    assert exhaustion, text
    assert 0 < int(exhaustion.group(1)) <= 256 and 0 < int(exhaustion.group(2)) <= 256, exhaustion.group(0)
    vectors = re.search(
        r'virtio-net-recovery: vectors (0x[0-9a-f]+)/(0x[0-9a-f]+) released after checked stop and reclaimed on restart',
        text,
    )
    assert vectors, text
    assert len(set(vectors.groups())) == 2 and all(0x50 <= int(vector, 16) <= 0xdf for vector in vectors.groups()), vectors.groups()
    exchanges = re.findall(r'virtio-net-recovery: exchange=(\d+) handle=(\d+) mac=\[([^]]+)\] local=(\d+) selected IRQ progressed', text)
    assert len(exchanges) == 11 and {int(r[0]) for r in exchanges} == set(range(11)), exchanges
    frames = {mac: WIRE['packets'](path) for mac, path in captures.items()}
    for sequence, handle, encoded, suffix in exchanges:
        mac = bytes.fromhex(encoded.replace(',', ''))
        assert mac == bytes([2, 0, 0, 0, 0, 0x20 + int(handle)]), exchanges
        local = bytes([10, 0, 2, int(suffix)])
        assert int(suffix) == 64 + int(sequence), exchanges
        request = (b'\xff' * 6 + mac + bytes.fromhex('08060001080006040001')
                   + mac + local + bytes(6) + bytes([10, 0, 2, 2]) + bytes(18))
        assert frames[mac].count(request) == 1, (label, sequence, 'missing/duplicate request')
        replies = [f for f in frames[mac] if len(f) >= 42 and f[:6] == mac
                   and f[6:12] == WIRE['GATEWAY'] and f[22:28] == WIRE['GATEWAY']
                   and f[12:22] == bytes.fromhex('08060001080006040002')
                   and f[28:32] == bytes([10, 0, 2, 2]) and f[32:38] == mac and f[38:42] == local]
        assert len(replies) == 1, (label, sequence, 'missing/duplicate reply')
        for other, observed in frames.items():
            if other != mac:
                assert request not in observed, (label, sequence, 'cross-adapter request')
    if machine == 'q35':
        assert 'pci: configuration transport=ECAM segment 0' in text, text
    print(f'PASS: {label}: partial allocation/RX/start/stop quarantine, independent MSI-X peer, '
          '11 unique captured ARP exchanges with own IRQs, checked recovery, stale MMIO, freeze and DMA 0', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build/nolx.elf')
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    for machine in ['pc', 'q35']:
        for modern in [False, True]:
            boot(args.qemu, args.kernel.resolve(), machine, modern)


if __name__ == '__main__':
    main()
