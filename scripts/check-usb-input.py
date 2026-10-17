#!/usr/bin/env python3
"""Verify native polled xHCI boot-HID input, ring wrap and root-port DMA reclamation.

Default kernel; no test feature. Direct USB2 boot keyboard/mouse only, PC and Q35.
Every injected transition must reach the USB input queue; PS/2 output is not evidence.
"""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import time
ROOT = Path(__file__).resolve().parents[1]


def boot(qemu, kernel, machine):
    log = ROOT / 'build' / f'input-usb-{machine}.log'
    qmp = ROOT / 'build' / f'input-usb-{machine}.sock'
    qmp.unlink(missing_ok=True)
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot', '-net', 'none',
               '-device', 'qemu-xhci,id=xhci', '-device', 'usb-kbd,bus=xhci.0,id=kbd',
               '-device', 'usb-mouse,bus=xhci.0,id=mouse', '-qmp', f'unix:{qmp},server=on,wait=off']
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
            execute('device_del', {'id': 'mouse'})
            wait(lambda t: 'live pages=3' in t and 'input: usb mouse dx=0 dy=0 buttons=0' in t, 'mouse disconnect and host-only DMA baseline')
            execute('device_add', {'driver': 'usb-kbd', 'bus': 'xhci.0', 'id': 'kbd2'})
            wait(lambda t: t.count('usb: HID ready') == 3, 'keyboard reconnect')
            key(True, 302)
            key(False, 302)
            execute('device_del', {'id': 'kbd2'})
            text = wait(lambda t: t.count('live pages=3') == 2 and t.count('DMA released') == 3, 'reconnected keyboard reclaimed to same baseline')
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
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    for machine in ['pc', 'q35']:
        boot(args.qemu, args.kernel.resolve(), machine)


if __name__ == '__main__':
    main()
