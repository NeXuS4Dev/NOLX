#!/usr/bin/env python3
"""Absent-device fallbacks: a machine without a given class of hardware.

Every native driver must refuse cleanly and say so, the block layer must dispatch nothing,
and the kernel must still finish its initialization. This is the companion fixture to the
per-device start tests: those prove a driver works when its device exists, this one proves
the absence of a device is reported instead of being mistaken for a failure or, worse,
fabricated success.

Two profiles:

* default (no controllers at all): each storage, USB and network driver reports its own
  clean refusal, nothing dispatches to the block layer, and initialization completes.
* --with-xhci-but-no-devices: the xHCI controller is present on the PCI bus but no USB
  device is attached (in particular no SuperSpeed device). The controller is reported, its
  root ports are empty, no input device is bound, and initialization still completes -- an
  absent SuperSpeed device is not mistaken for a controller fault or a slot leak.

The image is the default kernel (`make kernel`); no test feature is required.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]

# One line per native driver that has no device to bind. The wording is the driver's own
# refusal, so a driver that starts inventing success fails here.
ABSENT = [
    'virtio-blk: not brought up: NoDevice',
    'ahci: not brought up: NoController',
    'nvme: not brought up: NoController',
    'xhci: no controller on the PCI bus',
    'e1000: no controller on the PCI bus',
    'virtio-net: not brought up: NoDevice',
]


def boot(qemu, kernel, machine, with_xhci=False):
    if with_xhci:
        label = f'absent-devices-{machine}-xhci-no-devices'
    else:
        label = f'absent-devices-{machine}'
    log = ROOT / 'build' / f'{label}.log'
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot',
               '-net', 'none']
    if with_xhci:
        # The controller is present but nothing is attached: no keyboard, no mouse, and in
        # particular no SuperSpeed device. The xHCI driver must still bring the controller
        # up and report empty ports without binding any input device.
        command += ['-device', 'qemu-xhci,id=xhci']
    with log.open('w') as output:
        process = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                text = log.read_text(errors='replace')
                assert not re.search(r'PANIC|BUGCHECK|CPU exception|panicked at', text), text
                if 'NOLX: init sequence complete' in text:
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
    assert 'NOLX: init sequence complete' in text, f'{log}\n{text}'
    assert not re.search(r'PANIC|BUGCHECK|CPU exception|panicked at', text), text
    if not with_xhci:
        for marker in ABSENT:
            assert text.count(marker) == 1, (marker, text)
        assert 'blockdev: 0 requests dispatched, 0 completed' in text, text
        assert 'bound and started through nolx-df' not in text, text
        print(f'PASS: absent-devices-{machine}: six native drivers refused cleanly, no block '
              'request was dispatched, and initialization completed', flush=True)
    else:
        # The controller is discovered and its ports reported empty.
        assert re.search(r'xhci: [0-9a-f]{2}:[0-9a-f]{2}\.[0-9] bar0 0x[0-9a-f]+, version 0x[0-9a-f]+, [0-9]+ ports, [0-9]+ slots', text), text
        assert re.search(r'xhci: 0 of [0-9]+ ports connected', text), text
        assert 'usb: HID ready' not in text, text
        assert 'enumeration refused' not in text, text
        print(f'PASS: absent-devices-{machine}-xhci-no-devices: xHCI controller reported, '
              'empty root ports, no USB device bound (SuperSpeed-absent path), and '
              'initialization completed', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build' / 'nolx.elf')
    parser.add_argument('--with-xhci-but-no-devices', action='store_true',
                        help='boot with an xHCI controller but no USB device attached')
    args = parser.parse_args()
    for machine in ('pc', 'q35'):
        boot(os.environ.get('QEMU', 'qemu-system-x86_64'), args.kernel.resolve(), machine,
             args.with_xhci_but_no_devices)


if __name__ == '__main__':
    main()
