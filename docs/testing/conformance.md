# POSIX conformance strategy

## What "conforming" means here

POSIX.1-2024 defines an interface, not an implementation. It says what `read(2)` must do,
not how a kernel must be arranged. That distinction is the entire licence for this project:
NOLX's kernel is shaped like an object-based executive, and its POSIX behaviour is produced
by one subsystem layered on top.

Conformance is therefore a **measured** property, and the measurement has to be explicit or
it becomes a marketing claim. Three things make it explicit:

1. an assertion database, keyed to the specification text;
2. tests that reference those assertions;
3. a dashboard generated from the data, never written by hand.

## The assertion database

`docs/testing/assertions/` holds one file per specification area. Each line is:

```
ID | specification reference | status | test
```

For example:

```
RD-004 | XSH read() DESCRIPTION p.3 | tested-T1 | crates/nolxfs tests::read_*
RD-005 | XSH read() RETURN VALUE      | implemented | kernel/src/psx/read.rs
RD-006 | XSH read() ERRORS EINTR      | unimplemented | -
```

Statuses, in increasing order of confidence:

| Status | Meaning |
|---|---|
| `unimplemented` | Not written. |
| `implemented` | Written and reviewed; no automated test yet. |
| `tested-T1` | Covered by a host-side unit test. |
| `tested-T2` | Covered by a test that boots the kernel in QEMU. |
| `tested-T3` | Covered by an external conformance or functional suite running on NOLX. |

`tools/conformance-report.py` turns those files into `docs/testing/CONFORMANCE.md`. It is
generated on every CI run, so the dashboard cannot drift from the database, and a claim
without a test behind it is visible as one.

## Test tiers

| Tier | Runs on | Catches |
|---|---|---|
| **T1** | the build machine, `cargo test` | logic errors in pathname resolution, descriptor semantics, the B-tree, the on-disk layout, the scheduler model |
| **T2** | QEMU, booting the kernel | hardware bring-up, memory management, the trap path, driver stacks |
| **T3** | NOLX itself, running a suite | the thing that actually matters: whether a POSIX program behaves correctly |

T1 is where most of the effort goes, because it is cheap and fast. But T1 alone can prove a
pathname resolver correct and still ship a kernel that cannot open a file, so T3 is not
optional -- it is just later in the roadmap.

## External suites, and their limits

### Open POSIX Test Suite

The Open POSIX Test Suite is the obvious starting point and is used as one. It is also
worth being clear about what it is:

* it targets **POSIX.1-2001**, not Issue 8;
* its last release was 2013, and the project has been dormant since;
* it is licensed **GPLv2**, so its tests are run against NOLX but never linked into it;
* its coverage is strongest for threads, clocks and timers, signals, message queues, and
  semaphores, and thinnest for the file and directory interfaces a shell actually uses.

So it is a floor, not a ceiling. Where it covers an area, NOLX runs it in CI and publishes
the pass matrix. Where it does not, our own assertion-per-sentence tests fill the gap.

### Formal certification

The Open Group maintains separate, **authorized** conformance test suites for formal
certification, and certification is a legal and commercial process, not merely an
engineering one. This project does not claim to be certified and does not use those suites.
What it does claim is checkable: which functions are implemented, which are tested, and what
the deviations are.

### FreeBSD and illumos

The FreeBSD and illumos codebases are used as **references for behaviour under ambiguity**.
The specification leaves genuine gaps -- the exact ordering of `readdir` results across a
`getdents` boundary, how `rename` behaves when the target is a mount point -- and the two
long-lived BSD and SVR4-derived implementations are the best available guide to what
programs actually expect. Where NOLX follows one of them, the assertion record says which
and why.

Neither codebase is copied. NOLX is written from the specification, with those two used the
way a standards lawyer uses case law.

## Deliberate deviations

`docs/testing/deviations.md` records every place NOLX differs from the specification or
strengthens it. Each entry names the spec reference, the deviation, the reason, and the
compatibility risk. Two examples of the shape an entry takes:

* **`vfork`**: Issue 8 retains `vfork` with undefined behaviour for anything but storing a
  `pid_t`. NOLX implements it on the same copy-on-write primitive as `fork`, which makes the
  undefined behaviour well-defined. This is a strengthening, and it cannot break a
  conforming program, because a conforming program does not rely on undefined behaviour.
* **`EWOULDBLOCK` and `EAGAIN`**: the standard permits these to be equal and most systems
  make them so. NOLX keeps them distinct so that a socket error and a file error are
  distinguishable in a bug report. A program that compares against either constant still
  works.

A deviation that is not written down is a bug. That is the rule.
