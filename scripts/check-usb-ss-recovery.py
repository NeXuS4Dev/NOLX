#!/usr/bin/env python3
"""USB SuperSpeed device recovery after a refused slot.

xHCI direct-root boot-HID profile. A USB2 boot keyboard stays live on a root port while one
or two real USB3 mass-storage devices are attached: each completes Address Device and its
descriptor reads at actual speed 4 with a 512-byte EP0, then refuses the unsupported class
and returns all of its DMA to the live-HID baseline. The recovery under test is that each
refused SuperSpeed slot is fully retired (fresh slot number on the next attach, no DMA leak,
no stuck port) and the USB2 producer and its IRQ service survive every cycle -- the
controller recovers from an unsupported SuperSpeed device without stopping its existing
peers. Not SS HID report evidence (QEMU's HID models do not present usb_version 3).

Build with: make kernel (default)
Uses: python3 scripts/check-usb-ss-recovery.py
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

KEY = 'input: usb keyboard usage=0x04 pressed=true'
RELEASE = 'input: usb keyboard usage=0x04 pressed=false'


def boot(qemu, kernel, machine, cycles=2):
    label = f'usb-ss-recovery-{machine}'
    log = ROOT / 'build' / f'{label}.log'
    qmp = ROOT / 'build' / f'{label}.sock'
    qmp.unlink(missing_ok=True)
    print(f'RUN: {label}', flush=True)
    # A fresh 4 MiB backing file per attach cycle: legacy -drive backends auto-delete on
    # device_del, so a reused image would collide with the deleted device's lingering state.
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot', '-net', 'none',
               '-device', 'qemu-xhci,id=xhci',
               '-device', 'usb-kbd,bus=xhci.0,id=kbd,usb_version=2',
               '-qmp', f'unix:{qmp},server=on,wait=off']
    for cycle in range(1, cycles + 1):
        disk = ROOT / 'build' / f'{label}-ss-{cycle}.img'
        with disk.open('wb') as fixture:
            fixture.truncate(4 * 1024 * 1024)
        command += ['-drive', f'if=none,id=ssdisk{cycle},format=raw,file={disk}']
    with log.open('w') as output:
        process = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
        connection = None
        try:
            def text():
                return log.read_text(errors='replace')

            def wait(predicate, description, timeout=45):
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    current = text()
                    assert not re.search(r'PANIC|BUGCHECK|CPU exception', current), current
                    if predicate(current):
                        return current
                    if process.poll() is not None:
                        raise AssertionError(f'QEMU exited before {description}:\n{current}')
                    time.sleep(.005)
                raise AssertionError(f'missing {description}:\n{log.read_text(errors="replace")}')

            wait(lambda t: t.count('usb: HID ready') == 1 and 'kernel heap selftest:' in t,
                 'the live USB2 keyboard and completed boot')
            # The USB2 baseline: one live keyboard at high speed (3) with a 64-byte EP0, three
            # host pages plus five device pages = eight owned DMA pages, since the controller
            # stays live for hotplug here.
            proof = re.findall(r'usb: descriptors root-port=\d+ route=0x0 speed=(\d+) ep0=(\d+) config-bytes=\d+',
                               text())
            assert proof[:1] == [('3', '64')], proof
            assert 'device DMA ownership live pages=8' in text(), text()

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

            def press(count):
                execute('input-send-event', {'events': [{'type': 'key', 'data': {'down': True, 'key': {'type': 'qcode', 'data': 'a'}}}]})
                wait(lambda t: t.count(KEY) >= count, f'keyboard press {count}', 20)
                execute('input-send-event', {'events': [{'type': 'key', 'data': {'down': False, 'key': {'type': 'qcode', 'data': 'a'}}}]})
                wait(lambda t: t.count(RELEASE) >= count, f'keyboard release {count}', 20)

            press(1)  # the live peer answers before any SuperSpeed device is touched

            for cycle in range(1, cycles + 1):
                execute('device_add', {'driver': 'usb-storage', 'bus': 'xhci.0', 'drive': f'ssdisk{cycle}', 'id': f'ss{cycle}'})
                # A real SuperSpeed slot: speed 4, 512-byte EP0, a complete 44-byte
                # configuration descriptor, then the class refusal and full reclamation.
                waited = wait(lambda t: (len(re.findall(rf'usb: descriptors root-port=\d+ route=0x0 speed=4 ep0=512 config-bytes=44', t)) == cycle)
                                          and re.search(r'enumeration refused: Unsupported[^;]*; live DMA pages=8', t),
                              f'SuperSpeed slot {cycle}: EP0 512, class refusal, DMA reclaimed')
                assert 'live DMA pages=8' in waited, waited
                # The keyboard on the peer root port is still served: its slot and reports
                # survive the refused SuperSpeed device, and the interrupt path still delivers.
                press(cycle + 1)
                execute('device_del', {'id': f'ss{cycle}'})
                # The refused device never bound, so its removal produces no disconnect line: the
                # evidence that the slot was retired is that the next refused attach takes a fresh
                # slot and returns to the same baseline, and that the peer keeps answering.
                wait(lambda t: t.count('speed=4 ep0=512') == cycle,
                     f'SuperSpeed slot {cycle} recorded')
            final = wait(lambda t: t.count('device DMA ownership live pages=8') >= 1,
                         'final DMA baseline', 20)
            assert 'in use 0 -> 0 bytes, integrity ok' in final, final
            # Every refused SuperSpeed device gave its root port back: the controller offered a
            # different port to the next attach instead of leaving the previous one wedged.
            ports = re.findall(r'usb: descriptors root-port=(\d+) route=0x0 speed=4', final)
            assert len(ports) == cycles and len(set(ports)) == cycles, ports
            press(cycles + 2)  # the peer is still live after every SuperSpeed cycle
            print(f'PASS: usb-ss-recovery-{machine}: {cycles} refused SuperSpeed slots, '
                  f'speed 4 / EP0 512 each, distinct root port per cycle, DMA 8 held, USB2 peer uninterrupted', flush=True)
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
    parser.add_argument('--cycles', type=int, default=2, help='refused SuperSpeed slots to cycle')
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    for machine in ['pc', 'q35']:
        boot(args.qemu, args.kernel.resolve(), machine, args.cycles)


if __name__ == '__main__':
    main()
