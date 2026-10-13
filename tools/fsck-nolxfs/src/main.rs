//! `fsck.nolxfs` -- check and optionally repair a NOLXFS container.
//!
//! ```text
//! fsck.nolxfs [options] <device-or-image>
//!
//!   -y, --repair   repair the safe damage classes: torn checkpoint
//!                  descriptors, nlink drift, counter drift, bitmap mismatches
//!   --force-repair alias for -y kept for mkfs-style flag habits
//!   -b, --block-size N  block size; default 4096
//! ```
//!
//! Exit status: 0 when the volume is clean, 1 when issues were found (and, in
//! repair mode, fixed where possible), 2 when the container could not even be
//! probed.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use nolxfs::blockdev::{BlockDevice, DevError};
use nolxfs::fsck;

/// A block device backed by a file, which is what a disk image is.
struct FileDevice {
    file: File,
    block_size: usize,
    block_count: u64,
}

impl FileDevice {
    fn open(path: &PathBuf, block_size: usize) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let length = file
            .metadata()
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
            .len();
        if length == 0 {
            return Err(format!(
                "{} is empty; create the image first",
                path.display()
            ));
        }
        if length % block_size as u64 != 0 {
            return Err(format!(
                "{} is {length} bytes, not a whole number of {block_size}-byte blocks",
                path.display()
            ));
        }
        Ok(Self {
            file,
            block_size,
            block_count: length / block_size as u64,
        })
    }
}

impl BlockDevice for FileDevice {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_block(&self, index: u64, buf: &mut [u8]) -> Result<(), DevError> {
        if buf.len() != self.block_size {
            return Err(DevError::BadBufferLen {
                got: buf.len(),
                want: self.block_size,
            });
        }
        if index >= self.block_count {
            return Err(DevError::OutOfRange { block: index });
        }
        // `File` is behind a shared reference here because `BlockDevice::read_block` takes
        // `&self`. `File::read_at`-style positioned reads would be the clean answer; a
        // `&File` seek-and-read is what portable std offers, so the file is wrapped in a
        // lock by the caller's single-threaded use.
        // `read_block` takes `&self`, so a positioned read needs a second handle onto the
        // same file. `try_clone` gives one without exposing interior mutability.
        let mut file = self.file.try_clone().map_err(|_| DevError::Io(5))?;
        file.seek(SeekFrom::Start(index * self.block_size as u64))
            .map_err(|_| DevError::Io(5))?;
        file.read_exact(buf).map_err(|_| DevError::Io(5))
    }

    fn write_block(&mut self, index: u64, buf: &[u8]) -> Result<(), DevError> {
        if buf.len() != self.block_size {
            return Err(DevError::BadBufferLen {
                got: buf.len(),
                want: self.block_size,
            });
        }
        if index >= self.block_count {
            return Err(DevError::OutOfRange { block: index });
        }
        self.file
            .seek(SeekFrom::Start(index * self.block_size as u64))
            .map_err(|_| DevError::Io(5))?;
        self.file.write_all(buf).map_err(|_| DevError::Io(5))
    }

    fn flush(&mut self) -> Result<(), DevError> {
        // `sync_all` rather than `sync_data`: NOLXFS's commit point is a metadata block,
        // and its contents matter as much as a file's data does.
        self.file.sync_all().map_err(|_| DevError::Io(5))
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let mut repair = false;
    let mut block_size = 4096usize;
    let mut path: Option<PathBuf> = None;
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "-y" | "--repair" | "--force-repair" => repair = true,
            "-b" | "--block-size" => {
                i += 1;
                block_size = argv.get(i).and_then(|v| v.parse().ok()).unwrap_or(4096);
            }
            "-h" | "--help" => {
                eprintln!("usage: fsck.nolxfs [-y|--repair] <device-or-image>");
                return ExitCode::from(0);
            }
            other if other.starts_with('-') => {
                eprintln!("fsck.nolxfs: unknown option {other}");
                return ExitCode::from(2);
            }
            other => {
                if path.is_some() {
                    eprintln!("fsck.nolxfs: more than one device given");
                    return ExitCode::from(2);
                }
                path = Some(PathBuf::from(other));
            }
        }
        i += 1;
    }
    let Some(path) = path else {
        eprintln!("usage: fsck.nolxfs [-y|--repair] <device-or-image>");
        return ExitCode::from(2);
    };
    let mut dev = match FileDevice::open(&path, block_size) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fsck.nolxfs: {e}");
            return ExitCode::from(2);
        }
    };
    match fsck::check(&mut dev, repair) {
        Ok(report) => {
            for issue in &report.issues {
                println!("FSCK: {issue}");
            }
            for fix in &report.repairs {
                println!("FSCK: repaired: {fix}");
            }
            let files = report.num_files;
            let dirs = report.num_dirs;
            let x = if report.has_checkpoint { "checkpointed" } else { "genesis" };
            println!(
                "fsck.nolxfs: xid {} ({x}), {} blocks checked, {files} files, {dirs} directories, {} issue(s), {} repair(s)",
                report.xid,
                report.checked_blocks,
                report.issues.len(),
                report.repairs.len(),
            );
            if report.issues.is_empty() || !report.repairs.is_empty() && report.issues.iter().all(|i| {
                matches!(i, nolxfs::fsck::FsckIssue::TornCheckpoint { .. }
                    | nolxfs::fsck::FsckIssue::NlinkMismatch { .. }
                    | nolxfs::fsck::FsckIssue::CounterDrift { .. }
                    | nolxfs::fsck::FsckIssue::BitmapMismatch { .. })
            }) {
                ExitCode::from(0)
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("fsck.nolxfs: cannot check {}: {e}", path.display());
            ExitCode::from(2)
        }
    }
}
