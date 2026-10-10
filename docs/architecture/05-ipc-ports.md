# IPC: Ports

Ports are the executive's message-passing primitive, used for user-mode drivers, the
window server, and any cross-process service. They are *not* on the file-read hot path —
in-kernel components call each other directly.

## Objects

* **Port** — a named or unnamed endpoint with a bounded message queue and a receive
  right. Lives in the object namespace (`\BaseNamedObjects\...`), so it gets handles,
  security descriptors, and inheritance for free.
* **Port set** — a collection of ports waitable as one, so a server thread can service
  many clients.
* **Waitable port** — a port usable as a dispatcher object, so `WaitForMultipleObjects`
  covers "message arrived".

## Message shapes

```
request  { id, flags, inline[0..512], ool_buffers: &[Buffer], reply_port }
reply    { id, status, inline[0..512], ool_buffers: &[Buffer] }
```

* **Inline data** up to 512 bytes travels in the message body (the LPC-style fast path:
  for a same-CPU client/server pair the copy can be avoided entirely).
* **Out-of-line buffers** are section objects; ownership transfers to the receiver, so
  large payloads are never copied. This is how the compositor shares framebuffers.
* **Reply modes**: synchronous (caller waits), asynchronous (reply to another port), or
  fire-and-forget.
* Every message carries the sender's token, so a server can authorise by identity rather
  than by trusting a claimed pid.

## Semantics

* Bounded queues with a configurable policy: block, fail with `EWOULDBLOCK`-equivalent,
  or coalesce.
* Server death closes ports and fails pending requests with a distinct status, which
  `psx/` maps to `EPIPE`/`ECONNRESET` depending on the POSIX surface being emulated.
* Priority donation follows a synchronous request so a high-priority client cannot be
  blocked behind a low-priority server.

## POSIX surfaces built on ports

| POSIX surface | Implementation |
|---|---|
| `socketpair`/AF_UNIX | stream object; local sockets short-circuit without a port |
| POSIX `mq_*` | named port with message semantics and the specified `mq_notify` behaviour |
| `openpty`/`ptsname` | pty master/slave pair over a stream object |
| user-mode drivers | IORBs forwarded over a port to the driver process |
| window server protocol | request/reply with shared framebuffer sections |

## Why not a microkernel-style global message bus

Because the POSIX hot path must not pay for it. Ports exist where isolation or
cross-address-space communication is genuinely needed, and nowhere else. This is the
same trade the executive model makes, and it is the reason NOLX can be object-based and
still hit UNIX-class syscall latency.
