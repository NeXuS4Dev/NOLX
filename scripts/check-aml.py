#!/usr/bin/env python3
"""Execute real firmware AML plus injected SSDT methods on PC/Q35; refuse failures.

Build a default kernel and a hal-aml-test kernel. Test resources describe a catalog-only
synthetic device: they are never permission to access/map those physical addresses.
"""
import argparse
import os
from pathlib import Path
import re
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
                                   b'NOLX  ', b'AMLTEST ', 1, b'NLXC', 1))
    header[9] = (-sum(header + aml)) & 255
    return header + aml


def fixture():
    resources = (bytes([0x4b, 0xf8, 3, 8, 0x86, 9, 0, 1]) + struct.pack('<II', 0xfedc0000, 4096)
                 + bytes([0x22, 16, 0, 0x79, 0]))
    buffer = package([0x11], bytes([0x0a, len(resources)]) + resources)
    body = method(b'NLXM', bytes([0xa4, 0x7d, 0x68, 1, 0]), 1)
    body += device(b'NLX0', bytes([8]) + b'RBUF' + buffer
                   + method(b'_STA', bytes([0xa4]) + b'NLXM' + bytes([0x0a, 14]))
                   + method(b'_CRS', bytes([0xa4]) + b'RBUF'))
    body += device(b'NLX1', method(b'_STA', bytes([0xa4, 0])) + method(b'_CRS', bytes([0xff])))
    body += device(b'NLX2', method(b'_STA', bytes([0xa4, 0x5b, 0x23])))
    body += device(b'NLX3', method(b'_STA', bytes([0xa4, 0x0a, 15]))
                   + method(b'_CRS', bytes([0xa4]) + package([0x11], bytes([1, 0x79]))))
    body += device(b'NLX4', method(b'_STA', package([0xa2], bytes([1, 0xa3]))))
    body += device(b'NLX5', method(b'_STA', bytes([0xa4]) + b'_STA'))
    return table(package([0x10], b'\\_SB_' + body))


def dstate_fixture():
    """One device node that declares every device power method.

    `_PS3` and `_PS0` read the HPET general-configuration register -- inside the window the
    kernel authorizes from the parsed HPET table -- and store the value back. The store is a real
    memory-mapped write, its readback verifies exactly because the value is the one the register
    already holds, and the value is firmware's choice, which is exactly the case an exact-value
    permission cannot serve. `_PSC` reports D0 and `_PRW` names GPE 11 waking from S3.
    """
    region = (bytes([0x5b, 0x80]) + b'HPWR' + bytes([0x00, 0x0c])
              + struct.pack('<I', 0xfed00010) + bytes([0x0a, 0x08]))
    # Field (HPWR, DWordAcc, NoLock, Preserve) { HPCF, 32 }
    field = package([0x5b, 0x81], b'HPWR' + bytes([0x03]) + b'HPCF' + bytes([32]))
    # Store (HPCF, Local0); Store (Local0, HPCF)
    body = bytes([0x70]) + b'HPCF' + bytes([0x60]) + bytes([0x70, 0x60]) + b'HPCF'
    body = region + field + method(b'_PS3', body) + method(b'_PS0', body)
    body += bytes([0x08]) + b'_PSC' + bytes([0x00])
    body += bytes([0x08]) + b'_PRW' + package([0x12], bytes([2, 0x0a, 11, 0x0a, 3]))
    return table(package([0x10], b'\\_SB_' + device(b'PWR0', body)))


