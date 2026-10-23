#!/usr/bin/env python3
"""Exercise native virtio-net RX/TX and MSI-X, validating actual captured wire bytes.

The driver resolves the gateway over ARP before it reports the adapter started, so the fixture
needs a host-side peer. `user` (QEMU's slirp, the default) is what CI uses; `socket` serves the
same exchange from this script with `net_peer.ArpPeer`, for a QEMU build without libslirp.
Either way the captured bytes are the same, because the peer answers as slirp's gateway does.
"""
import argparse
import os
from pathlib import Path
import re
import struct
import subprocess
import time

from net_peer import ArpPeer

ROOT = Path(__file__).resolve().parents[1]
BUILD = ROOT / 'build'
QEMU = os.environ.get('QEMU', 'qemu-system-x86_64')
MAC = bytes.fromhex('020000000011')
GATEWAY = bytes.fromhex('52550a000202')


def packets(path):
    data = path.read_bytes()
    assert data[:4] == bytes.fromhex('d4c3b2a1'), 'expected little-endian pcap'
    assert struct.unpack_from('<I', data, 20)[0] == 1, 'capture must hold Ethernet frames'
    offset = 24
    frames = []
    while offset < len(data):
        assert offset + 16 <= len(data), 'truncated pcap record'
        _, _, length, original = struct.unpack_from('<IIII', data, offset)
        assert length == original and offset + 16 + length <= len(data), 'truncated captured packet'
        frames.append(data[offset + 16:offset + 16 + length])
        offset += 16 + length
    return frames


def boot(machine, modern, present=True, apic=True, netdev='user'):
    name = f'virtio-net-{machine}-{"modern" if modern else "transitional"}'
    if not present:
        name += '-absent'
    if not apic:
        name += '-no-apic'
    log = BUILD / f'{name}.log'
    capture = BUILD / f'{name}.pcap'
    command = [QEMU, '-machine', machine, '-kernel', str(BUILD / 'nolx.elf'),
               '-m', '512M', '-nographic', '-no-reboot']
    peer = None
    if present:
        if netdev == 'socket':
            peer = ArpPeer().start()
            backend = f'socket,id=net0,connect=127.0.0.1:{peer.port}'
        else:
            backend = 'user,id=net0,ipv6=off'
        command += ['-netdev', backend, '-device',
                    f'virtio-net-pci,netdev=net0,mac=02:00:00:00:00:11,disable-legacy={"on" if modern else "off"}',
                    '-object', f'filter-dump,id=capture,netdev=net0,file={capture}']
    else:
        command += ['-net', 'none']
    if not apic:
        command += ['-cpu', 'qemu64,-apic']
    with log.open('w') as out:
        process = subprocess.Popen(command, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 90
            while process.poll() is None and time.monotonic() < deadline:
                if 'NOLX: address space selftest:' in log.read_text(errors='replace'):
                    break
                time.sleep(.05)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    if peer is not None:
        peer.close()
    text = log.read_text(errors='replace')
    assert not re.search(r'PANIC|BUGCHECK|CPU exception', text), text
    assert 'NOLX: init sequence complete' in text, text
    assert 'NOLX: timer reached 100 ticks' in text, text
    assert 'NOLX: address space selftest:' in text, text
    assert re.search(r'NOLX: kernel heap selftest: .*in use 0 -> 0 bytes, integrity ok', text), text
    if not present:
        assert 'virtio-net: not brought up: NoDevice' in text, text
    elif not apic:
        assert 'virtio-net: not brought up: Interrupt' in text, text
    else:
        assert 'virtio-net: bound and started through nolx-df' in text, text
        assert 'e1000: no controller on the PCI bus' in text, text
        assert 'virtio-net: MSI-X RX 2 TX 2 completions, vectors 0x53/0x54' in text, text
        assert 'exchange 2, descriptor recycled' in text, text
        frames = packets(capture)
        request = (b'\xff' * 6 + MAC + bytes.fromhex('08060001080006040001')
                   + MAC + bytes([10, 0, 2, 15]) + bytes(6) + bytes([10, 0, 2, 2]) + bytes(18))
        assert frames.count(request) == 2, 'transmitted frames differ from the expected ARP bytes'
        replies = [frame for frame in frames if len(frame) >= 42
                   and frame[:6] == MAC and frame[6:12] == GATEWAY
                   and frame[12:22] == bytes.fromhex('08060001080006040002')
                   and frame[22:28] == GATEWAY and frame[28:32] == bytes([10, 0, 2, 2])
                   and frame[32:38] == MAC and frame[38:42] == bytes([10, 0, 2, 15])]
        assert len(replies) == 2, 'the host did not supply both matched ARP replies'
    print(f'PASS: {name}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--netdev', choices=['user', 'socket'],
                        default=os.environ.get('NOLX_NETDEV', 'user'),
                        help='host-side network backend for the bound cases')
    parser.add_argument('--machine', choices=['pc', 'q35'], action='append')
    args = parser.parse_args()
    BUILD.mkdir(exist_ok=True)
    for machine in args.machine or ['pc', 'q35']:
        for modern in [False, True]:
            boot(machine, modern, netdev=args.netdev)
    boot('pc', True, present=False, netdev=args.netdev)
    boot('pc', True, apic=False, netdev=args.netdev)


if __name__ == '__main__':
    main()
