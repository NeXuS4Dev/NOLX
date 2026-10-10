# GUI: Display Stack, Compositor, Window Server

The GUI is a user-mode stack. The kernel provides buffers, page flips, and input events;
everything else lives in servers and clients that talk over IPC ports. A crashed
application must never be able to take the display down — that requirement shapes the
whole design.

## Layers

```
+--------------------------------------------------------------+
|  applications (nolx-ui toolkit)                              |
+--------------------------------------------------------------+
|  window server  (nxwm)   input router   clipboard   a11y bus  |
+--------------------------------------------------------------+
|  compositor (nxd)  — damage tracking, output, vsync          |
+--------------------------------------------------------------+
|  buffer manager (nxbuf) — dmabuf-equivalent buffer objects   |
+--------------------------------------------------------------+
|  display driver (modesetting) + input drivers (evdev-equiv)  |
+--------------------------------------------------------------+
|  kernel: section objects, page flip ioctl, input event objs  |
+--------------------------------------------------------------+
```

## Buffer management

* A buffer is a **section object** with a graphics allocation backing it. Clients render
  into a buffer they own; the compositor imports it read-only. No client can write to
  another client's buffer or to the screen directly.
* Buffer flags carry modifier/tiling information so drivers can use compressed or tiled
  layouts without the toolkit knowing.
* Explicit fencing: a client attaches a release fence; the compositor attaches an acquire
  fence. No implicit global synchronisation, which is what keeps a slow client from
  stalling the display.

## Compositor (`nxd`)

* Owns the scanout buffers, does damage-region tracking, and composites only what changed.
* Page-flip driven with vblank feedback; frame pacing with a target latency budget.
* Multi-output: per-output scale, rotation, and colour pipeline; HiDPI as a first-class
  concept (logical pixels everywhere in the toolkit, physical pixels only in the driver).
* Fallback path: a direct-scanout fast path for a single fullscreen opaque surface, so a
  video player or a game does not pay for compositing.

## Window server (`nxwm`)

* Windows are **objects in the object namespace**, so they get handles, security
  descriptors, and lifetime management from the Object Manager instead of a bespoke
  registry.
* Surface hierarchy with per-node transform, clip, opacity, and shadow.
* Input routing with pointer and keyboard grabs, focus policy, and per-surface input
  regions. A grab is a privileged operation checked against the token.
* Activation and stacking policy, window decorations, drag-and-drop with a session-scoped
  clipboard owner.
* **Accessibility is not an afterthought**: the window server maintains the semantic tree
  the toolkit publishes, so a screen reader reads the tree rather than the pixels.

## Input

* Kernel input drivers publish event objects (a stream of typed records).
* The input router applies the device map, produces logical events, and delivers them to
  the focused or grabbed surface with monotonic timestamps from the same clock
  `clock_gettime(CLOCK_MONOTONIC)` reports, so client-side animation and input
  correlation agree.
* Keyboard state (modifiers, locked keys, keymap) is server-owned; clients get logical
  keysyms plus the raw code, never a guess.

## Text and rendering

* The toolkit does shaping, bidi, and line breaking itself (no server-side text), and
* renders with a GPU backend (a small 3D API abstraction over the driver) and a software
  backend that must produce pixel-identical output — that is what makes UI tests
  deterministic in CI.

## Testing

* T1: compositor state machine (damage algebra, stacking, focus transitions) — pure logic.
* T2: boot under QEMU with virtio-gpu, compositor reaches a desktop, screenshot compared
  against a golden image with a perceptual threshold.
* T1: the software renderer is golden-tested per widget, which is how regressions in text
  layout get caught without a GPU.
