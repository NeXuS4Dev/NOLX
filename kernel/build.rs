//! Assembles the boot trampoline and interrupt stubs for the freestanding kernel target.
//!
//! The assembly is kept out of the Rust host-test path: `make bootcheck` assembles the
//! Multiboot trampoline without Rust, while this script only adds hardware entry code to the
//! x86-64 kernel image.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    if target != "x86_64-unknown-none" {
        // Host tests exercise nolx-hal-core and do not need privileged machine code.
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let sources = [
        manifest.join("../boot/boot.S"),
        manifest.join("src/hal/interrupts.S"),
    ];

    for source in sources {
        let name = source
            .file_stem()
            .expect("assembly source has a file name")
            .to_string_lossy();
        let object = out_dir.join(format!("{name}.o"));

        println!("cargo:rerun-if-changed={}", source.display());
        println!("cargo:rerun-if-changed=linker.ld");

        let status = Command::new("as")
            .arg("--64")
            .arg("--fatal-warnings")
            .arg("-o")
            .arg(&object)
            .arg(&source)
            .status()
            .expect("failed to run `as`; binutils is required to build the kernel");
        if !status.success() {
            panic!("`as` could not assemble {}", source.display());
        }

        // Each object is linked directly into the kernel binary. `.multiboot` remains
        // retained by the linker script's KEEP rule; ISR stubs are retained because the
        // live IDT references their addresses through `nolx_isr_stub_table`.
        println!("cargo:rustc-link-arg={}", object.display());
    }
}
