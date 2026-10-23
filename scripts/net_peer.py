"""A minimal ARP peer for QEMU's raw-Ethernet `socket` netdev.

QEMU's `user` backend (slirp) is the natural host side of a test network: it answers the
guest's ARP request for 10.0.2.2 and its MAC encodes the address it speaks for
(`52:55:0a:00:02:02`). Not every QEMU build in every environment has it -- a build without
libslirp refuses `-netdev user` outright, and a locked-down host may not offer a tap.

`ArpPeer` fills that gap without any host networking privileges: QEMU's `socket` netdev carries
raw Ethernet frames over TCP (each frame prefixed with a four-byte big-endian length), so a
small server in the test process can answer the one exchange the kernel's virtio-net driver
requires before it reports the adapter started. The guest-side bytes are unchanged, which is
what the fixtures assert: check-virtio-net.py captures the replies on the wire and matches
them against the request, and check-power-s5.py only needs the adapter to end up owning the
bus-master bit.

The MAC and addresses default to what slirp uses, so the same assertions hold whichever
backend produced the traffic.
"""

import socket
import struct
import threading

#: Defaults matching QEMU's slirp: the gateway's MAC encodes 10.0.2.2, and the guest address
#: is the one the driver claims when it resolves the gateway.
GATEWAY_MAC = bytes.fromhex('52550a000202')
GATEWAY = bytes([10, 0, 2, 2])
GUEST = bytes([10, 0, 2, 15])

_ETHERNET_ARP = b'\x08\x06'
_ARP_REQUEST = b'\x00\x01'
_ARP_REPLY_HEADER = bytes([0, 1, 8, 0, 6, 4, 0, 2])


class ArpPeer:
    """Answer the guest's ARP request for the gateway on a QEMU `socket` netdev.

    Use `port` in `-netdev socket,id=...,connect=127.0.0.1:<port>`. `replies` counts the
    answers actually sent, which a caller can assert on: zero replies means the peer never saw
    the exchange, not that the driver accepted it.
    """

    def __init__(self, mac=GATEWAY_MAC, gateway=GATEWAY, guest=GUEST):
        self.mac = mac
        self.gateway = gateway
        self.guest = guest
        # QEMU's `socket` backend connects to a TCP address; loopback with an ephemeral port
        # keeps the peer inside this process and out of the host's firewall and routing.
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(('127.0.0.1', 0))
        self.listener.listen(1)
        self.port = self.listener.getsockname()[1]
        self.stopped = threading.Event()
        self.replies = 0
        self.frames = 0
        self.thread = threading.Thread(target=self._serve, daemon=True)

    def start(self):
        self.thread.start()
        return self

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.close()

    def _serve(self):
        self.listener.settimeout(.2)
        connection = None
        while not self.stopped.is_set():
            try:
                connection, _ = self.listener.accept()
                break
            except socket.timeout:
                continue
            except OSError:
                return
        if connection is None:
            return
        connection.settimeout(.2)
        try:
            while not self.stopped.is_set():
                header = self._exactly(connection, 4)
                if header is None:
                    break
                frame = self._exactly(connection, struct.unpack('>I', header)[0])
                if frame is None:
                    break
                self.frames += 1
                reply = self._answer(frame)
                if reply is not None:
                    connection.sendall(struct.pack('>I', len(reply)) + reply)
                    self.replies += 1
        finally:
            connection.close()

    def _exactly(self, connection, size):
        """Read exactly `size` bytes, or nothing at end of stream.

        A timeout is not the end of the stream: QEMU connects to this peer when it starts the
        virtual machine, and the guest does not address the gateway until its driver has
        negotiated the adapter several seconds later. Treating the first quiet interval as EOF
        made the peer exit before it could answer anything, which looked exactly like a dead
        network.
        """
        data = b''
        while len(data) < size and not self.stopped.is_set():
            try:
                chunk = connection.recv(size - len(data))
            except socket.timeout:
                continue
            except OSError:
                return None
            if not chunk:
                return None
            data += chunk
        return data if len(data) == size else None

    def _answer(self, frame):
        # Ethernet + ARP: a request for the gateway address from the guest's own address.
        if (len(frame) < 42
                or frame[12:14] != _ETHERNET_ARP
                or frame[20:22] != _ARP_REQUEST
                or frame[28:32] != self.guest
                or frame[38:42] != self.gateway
                or frame[22:28] == bytes(6)
                or frame[22] & 1):
            return None
        reply = bytearray(60)
        reply[0:6] = frame[22:28]
        reply[6:12] = self.mac
        reply[12:14] = _ETHERNET_ARP
        reply[14:22] = _ARP_REPLY_HEADER
        reply[22:28] = self.mac
        reply[28:32] = self.gateway
        reply[32:38] = frame[22:28]
        reply[38:42] = self.guest
        return bytes(reply)

    def close(self):
        self.stopped.set()
        self.listener.close()
