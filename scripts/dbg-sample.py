#!/usr/bin/env python3
"""Sample one kernel address at each boot marker, to localize when it stops being sane."""
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PHYS = int(sys.argv[1], 16) if len(sys.argv) > 1 else 0x1664558
SIZE = int(sys.argv[2]) if len(sys.argv) > 2 else 16
SERIAL = ROOT / "build" / "sample-serial.log"
SOCK = ROOT / "build" / "sample-qmp.sock"

MARKERS = [
    "NOLX: init sequence complete",
    "NOLX: round-robin T2:",
    "NOLX: user mode T2: init spawned",
    "hello: hello from ring 3",
    "tmpfs: ring-3 read/write ok",
    "NOLX: user mode T2: hello ran in ring 3",
    "NOLX: tmpfs T2:",
    "NOLX: timer reached 100 ticks",
    "NOLX: mm selftest: starting",
    "NOLX: address space online",
]


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

    def text():
        return Path(SERIAL).read_text(errors="replace")

    def sample():
        command("stop")
        reply = command("human-monitor-command", **{
            "command-line": f"xp /{SIZE}xb {PHYS:#x}"})
        command("cont")
        return reply.get("return", "").strip()

    seen = set()
    end = time.monotonic() + 120
    while time.monotonic() < end:
        current = text()
        for marker in MARKERS:
            if marker in current and marker not in seen:
                if "address space online" in marker:
                    # The sampling itself perturbs timing; stop once the last marker is here.
                    seen.add(marker)
                seen.add(marker)
                print(f"{marker!r:55} {sample()}", flush=True)
        if "NOLX: timer reached 100 ticks" in current:
            break
        time.sleep(0.02)
    sock.close()
    process.terminate()
    process.wait()


main()
