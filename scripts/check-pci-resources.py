#!/usr/bin/env python3
"""Clear firmware BARs/windows, then require native assignment and real ten-device I/O.

Build with features hal-pci-resource-test,hal-driver-multi-test,hal-driver-net-multi-test.
These bounded PC/Q35 fixtures retain firmware bus numbers. I/O BAR/window assignments
are verified via configuration readback; data traffic exercises native memory decoding.
"""
import argparse
import os
from pathlib import Path
import re
import runpy

ROOT = Path(__file__).resolve().parents[1]
boot = runpy.run_path(str(ROOT / 'scripts' / 'check-network-multi.py'))['boot']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, default=ROOT / 'build' / 'nolx.elf')
    parser.add_argument('--qemu', default=os.environ.get('QEMU', 'qemu-system-x86_64'))
    args = parser.parse_args()
    (ROOT / 'build').mkdir(exist_ok=True)
    for machine in ['pc', 'q35']:
        for levels, memory in [(0,512),(1,512),(2,512),(0,3072 if machine == 'q35' else 1536)]:
            text = boot(args.qemu, args.kernel.resolve(), ['e1000', 'e1000', 'virtio-net', 'virtio-net'],
                        machine, bridge=levels > 0, nested=levels == 2, storage=True, memory=memory)
            assert 'pci: resource policy=AML root _CRS;' in text, text
            assert 'aml: evaluated PCI root resources authorized' in text, text
            injected = re.search(r'pci: resource fault injection: (\d+) BAR addresses and bridge forwarding cleared before native assignment', text)
            assert injected, text
            summary = re.search(r'pci: native resource assignment: (\d+) BARs, (\d+) bridge windows, readback verified', text)
            assert summary, text
            assert int(summary[1]) == int(injected[1]) > 0, text
            assert int(summary[2]) == 2 * levels, text
            assignments = re.findall(r'pci: assigned (\S+) bar(\d+) (mem|io) (0x[0-9a-f]+)\.\.(0x[0-9a-f]+)', text)
            assert len(assignments) == int(summary[1]), text
            assert len({(bdf, slot) for bdf, slot, _, _, _ in assignments}) == len(assignments), assignments
            ranges = {'mem': [], 'io': []}
            for bdf, slot, kind, start, end in assignments:
                start, end = int(start, 16), int(end, 16)
                size = end - start
                assert size > 0 and size & (size - 1) == 0 and start % size == 0, assignments
                if kind == 'mem':
                    windows = [(int(a,16),int(b,16)) for a,b in re.findall(r'aml: PCI root Memory aperture (0x[0-9a-f]+)\.\.(0x[0-9a-f]+) flags=0x1',text)]
                    assert any(a <= start < end <= b for a,b in windows), assignments
                    assert 0x80000000 <= start < end <= 0xf0000000, assignments
                    excluded = [(int(a,16),int(b,16)) for a,b in re.findall(r'pci: resource memory exclusion (0x[0-9a-f]+)\.\.(0x[0-9a-f]+)',text)]
                    assert excluded and all(end <= a or b <= start for a,b in excluded), assignments
                else:
                    assert 0xc000 <= start < end <= 0xf000, assignments
                assert all(end <= a or b <= start for a, b in ranges[kind]), assignments
                ranges[kind].append((start, end))
                if levels:
                    # All endpoints and inner bridges are behind the top-level bridge;
                    # its own BAR is correctly assigned on its parent/root bus.
                    assert bdf[:2] in {'00', '01', '02'}, assignments
            print(f'PASS: native resources {machine}, RAM {memory} MiB, bridge depth {levels}: {summary[1]} BARs / {summary[2]} windows, no aliases, 6 media and 4 wire endpoints verified', flush=True)


if __name__ == '__main__':
    main()
