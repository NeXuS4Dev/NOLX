//! Assembles the boot trampoline and hands it to the linker.
//!
//! `boot/boot.S` is what the Multiboot loader jumps to: the header, the switch from
//! protected mode to long mode, the GDT, and the initial page tables. Without it the kernel
//! is an ELF file with nowhere to start, and `kernel/linker.ld` will refuse to link it --
//! the script asserts that `.multiboot` is present precisely so that this cannot be
//! forgotten.
//!
//! Assembling it here rather than in a separate `make` step means there is exactly one
//! kernel image and it is always complete. `scripts/check-boot-layer.sh` still assembles it
//! separately, because that check has to run on a machine with no Rust toolchain at all.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    if target != "x86_64-unknown-none" {
        // A host build has no use for a trampoline, and `as --64` output would not link.
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let source = manifest.join("../boot/boot.S");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let object = out_dir.join("boot.o");

    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed=linker.ld");

    // GAS syntax, assembled 64-bit. The file carries its own `.code32`/`.code64`
    // directives, so the whole trampoline -- including the part that runs before long mode
    // exists -- comes out of one object file.
    let status = Command::new("as")
        .arg("--64")
        .arg("-o")
        .arg(&object)
        .arg(&source)
        .status()
        .expect("failed to run `as`; binutils is required to build the kernel");
    if !status.success() {
        panic!("`as` could not assemble {}", source.display());
    }

    // Appended to the link line. Order does not matter for correctness because
    // `linker.ld` marks `.multiboot` as KEEP, so the garbage collector cannot drop it.
    println!("cargo:rustc-link-arg={}", object.display());
}
