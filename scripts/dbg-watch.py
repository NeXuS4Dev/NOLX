#!/usr/bin/env python3
"""Arm a GDB hardware write watchpoint after a serial marker and report every writer."""
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PHYS = int(sys.argv[1], 16)
MARKER = sys.argv[2]
LIMIT = float(sys.argv[3]) if len(sys.argv) > 3 else 30.0
SERIAL = ROOT / "build" / "watchpoint-serial.log"


def packet(sock, payload):
    data = payload.encode()
    sock.sendall(b"$" + data + b"#%02x" % (sum(data) & 0xFF))
    while True:
        byte = sock.recv(1)
        if not byte:
            raise SystemExit("closed")
        if byte == b"+":
            return


def receive(sock):
    buffer = b""
    while True:
        byte = sock.recv(1)
        if not byte:
            raise SystemExit("closed")
        if byte == b"$":
            buffer = b""
            continue
        if byte == b"#":
            sock.recv(2)
            sock.sendall(b"+")
            return buffer.decode(errors="replace")
        buffer += byte


def main():
    if os.path.exists(SERIAL):
        os.unlink(SERIAL)
    cmd = ["qemu-system-x86_64", "-machine", "q35", "-kernel", "build/nolx.elf",
           "-m", "512M", "-no-reboot", "-net", "none", "-display", "none",
           "-serial", f"file:{SERIAL}", "-S", "-s"]
    process = subprocess.Popen(cmd, cwd=ROOT, stdout=subprocess.DEVNULL,
                               stderr=subprocess.STDOUT)
    sock = None
    for _ in range(200):
        try:
            sock = socket.create_connection(("127.0.0.1", 1234), timeout=5)
            break
        except OSError:
            sock = None
            time.sleep(0.05)
    if sock is None:
        raise SystemExit("no gdbstub")
    sock.settimeout(20)
    packet(sock, "?")
    receive(sock)
    packet(sock, "c")
    end = time.monotonic() + 120
    while time.monotonic() < end:
        if MARKER in Path(SERIAL).read_text(errors="replace"):
            break
        time.sleep(0.01)
    sock.sendall(b"\x03")
    try:
        print("stopped:", receive(sock), flush=True)
    except socket.timeout:
        print("interrupt timed out", flush=True)
    packet(sock, f"Z2,{PHYS:x},1")
    print("watch:", receive(sock), flush=True)
    hits = 0
    end = time.monotonic() + LIMIT
    packet(sock, "c")
    while time.monotonic() < end:
        try:
            stop = receive(sock)
        except socket.timeout:
            continue
        if not stop.startswith("T"):
            continue
        packet(sock, "g")
        raw = bytes.fromhex(receive(sock))
        values = [int.from_bytes(raw[i * 8:(i + 1) * 8], "little") for i in range(17)]
        names = ["rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp"]
        detail = " ".join(f"{n}={v:#x}" for n, v in zip(names, values))
        print(f"write rip={values[16]:#x} {detail}", flush=True)
        stack = values[7]
        packet(sock, f"m{stack:x},{32 * 8:x}")
        try:
            words = bytes.fromhex(receive(sock))
        except Exception:
            words = b""
        chain = [int.from_bytes(words[i * 8:(i + 1) * 8], "little")
                 for i in range(min(32, len(words) // 8))]
        print("  stack: " + " ".join(f"{w:#x}" for w in chain[:16]), flush=True)
        hits += 1
        if hits >= 10:
            break
        packet(sock, "c")
    print(f"hits={hits}", flush=True)
    sock.close()
    process.terminate()
    process.wait()


main()
