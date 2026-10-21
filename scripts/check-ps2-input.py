#!/usr/bin/env python3
"""Inject actual QMP keyboard/pointer input and assert the kernel-delivered PS/2 events."""
import argparse
import json
import re
import socket
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BUILD = ROOT / "build"
LOG = BUILD / "input-ps2.serial.log"
QMP = BUILD / "input-qmp.sock"


def main():
    global LOG, QMP
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--objects',action='store_true',help='require hal-input-object-test and independent reader evidence')
    parser.add_argument('--read-t2',action='store_true',help='require hal-input-read-test: a ring-3 read(2) on the keyboard device')
    parser.add_argument('--kernel',type=Path,default=BUILD/'nolx.elf')
    args=parser.parse_args()
    if args.objects:
        LOG=BUILD/'input-ps2-objects.serial.log'
        QMP=BUILD/'input-ps2-objects-qmp.sock'
    if args.read_t2:
        LOG=BUILD/'input-ps2-read.serial.log'
        QMP=BUILD/'input-ps2-read-qmp.sock'
    BUILD.mkdir(exist_ok=True)
    QMP.unlink(missing_ok=True)
    with LOG.open("w") as log:
        process = subprocess.Popen(
            ["qemu-system-x86_64", "-kernel", str(args.kernel.resolve()),
             "-m", "512M", "-nographic", "-no-reboot", "-net", "none",
             "-qmp", f"unix:{QMP},server=on,wait=off"],
            cwd=ROOT, stdout=log, stderr=subprocess.STDOUT,
        )
        connection = None
        try:
            def wait_for(predicate, description, timeout=45):
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    text = LOG.read_text(errors="replace")
                    if re.search(r"PANIC|BUGCHECK|CPU exception", text):
                        raise RuntimeError("kernel fault during input test")
                    if predicate(text):
                        return
                    if process.poll() is not None:
                        raise RuntimeError(f"QEMU exited before {description}")
                    time.sleep(0.05)
                raise TimeoutError(f"missing: {description}")

            # Do not inject while the strict scheduler trace is still running.
            wait_for(lambda text: "input: PS/2 keyboard IRQ1, mouse IRQ12 ready" in text
                     and "NOLX: timer reached 100 ticks; nominal 100 Hz" in text,
                     "PS/2 readiness and completed scheduler/user boot")
            connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            connection.settimeout(5)
            connection.connect(str(QMP))
            stream = connection.makefile("rwb")
            greeting = json.loads(stream.readline())
            if "QMP" not in greeting:
                raise RuntimeError("invalid QMP greeting")

            def execute(command, arguments=None):
                message = {"execute": command}
                if arguments is not None:
                    message["arguments"] = arguments
                stream.write(json.dumps(message).encode() + b"\n")
                stream.flush()
                while True:
                    line = stream.readline()
                    if not line:
                        raise RuntimeError(f"QMP closed during {command}")
                    response = json.loads(line)
                    if "error" in response:
                        raise RuntimeError(f"QMP {command}: {response['error']}")
                    if "return" in response:
                        return response["return"]

            execute("qmp_capabilities")

            if args.read_t2:
                # The ring-3 reader has its own subscription: the key must be injected after it
                # opens, not when the console reports the keyboard, or the program would be
                # waiting for a record that was published before it existed.
                wait_for(lambda t: "NOLX: input read(2) T2: inputread (pid " in t
                         and "send one key now" in t,
                         "ring-3 inputread waiting on the keyboard descriptor", 90)
                execute("send-key", {"keys": [{"type": "qcode", "data": "a"}], "hold-time": 60})
                wait_for(lambda t: "NOLX: input read(2) T2: ring-3 open/read/close on the PS/2 "
                         "keyboard device returned the injected press and release records" in t,
                         "ring-3 read(2) evidence chain", 60)
                text = LOG.read_text(errors="replace")
                assert re.search(r"inputread: open/read/close on \\Device\\Input\d+ ok", text), text
                assert "NOLX: input read(2) T2: device descriptors still open: 0" in text, text
                assert re.search(r"NOLX: input read\(2\) T2: inputread zombie=true status=42 "
                                 r"wrote=48", text), text
                assert re.search(r"input: ps2 keyboard usage=0x04 pressed=true device="
                                 r"\d+:\d+ sequence=[1-9]", text), text
                assert re.search(r"input: ps2 keyboard usage=0x04 pressed=false device="
                                 r"\d+:\d+ sequence=[1-9]", text), text
                execute("quit")
                if process.wait(timeout=5) != 0:
                    raise RuntimeError("QEMU did not exit cleanly")
                print("PASS: a ring-3 program read the injected press and release through "
                      "open/read/close on the PS/2 keyboard device")
                return

            execute("send-key", {"keys": [{"type": "qcode", "data": "a"}], "hold-time": 60})
            wait_for(lambda text: "input: ps2 keyboard usage=0x04 pressed=true" in text
                     and "input: ps2 keyboard usage=0x04 pressed=false" in text,
                     "A key press and release", 10)
            execute("input-send-event", {"events": [
                {"type": "rel", "data": {"axis": "x", "value": 6}},
                {"type": "rel", "data": {"axis": "y", "value": 4}},
                {"type": "btn", "data": {"button": "left", "down": True}},
            ]})
            wait_for(lambda text: "input: ps2 mouse dx=6 dy=4 buttons=1" in text,
                     "mouse movement and left press", 10)
            execute("input-send-event", {"events": [
                {"type": "btn", "data": {"button": "left", "down": False}},
            ]})
            wait_for(lambda text: "input: ps2 mouse dx=0 dy=0 buttons=0" in text,
                     "mouse button release", 10)
            text=LOG.read_text(errors="replace")
            keyboard=re.findall(r'input: object attached device=(\d+:\d+) source=Ps2Keyboard',text)
            mouse=re.findall(r'input: object attached device=(\d+:\d+) source=Ps2Mouse',text)
            assert len(keyboard)==1 and len(mouse)==1 and keyboard[0]!=mouse[0],text
            assert re.search(r'input: ps2 keyboard usage=0x04 pressed=true device='+keyboard[0]+r' sequence=[1-9]',text),text
            assert re.search(r'input: ps2 mouse dx=6 dy=4 buttons=1 device='+mouse[0]+r' sequence=[1-9]',text),text
            if args.objects:
                wait_for(lambda t:'input: object independent reader verified PS/2 class=Keyboard' in t and 'input: object independent reader verified PS/2 class=Pointer' in t,'independent PS/2 object consumers',10)
            execute("quit")
            if process.wait(timeout=5) != 0:
                raise RuntimeError("QEMU did not exit cleanly")
            print("PASS: PS/2 IRQs delivered injected key press/release, motion and button events")
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


if __name__ == "__main__":
    main()
