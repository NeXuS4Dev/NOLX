#!/usr/bin/env python3
"""Prove a ring-3 `poll(2)` on an input device is a scheduler block that input wakes.

The kernel image must be built with `hal-input-wait-test`. Two runs:

* the default run gives the guest a USB pointer, waits until the kernel reports the program
  suspended on a five-second poll, and only then injects relative motion through QMP. The
  injection therefore cannot have been queued before the program blocked, which is what makes
  the wake evidence of a block rather than of a race that the program happened to win;
* `--absent` boots the same image with no pointer at all. The phase must report that it has
  nothing to wait on and leave the boot running: a machine without a pointer is not a failure,
  but a gate that quietly claimed a pass would be.
"""
import argparse
import json
import re
import socket
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BUILD = ROOT / "build"
LOG = BUILD / "input-poll.serial.log"
QMP = BUILD / "input-poll-qmp.sock"

# The injected report: relative x=7, y=-3, no buttons. These are the values the ring-3 program
# checks byte for byte in the record it reads after the wake.
DX = 7
DY = -3

PASS_MARKER = ("NOLX: input poll T2: ring-3 poll(2) suspended the caller, woke on the injected "
               "pointer report and the following read(2) matched it")
SKIP_MARKER = "NOLX: input poll T2: no USB pointer device on this machine; phase skipped"


def main():
    global LOG, QMP
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--absent', action='store_true',
                        help='boot without a USB pointer and require the honest skip')
    parser.add_argument('--kernel', type=Path, default=BUILD / 'nolx.elf')
    args = parser.parse_args()
    if args.absent:
        LOG = BUILD / 'input-poll-absent.serial.log'
        QMP = BUILD / 'input-poll-absent-qmp.sock'
    BUILD.mkdir(exist_ok=True)
    QMP.unlink(missing_ok=True)
    command = ["qemu-system-x86_64", "-kernel", str(args.kernel.resolve()),
               "-m", "512M", "-nographic", "-no-reboot", "-net", "none",
               "-qmp", f"unix:{QMP},server=on,wait=off"]
    if not args.absent:
        # One USB pointer on one controller: the only device the phase can wait on, so the
        # record that wakes it can only be the one injected here.
        command += ["-device", "qemu-xhci,id=xhci", "-device", "usb-mouse,bus=xhci.0,id=mouse0"]
    with LOG.open("w") as log:
        process = subprocess.Popen(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
        connection = None
        try:
            def wait_for(predicate, description, timeout=90):
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    text = LOG.read_text(errors="replace")
                    if re.search(r"PANIC|BUGCHECK|CPU exception", text):
                        raise RuntimeError(f"kernel fault while waiting for {description}")
                    if predicate(text):
                        return text
                    if process.poll() is not None:
                        raise RuntimeError(f"QEMU exited before {description}")
                    time.sleep(0.05)
                raise TimeoutError(f"missing: {description}")

            if args.absent:
                text = wait_for(lambda t: SKIP_MARKER in t, "the no-pointer skip report", 120)
                assert "NOLX: input poll T2: inputwait (pid" not in text, text
                assert "input: poll blocked" not in text, text
                assert PASS_MARKER not in text, text
                # The boot has to continue past the phase: the memory selftests run after it.
                wait_for(lambda t: "NOLX: mm selftest" in t or "mm selftest" in t,
                         "the boot to continue past the skipped phase", 120)
                execute_quit(None, process)
                print("PASS: a machine without a USB pointer reports the phase as skipped and "
                      "keeps booting")
                return

            # The short timeout has to expire first, with nothing ready: a poll that answered
            # "ready" for a descriptor with an empty queue would pass the rest of this check
            # without proving anything.
            short = wait_for(
                lambda t: "NOLX: input poll T2: inputwait (pid" in t
                and re.search(r"input: poll blocked pid=\d+ fds=1 timeout=20ms", t)
                and re.search(r"input: poll woke pid=\d+ ready=0 timed_out=true", t),
                "the twenty-millisecond poll to block and expire with nothing ready", 120)
            assert short.index("timeout=20ms") < short.index("ready=0 timed_out=true"), short
            wait_for(lambda t: re.search(r"input: poll blocked pid=\d+ fds=1 timeout=5000ms", t),
                     "the ring-3 program to suspend on the five-second poll", 120)

            connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            connection.settimeout(5)
            connection.connect(str(QMP))
            stream = connection.makefile("rwb")
            greeting = json.loads(stream.readline())
            if "QMP" not in greeting:
                raise RuntimeError("invalid QMP greeting")

            def execute(name, arguments=None):
                message = {"execute": name}
                if arguments is not None:
                    message["arguments"] = arguments
                stream.write(json.dumps(message).encode() + b"\n")
                stream.flush()
                while True:
                    line = stream.readline()
                    if not line:
                        raise RuntimeError(f"QMP closed during {name}")
                    response = json.loads(line)
                    if "error" in response:
                        raise RuntimeError(f"QMP {name}: {response['error']}")
                    if "return" in response:
                        return response["return"]

            execute("qmp_capabilities")
            execute("input-send-event", {"events": [
                {"type": "rel", "data": {"axis": "x", "value": DX}},
                {"type": "rel", "data": {"axis": "y", "value": DY}},
            ]})
            text = wait_for(lambda t: PASS_MARKER in t, "the blocking poll evidence chain", 60)
            assert re.search(r"input: poll woke pid=\d+ ready=1 timed_out=false", text), text
            # The program's own verdict: it got one ready descriptor with POLLIN, read the
            # eight-byte record that woke it, matched every field, and closed cleanly.
            assert re.search(r"NOLX: input poll T2: inputwait zombie=true status=42 wrote=64",
                             text), text
            assert "NOLX: input poll T2: device descriptors still open: 0" in text, text
            assert "inputwait: blocking poll(2) woke on the injected pointer record" in text, \
                text
            assert re.search(rf"input: usb mouse dx={DX} dy={DY} buttons=0 device=\d+:\d+ "
                             r"sequence=[1-9]", text), text
            # The injected record is what the program read: it is the only pointer publication
            # in the image, and the program's wire-format check would have failed otherwise.
            assert text.count("input: usb mouse") >= 1, text
            execute("quit")
            if process.wait(timeout=5) != 0:
                raise RuntimeError("QEMU did not exit cleanly")
            print("PASS: a ring-3 poll(2) suspended the caller until injected pointer motion, "
                  "reported one ready descriptor and the following read(2) matched the record")
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
            print(LOG.read_text(errors="replace"))
            QMP.unlink(missing_ok=True)


def execute_quit(stream, process):
    """Leave a QEMU that was never given a QMP connection for the skip case."""
    if stream is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


if __name__ == "__main__":
    main()
