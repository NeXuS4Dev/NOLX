//! `nolxfs-dump` -- inspect a NOLXFS container without mounting it.
//!
//! This is the FUSE-less host mounter: it reads a container image directly and
//! offers the read-only shell a mounter would.
//!
//! ```text
//! nolxfs-dump <image>                  full container dump (default action)
//! nolxfs-dump <image> dump             same, explicitly
//! nolxfs-dump <image> ls [path]        list a directory (default /)
//! nolxfs-dump <image> cat <path>       print a file's contents (or a symlink target)
//! nolxfs-dump <image> stat <path>      print one inode's metadata
//! nolxfs-dump <image> snapshots        list snapshots
//! ```

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use nolxfs::blockdev::{BlockDevice, DevError};
use nolxfs::on_disk_gen as gen;
use nolxfs::volume::Volume;

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

fn open_image(path: &PathBuf) -> Result<FileDevice, String> {
    FileDevice::open(path, 4096)
}

fn resolve(vol: &Volume<FileDevice>, path: &str) -> Result<u64, String> {
    let mut ino = gen::ROOT_INO;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        ino = vol
            .lookup(ino, part)
            .map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(ino)
}

fn kind_of(mode: u32) -> &'static str {
    match mode & 0o170_000 {
        0o040_000 => "directory",
        0o120_000 => "symlink",
        0o100_000 => "regular file",
        _ => "other",
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let (Some(image), action) = (argv.get(1), argv.get(2).map(String::as_str)) else {
        eprintln!("usage: nolxfs-dump <image> [dump|ls|cat|stat|snapshots] [path]");
        return ExitCode::from(2);
    };
    let path = PathBuf::from(image);
    match action {
        None | Some("dump") => {
            let mut dev = match open_image(&path) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("nolxfs-dump: {e}");
                    return ExitCode::from(2);
                }
            };
            match nolxfs::dump::dump_to_string(&mut dev) {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::from(0)
                }
                Err(e) => {
                    eprintln!("nolxfs-dump: {e}");
                    ExitCode::from(2)
                }
            }
        }
        Some(action) => {
            let dev = match open_image(&path) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("nolxfs-dump: {e}");
                    return ExitCode::from(2);
                }
            };
            let vol = match Volume::mount(dev) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("nolxfs-dump: cannot read {}: {e}", path.display());
                    return ExitCode::from(2);
                }
            };
            let target = argv.get(3).map_or("/", String::as_str);
            match action {
                "snapshots" => {
                    for snap in vol.snapshots() {
                        println!("{} (xid {})", snap.name, snap.xid);
                    }
                    ExitCode::from(0)
                }
                "ls" => match resolve(&vol, target).and_then(|ino| {
                    vol.readdir(ino).map_err(|e| format!("{target}: {e}"))
                }) {
                    Ok(entries) => {
                        for (name, ino) in entries {
                            println!("{name}  (ino {ino})");
                        }
                        ExitCode::from(0)
                    }
                    Err(e) => {
                        eprintln!("nolxfs-dump: {e}");
                        ExitCode::from(1)
                    }
                },
                "stat" => match resolve(&vol, target).and_then(|ino| {
                    vol.read_inode(ino)
                        .map(|inode| (ino, inode))
                        .map_err(|e| format!("{target}: {e}"))
                }) {
                    Ok((ino, inode)) => {
                        let mode = u32::from(inode.mode);
                        println!("path:  {target}");
                        println!("inode: {ino}");
                        println!("type:  {}", kind_of(mode));
                        println!("mode:  {mode:o}");
                        println!("nlink: {}", inode.nlink);
                        println!("uid:   {} gid: {}", inode.uid, inode.gid);
                        println!("size:  {}", Volume::<FileDevice>::file_size(&inode));
                        println!("create_time: {} change_time: {}", inode.create_time, inode.change_time);
                        ExitCode::from(0)
                    }
                    Err(e) => {
                        eprintln!("nolxfs-dump: {e}");
                        ExitCode::from(1)
                    }
                },
                "cat" => match resolve(&vol, target).and_then(|ino| {
                    let inode = vol.read_inode(ino).map_err(|e| format!("{target}: {e}"))?;
                    if u32::from(inode.mode) & 0o170_000 == 0o120_000 {
                        vol.readlink(ino).map_err(|e| format!("{target}: {e}"))
                    } else {
                        let size = usize::try_from(Volume::<FileDevice>::file_size(&inode))
                            .map_err(|_| format!("{target}: too large"))?;
                        let mut buf = vec![0u8; size];
                        vol.read_data(ino, 0, &mut buf)
                            .map_err(|e| format!("{target}: {e}"))?;
                        Ok(buf)
                    }
                }) {
                    Ok(bytes) => {
                        use std::io::Write as _;
                        let mut stdout = std::io::stdout();
                        let _ = stdout.write_all(&bytes);
                        ExitCode::from(0)
                    }
                    Err(e) => {
                        eprintln!("nolxfs-dump: {e}");
                        ExitCode::from(1)
                    }
                },
                other => {
                    eprintln!("nolxfs-dump: unknown action {other}");
                    ExitCode::from(2)
                }
            }
        }
    }
}
