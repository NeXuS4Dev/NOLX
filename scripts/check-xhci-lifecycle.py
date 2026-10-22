#!/usr/bin/env python3
"""Real input across two xHCI instances and three framework restarts (PC and Q35).

Each mode requires its hal-xhci-<mode>-test image (default mode: lifecycle).
Keyboard and mouse deliberately use different PCI controllers; fresh peer input
gates restart/recovery, not a boot-only marker.
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


def exhaust_boot(qemu, kernel, machine):
    """Boot-time vector-exhaustion fixture: no QMP input, no second controller.

    Requires hal-xhci-exhaust-test. The fixture stops the controller QEMU enumerated, takes
    every candidate of its vector pool for a synthetic owner, and requires the start that has
    no vector to be refused as NoVector without leaking DMA, MMIO or a held vector.
    """
    label = f'input-xhci-lifecycle-{machine}-exhaust'
    log = ROOT / 'build' / f'{label}.log'
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot',
               '-net', 'none', '-device', 'qemu-xhci,id=keys',
               '-device', 'usb-kbd,bus=keys.0,id=kbd']
    with log.open('w') as output:
        process = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                assert not re.search(r'PANIC|BUGCHECK|CPU exception|panicked at', text), text
                if 'xhci-exhaust: PASS' in text and 'final DMA 0' in text:
                    break
                assert process.poll() is None, text
                time.sleep(.05)
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    text = log.read_text(errors='replace')
    assert 'xhci-exhaust: PASS' in text, f'{log}\n{text}'
    assert not re.search(r'PANIC|BUGCHECK|CPU exception|panicked at', text), text
    match = re.search(
        r'xhci-exhaust: synthetic handle took (\d+) vector\(s\), pool exhausted', text)
    assert match and 0 < int(match.group(1)) <= 256, text
    assert 'xhci-exhaust: start refused as NoVector with no DMA/MMIO/ownership leak' in text, text
    assert 'xhci-exhaust: released pool reacquired by the controller, DMA/MMIO restored' in text, text
    assert 'xhci-exhaust: PASS' in text and 'final DMA 0' in text, text
    print(f'PASS: xhci-exhaust-{machine}: a {match.group(1)}-vector pool was exhausted and '
          'refused start as NoVector with no DMA/MMIO/ownership leak, reclaimed and removed',
          flush=True)


def boot(qemu, kernel, machine, mode, interrupts=False):
    if mode == 'exhaust':
        exhaust_boot(qemu, kernel, machine)
        return
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
            initial = {'transfer-fault': 'xhci-transfer: armed held keyboard', 'start-fault': 'xhci-test: start fault armed peer=', 'irq-start-fault': 'xhci-test: start fault armed peer=', 'recovery': 'xhci-recovery: armed startup quarantine'}
            wait(initial.get(mode, 'xhci-test: armed cycle=0'))
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

            if mode == 'transfer-fault':
                def mouse(dx):
                    execute('input-send-event', {'events': [
                        {'type': 'rel', 'data': {'axis': 'x', 'value': dx}},
                        {'type': 'btn', 'data': {'down': True, 'button': 'left'}}]})
                key('a', True)
                wait('repeated cleanup refusal retains 17 pages; require peer dx=7')
                key('a', False)
                mouse(7)
                wait('slot repair completed, DMA/MMIO baselines and peer identity preserved; require c')
                key('c', True)
                wait('armed lost disconnect cleanup proof; remove keyboard')
                execute('device_del', {'id': 'kbd'})
                wait('lost Disable Slot proof pins 16 pages; command reuse/repair refused, neutral EOF; require peer dx=8')
                key('c', False)
                mouse(8)
                wait('controller reset cleared poisoned ring, DMA 16 -> 8 -> 11; reconnect keyboard')
                execute('device_add', {'driver': 'usb-kbd', 'bus': 'keys.0', 'id': 'kbd2'})
                wait('reconnected fresh generation; require d')
                key('d', True)
                wait('automatic successful slot cleanup returned to 11 pages; require peer dx=9')
                key('d', False)
                mouse(9)
                wait('faulted empty port explicitly repaired without controller reset; require e')
                key('e', True)
                text = wait('PASS actual stalled transfer, slot repair, lost command proof, live peer, fresh input, stale identities and final DMA 0')
                assert text.count('outcome=Failed(6)') == 4, 'two actual STALLs, exactly two refused cleanup attempts and two successful cleanups required'
                assert 'injected lost Disable Slot proof' in text, text
                assert 'actual STALL code=6 and Halted endpoint' in text, text
                if interrupts:
                    verify_interrupts(text, reports=7, controllers=2, arms=3)
                print(f'PASS: xhci-transfer-fault-{machine}: actual IRQ-delivered STALL/Halted endpoint, pinned slot cleanup/refusal, explicit repair and fresh input, lost Disable Slot proof poisons command reuse, checked controller reset, live peer, stale input and final DMA 0', flush=True)
                return
            if mode == 'recovery':
                def mouse(dx):
                    execute('input-send-event', {'events': [
                        {'type': 'rel', 'data': {'axis': 'x', 'value': dx}},
                        {'type': 'btn', 'data': {'down': True, 'button': 'left'}}]})
                mouse(7)
                wait('xhci-recovery: armed first restarted keyboard')
                key('a', True)
                wait('xhci-recovery: armed stop quarantine')
                key('a', False)
                mouse(8)
                wait('xhci-recovery: armed second restarted keyboard')
                key('b', True)
                text = wait('removal and shutdown freeze; DMA 0')
                assert 'injected quarantined post-run failure' in text, text
                assert 'failed-start quarantine retains MMIO/DMA' in text, text
                assert 'repeated failed cleanup retains 11 DMA pages/MMIO' in text, text
                assert 'repeated cleanup failure retain 16 DMA pages/MMIO' in text, text
                if interrupts:
                    verify_interrupts(text, reports=4, controllers=2, arms=3)
                print(f'PASS: xhci-recovery-{machine}: failed start/stop, repeated cleanup refusal pins DMA/MMIO, checked recovery and fresh hardware input, live peer, neutral EOF/stale identities, final DMA 0 and frozen-shutdown recovery refusal', flush=True)
                return
            if mode in ('start-fault', 'irq-start-fault'):
                execute('input-send-event', {'events': [
                    {'type': 'rel', 'data': {'axis': 'x', 'value': 7}},
                    {'type': 'btn', 'data': {'down': True, 'button': 'left'}}]})
                text = wait('failed record safely removed, DMA 0')
                assert 'injected post-run start failure' in text, text
                assert 'failed-start rollback reclaimed MMIO/DMA' in text, text
                if mode == 'irq-start-fault':
                    assert 'injected post-run start failure (after armed MSI-X)' in text, text
                if interrupts:
                    verify_interrupts(
                        text,
                        controllers=2 if mode == 'irq-start-fault' else 1,
                        arms=2 if mode == 'irq-start-fault' else 1,
                        unique_vectors=mode != 'irq-start-fault',
                    )
                print(f'PASS: xhci-{mode}-{machine}: post-run exposed DMA/MMIO rollback, live peer input and safe failed-record removal, DMA 0', flush=True)
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
                if interrupts:
                    verify_interrupts(text, reports=8, controllers=2, arms=5)
                print(f'PASS: xhci-stop-fault-{machine}: unproven quiescence pins eight DMA pages and MMIO; neutral EOF, stale polling/start/removal refused', flush=True)
                return
            text = wait('final removal; DMA 16 -> 0')
            assert text.count('xhci-test: stopped cycle=') == 4, text
            assert text.count('restart DMA/MMIO baselines restored; peer identity preserved') == 3, text
            assert 'DMA 16 -> 0' in text, text
            text = wait('input: usb mouse dx=0 dy=0 buttons=0')
            assert 'in use 0 -> 0 bytes, integrity ok' in text, text
            if interrupts:
                verify_interrupts(text, reports=8, controllers=2, arms=5)
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
    parser.add_argument('--mode', choices=('lifecycle', 'start-fault', 'stop-fault', 'recovery', 'irq-start-fault', 'transfer-fault', 'exhaust'), default='lifecycle')
    parser.add_argument('--interrupts', action='store_true', help='also require hal-xhci-irq-test; prove independent IRQ vectors and re-arming')
    args = parser.parse_args()
    for machine in ('pc', 'q35'):
        boot(os.environ.get('QEMU', 'qemu-system-x86_64'), args.kernel.resolve(), machine, args.mode, args.interrupts)


if __name__ == '__main__':
    main()
