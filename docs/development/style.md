# Coding style

## The rules that are enforced

| Rule | How it is enforced |
|---|---|
| `cargo fmt` | `make lint`, CI |
| No clippy warnings | `cargo clippy -- -D warnings`, CI |
| Every `unsafe` block carries a `// SAFETY:` comment naming its invariant | `scripts/check-unsafe.sh`, CI |
| Generated files are up to date | `gen.py --check`, CI |
| On-disk layout invariants hold | `test_layout.py`, CI |
| The boot layer links and its Multiboot header is valid | `make bootcheck`, CI |

Everything else in this document is a convention. Conventions are followed because they
make the code easier to read, and broken when following them would make it harder.

## Documentation

* Every public item has a doc comment. `#![deny(missing_docs)]` is set in every crate except
  the generated one, which opts out with a reason stated in the file.
* A doc comment that only restates the signature is worse than none, because it looks like
  documentation and is not. Write what the caller cannot see from the type: what errors
  mean, what the invariants are, what the surprising behaviour is.
* `# Errors`, `# Panics`, and `# Safety` sections are required on any function that can
  return `Err`, panic, or take `unsafe`. Clippy enforces the first two.
* When a design decision is arguable, the doc comment says so and says why the other option
  was rejected. A reader should not have to reconstruct the argument.

## `unsafe`

The kernel's safety budget is spent in three places: the HAL (ports and MSRs), the memory
manager (page tables and the frame map), and the Object Manager's raw pointer paths. Nowhere
else, and not because the rest of the kernel is trivial.

Every block states its invariant:

```rust
// SAFETY: `xp_desc_base` and `xp_desc_blocks` come from the container superblock, which
// `validate_container` has already checked to lie inside the device. The addition cannot
// overflow because both were bounds-checked against `block_count`.
```

A `SAFETY` comment that says "this is safe because we checked" is not a comment, it is a
restatement. Name the invariant and where it was established.

## Errors

* Kernel-internal code returns `Result<T, FsError>` or an executive status type.
* POSIX `errno` appears only at the POSIX subsystem boundary. A filesystem driver that
  returns `ENOENT` has leaked a POSIX concept into a place that has no business knowing
  about it.
* Errors carry context. `FsError::TooShort { have, need }` is debuggable; `FsError::Bad` is
  not.
* Nothing in the kernel panics on malformed input from a device or from user space. A
  malformed filesystem image must produce an error and a refusal to mount. The fuzzing
  requirement in ROADMAP M13 is exactly this, stated as a test.

## Naming

* Types are `CamelCase`; on-disk structures keep their C names (`nolx_inode_t`) so the Rust,
  the C header, and the specification all say the same thing.
* Constants that mirror the specification keep the specification's names: `EINTR`,
  `S_IFDIR`, `NAME_MAX`. Inventing a prettier name for a standardised constant makes the
  code harder to check against the document.
* Internal executive names follow the Windows-style convention the architecture borrows
  (`Iorb`, `ObjectHeader`, `DriverObject`), because that is what the design documents call
  them and because a reader with Windows Internals to hand should not have to translate.
* Booleans read as assertions: `is_dir`, `has_syscall`, `from_user_mode`.

## Tests

* Tests are named for the behaviour they pin, not the function they call:
  `dotdot_at_the_root_is_the_root`, not `test_resolve_5`.
* A test that asserts a value the implementation happens to produce is a change detector.
  A test that asserts a property the specification requires is a test. Prefer the second,
  and when you must write the first, say in a comment why the value matters.
* Negative tests are as important as positive ones, especially at the parsing boundary.
  `decode_rejects_a_node_that_is_not_even_a_valid_object` has caught more bugs than the
  round-trip tests have.
* Property tests over the allocator, the VAD tree, and the B-tree are worth more than
  examples, because the failure modes of those structures are combinatorial.

## Comments that earn their place

* Why, not what. The code says what.
* A comment that would become wrong if the code changed is a test waiting to be written.
* `TODO` names a milestone: `// TODO(M8b): merge under-filled siblings`. A `TODO` with no
  owner is a note to nobody.

## Things this project does not do

* No `unwrap()` or `expect()` outside tests and `main`. A kernel panic is a machine down.
* No integer arithmetic on sizes or offsets without `checked_*` at a boundary where the
  input is not already validated.
* No `as` casts that can truncate, except where a comment says the truncation is the point.
* No `unsafe` block that could be a safe one with a small amount of restructuring. The
  restructuring is always cheaper than the review, forever after.
