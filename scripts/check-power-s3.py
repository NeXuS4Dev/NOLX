#!/usr/bin/env python3
"""Observe a real ACPI S3 suspend and the platform wake path through QMP.

Build with: make kernel KERNEL_FEATURES=hal-power-test-s3
This fixture proves that the kernel stores the firmware's S3 sleep type with SLP_EN and that
the platform parks the VM (QMP SUSPEND). Before suspending, the kernel installs a real-mode
hand-off in low memory, writes it into the FACS waking vector and sets CMOS shutdown status
0xfe. A QMP system_wakeup then produces WAKEUP, SeaBIOS calls the waking vector, and the
hand-off re-enters long mode and prints a tag stored in kernel RAM before the sleep.

The wake reset also clears the chipset state the kernel was using: on Q35 the memory-mapped
configuration window stops answering and the ICH9 LPC bridge forgets both its PM base and the
ACPI enable bit. The resumed kernel therefore re-selects its configuration transport, restores
the PM base, enters ACPI mode again and powers the machine off through S5. The fixture requires
that shutdown to happen on its own: it never sends `quit`, so a clean QEMU exit is the proof
that the resumed kernel could drive the platform again. Device state (drivers, PCI command
registers, interrupts) is still not restored, and that stays open.
"""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import struct
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def package(op, body):
    size = len(body) + 1
    if size < 64:
        length = bytes([size])
    else:
        size = len(body) + 2
        assert size < 4096
        length = bytes([0x40 | (size & 15), size >> 4])
    return bytes(op) + length + body


def method(name, body, args=0):
    return package([0x14], name + bytes([args]) + body)


def device(name, body):
    return package([0x5b, 0x82], name + body)


def table(aml):
    header = bytearray(struct.pack('<4sIBB6s8sI4sI', b'SSDT', 36 + len(aml), 2, 0,
                                   b'NOLX  ', b'PWRSTATE', 1, b'NLXP', 1))
    header[9] = (-sum(header + aml)) & 255
    return header + aml


def dstate_fixture(outside=False):
    """A device that declares every device power method, writing a real authorized register.

    `_PS3` and `_PS0` read the HPET general-configuration register -- inside the window the
    kernel authorizes from the parsed HPET table -- and store the value back, so the journal is
    applied as a real memory-mapped write with an exact readback and a value the kernel never
    chose. `_PSC` reports D0 and `_PRW` names GPE 11 waking from S3.
    """
    # The refused variant names a SystemMemory address no validated table authorized, so the
    # same method must be refused before any byte is touched -- and the sleep with it.
    address = 0x0000_2000 if outside else 0xfed0_0010
    region = (bytes([0x5b, 0x80]) + b'HPWR' + bytes([0x00, 0x0c])
              + struct.pack('<I', address) + bytes([0x0a, 0x08]))
    field = package([0x5b, 0x81], b'HPWR' + bytes([0x03]) + b'HPCF' + bytes([32]))
    if outside:
        # A write with no preceding read: the journal is built, and the applying transaction is
        # what refuses it, at its own read-before-write, before any byte is stored.
        body = bytes([0x70, 0x0a, 0x2a]) + b'HPCF'
    else:
        # Store (HPCF, Local0); Store (Local0, HPCF): a real read-modify-write whose value the
        # kernel never chose and whose readback verifies exactly.
        body = bytes([0x70]) + b'HPCF' + bytes([0x60]) + bytes([0x70, 0x60]) + b'HPCF'
    body = region + field + method(b'_PS3', body) + method(b'_PS0', body)
    body += bytes([0x08]) + b'_PSC' + bytes([0x00])
    body += bytes([0x08]) + b'_PRW' + package([0x12], bytes([2, 0x0a, 11, 0x0a, 3]))
    return table(package([0x10], b'\\_SB_' + device(b'PWR0', body)))