def boot(qemu, kernel, machine, mode):
    name = f'aml-{machine}-{mode}'
    log = ROOT / 'build' / f'{name}.log'
    platform = 'q35,sata=off' if machine == 'q35' else 'pc'
    if mode == 'no-acpi':
        platform += ',acpi=off'
    elif mode == 'unmapped-tables':
        platform += ',max-ram-below-4g=3G'
    command = [qemu, '-machine', platform, '-kernel', str(kernel), '-m', '3072M' if mode == 'unmapped-tables' else '512M',
               '-nographic', '-no-reboot', '-net', 'none']
    if mode in ['methods', 'dstate', 'bad-table', 'bad-segment', 'multiple-roots', 'catalog-full']:
        aml = ROOT / 'build' / f'{name}.aml'
        if mode == 'methods':
            payload = fixture()
        elif mode == 'dstate':
            payload = dstate_fixture()
        elif mode == 'bad-table':
            # A late unsupported opcode must roll back the earlier device.
            payload = table(package([0x10], b'\\_SB_' + device(b'TMP0', method(b'_STA', bytes([0xa4, 1])))) + bytes([0xfe]))
        elif mode == 'bad-segment':
            # A missing name INSIDE an existing method is not an absent _SEG default.
            payload = table(package([0x10], b'\\' + bytes([0x2e]) + b'_SB_PCI0' + method(b'_SEG', bytes([0xa4]) + b'MISS')))
        elif mode == 'catalog-full':
            payload = table(package([0x10], b'\\_SB_' + b''.join(device(f'D{i:03X}'.encode(),b'') for i in range(128))))
        else:
            payload = table(package([0x10], b'\\_SB_' + device(b'PCI1', bytes([8]) + b'_HID' + bytes([0x0c,0x41,0xd0,0x0a,3]))))
        aml.write_bytes(payload)
        command += ['-acpitable', f'file={aml}']
    with log.open('w') as output:
        process = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                # The dstate fixture's own marker is the later one: the transition runs from the
                # main loop after the memory selftest has already printed.
                done = ('power: dstate selftest:' in text if mode == 'dstate'
                        else 'NOLX: address space selftest:' in text)
                if done or re.search(r'PANIC|BUGCHECK|CPU exception', text) or process.poll() is not None:
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
    assert not re.search(r'PANIC|BUGCHECK|CPU exception', text), text
    assert 'NOLX: address space selftest:' in text, f'{log}\n{text}'
    assert 'NOLX: timer reached 100 ticks' in text, text
    assert 'in use 0 -> 0 bytes, integrity ok' in text, text
    if mode == 'no-acpi':
        assert 'aml: no ACPI platform; namespace unavailable' in text, text
        assert 'pci: resource policy=no-ACPI bounded board fallback' in text, text
    elif mode == 'unmapped-tables':
        assert 'aml: ACPI advertised but platform tables unavailable; allocation refused' in text, text
        assert 'pci: native resource profile unavailable; firmware assignments retained' in text, text
        assert 'pci: resource policy=' not in text, text
    else:
        assert 'aml: DSDT loaded atomically' in text, text
        # These are the machine's real SeaBIOS methods, not the injected SSDT.
        assert re.search(r'aml: \\_SB_\.PCI0\..*KBD_ _STA=0xf, defaulted=false', text), text
        assert re.search(r'aml: \\_SB_\.PCI0\..*KBD_ _CRS evaluated and decoded: 3 resources', text), text
        assert re.search(r'aml: \\_SB_\.PCI0\..*COM1 _STA=0xf, defaulted=false', text), text
        assert re.search(r'aml: \\_SB_\.PCI0\..*COM1 _CRS evaluated and decoded: 2 resources', text), text
        if mode == 'methods':
            assert 'aml: firmware method selftest: computed status, decoded resources, disabled-device gating, unsupported/loop/recursion refusal verified' in text, text
            # The applying half of the operation-region write path, on real hardware: an
            # authorized window is written with a permitted value and read back, an
            # unpermitted value is refused before any byte is written, a field outside every
            # window is refused, and an embedded controller with no authorized ports is
            # refused rather than guessed.
            assert re.search(
                r'aml: region-write selftest: authorized port write applied and read back '
                r'\(0x[0-9a-f]{8}\), unpermitted value denied without a write, '
                r'outside-window write refused, unauthorized embedded controller refused', text), text
            assert 'NLX2 _STA refused: Unsupported' in text, text
            assert 'NLX3 _CRS resource refusal: Truncated' in text, text
            assert 'NLX4 _STA refused: Steps' in text, text
            assert 'NLX5 _STA refused: Limit' in text, text
            assert 'NLX1 _CRS skipped: device status is not usable' in text, text
        else:
            assert 'NLX0' not in text, text
        if mode == 'dstate':
            # The catalog is recorded statically, then the fixture's own methods are evaluated
            # and their journal is applied with a verified readback in both directions.
            assert ('aml: \\_SB_.PWR0 power: _PS0=1 _PS3=1 _PSC=1 _PRW=1 current=D0 '
                    'wake=gpe11@S3') in text, text
            assert 'aml: device power targets: 1 recorded, 0 refused' in text, text
            assert re.search(r'power: \\_SB_\.PWR0 _PS3 applied: 1 region field write\(s\) '
                             r'verified \(1 journaled\)', text), text
            assert re.search(r'power: \\_SB_\.PWR0 _PS0 applied: 1 region field write\(s\) '
                             r'verified', text), text
            assert 'power: device power transition _PS3: 1 transitioned, 0 refused' in text, text
            assert 'power: device power transition _PS0: 1 transitioned, 0 refused' in text, text
            assert 'power: dstate selftest: 1 firmware power target(s), _PSC=D0, _PRW=11@S3' in text, text
        if mode == 'bad-table':
            assert 'aml: SSDT refused atomically: Unsupported(254)' in text, text
            assert 'TMP0 _STA=' not in text, text
        if mode in ['bad-table','bad-segment','multiple-roots','catalog-full']:
            assert 'aml: PCI root resource authorization refused:' in text, text
            assert 'pci: native resource profile unavailable; firmware assignments retained' in text, text
            assert 'pci: resource policy=' not in text, text
            if mode == 'bad-segment':
                assert 'aml: SSDT loaded atomically' in text, text
                assert 'PCI root resource authorization refused: Aml(Missing)' in text, text
            if mode == 'multiple-roots':
                assert 'PCI root resource authorization refused: Topology' in text, text
        else:
            assert 'aml: evaluated ' in text and 'PCI root bridge' in text and ' authorized' in text, text
            assert 'pci: resource policy=AML root _CRS;' in text, text
    print(f'PASS: {name}: firmware AML evaluated, status/resource refusals preserved, no heap leak', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build' / 'nolx.elf')
    parser.add_argument('--test-kernel', type=Path, default=ROOT / 'build' / 'nolx-aml.elf')
    parser.add_argument('--power-kernel', type=Path,
                        default=ROOT / 'build' / 'nolx-dstate.elf')
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    for machine in ['pc', 'q35']:
        boot(args.qemu, args.test_kernel.resolve(), machine, 'methods')
        boot(args.qemu, args.power_kernel.resolve(), machine, 'dstate')
        boot(args.qemu, args.kernel.resolve(), machine, 'absent')
        for mode in ['bad-table','bad-segment','multiple-roots']:
            boot(args.qemu, args.kernel.resolve(), machine, mode)
    boot(args.qemu, args.kernel.resolve(), 'pc', 'catalog-full')
    boot(args.qemu, args.kernel.resolve(), 'pc', 'unmapped-tables')
    boot(args.qemu, args.kernel.resolve(), 'pc', 'no-acpi')


if __name__ == '__main__':
    main()
