#!/usr/bin/env python3
"""Dump a kernel range at two boot markers and report the bytes that changed."""
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PHYS = int(sys.argv[1], 16)
SIZE = int(sys.argv[2], 16)
BEFORE = sys.argv[3]
AFTER = sys.argv[4]
SERIAL = ROOT / "build" / "diff-serial.log"
SOCK = ROOT / "build" / "diff-qmp.sock"


def main():
    for path in (SERIAL, SOCK):
        if os.path.exists(path):
            os.unlink(path)
    cmd = ["qemu-system-x86_64", "-machine", "q35", "-kernel", "build/nolx.elf",
           "-m", "512M", "-no-reboot", "-net", "none", "-display", "none",
           "-serial", f"file:{SERIAL}", "-qmp", f"unix:{SOCK},server,nowait"]
    process = subprocess.Popen(cmd, cwd=ROOT, stdout=subprocess.DEVNULL,
                               stderr=subprocess.STDOUT)
    sock = None
    for _ in range(200):
        try:
            sock = socket.socket(socket.AF_UNIX)
            sock.connect(str(SOCK))
            break
        except OSError:
            sock.close()
            sock = None
            time.sleep(0.05)
    if sock is None:
        raise SystemExit("no qmp")
    stream = sock.makefile("rw")

    def read():
        while True:
            line = stream.readline()
            if not line:
                return None
            obj = json.loads(line)
            if "event" not in obj:
                return obj

    def command(name, **args):
        stream.write(json.dumps({"execute": name, "arguments": args}) + "\n")
        stream.flush()
        return read()

    read()
    command("qmp_capabilities")

    def wait_for(marker):
        end = time.monotonic() + 120
        while time.monotonic() < end:
            if marker in Path(SERIAL).read_text(errors="replace"):
                return
            time.sleep(0.02)
        raise SystemExit(f"marker not reached: {marker}")

    def dump():
        command("stop")
        data = {}
        for offset in range(0, SIZE, 16):
            reply = command("human-monitor-command", **{
                "command-line": f"xp /16xb {PHYS + offset:#x}"})
            line = reply.get("return", "")
            if ":" not in line:
                continue
            address = int(line.split(":")[0], 16)
            values = [int(v, 16) for v in line.split(":")[1].split() if v.startswith("0x")]
            for index, value in enumerate(values):
                data[address + index] = value
        command("cont")
        return data

    wait_for(BEFORE)
    first = dump()
    wait_for(AFTER)
    second = dump()
    changed = {a: (first.get(a), b) for a, b in sorted(second.items()) if first.get(a) != b}
    print(f"changed {len(changed)} byte(s) between {BEFORE!r} and {AFTER!r}")
    for address, (old, new) in changed.items():
        print(f"  +{address - PHYS:#05x}  {address:#x}  {old:#04x} -> {new:#04x}")
    sock.close()
    process.terminate()
    process.wait()


main()
