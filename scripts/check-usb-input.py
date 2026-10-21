#!/usr/bin/env python3
"""Verify native xHCI boot-HID input, ring wrap and root-port DMA reclamation.

Default kernel by default; --objects additionally requires hal-input-object-test.
Direct boot keyboard/mouse, PC and Q35. Optional USB1 and SuperSpeed control/refusal profiles. Every transition must reach the
USB object's console subscription; PS/2 output is not evidence.

--hubs switches to a real USB 2.0 hub topology: a hub on a root port, a device behind it, hotplug
onto another hub port, route-scoped unplug and hub removal with its subtree. There the assertions
are the hub descriptor, the route the child is addressed with, hub-port hotplug, the neutral state
of a held button or key, leaf-first slot release and the DMA baseline.
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


def boot(qemu, kernel, machine, objects=False, interrupts=False, transport="default", no_apic=False, usb_version=2, superspeed=False):
    label = f'input-usb-{machine}' + ('-objects' if objects else '')
    label += f'-{transport}' + ('-no-apic' if no_apic else '')
    label += f'-usb{usb_version}' + ('-ss-control' if superspeed else '')
    log = ROOT / 'build' / f'{label}.log'
    qmp = ROOT / 'build' / f'{label}.sock'
    qmp.unlink(missing_ok=True)
    print(f'RUN: {label}', flush=True)
    # Older QEMU exposes MSI/MSI-X switches only on the NEC model, not qemu-xhci.
    # Use an explicit model for both controlled capability profiles; never skip IRQ proof.
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot', '-net', 'none',
               '-device', {'default': 'qemu-xhci,id=xhci', 'msi': 'nec-usb-xhci,id=xhci,msi=on,msix=off', 'polled': 'nec-usb-xhci,id=xhci,msi=off,msix=off'}[transport], '-device', f'usb-kbd,bus=xhci.0,id=kbd,usb_version={usb_version}',
               '-device', f'usb-mouse,bus=xhci.0,id=mouse,usb_version={usb_version}', '-qmp', f'unix:{qmp},server=on,wait=off']
    if superspeed:
        # Legacy -drive backends auto-delete on device_del; use a fresh backend per cycle.
        for cycle in (1, 2):
            disk = ROOT / 'build' / f'{label}-ss-{cycle}.img'
            with disk.open('wb') as fixture:
                fixture.truncate(4 * 1024 * 1024)
            command += ['-drive', f'if=none,id=ssdisk{cycle},format=raw,file={disk}']
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
            # Enumeration evidence first: a later key failure must not hide a descriptor problem.
            expected_speed = 1 if usb_version == 1 else 3
            proof = re.findall(r'usb: descriptors root-port=\d+ route=0x0 speed=(\d+) ep0=(\d+) config-bytes=\d+', log.read_text())
            assert proof[:2] == [(str(expected_speed), '8' if usb_version == 1 else '64')] * 2, proof
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
            if superspeed:
                for cycle in range(1, 3):
                    execute('device_add', {'driver': 'usb-storage', 'bus': 'xhci.0', 'drive': f'ssdisk{cycle}', 'id': f'ss{cycle}'})
                    def ss_refused(t):
                        descriptors = re.findall(r'usb: descriptors root-port=(\d+) route=0x0 speed=4 ep0=512 config-bytes=44', t)
                        return len(descriptors) == cycle and f'usb: port={descriptors[-1]} enumeration refused: Unsupported; live DMA pages=13' in t
                    text = wait(ss_refused, 'actual SuperSpeed EP0 descriptors and checked slot reclamation')
                    assert text.count('usb: HID ready') == 2, text
                    # A fresh keyboard report proves the existing producer and IRQ service survive.
                    execute('device_del', {'id': f'ss{cycle}'})
                    execute('input-send-event', {'events': [{'type': 'key', 'data': {'down': True, 'key': {'type': 'qcode', 'data': 'b'}}}]})
                    wait(lambda t: t.count('input: usb keyboard usage=0x05 pressed=true') == cycle, 'live USB2 peer after SuperSpeed refusal')
                    execute('input-send-event', {'events': [{'type': 'key', 'data': {'down': False, 'key': {'type': 'qcode', 'data': 'b'}}}]})
                    wait(lambda t: t.count('input: usb keyboard usage=0x05 pressed=false') == cycle, 'peer release after SuperSpeed refusal')
                print(f'PASS: ss-control-{machine}: two actual speed-4 slots, 512-byte EP0, complete configuration reads, unsupported-class refusal, DMA 13 baseline and live USB2 peer', flush=True)
            refused_before = log.read_text().count('enumeration refused: Unsupported; live DMA pages=13')
            # Non-boot tablet must refuse enumeration and return all five slot pages.
            execute('device_add', {'driver': 'usb-tablet', 'bus': 'xhci.0', 'id': 'tablet'})
            wait(lambda t: t.count('enumeration refused: Unsupported; live DMA pages=13') == refused_before + 1, 'unsupported device refusal and reclamation')
            execute('device_del', {'id': 'tablet'})
            # Remove a held keyboard key and mouse button: neutral events must be synthesized.
            key(True, 301)
            execute('device_del', {'id': 'kbd'})
            wait(lambda t: t.count('input: usb keyboard usage=0x04 pressed=false') == 301 and 'live pages=8' in t, 'keyboard disconnect release and five pages reclaimed')
            neutral_before = log.read_text(errors='replace').count('input: usb mouse dx=0 dy=0 buttons=0')
            execute('device_del', {'id': 'mouse'})
            wait(lambda t: 'live pages=3' in t and t.count('input: usb mouse dx=0 dy=0 buttons=0') > neutral_before, 'mouse disconnect and host-only DMA baseline')
            execute('device_add', {'driver': 'usb-kbd', 'bus': 'xhci.0', 'id': 'kbd2', 'usb_version': usb_version})
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
                arms, batches = verify_interrupts(text, reports=608 if superspeed else 604, transport='MSI' if transport == 'msi' else 'MSI-X')
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


def hubs(qemu, kernel, machine):
    """Verify a real hub: descriptor, a device behind it, hotplug and subtree removal.

    QEMU assigns the hub to the first free xHCI root port, so the fixture reads that port from
    the boot log instead of assuming a number; the keyboard takes the next port. Every device
    behind the hub gets a route string in its slot context and in its input-object location, so
    `route: 1` and `route: 3` are different devices on the same root port. Removal is checked
    for a device on a hub port and for the hub itself, which must take its child with it and
    restore the DMA baseline.
    """
    label = f'input-usb-{machine}-hubs'
    log = ROOT / 'build' / f'{label}.log'
    qmp = ROOT / 'build' / f'{label}.sock'
    qmp.unlink(missing_ok=True)
    print(f'RUN: {label}', flush=True)
    command = [qemu, '-machine', 'q35,sata=off' if machine == 'q35' else 'pc',
               '-kernel', str(kernel), '-m', '512M', '-nographic', '-no-reboot', '-net', 'none',
               '-device', 'qemu-xhci,id=xhci',
               '-device', 'usb-hub,bus=xhci.0,port=1,id=hub0,ports=4',
               '-device', 'usb-kbd,bus=xhci.0,port=2,id=kbd0',
               '-device', 'usb-mouse,bus=xhci.0,port=1.1,id=mouse1',
               '-qmp', f'unix:{qmp},server=on,wait=off']
    with log.open('w') as output:
        process = subprocess.Popen(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
        connection = None
        try:
            def text():
                return log.read_text(errors='replace')

            def wait(predicate, description, timeout=60):
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    current = text()
                    assert not re.search(r'PANIC|BUGCHECK|CPU exception', current), current
                    if predicate(current):
                        return current
                    if process.poll() is not None:
                        raise AssertionError(f'QEMU exited before {description}:\n{current}')
                    time.sleep(.01)
                raise AssertionError(f'missing {description}:\n{text()}')

            booted = wait(lambda t: t.count('usb: HID ready') == 2
                          and t.count('usb: hub ready') == 1
                          and 'kernel heap selftest:' in t, 'hub, two HID devices and completed boot')
            hub = re.search(r'usb: hub ready root-port=(\d+) route=0x0 slot=(\d+) ports=(\d+) '
                            r'per-port-power=(\w+) think-time=(\d+) power-on-delay=(\d+)ms', booted)
            assert hub, booted
            hub_port, hub_slot, hub_ports = int(hub.group(1)), int(hub.group(2)), int(hub.group(3))
            assert hub_ports == 4, hub.group(0)
            # The hub descriptor is read with a class request; a standard one stalls.
            assert re.search(rf'usb: hub descriptors root-port={hub_port} route=0x0 ports=4 '
                             rf'removable=0x[0-9a-f]{{4}} characteristics=0x[0-9a-f]{{4}}', booted), booted
            # The mouse hanging off hub port 1: route 0x1, full-speed, a slot of its own.
            assert re.search(rf'usb: descriptors root-port={hub_port} route=0x1 speed=1 ep0=8 '
                             r'config-bytes=34', booted), booted
            child = re.search(rf'usb: HID ready root-port={hub_port} route=0x1 slot=(\d+) '
                              r'protocol=2 endpoint=0x81', booted)
            assert child, booted
            assert int(child.group(1)) != hub_slot, 'a child must not share the hub slot'
            # The route is part of the input object identity, not only a log field.
            assert re.search(rf'input: object attached .*location=Usb \{{ controller: \d+, '
                             rf'port: {hub_port}, route: 1 \}}', booted), booted
            assert re.search(r'input: object attached .*location=Usb \{ controller: \d+, '
                             r'port: \d+, route: 0 \}', booted), booted
            # The keyboard is on a root port of its own, and nothing was refused at boot.
            assert re.search(r'usb: HID ready root-port=(?!' + str(hub_port) + r'\b)\d+ route=0x0 slot=\d+ protocol=1 endpoint=0x81', booted), booted
            assert 'enumeration refused' not in booted, booted

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

            def pages(t):
                return [int(value) for value in re.findall(r'live pages=(\d+)', t)][-1]

            # A pointer report from behind the hub proves the route reached real transfers.
            execute('input-send-event', {'events': [
                {'type': 'rel', 'data': {'axis': 'x', 'value': 7}},
                {'type': 'rel', 'data': {'axis': 'y', 'value': -3}},
                {'type': 'btn', 'data': {'down': True, 'button': 'left'}}]})
            routed = wait(lambda t: 'input: usb mouse dx=7 dy=-3 buttons=1 device=' in t,
                          'pointer report from behind the hub')
            mouse_id = re.search(r'input: usb mouse dx=7 dy=-3 buttons=1 device=(\d+:\d+)', routed).group(1)

            # Unplugging a device from a hub port retires exactly that route, and nothing else.
            execute('device_del', {'id': 'mouse1'})
            # Presence, not a count: a hub can be re-enumerated on its own after a port change,
            # and an exact release count would then turn a healthy run into a timeout.
            removed = wait(lambda t: f'usb: disconnected port={hub_port} route=0x1 slot=' in t
                           and 'input: usb mouse dx=0 dy=0 buttons=0 device=' in t,
                           'hub port unplug, route-scoped release and neutral button record')
            assert f'usb: hub port disconnected root-port={hub_port} route=0x1' in removed, removed
            assert f'device={mouse_id} sequence=' in removed, removed
            assert f'usb: disconnected port={hub_port} route=0x0 slot=' not in removed, removed
            print(f'{machine}: unplug route 0x1 -> {pages(removed)} pages, hub slot {hub_slot} untouched', flush=True)

            # Hotplug into another hub port: a fresh slot, a fresh route, a live report.
            execute('device_add', {'driver': 'usb-mouse', 'bus': 'xhci.0', 'port': '1.3', 'id': 'mouse3'})
            added = wait(lambda t: f'usb: HID ready root-port={hub_port} route=0x3' in t,
                         'device hotplugged onto hub port 3')
            assert re.search(rf'usb: descriptors root-port={hub_port} route=0x3 speed=1 ep0=8 '
                             r'config-bytes=34', added), added
            assert re.search(r'input: object attached .*route: 3 \}', added), added
            execute('input-send-event', {'events': [{'type': 'rel', 'data': {'axis': 'x', 'value': 5}}]})
            wait(lambda t: 'input: usb mouse dx=5 dy=0 buttons=0 device=' in t,
                 'report from the hotplugged device')
            print(f'{machine}: hotplug route 0x3 -> {pages(text())} pages', flush=True)
            execute('device_del', {'id': 'mouse3'})
            wait(lambda t: f'usb: disconnected port={hub_port} route=0x3 slot=' in t,
                 'hotplugged device release')

            # Two devices below the hub: a pointer with a button held and a keyboard with a key
            # held. Removing the hub must retire both, leaves first, and publish the neutral
            # state of each — a program reading a device that vanishes may never learn the
            # button or key was released otherwise.
            execute('device_add', {'driver': 'usb-mouse', 'bus': 'xhci.0', 'port': '1.2', 'id': 'mouse2'})
            wait(lambda t: f'usb: HID ready root-port={hub_port} route=0x2' in t, 'pointer on hub port 2')
            execute('device_add', {'driver': 'usb-kbd', 'bus': 'xhci.0', 'port': '1.4', 'id': 'kbd4'})
            wait(lambda t: f'usb: HID ready root-port={hub_port} route=0x4' in t, 'keyboard on hub port 4')
            assert re.search(rf'usb: HID ready root-port={hub_port} route=0x4 slot=\d+ protocol=1', text()), text()
            execute('input-send-event', {'events': [{'type': 'btn', 'data': {'down': True, 'button': 'left'}}]})
            held = wait(lambda t: 'input: usb mouse dx=0 dy=0 buttons=1 device=' in t, 'button held behind the hub')
            held_id = re.findall(r'input: usb mouse dx=0 dy=0 buttons=1 device=(\d+:\d+)', held)[-1]
            execute('input-send-event', {'events': [{'type': 'key', 'data': {'down': True, 'key': {'type': 'qcode', 'data': 'a'}}}]})
            keys = wait(lambda t: 'input: usb keyboard usage=0x04 pressed=true' in t,
                        'key held behind the hub', 10)
            key_id = re.findall(r'input: usb keyboard usage=0x04 pressed=true device=(\d+:\d+)', keys)[-1]
            execute('device_del', {'id': 'hub0'})
            mouse_line = f'usb: disconnected port={hub_port} route=0x2 slot='
            keyboard_line = f'usb: disconnected port={hub_port} route=0x4 slot='
            hub_line = f'usb: disconnected port={hub_port} route=0x0 slot='
            gone = wait(lambda t: mouse_line in t and keyboard_line in t and hub_line in t
                        and f'input: usb mouse dx=0 dy=0 buttons=0 device={held_id}' in t
                        and f'input: usb keyboard usage=0x04 pressed=false device={key_id}' in t,
                        'hub removal retires both children and releases the held button and key')
            # Leaves first: both children are released before the hub they hang off.
            assert gone.index(mouse_line) < gone.index(hub_line), gone[-4000:]
            assert gone.index(keyboard_line) < gone.index(hub_line), gone[-4000:]
            assert f'usb: port={hub_port} enumeration refused' not in gone, gone
            print(f'{machine}: hub removal retired routes 0x2 and 0x4, then 0x0 -> {pages(gone)} pages', flush=True)

            # Only the root keyboard remains, so the DMA baseline returns to one device.
            final = wait(lambda t: pages(t) == 8, 'keyboard-only DMA baseline after hub removal')
            assert 'in use 0 -> 0 bytes, integrity ok' in final, final
            execute('input-send-event', {'events': [{'type': 'key', 'data': {'down': True, 'key': {'type': 'qcode', 'data': 'b'}}}]})
            wait(lambda t: 'input: usb keyboard usage=0x05 pressed=true' in t, 'root keyboard after hub removal', 10)
            execute('input-send-event', {'events': [{'type': 'key', 'data': {'down': False, 'key': {'type': 'qcode', 'data': 'b'}}}]})
            wait(lambda t: 'input: usb keyboard usage=0x05 pressed=false' in t, 'root keyboard release', 10)

            # Re-plug a hub into the same root port: the port must become usable again.
            # The predicate names the re-plugged hub's own descriptor, not "one more hub than
            # before": a hub may be re-enumerated by the kernel at any point, and counting
            # `usb: hub ready` lines would then match text that predates this device.
            replug_line = rf'usb: hub ready root-port={hub_port} route=0x0 slot=\d+ ports=2'
            execute('device_add', {'driver': 'usb-hub', 'bus': 'xhci.0', 'port': '1', 'id': 'hub1', 'ports': 2})
            replugged = wait(lambda t: re.search(replug_line, t) is not None,
                             'hub re-plug on the same root port')
            assert re.search(replug_line, replugged), replugged[-1500:]
            assert f'usb: port={hub_port} enumeration refused' not in replugged, replugged[-1500:]
            print(f'PASS: usb-hubs-{machine}: hub slot {hub_slot} on root port {hub_port}, device at route 0x1, '
                  'hotplug at route 0x3, route-scoped unplug, leaf-first subtree removal of routes 0x2 and 0x4 '
                  'with held-button and held-key release, DMA back to one device and a working re-plug', flush=True)
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
    parser.add_argument('--usb-version', type=int, choices=(1, 2), default=2, help='QEMU HID version; assert actual full/high-speed descriptor evidence')
    parser.add_argument('--superspeed-control', action='store_true', help='exercise real USB3 mass-storage EP0 descriptors, class refusal and reclamation with live HID peers; not SS HID evidence')
    parser.add_argument('--hubs', action='store_true', help='verify a USB 2.0 hub: descriptor, a device behind it, hotplug and subtree removal')
    args = parser.parse_args()
    if args.superspeed_control and args.objects:
        parser.error('--superspeed-control has a dedicated peer sequence, not the fixed input-object observer sequence')
    if args.interrupts and (args.transport == 'polled' or args.no_apic):
        parser.error('--interrupts cannot claim the polled/PIC-only profiles')
    if args.hubs:
        if args.objects or args.interrupts or args.superspeed_control:
            parser.error('--hubs has a dedicated topology and peers, not the root-port HID sequence')
        (ROOT / 'build').mkdir(exist_ok=True)
        for machine in ['pc', 'q35']:
            hubs(args.qemu, args.kernel.resolve(), machine)
        return
    (ROOT / 'build').mkdir(exist_ok=True)
    for machine in ['pc', 'q35']:
        boot(args.qemu, args.kernel.resolve(), machine, args.objects, args.interrupts, args.transport, args.no_apic, args.usb_version, args.superspeed_control)


if __name__ == '__main__':
    main()
