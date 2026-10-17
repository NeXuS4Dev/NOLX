#!/usr/bin/env python3
"""Verify native xHCI boot-HID input, ring wrap and root-port DMA reclamation.

Default kernel by default; --objects additionally requires hal-input-object-test.
Direct USB2 boot keyboard/mouse only, PC and Q35. Every transition must reach the
USB object's console subscription; PS/2 output is not evidence.
"""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import time
from xhci_irq_checks import verify_interrupts
ROOT = Path(__file__).resolve().parents[1]


def boot(qemu, kernel, machine, objects=False, interrupts=False, transport="default", no_apic=False):
    label = f'input-usb-{machine}' + ('-objects' if objects else '')
    label += f'-{transport}' + ('-no-apic' if no_apic else '')
    log = ROOT / 'build' / f'{label}.log'
    qmp = ROOT / 'build' / f'{label}.sock'
    qmp.unlink(missing_ok=True)
    print(f'RUN: {label}', flush=True)
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot', '-net', 'none',
               '-device', 'qemu-xhci,id=xhci' + {'default': '', 'msi': ',msi=on,msix=off', 'polled': ',msi=off,msix=off'}[transport], '-device', 'usb-kbd,bus=xhci.0,id=kbd',
               '-device', 'usb-mouse,bus=xhci.0,id=mouse', '-qmp', f'unix:{qmp},server=on,wait=off']
    if no_apic:
        command += ['-cpu', 'qemu64,-apic']
    with log.open('w') as output:
        process = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
        connection = None
        try:
            def wait(predicate, description, timeout=45):
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    text = log.read_text(errors='replace')
                    assert not re.search(r'PANIC|BUGCHECK|CPU exception', text), text
                    if predicate(text):
                        return text
                    if process.poll() is not None:
                        raise AssertionError(f'QEMU exited before {description}:\n{text}')
                    time.sleep(.005)
                raise AssertionError(f'missing {description}:\n{log.read_text(errors="replace")}')

            wait(lambda t: t.count('usb: HID ready') == 2 and 'kernel heap selftest:' in t, 'two HID devices and completed boot')
            if objects:
                wait(lambda t: 'input: object hardware observers armed' in t, 'independent kernel input readers')
            connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            connection.settimeout(5)
            connection.connect(str(qmp))
            stream = connection.makefile('rwb')
            assert 'QMP' in json.loads(stream.readline())

            def execute(name, arguments=None):
                stream.write((json.dumps({'execute': name, 'arguments': arguments or {}}) + '\n').encode())
                stream.flush()
                while True:
                    response = json.loads(stream.readline())
                    assert 'error' not in response, response
                    if 'return' in response:
                        return response['return']
            execute('qmp_capabilities')

            def key(down, count):
                execute('input-send-event', {'events': [{'type': 'key', 'data': {'down': down, 'key': {'type': 'qcode', 'data': 'a'}}}]})
                marker = f'input: usb keyboard usage=0x04 pressed={str(down).lower()}'
                wait(lambda t: t.count(marker) == count, marker, 5)

            # >255 report completions per keyboard ring, >256 events per event ring, two wraps.
            for i in range(1, 301):
                key(True, i)
                key(False, i)
            execute('input-send-event', {'events': [
                {'type': 'rel', 'data': {'axis': 'x', 'value': 7}},
                {'type': 'rel', 'data': {'axis': 'y', 'value': -3}},
                {'type': 'btn', 'data': {'down': True, 'button': 'left'}}]})
            wait(lambda t: 'input: usb mouse dx=7 dy=-3 buttons=1' in t, 'mouse packet')
            # Non-boot tablet must refuse enumeration and return all five slot pages.
            execute('device_add', {'driver': 'usb-tablet', 'bus': 'xhci.0', 'id': 'tablet'})
            wait(lambda t: 'enumeration refused: Unsupported; live DMA pages=13' in t, 'unsupported device refusal and reclamation')
            execute('device_del', {'id': 'tablet'})
            # Remove a held keyboard key and mouse button: neutral events must be synthesized.
            key(True, 301)
            execute('device_del', {'id': 'kbd'})
            wait(lambda t: t.count('input: usb keyboard usage=0x04 pressed=false') == 301 and 'live pages=8' in t, 'keyboard disconnect release and five pages reclaimed')
            neutral_before = log.read_text(errors='replace').count('input: usb mouse dx=0 dy=0 buttons=0')
            execute('device_del', {'id': 'mouse'})
            wait(lambda t: 'live pages=3' in t and t.count('input: usb mouse dx=0 dy=0 buttons=0') > neutral_before, 'mouse disconnect and host-only DMA baseline')
            execute('device_add', {'driver': 'usb-kbd', 'bus': 'xhci.0', 'id': 'kbd2'})
            wait(lambda t: t.count('usb: HID ready') == 3, 'keyboard reconnect')
            key(True, 302)
            key(False, 302)
            execute('device_del', {'id': 'kbd2'})
            text = wait(lambda t: t.count('live pages=3') == 2 and t.count('DMA released') == 3, 'reconnected keyboard reclaimed to same baseline')
            if objects:
                text = wait(lambda t: t.count('input: object independent reader drained neutral state and EOF') == 3
                            and 'input: object reconnect generation advanced; stale producer/reader/open refused' in t,
                            'independent reader terminal states and stale object refusal')
                assert 'input: object independent reader verified 600 ordered hardware events' in text, text
                assert 'input: object independent reader overflow repaired; console unaffected, stale reader refused' in text, text
                ids = re.findall(r'input: usb keyboard usage=0x04 pressed=true device=(\d+):(\d+)', text)
                assert len(ids) == 302 and len(set(ids)) == 2 and ids[0][0] == ids[-1][0] and int(ids[-1][1]) > int(ids[0][1]), ids
                print(f'PASS: input-objects-{machine}: independent fast/slow readers, explicit overflow resync, neutral EOF, generation reuse and stale producer/reader/open refusal', flush=True)
            if interrupts:
                arms, batches = verify_interrupts(text, reports=604, transport='MSI' if transport == 'msi' else 'MSI-X')
                print(f'PASS: xhci-irq-{machine}-{transport}: {arms} live route(s), {batches} IRQ-admitted HID batches, 600+ hardware interrupts, no timer-poll fallback', flush=True)
            if transport == 'polled' or no_apic:
                assert 'polled fallback' in text and 'xhci: handle=0 MSI' not in text, text
                assert 'xhci: irq work' not in text, text
            assert 'in use 0 -> 0 bytes, integrity ok' in text, text
            assert text.count('DMA released') == 3, text
            print(f'PASS: usb-{machine}: 604 key transitions, mouse motion/buttons, two ring wraps, unsupported HID refusal, disconnect/reconnect; DMA baseline 13 -> 8 -> 3 -> 8 -> 3', flush=True)
        finally:
            if connection is not None:
                connection.close()
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            qmp.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build' / 'nolx.elf')
    parser.add_argument('--objects', action='store_true', help='require hal-input-object-test; verify extra independent consumers and stale handles')
    parser.add_argument('--interrupts', action='store_true', help='require hal-xhci-irq-test and real MSI/MSI-X IRQ-admitted HID work')
    parser.add_argument('--transport', choices=('default', 'msi', 'polled'), default='default')
    parser.add_argument('--no-apic', action='store_true', help='verify explicitly labelled polling on a PIC-only CPU')
    args = parser.parse_args()
    if args.interrupts and (args.transport == 'polled' or args.no_apic):
        parser.error('--interrupts cannot claim the polled/PIC-only profiles')
    (ROOT / 'build').mkdir(exist_ok=True)
    for machine in ['pc', 'q35']:
        boot(args.qemu, args.kernel.resolve(), machine, args.objects, args.interrupts, args.transport, args.no_apic)


if __name__ == '__main__':
    main()
