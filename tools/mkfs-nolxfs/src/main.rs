//! `mkfs.nolxfs` -- create a NOLXFS container.
//!
//! This tool and the kernel's mount path share `crates/nolxfs`, so the format cannot drift
//! between them. If the kernel can read what this writes, it is because they are literally
//! the same code.
//!
//! ```text
//! mkfs.nolxfs [options] <device-or-image>
//!
//!   -b, --block-size N    block size (1024, 2048, 4096, 8192, 16384); default 4096
//!   -L, --label NAME      volume name; default "nolx"
//!   -u, --uid N           owner of the root directory; default 0
//!   -g, --gid N           group of the root directory; default 0
//!   -m, --mode OCTAL      permission bits of the root directory; default 755
//!       --casefold        case-insensitive, case-preserving names
//!       --dry-run         plan and print the layout without writing anything
//!   -f, --force           do not ask before overwriting
//! ```

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use nolxfs::blockdev::{BlockDevice, DevError};
use nolxfs::mkfs::{self, FormatOptions};
use nolxfs::on_disk;

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

/// Parsed command line.
#[derive(Debug)]
struct Args {
    path: PathBuf,
    options: FormatOptions,
    dry_run: bool,
    force: bool,
}

fn usage(program: &str) {
    eprintln!(
        "usage: {program} [options] <device-or-image>\n\
         \n\
         \x20 -b, --block-size N   block size (1024..16384, power of two); default 4096\n\
         \x20 -L, --label NAME     volume name; default \"nolx\"\n\
         \x20 -u, --uid N          root directory owner; default 0\n\
         \x20 -g, --gid N          root directory group; default 0\n\
         \x20 -m, --mode OCTAL     root directory permissions; default 755\n\
         \x20     --casefold       case-insensitive, case-preserving names\n\
         \x20     --dry-run        print the layout without writing\n\
         \x20 -f, --force          do not ask before overwriting\n\
         \x20 -h, --help           this message"
    );
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut options = FormatOptions::default();
    let mut path: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut force = false;

    // A flag that consumed the following word, so the loop can skip it. Written as data
    // rather than as a closure because the closure would borrow the cursor mutably for as
    // long as it is in scope, which is the whole loop.
    let mut skip_next = false;

    let mut i = 1;
    while i < argv.len() {
        if skip_next {
            skip_next = false;
            i += 1;
            continue;
        }
        let arg = argv[i].as_str();

        // Options that take a value.
        let value_flag = matches!(
            arg,
            "-b" | "--block-size"
                | "-L"
                | "--label"
                | "-u"
                | "--uid"
                | "-g"
                | "--gid"
                | "-m"
                | "--mode"
        );
        let value = if value_flag {
            let v = argv
                .get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))?;
            skip_next = true;
            Some(v)
        } else {
            None
        };

        match (arg, value) {
            ("-h", _) | ("--help", _) => return Err(String::new()),
            ("-b", Some(v)) | ("--block-size", Some(v)) => {
                options.block_size = v.parse().map_err(|_| "block size is not a number")?;
            }
            ("-L", Some(v)) | ("--label", Some(v)) => options.volume_name = v,
            ("-u", Some(v)) | ("--uid", Some(v)) => {
                options.root_uid = v.parse().map_err(|_| "uid is not a number")?;
            }
            ("-g", Some(v)) | ("--gid", Some(v)) => {
                options.root_gid = v.parse().map_err(|_| "gid is not a number")?;
            }
            ("-m", Some(v)) | ("--mode", Some(v)) => {
                options.root_mode = u16::from_str_radix(v.trim_start_matches("0o"), 8)
                    .map_err(|_| "mode must be octal, e.g. 755")?;
            }
            ("--casefold", _) => options.casefold = true,
            ("--dry-run", _) => dry_run = true,
            ("-f", _) | ("--force", _) => force = true,
            (other, _) if other.starts_with('-') => {
                return Err(format!("unknown option {other}"));
            }
            (other, _) => {
                if path.is_some() {
                    return Err("more than one device given".to_string());
                }
                path = Some(PathBuf::from(other));
            }
        }
        i += 1;
    }

    Ok(Args {
        path: path.ok_or("no device given")?,
        options,
        dry_run,
        force,
    })
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let program = argv
        .first()
        .cloned()
        .unwrap_or_else(|| "mkfs.nolxfs".to_string());

    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("mkfs.nolxfs: {message}");
            }
            usage(&program);
            return ExitCode::from(if message.is_empty() { 0 } else { 2 });
        }
    };

    if !on_disk::is_valid_block_size(args.options.block_size) {
        eprintln!(
            "mkfs.nolxfs: block size {} is not a power of two between {} and {}",
            args.options.block_size,
            nolxfs::on_disk_gen::MIN_BLOCK_SIZE,
            nolxfs::on_disk_gen::MAX_BLOCK_SIZE
        );
        return ExitCode::FAILURE;
    }

    let mut device = match FileDevice::open(&args.path, args.options.block_size as usize) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("mkfs.nolxfs: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!(
        "mkfs.nolxfs: {} blocks of {} bytes ({} MiB) on {}",
        device.block_count(),
        device.block_size(),
        device.block_count() * device.block_size() as u64 / (1024 * 1024),
        args.path.display()
    );

    let layout = match mkfs::plan(args.options.block_size, device.block_count()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mkfs.nolxfs: {e}");
            return ExitCode::FAILURE;
        }
    };
    print_layout(&layout);

    if args.dry_run {
        println!("mkfs.nolxfs: dry run, nothing written");
        return ExitCode::SUCCESS;
    }

    if !args.force {
        eprint!(
            "mkfs.nolxfs: this will destroy everything on {}. Type YES to continue: ",
            args.path.display()
        );
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() || answer.trim() != "YES" {
            eprintln!("mkfs.nolxfs: aborted");
            return ExitCode::FAILURE;
        }
    }

    match mkfs::format(&mut device, &args.options) {
        Ok(layout) => {
            println!("mkfs.nolxfs: formatted \"{}\"", args.options.volume_name);
            println!(
                "mkfs.nolxfs: {} blocks available for data, starting at block {}",
                layout.block_count - layout.first_data_block,
                layout.first_data_block
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("mkfs.nolxfs: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_layout(layout: &mkfs::Layout) {
    println!("  container superblock      block 0");
    println!(
        "  checkpoint descriptors    block {} (+{})",
        layout.xp_desc_base, layout.xp_desc_blocks
    );
    println!(
        "  checkpoint data           block {} (+{})",
        layout.xp_data_base, layout.xp_data_blocks
    );
    println!(
        "  container object map      block {}",
        layout.container_omap
    );
    println!(
        "    ... B-tree root         block {}",
        layout.container_omap_tree
    );
    println!("  space manager             block {}", layout.spaceman);
    println!("  reaper                    block {}", layout.reaper);
    println!("  volume superblock         block {}", layout.volume_sb);
    println!("  volume object map         block {}", layout.volume_omap);
    println!(
        "    ... B-tree root         block {}",
        layout.volume_omap_tree
    );
    println!("  fs-tree root              block {}", layout.fs_tree_root);
    println!(
        "  extent reference tree     block {}",
        layout.extentref_tree_root
    );
    println!(
        "  snapshot metadata tree    block {}",
        layout.snap_meta_tree_root
    );
    println!("  first data block          {}", layout.first_data_block);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        let mut v = vec!["mkfs-nolxfs".to_string()];
        v.extend(parts.iter().map(|s| (*s).to_string()));
        v
    }

    #[test]
    fn defaults_are_applied_when_nothing_is_given() {
        let a = parse_args(&argv(&["/tmp/disk.img"])).unwrap();
        assert_eq!(a.path, PathBuf::from("/tmp/disk.img"));
        assert_eq!(a.options.block_size, 4096);
        assert_eq!(a.options.volume_name, "nolx");
        assert_eq!(a.options.root_mode, 0o755);
        assert!(!a.dry_run);
        assert!(!a.force);
    }

    #[test]
    fn every_option_is_parsed() {
        let a = parse_args(&argv(&[
            "-b",
            "8192",
            "-L",
            "backup",
            "-u",
            "1000",
            "-g",
            "1000",
            "-m",
            "0750",
            "--casefold",
            "--dry-run",
            "-f",
            "/dev/sdb",
        ]))
        .unwrap();
        assert_eq!(a.options.block_size, 8192);
        assert_eq!(a.options.volume_name, "backup");
        assert_eq!(a.options.root_uid, 1000);
        assert_eq!(a.options.root_gid, 1000);
        assert_eq!(a.options.root_mode, 0o750);
        assert!(a.options.casefold);
        assert!(a.dry_run);
        assert!(a.force);
        assert_eq!(a.path, PathBuf::from("/dev/sdb"));
    }

    #[test]
    fn long_and_short_forms_agree() {
        let short = parse_args(&argv(&["-b", "1024", "-L", "x", "/dev/a"])).unwrap();
        let long = parse_args(&argv(&["--block-size", "1024", "--label", "x", "/dev/a"])).unwrap();
        assert_eq!(short.options.block_size, long.options.block_size);
        assert_eq!(short.options.volume_name, long.options.volume_name);
    }

    #[test]
    fn a_missing_device_is_an_error() {
        let e = parse_args(&argv(&["-b", "4096"])).unwrap_err();
        assert_eq!(e, "no device given");
    }

    #[test]
    fn two_devices_are_rejected() {
        assert!(parse_args(&argv(&["/dev/a", "/dev/b"])).is_err());
    }

    #[test]
    fn an_unknown_option_is_rejected() {
        let e = parse_args(&argv(&["--frobnicate", "/dev/a"])).unwrap_err();
        assert!(e.contains("--frobnicate"));
    }

    #[test]
    fn an_option_without_a_value_is_rejected() {
        let e = parse_args(&argv(&["-b"])).unwrap_err();
        assert!(e.contains("needs a value"));
    }

    #[test]
    fn a_non_numeric_block_size_is_rejected() {
        assert!(parse_args(&argv(&["-b", "large", "/dev/a"])).is_err());
    }

    #[test]
    fn mode_is_read_as_octal_not_decimal() {
        let a = parse_args(&argv(&["-m", "755", "/dev/a"])).unwrap();
        assert_eq!(a.options.root_mode, 0o755);
        assert_ne!(a.options.root_mode, 755, "755 must not be read as decimal");
    }

    #[test]
    fn mode_accepts_a_zero_o_prefix() {
        let a = parse_args(&argv(&["-m", "0o700", "/dev/a"])).unwrap();
        assert_eq!(a.options.root_mode, 0o700);
    }

    #[test]
    fn help_is_not_an_error_message() {
        // `--help` returns an empty message, which main treats as "print usage, exit 0".
        assert_eq!(parse_args(&argv(&["--help"])).unwrap_err(), "");
    }
}
