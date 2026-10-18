#!/usr/bin/env python3
"""Checked AHCI and NVMe recovery with two real disks, independent IRQs and host persistence.

Build hal-ahci-nvme-recovery-test and nolxfs-dump first. Fault hooks refuse cleanup,
allocation, flush or final proof; they do not claim naturally failing QEMU hardware.
Each PC/Q35 profile boots twice without regenerating its media.
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
# The phases each transport walks through, in order, and the line that closes the run.
PHASES = {
    'ahci': [
        'post-filesystem startup quarantine recovered; DMA 4 -> 2 -> 4, independent MSI peer live',
        'failed stop/repeated refusal retained owners; stale window and IRQ retired after recovery',
        'single-page allocation rollback reclaimed before the port named the buffer',
        'enabled-port partial startup retained both pages until the checked port stop',
        'withheld PCI exclusion proof retained both pages despite actual bus-master disable',
        'published-command failure quarantined without flush/buffer reuse until the checked port stop',
        'refused flush quarantined without reset/free; explicit recovery avoided unsafe flush retry',
    ],
    'nvme': [
        'post-filesystem startup quarantine recovered; DMA 12 -> 6 -> 12, independent MSI-X peer live',
        'failed stop/repeated refusal retained owners; stale window and IRQ retired after recovery',
        'admin-queue allocation rollback reclaimed before the controller named the queue',
        'enabled-controller partial startup retained all six pages until the checked disable',
        'withheld PCI exclusion proof retained all six pages despite actual bus-master disable',
        'published-command failure quarantined without flush/buffer reuse until the checked disable',
        'refused flush quarantined without reset/free; explicit recovery avoided unsafe flush retry',
    ],
}
COMPLETE = {
    'ahci': ('ahci-recovery: PASS fifteen durable media exchanges, checked failure recovery, '
             'independent MSI peer, stale window, freeze refusal, final DMA 0'),
    'nvme': ('nvme-recovery: PASS fifteen durable media exchanges, checked failure recovery, '
             'independent MSI-X peer, stale window, freeze refusal, final DMA 0'),
}
# The sequences the peer controller writes; the target writes the rest.
PEER_SEQUENCES = {0, 2, 3, 5, 7, 9, 11, 13}


def devices(backend, disks):
    """The two controllers of one backend, each with its own media and its own PCI slot."""
    command = []
    for index, disk in enumerate(disks):
        command += ['-drive', f'file={disk},if=none,id=disk{index},format=raw']
        address = f'{0x10 + index * 2:x}'
        if backend == 'ahci':
            command += ['-device', f'ahci,id=ahci{index},addr={address}',
                        '-device', f'ide-hd,drive=disk{index},bus=ahci{index}.0']
        else:
            command += ['-device', f'nvme,drive=disk{index},serial=nolx{index},addr={address}']
    return command


def run_boot(qemu, kernel, machine, backend, disks, label, boot):
    log = ROOT / 'build' / f'{label}-boot{boot}.log'
    platform = 'q35,sata=off' if machine == 'q35' else 'pc'
    command = [qemu, '-machine', platform, '-kernel', str(kernel), '-m', '512M',
               '-nographic', '-no-reboot', '-net', 'none']
    command += devices(backend, disks)
    with log.open('w') as out:
        process = subprocess.Popen(command, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 150
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                if COMPLETE[backend] in text or re.search(r'PANIC|BUGCHECK|CPU exception', text) or process.poll() is not None:
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
    assert COMPLETE[backend] in text, f'{log}\n{text}'
    assert not re.search(r'PANIC|BUGCHECK|CPU exception', text), text
    for phase in PHASES[backend]:
        assert f'{backend}-recovery: {phase}' in text, (label, boot, phase, text)
    rows = re.findall(
        rf'{backend}-recovery: exchange=(\d+) handle=(\d+) block=(\d+) tag=(\d+) lba=(\d+) '
        r'write/flush/read selected IRQ progressed', text)
    assert len(rows) == 15 and [int(r[0]) for r in rows] == list(range(15)), (label, rows)
    writers = {}
    for sequence, handle, block, tag, lba in rows:
        sequence, tag, lba = int(sequence), int(tag), int(lba)
        assert lba == 1770 + sequence, rows
        assert tag in (0x31, 0x32), rows
        writers[sequence] = tag
    # The peer writes its own sequences and the target the rest, on both boots.
    for sequence, tag in writers.items():
        assert (tag == 0x32) == (sequence in PEER_SEQUENCES), rows
    assert 'duplicate attach refused' in text, text
    assert f'{backend}: not brought up:' in text, text
    if boot == 2:
        initial = text.split('NOLX: init sequence complete')[0]
        assert initial.count('existing committed file verified before write') == 2, initial
    if machine == 'q35':
        assert 'pci: configuration transport=ECAM segment 0' in text, text
    return writers


def verify_media(disks, dump, label, writers):
    for index, disk in enumerate(disks):
        tag = 0x31 + index
        image = disk.read_bytes()
        assert image[1740 * 512:1741 * 512] == bytes([tag]) * 512, disk
        for sequence in range(15):
            owner = writers[sequence]
            expected = owner ^ 0x80 ^ sequence if owner == tag else 0
            assert image[(1770 + sequence) * 512:(1771 + sequence) * 512] == bytes([expected]) * 512, (disk, sequence)
        host_read(disk, ROOT / 'build' / f'{label}-{index}-partition.img', dump)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build/nolx.elf')
    parser.add_argument('--dump', type=Path, default=ROOT / 'target/debug/nolxfs-dump')
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    for backend in ['ahci', 'nvme']:
        for machine in ['pc', 'q35']:
            label = f'{backend}-nvme-recovery-{machine}'
            disks = [ROOT / 'build' / f'{label}-{i}.img' for i in range(2)]
            for index, disk in enumerate(disks):
                subprocess.run(['python3', 'tools/make-test-disk.py', str(disk)], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
                with disk.open('r+b') as image:
                    image.seek(1740 * 512)
                    image.write(bytes([0x31 + index]) * 512)
            for boot in [1, 2]:
                writers = run_boot(args.qemu, args.kernel.resolve(), machine, backend, disks, label, boot)
                verify_media(disks, args.dump.resolve(), label, writers)
            print(f'PASS: {label}: two unchanged-media boots, 15 unique durable writes each, '
                  'host-mounted files, partial/flush/stop quarantine, stale window, live peer and DMA 0', flush=True)


if __name__ == '__main__':
    main()