def boot(qemu, kernel, machine, devices=False, dstate=False, dstate_refused=False):
    name = f'power-s3-{machine}'
    log = ROOT / 'build' / f'{name}.log'
    qmp = ROOT / 'build' / f'{name}.sock'
    qmp.unlink(missing_ok=True)
    # Q35's SATA controller would be probed and enabled even without disks. The S3 fixture
    # deliberately supplies neither storage nor network controllers, so no DMA bus master can
    # write memory while the sleep word is stored.
    platform = 'q35,sata=off' if machine == 'q35' else machine
    command = [qemu, '-machine', platform, '-kernel', str(kernel), '-m', '512M',
               '-nographic', '-no-reboot', '-qmp', f'unix:{qmp},server=on,wait=off',
               '-net', 'none']
    disk = None
    if devices:
        # One real virtio-blk controller: its driver suspends before the sleep and must be
        # re-taken, verified and quiesced again after the wake.
        disk = ROOT / 'build' / f'{name}.img'
        subprocess.run(['python3', 'tools/make-test-disk.py', str(disk)],
                       cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
        command += ['-drive', f'file={disk},if=none,id=s3disk,format=raw',
                    '-device', 'virtio-blk-pci,drive=s3disk']
    if dstate or dstate_refused:
        # An injected SSDT declaring one device with `_PS0`/`_PS3`/`_PSC`/`_PRW`. QEMU's own
        # DSDT declares no device power methods, so without this the transition path would have
        # nothing to act on and the fixture would only prove the empty case.
        aml = ROOT / 'build' / f'{name}.aml'
        aml.write_bytes(dstate_fixture(outside=dstate_refused))
        command += ['-acpitable', f'file={aml}']
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    events = []
    pending = b''
    with log.open('w') as out:
        process = subprocess.Popen(command, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 60
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
            suspended_at = None
            woke_at = None
            requested = False
            resumed_at = None
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
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
                except (socket.timeout, ValueError):
                    pass
                if 'power: requesting ACPI S3' in text and not requested:
                    requested = True
                    assert any(e['event'] == 'SUSPEND' for e in events), events
                    suspended_at = time.monotonic()
                    connection.sendall(b'{"execute":"query-status"}\r\n')
                if requested and suspended_at and not woke_at:
                    # The machine is parked; ask the platform to wake it. QEMU requires
                    # RUN_STATE_SUSPENDED or the request is refused.
                    connection.sendall(b'{"execute":"system_wakeup"}\r\n')
                    woke_at = time.monotonic()
                if dstate_refused and ('power: S3 refused: 1 device power transition(s) '
                                       'refused' in text):
                    # The fixture never sleeps, so it halts in the ordinary boot loop; the
                    # refusal has been printed and recorded.
                    process.terminate()
                    break
                if 'power: resumed from S3 with tag' in text:
                    # The resumed kernel powers the machine off by itself; sending `quit` here
                    # would hide whether the post-resume PM path actually works.
                    if resumed_at is None:
                        resumed_at = time.monotonic()
                    if any(e['event'] == 'SHUTDOWN' for e in events):
                        break
                    assert time.monotonic() - resumed_at < 20, \
                        'the resumed kernel never powered the machine off' + text
                if process.poll() is not None:
                    break
                time.sleep(.05)
            code = process.wait(timeout=10)
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
            if disk is not None:
                disk.unlink(missing_ok=True)
    text = log.read_text(errors='replace')
    if dstate_refused:
        # The field is outside every authorized window: the journal is refused at the read that
        # precedes the write, and the sleep is refused with it, because a device firmware would
        # not power down can still be a bus master.
        assert 'power: requesting ACPI S3' not in text, text
        assert 'power: resumed from S3' not in text, text
        assert not any(e['event'] == 'SUSPEND' for e in events), events
        assert re.search(r'power: \\_SB_\.PWR0 _PS3 refused: Read\(Unsupported\(23424\)\)',
                         text), text
        assert 'power: device power transition _PS3: 0 transitioned, 1 refused' in text, text
        assert 'power: S3 refused: 1 device power transition(s) refused' in text, text
        assert 'NOLX: address space selftest:' in text, text
        print(f'PASS: {name}; unauthorized power-transition field refused before any access, '
              f'sleep refused, no SUSPEND event')
        return
    assert code == 0, (code, text)
    assert 'NOLX: address space selftest:' in text, text
    assert 'in use 0 -> 0 bytes, integrity ok' in text, text
    assert text.count('power: requesting ACPI S3') == 1, text
    assert 'power: S3 refused' not in text, text
    assert 'power: resumed from S3 with tag 0x534c33524553554d' in text, text
    resumed = text.split('power: resumed from S3', 1)[1]
    assert 'power: post-resume PM1a 0x604' in resumed, text
    assert 'ACPI mode re-entered' in resumed, text
    assert resumed.count('power: requesting ACPI S5') == 1, text
    if machine == 'q35':
        # The stale ECAM mapping must be rejected rather than read as "no device".
        assert 'pci: ECAM identity mismatch; legacy fallback retained' in resumed, text
    word = int(re.search(r'power: requesting ACPI S3 via PM1a 0x[0-9a-f]+, control (0x[0-9a-f]+)', text).group(1), 16)
    # SLP_TYP comes from the firmware's `_S3_` (1 here); SLP_EN must be set and the unrelated
    # SCI_EN bit preserved from the current control word.
    assert word & 0x3C00 == 0x2400, hex(word)
    assert word & 0x2000, hex(word)
    order = [e['event'] for e in events if e['event'] in {'SUSPEND', 'WAKEUP', 'SHUTDOWN'}]
    assert order == ['SUSPEND', 'WAKEUP', 'SHUTDOWN'], events
    post = re.search(r'power: post-resume PM1a 0x[0-9a-f]+ control (0x[0-9a-f]+)', resumed)
    assert post and int(post.group(1), 16) & 1, text
    if dstate:
        # Firmware's own device power transition, in both directions, across a real sleep.
        before = text.split('power: requesting ACPI S3', 1)[0]
        assert re.search(r'power: \\_SB_\.PWR0 _PS3 applied: 1 region field write\(s\) '
                         r'verified \(1 journaled\)', before), text
        assert ('power: device power transition _PS3: 1 transitioned, 0 refused'
                in before), text
        assert ('power: pre-suspend device power transition: 1 device(s) in firmware low power'
                in before), text
        assert re.search(r'power: \\_SB_\.PWR0 _PS0 applied: 1 region field write\(s\) '
                         r'verified', resumed), text
        assert ('power: post-resume device power transition: 1 device(s) returned to D0, '
                '0 refused' in resumed), text
        # `_PS0` must run before any driver is restarted: a driver must never programme a
        # controller firmware still considers asleep.
        assert resumed.index('device(s) returned to D0') < resumed.index('post-resume device restore:'), text
    if not dstate and not dstate_refused:
        # The absent case, on real firmware: neither SeaBIOS DSDT declares a device power method,
        # so the policy reports the empty plan in both directions rather than inventing a target.
        assert ('power: device power transition _PS3: no firmware device declares it'
                in text), text
        assert ('power: device power transition _PS0: no firmware device declares it'
                in resumed), text
    if devices:
        # The device tree is suspended before the sleep, re-taken after the wake, proven
        # usable by a real sector read, and quiesced again before the S5 write.
        before = text.split('power: requesting ACPI S3', 1)[0]
        assert re.search(r'power: suspend barrier handle=\d+ driver=virtio-blk durable=true', before), text
        assert re.search(r'power: pre-suspend device quiesce: 1 controller\(s\) stopped, admission retained', before), text
        assert re.search(r'power: virtio-blk handle=\d+ restored after S3 wake', resumed), text
        assert re.search(r'power: post-resume device restore: 1 restored, 0 not restored', resumed), text
        assert re.search(r'power: post-resume virtio-blk handle=\d+ read sector 0: '
                         r'signature=0x[0-9a-f]{2}0x[0-9a-f]{2} ok=true', resumed), text
        assert 'power: post-resume S5 refused' not in text, text
        assert re.search(r'power: post-resume shutdown quiesce: 1 controller\(s\) stopped, '
                         r'1 verified after wake', resumed), text
        print(f'PASS: {name}; QMP SUSPEND + WAKEUP, control {hex(word)}, resume tag verified, '
              f'PM re-entered at {post.group(1)}, 1 device suspended/restored/verified, powered off')
    else:
        print(f'PASS: {name}; QMP SUSPEND + WAKEUP, control {hex(word)}, resume tag verified, '
              f'PM re-entered at {post.group(1)} and powered off')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build/nolx.elf')
    parser.add_argument('--with-devices', action='store_true',
                        help='boot hal-power-s3-device-test: one virtio-blk suspended, restored '
                             'and verified across the wake')
    parser.add_argument('--dstate', action='store_true',
                        help='inject an SSDT declaring _PS0/_PS3/_PSC/_PRW and require the '
                             'firmware transition in both directions across the wake')
    parser.add_argument('--dstate-refused', action='store_true',
                        help='inject the same device with a field outside every authorized '
                             'window: the transition must be refused and the sleep with it')
    parser.add_argument('--machine', choices=['pc', 'q35'], action='append')
    args = parser.parse_args()
    qemu = os.environ.get('QEMU', 'qemu-system-x86_64')
    for machine in args.machine or ['pc', 'q35']:
        boot(qemu, args.kernel.resolve(), machine, devices=args.with_devices,
             dstate=args.dstate, dstate_refused=args.dstate_refused)


if __name__ == '__main__':
    main()
