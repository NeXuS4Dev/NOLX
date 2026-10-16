#!/usr/bin/env python3
"""Verify file persistence through a real storage backend, outside the guest's caches.

Requires build/nolx.elf, QEMU, and target/debug/nolxfs-dump (cargo build -p nolxfs-dump).
An existing first-boot image is never regenerated. Run after the storage-backends CI boot.
A second QEMU boot must read the previously committed file before writing anything to it.
"""
import argparse
from pathlib import Path
import os
import re
import struct
import subprocess
import time
import zlib

ROOT = Path(__file__).resolve().parents[1]


def pattern(length):
    return bytes((((i & 255) * 7) & 255) ^ ((((i >> 8) & 255) * 131) & 255) ^ 0xA5
                 for i in range(length))


def partition(image):
    """Independently check the primary GPT before extracting its first used partition."""
    header = bytearray(image[512:1024])
    assert header[:8] == b'EFI PART', 'bad GPT signature'
    size, checksum = struct.unpack_from('<II', header, 12)
    assert 92 <= size <= 512, 'bad GPT header length'
    struct.pack_into('<I', header, 16, 0)
    assert zlib.crc32(header[:size]) == checksum, 'bad GPT header checksum'
    table_lba, count, stride, checksum = struct.unpack_from('<QIII', header, 72)
    assert stride >= 128 and stride % 8 == 0, 'bad GPT entry size'
    table = image[table_lba * 512:table_lba * 512 + count * stride]
    assert len(table) == count * stride and zlib.crc32(table) == checksum, 'bad GPT table'
    for index in range(count):
        entry = table[index * stride:(index + 1) * stride]
        if any(entry[:16]):
            first, last = struct.unpack_from('<QQ', entry, 32)
            assert first <= last < len(image) // 512, 'bad GPT partition extent'
            return image[first * 512:(last + 1) * 512]
    raise AssertionError('no data partition')


def host_read(image, output, dump):
    # A whole trailing partial block is not exposed by the kernel adapter either.
    data = partition(image.read_bytes())
    output.write_bytes(data[:len(data) // 4096 * 4096])
    result = subprocess.run([str(dump), str(output), 'cat', '/checksum.bin'],
                            cwd=ROOT, check=True, stdout=subprocess.PIPE)
    assert result.stdout == pattern(8192), 'persisted file differs from the guest pattern'
    raw = image.read_bytes()[1600 * 512:1604 * 512]
    assert raw == pattern(2048), 'persisted raw sectors differ'
    print(f'PASS: host independently mounted {image.name}, file crc32 0x{zlib.crc32(result.stdout):08x}')


def reboot(backend, image, log, qemu, extra, corrupt=False):
    device = {
        'virtio': ['-device', 'virtio-blk-pci,drive=disk'],
        'ahci': ['-device', 'ahci,id=ahci', '-device', 'ide-hd,drive=disk,bus=ahci.0'],
        'nvme': ['-device', 'nvme,drive=disk,serial=nolx0'],
    }[backend]
    command = [qemu, '-kernel', str(ROOT / 'build/nolx.elf'), '-m', '512M',
               '-nographic', '-no-reboot', '-drive', f'file={image},if=none,id=disk,format=raw',
               *device, '-net', 'none', *extra]
    with log.open('w') as out:
        process = subprocess.Popen(command, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 90
            while process.poll() is None and time.monotonic() < deadline:
                if 'NOLX: init sequence complete' in log.read_text(errors='replace'):
                    break
                time.sleep(0.05)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    text = log.read_text(errors='replace')
    prefix = 'virtio-blk' if backend == 'virtio' else backend
    assert 'NOLX: init sequence complete' in text, text
    assert not re.search(r'PANIC|BUGCHECK|CPU exception', text), text
    if corrupt:
        assert re.search(rf'{prefix}: not brought up: (Filesystem|Gpt)\(', text), text
        assert f'{prefix}: nolxfs file' not in text, text
    else:
        assert f'{prefix}: existing committed file verified before write' in text, text
        assert re.search(rf'{prefix}: nolxfs file checksum.bin: 8192 bytes .*crc32 0x9ea14b00 matches', text), text
        print(f'PASS: {backend} second boot read the old committed file before writing')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('backend', choices=['virtio', 'ahci', 'nvme'])
    parser.add_argument('image', type=Path)
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    parser.add_argument('--machine', choices=['q35', 'pc'], default='pc')
    parser.add_argument('--no-apic', action='store_true')
    args = parser.parse_args()
    image = args.image.resolve()
    dump = ROOT / 'target/debug/nolxfs-dump'
    output = image.with_suffix('.partition.img')
    host_read(image, output, dump)
    extra = ['-machine', args.machine]
    if args.no_apic:
        assert args.backend == 'ahci', 'only AHCI supports the no-APIC configuration'
        extra += ['-cpu', 'qemu64,-apic']
    reboot(args.backend, image, image.with_suffix('.reboot.log'), args.qemu, extra)
    host_read(image, output, dump)
    # Neither bad partition metadata nor damaged existing filesystem metadata authorizes mkfs.
    original = image.read_bytes()
    first = struct.unpack_from('<Q', original, 2 * 512 + 32)[0]
    for name, offset in [('bad-gpt', 2 * 512), ('bad-volume', first * 512 + 32), ('erased-superblock', None)]:
        damaged = bytearray(original)
        if offset is None:
            damaged[first * 512:first * 512 + 4096] = bytes(4096)
        else:
            damaged[offset] ^= 0x80
        bad_image = image.with_name(image.stem + f'.{name}.img')
        bad_image.write_bytes(damaged)
        reboot(args.backend, bad_image, bad_image.with_suffix('.log'), args.qemu, extra, corrupt=True)
        assert bad_image.read_bytes() == damaged, f'{name}: rejected media was modified'
        print(f'PASS: {args.backend} refused {name} without formatting or modifying the image')


if __name__ == '__main__':
    main()
