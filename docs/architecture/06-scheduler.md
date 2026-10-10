# Scheduler and Dispatcher

## Priorities

32 levels in three bands, mirroring the executive model:

| Band | Levels | Policy |
|---|---|---|
| Real-time | 16–31 | `SCHED_FIFO`, `SCHED_RR`, `SCHED_SPORADIC` (POSIX) |
| Dynamic | 1–15 | `SCHED_OTHER`, with boosts and decay |
| Idle | 0 | the idle thread only |

POSIX `sched_get_priority_min`/`max` map onto the real-time band. `nice(2)` shifts the
dynamic base priority; `nice` values −20..19 map to dynamic levels 15..1, which is why
`nice` bounds are enforced by `RLIMIT_NICE`-equivalent privilege checks.

## Boost and decay

* A thread woken from a wait gets a temporary boost that decays by 1 per quantum, so
  interactive latency stays low without letting a thread squat at the top of the band.
* Priority donation: when a thread waits on a mutex held by another thread, the holder
  is boosted to at least the waiter's priority, bounding priority inversion. This is
  required for `SCHED_FIFO` correctness and is tested as a T1 property.
* Anti-starvation: the balance-set manager periodically boosts a starved dynamic thread
  by a bounded amount. Starvation-freedom is a T1 test, not a hope.

## Per-CPU run queues

* One run queue per CPU, each an array of 32 bitmap-indexed lists, plus a `ready_summary`
  bitmap so "find highest non-empty level" is one `bsr`.
* Affinity masks per thread; `sched_setaffinity`-equivalent and job-object limits narrow it.
* Load balancing: periodic pull from the busiest sibling, plus an immediate pull on idle.
  NUMA-aware — prefer the local node's queue.
* Hyperthread/core awareness: prefer an idle SMT sibling of an already-busy core only when
  nothing else is free.

## Quantum

* Dynamic band: fixed quantum per thread, decremented on each clock interrupt; on expiry
  the thread goes to the tail of its level (round robin within level).
* Real-time band: `SCHED_FIFO` runs until it blocks or yields; `SCHED_RR` uses the quantum
  from `sched_rr_get_interval`.
* Quantum length is a Configuration Manager tunable and is reported honestly by
  `sysconf(_SC_...)` where POSIX asks.

## Dispatcher objects

One wait primitive underneath everything:

```
wait(object_handles: &[Handle], wait_type: WaitAll | WaitAny,
     timeout: Option<Timeout>, alertable: bool) -> WaitStatus
```

* `WaitAny`/`WaitAll` over events, mutexes, semaphores, timers, threads, ports, and the
  POSIX subsystem's own waitables (fd readiness, `sigwait`, `waitpid`).
* `alertable` waits are how POSIX signal delivery interrupts a wait — the wait returns
  `Alerted`, `psx/` delivers the signal, and then decides `EINTR` vs restart per the
  `SA_RESTART` matrix.
* Mutexes are recursive with owner tracking (required for `PTHREAD_MUTEX_RECURSIVE`).
* Timers are one-shot or periodic with an optional DPC routine.

## Context switch

* Lazy FPU/SSE/AVX state save-restore keyed on a per-thread "used FPU" flag.
* Kernel stack switch via `TSS.RSP0` on the ring transition; per-CPU IST stacks for
  `#DF`, `#NMI`, `#MC`, and debug.
* Address space switch only when the process actually changes; PCID/ASID tags avoid TLB
  flushes.
* The switch happens with the run-queue lock held at `DISPATCH`, is measured by a
  cycle counter, and is a published benchmark target.

## Idle and power

Per-CPU idle thread runs `MWAIT`/`HLT` in an escalating ladder, with tickless idle
(`NO_HZ`-equivalent) when the next timer expiry is far enough away that programming the
deadline timer is cheaper than taking the tick.
