#!/usr/bin/env python3
"""Real input across two xHCI instances and three framework restarts (PC and Q35).

Requires hal-xhci-lifecycle-test. Keyboard and mouse deliberately use different PCI
controllers; a fresh peer mouse report gates every restart, not a boot-only marker.
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


def boot(qemu, kernel, machine, mode):
    label = f'input-xhci-lifecycle-{machine}-{mode}'
    log = ROOT / 'build' / f'{label}.log'
    qmp = ROOT / 'build' / f'{label}.sock'
    qmp.unlink(missing_ok=True)
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot', '-net', 'none',
               '-device', 'qemu-xhci,id=keys', '-device', 'usb-kbd,bus=keys.0,id=kbd',
               '-device', 'qemu-xhci,id=pointer', '-device', 'usb-mouse,bus=pointer.0,id=mouse',
               '-qmp', f'unix:{qmp},server=on,wait=off']
    with log.open('w') as output:
        process = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
        connection = None
        try:
            def wait(marker, timeout=45):
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    text = log.read_text(errors='replace')
                    assert not re.search(r'PANIC|BUGCHECK|CPU exception|panicked at', text), text
                    if marker in text:
                        return text
                    assert process.poll() is None, text
                    time.sleep(.005)
                raise AssertionError(f'missing {marker}:\n{log.read_text(errors="replace")}')
            wait('xhci-test: start fault armed peer=' if mode == 'start-fault' else 'xhci-test: armed cycle=0')
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

            def key(name, down):
                execute('input-send-event', {'events': [{'type': 'key', 'data': {
                    'down': down, 'key': {'type': 'qcode', 'data': name}}}]})

            if mode == 'start-fault':
                execute('input-send-event', {'events': [
                    {'type': 'rel', 'data': {'axis': 'x', 'value': 7}},
                    {'type': 'btn', 'data': {'down': True, 'button': 'left'}}]})
                text = wait('failed record safely removed, DMA 0')
                assert 'injected post-run start failure' in text, text
                assert 'failed-start rollback reclaimed MMIO/DMA' in text, text
                print(f'PASS: xhci-start-fault-{machine}: post-run exposed DMA/MMIO rollback, live peer input and safe failed-record removal, DMA 0', flush=True)
                return
            for cycle, name in enumerate('abcd'):
                wait(f'xhci-test: armed cycle={cycle}')
                key(name, True)
                wait(f'xhci-test: stopped cycle={cycle}')
                key(name, False)
                execute('input-send-event', {'events': [
                    {'type': 'rel', 'data': {'axis': 'x', 'value': 7 + cycle}},
                    {'type': 'btn', 'data': {'down': True, 'button': 'left'}}]})
            if mode == 'stop-fault':
                text = wait('neutral EOF, polling/start/removal refused')
                assert 'retains 8 DMA pages and MMIO, bus master active' in text, text
                print(f'PASS: xhci-stop-fault-{machine}: unproven quiescence pins eight DMA pages and MMIO; neutral EOF, stale polling/start/removal refused', flush=True)
                return
            text = wait('final removal; DMA 16 -> 0')
            assert text.count('xhci-test: stopped cycle=') == 4, text
            assert text.count('restart DMA/MMIO baselines restored; peer identity preserved') == 3, text
            assert 'DMA 16 -> 0' in text, text
            text = wait('input: usb mouse dx=0 dy=0 buttons=0')
            assert 'in use 0 -> 0 bytes, integrity ok' in text, text
            print(f'PASS: xhci-lifecycle-{machine}: two PCI controllers, three restarts, fresh input and live peer, neutral EOF, stale input/MMIO refusal, DMA 16 -> 8 -> 16 (three times) -> 0, final removal', flush=True)
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
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build' / 'nolx.elf')
    parser.add_argument('--mode', choices=('lifecycle', 'start-fault', 'stop-fault'), default='lifecycle')
    args = parser.parse_args()
    for machine in ('pc', 'q35'):
        boot(os.environ.get('QEMU', 'qemu-system-x86_64'), args.kernel.resolve(), machine, args.mode)


if __name__ == '__main__':
    main()
