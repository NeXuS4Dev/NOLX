#!/usr/bin/env python3
"""Checked virtio-blk recovery with two real disks, independent IRQs and host persistence.

Build hal-virtio-blk-recovery-test and nolxfs-dump first. Fault hooks refuse cleanup,
allocation, flush or final proof; they do not claim naturally failing QEMU hardware.
Each PC/Q35 modern/transitional profile boots twice without regenerating its media.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import time

import importlib.util

_spec = importlib.util.spec_from_file_location('storage_files', Path(__file__).with_name('check-storage-files.py'))
_storage_files = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_storage_files)
host_read = _storage_files.host_read

ROOT = Path(__file__).resolve().parents[1]
PHASES = [
    'post-filesystem startup quarantine recovered',
    'failed stop/repeated refusal retained owners',
    'single-page allocation rollback reclaimed before descriptor exposure',
    'enabled-queue partial startup retained both pages until checked reset',
    'withheld PCI exclusion proof retained both pages despite actual bus-master disable',
    'published-request failure quarantined without flush/buffer reuse until checked reset',
    'refused flush quarantined without reset/free; explicit recovery avoided unsafe flush retry',
]
PEER_SEQUENCES = {0, 2, 3, 5, 7, 9, 11, 13}
COMPLETE = ('virtio-blk-recovery: PASS fifteen durable media exchanges, checked failure recovery, '
            'independent MSI-X peer, stale mappings, freeze refusal, final DMA 0')


def run_boot(qemu, kernel, machine, modern, disks, label, boot):
    log = ROOT / 'build' / f'{label}-boot{boot}.log'
    platform = 'q35,sata=off' if machine == 'q35' else 'pc'
    command = [qemu, '-machine', platform, '-kernel', str(kernel), '-m', '512M',
               '-nographic', '-no-reboot', '-net', 'none']
    for index, disk in enumerate(disks):
        role_modern = modern if index == 0 else not modern
        option = ',disable-legacy=' + ('on' if role_modern else 'off')
        command += ['-drive', f'file={disk},if=none,id=disk{index},format=raw',
                    '-device', f'virtio-blk-pci,drive=disk{index},addr={4 + index * 4:x}{option}']
    with log.open('w') as out:
        process = subprocess.Popen(command, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 150
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                if COMPLETE in text or re.search(r'PANIC|BUGCHECK|CPU exception', text) or process.poll() is not None:
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
    assert COMPLETE in text, f'{log}\n{text}'
    assert not re.search(r'PANIC|BUGCHECK|CPU exception', text), text
    for phase in PHASES:
        assert f'virtio-blk-recovery: {phase}' in text, (label, boot, phase, text)
    rows = re.findall(r'virtio-blk-recovery: exchange=(\d+) handle=(\d+) block=(\d+) tag=(\d+) lba=(\d+) write/flush/read selected IRQ progressed', text)
    assert len(rows) == 15 and [int(r[0]) for r in rows] == list(range(15)), (label, rows)
    owners = {}
    for sequence, handle, block, tag, lba in rows:
        sequence, tag, lba = int(sequence), int(tag), int(lba)
        assert tag == (0x32 if sequence in PEER_SEQUENCES else 0x31), rows
        assert lba == 1770 + sequence, rows
        owners.setdefault(tag, (handle, block))
        assert owners[tag] == (handle, block), rows
    assert len(set(owners.values())) == 2, rows
    assert 'duplicate attach refused' in text, text
    target = 'modern' if modern else 'transitional'
    peer = 'transitional' if modern else 'modern'
    assert f'virtio-blk-recovery: transport target={target} peer={peer}' in text, text
    if boot == 2:
        initial = text.split('NOLX: init sequence complete')[0]
        assert initial.count('virtio-blk: existing committed file verified before write') == 2, initial
    if machine == 'q35':
        assert 'pci: configuration transport=ECAM segment 0' in text, text
    return log


def verify_media(disks, dump, label):
    for index, disk in enumerate(disks):
        tag = 0x31 + index
        image = disk.read_bytes()
        assert image[1740 * 512:1741 * 512] == bytes([tag]) * 512, disk
        for sequence in range(15):
            owned = (sequence in PEER_SEQUENCES) == (index == 1)
            expected = tag ^ 0x80 ^ sequence if owned else 0
            assert image[(1770 + sequence) * 512:(1771 + sequence) * 512] == bytes([expected]) * 512, (disk, sequence)
        host_read(disk, ROOT / 'build' / f'{label}-{index}-partition.img', dump)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build/nolx.elf')
    parser.add_argument('--dump', type=Path, default=ROOT / 'target/debug/nolxfs-dump')
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    for machine in ['pc', 'q35']:
        for modern in [False, True]:
            label = f'virtio-blk-recovery-{machine}-' + ('modern' if modern else 'transitional')
            disks = [ROOT / 'build' / f'{label}-{i}.img' for i in range(2)]
            for index, disk in enumerate(disks):
                subprocess.run(['python3', 'tools/make-test-disk.py', str(disk)], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
                with disk.open('r+b') as image:
                    image.seek(1740 * 512)
                    image.write(bytes([0x31 + index]) * 512)
            for boot in [1, 2]:
                run_boot(args.qemu, args.kernel.resolve(), machine, modern, disks, label, boot)
                verify_media(disks, args.dump.resolve(), label)
            print(f'PASS: {label}: two unchanged-media boots, 15 unique durable writes each, '
                  'host-mounted files, partial/flush/stop quarantine, stale MMIO, live peer and DMA 0', flush=True)


if __name__ == '__main__':
    main()
