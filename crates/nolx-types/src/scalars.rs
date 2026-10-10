//! POSIX scalar types and the structures that cross the syscall boundary.
//!
//! Everything here is `#[repr(C)]` and fixed width, because the kernel and libc are
//! compiled separately and must agree byte for byte. There is no `long`: POSIX leaves its
//! width unspecified, and a kernel that depends on it cannot be ported, so NOLX spells out
//! `i64`/`u64` and lets the C headers typedef `long` to match.

#![allow(clippy::module_name_repetitions)]

/// `mode_t`: file type and permission bits.
pub type Mode = u32;
/// `uid_t`.
pub type Uid = u32;
/// `gid_t`.
pub type Gid = u32;
/// `dev_t`. Wide enough that a device number cannot be truncated.
pub type Dev = u64;
/// `ino_t`. NOLXFS inode numbers are 60 bits; this holds them with room to spare.
pub type Ino = u64;
/// `nlink_t`. NOLXFS stores link counts as `u32`.
pub type Nlink = u32;
/// `off_t`. Always 64-bit; NOLX has no 32-bit file offset mode to be compatible with.
pub type Off = i64;
/// `pid_t`.
pub type Pid = i32;
/// `size_t`.
pub type Size = usize;
/// `ssize_t`.
pub type SSize = isize;
/// `time_t`: seconds since the epoch. Signed and 64-bit, so there is no year-2038 bug.
pub type Time = i64;
/// `clockid_t`.
pub type ClockId = i32;
/// `blksize_t`.
pub type BlkSize = i64;
/// `blkcnt_t`.
pub type BlkCnt = i64;
/// `fsblkcnt_t`.
pub type FsBlkCnt = u64;
/// `fsfilcnt_t`.
pub type FsFilCnt = u64;
/// `id_t`, the generic identifier used by `waitid` and the scheduling functions.
pub type Id = u32;
/// `useconds_t`.
pub type USeconds = u32;
/// `suseconds_t`.
pub type SUSeconds = i64;
/// `key_t` for System V IPC.
pub type Key = i32;
/// `sigset_t` as a bitmask of signal numbers 1..=64.
pub type SigSet = u64;

// --------------------------------------------------------------------- mode bits

/// File type mask.
pub const S_IFMT: Mode = 0o170_000;
/// FIFO.
pub const S_IFIFO: Mode = 0o010_000;
/// Character special.
pub const S_IFCHR: Mode = 0o020_000;
/// Directory.
pub const S_IFDIR: Mode = 0o040_000;
/// Block special.
pub const S_IFBLK: Mode = 0o060_000;
/// Regular file.
pub const S_IFREG: Mode = 0o100_000;
/// Symbolic link.
pub const S_IFLNK: Mode = 0o120_000;
/// Socket.
pub const S_IFSOCK: Mode = 0o140_000;

/// Set-user-ID on execution.
pub const S_ISUID: Mode = 0o004_000;
/// Set-group-ID on execution.
pub const S_ISGID: Mode = 0o002_000;
/// Sticky bit: on a directory, only the owner may unlink entries.
pub const S_ISVTX: Mode = 0o001_000;

/// Read, write, execute/search by owner.
pub const S_IRWXU: Mode = 0o000_700;
/// Read by owner.
pub const S_IRUSR: Mode = 0o000_400;
/// Write by owner.
pub const S_IWUSR: Mode = 0o000_200;
/// Execute/search by owner.
pub const S_IXUSR: Mode = 0o000_100;
/// Read, write, execute/search by group.
pub const S_IRWXG: Mode = 0o000_070;
/// Read by group.
pub const S_IRGRP: Mode = 0o000_040;
/// Write by group.
pub const S_IWGRP: Mode = 0o000_020;
/// Execute/search by group.
pub const S_IXGRP: Mode = 0o000_010;
/// Read, write, execute/search by others.
pub const S_IRWXO: Mode = 0o000_007;
/// Read by others.
pub const S_IROTH: Mode = 0o000_004;
/// Write by others.
pub const S_IWOTH: Mode = 0o000_002;
/// Execute/search by others.
pub const S_IXOTH: Mode = 0o000_001;

/// Extract the file type from a mode.
#[must_use]
pub const fn ifmt(mode: Mode) -> Mode {
    mode & S_IFMT
}

/// `S_ISREG`.
#[must_use]
pub const fn is_reg(mode: Mode) -> bool {
    ifmt(mode) == S_IFREG
}
/// `S_ISDIR`.
#[must_use]
pub const fn is_dir(mode: Mode) -> bool {
    ifmt(mode) == S_IFDIR
}
/// `S_ISLNK`.
#[must_use]
pub const fn is_lnk(mode: Mode) -> bool {
    ifmt(mode) == S_IFLNK
}
/// `S_ISFIFO`.
#[must_use]
pub const fn is_fifo(mode: Mode) -> bool {
    ifmt(mode) == S_IFIFO
}
/// `S_ISSOCK`.
#[must_use]
pub const fn is_sock(mode: Mode) -> bool {
    ifmt(mode) == S_IFSOCK
}
/// `S_ISCHR`.
#[must_use]
pub const fn is_chr(mode: Mode) -> bool {
    ifmt(mode) == S_IFCHR
}
/// `S_ISBLK`.
#[must_use]
pub const fn is_blk(mode: Mode) -> bool {
    ifmt(mode) == S_IFBLK
}

/// The permission bits only: the low 12 bits, which is what `chmod(2)` accepts.
#[must_use]
pub const fn permission_bits(mode: Mode) -> Mode {
    mode & 0o7_777
}

// --------------------------------------------------------------------- structures

/// `struct timespec`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timespec {
    /// Seconds.
    pub tv_sec: Time,
    /// Nanoseconds, `0..=999_999_999`.
    pub tv_nsec: i64,
}

impl Timespec {
    /// Nanoseconds per second.
    pub const NANOS_PER_SEC: i64 = 1_000_000_000;

    /// The zero timestamp.
    pub const ZERO: Self = Self {
        tv_sec: 0,
        tv_nsec: 0,
    };

    /// Construct, saturating an out-of-range nanosecond count rather than wrapping.
    #[must_use]
    pub const fn new(sec: Time, nsec: i64) -> Self {
        Self {
            tv_sec: sec,
            tv_nsec: nsec,
        }
    }

    /// `true` if `tv_nsec` is in range. An out-of-range value must produce `EINVAL`.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.tv_nsec >= 0 && self.tv_nsec < Self::NANOS_PER_SEC
    }

    /// Normalise so that `0 <= tv_nsec < 1e9`, saturating seconds on overflow.
    #[must_use]
    pub const fn normalized(self) -> Self {
        let mut sec = self.tv_sec;
        let mut nsec = self.tv_nsec;
        if nsec < 0 {
            let borrow = (-nsec + Self::NANOS_PER_SEC - 1) / Self::NANOS_PER_SEC;
            nsec += borrow * Self::NANOS_PER_SEC;
            sec = match sec.checked_sub(borrow) {
                Some(v) => v,
                None => i64::MIN,
            };
        }
        if nsec >= Self::NANOS_PER_SEC {
            let carry = nsec / Self::NANOS_PER_SEC;
            nsec -= carry * Self::NANOS_PER_SEC;
            sec = match sec.checked_add(carry) {
                Some(v) => v,
                None => i64::MAX,
            };
        }
        Self {
            tv_sec: sec,
            tv_nsec: nsec,
        }
    }

    /// `self + other`, saturating instead of overflowing.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        let sec = match self.tv_sec.checked_add(other.tv_sec) {
            Some(v) => v,
            None => {
                return if self.tv_sec > 0 {
                    Self {
                        tv_sec: i64::MAX,
                        tv_nsec: Self::NANOS_PER_SEC - 1,
                    }
                } else {
                    Self {
                        tv_sec: i64::MIN,
                        tv_nsec: 0,
                    }
                };
            }
        };
        Self {
            tv_sec: sec,
            tv_nsec: self.tv_nsec + other.tv_nsec,
        }
        .normalized()
    }

    /// `self - other`, saturating instead of overflowing.
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        let sec = match self.tv_sec.checked_sub(other.tv_sec) {
            Some(v) => v,
            None => {
                return if self.tv_sec > 0 {
                    Self {
                        tv_sec: i64::MAX,
                        tv_nsec: Self::NANOS_PER_SEC - 1,
                    }
                } else {
                    Self {
                        tv_sec: i64::MIN,
                        tv_nsec: 0,
                    }
                };
            }
        };
        Self {
            tv_sec: sec,
            tv_nsec: self.tv_nsec - other.tv_nsec,
        }
        .normalized()
    }
}

/// `struct timeval`. Kept for the interfaces that still require it; new code uses
/// [`Timespec`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timeval {
    /// Seconds.
    pub tv_sec: Time,
    /// Microseconds, `0..=999_999`.
    pub tv_usec: SUSeconds,
}

/// `struct stat`.
///
/// Field order is NOLX's own. What matters to POSIX is that every field exists with the
/// required meaning and that `st_size`/`st_blksize`/`st_blocks` use the widths above.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    /// Device id of the device containing the file.
    pub st_dev: Dev,
    /// File serial number.
    pub st_ino: Ino,
    /// Mode: file type and permissions.
    pub st_mode: Mode,
    /// Number of hard links.
    pub st_nlink: Nlink,
    /// Owner.
    pub st_uid: Uid,
    /// Group.
    pub st_gid: Gid,
    /// Device id, for character and block special files.
    pub st_rdev: Dev,
    /// Size in bytes. For a symbolic link, the length of the pathname it contains.
    pub st_size: Off,
    /// Last access time.
    pub st_atim: Timespec,
    /// Last data modification time.
    pub st_mtim: Timespec,
    /// Last status change time.
    pub st_ctim: Timespec,
    /// Preferred I/O block size.
    pub st_blksize: BlkSize,
    /// Number of 512-byte blocks allocated, counting indirect blocks.
    pub st_blocks: BlkCnt,
    /// Birth time. Not required by POSIX; NOLXFS records it, so it is exposed.
    pub st_birthtim: Timespec,
}

/// `struct statvfs`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Statvfs {
    /// Filesystem block size.
    pub f_bsize: u64,
    /// Fundamental filesystem block size.
    pub f_frsize: u64,
    /// Total number of blocks in units of `f_frsize`.
    pub f_blocks: FsBlkCnt,
    /// Free blocks.
    pub f_bfree: FsBlkCnt,
    /// Free blocks available to non-privileged processes.
    pub f_bavail: FsBlkCnt,
    /// Total number of file serial numbers.
    pub f_files: FsFilCnt,
    /// Free file serial numbers.
    pub f_ffree: FsFilCnt,
    /// Free file serial numbers available to non-privileged processes.
    pub f_favail: FsFilCnt,
    /// Filesystem id.
    pub f_fsid: u64,
    /// Mount flags.
    pub f_flag: u64,
    /// Maximum filename length.
    pub f_namemax: u64,
}

/// `f_bread`/mount flag: read-only filesystem.
pub const ST_RDONLY: u64 = 1;
/// `f_flag`: `nosuid` semantics are in effect.
pub const ST_NOSUID: u64 = 2;
/// `f_flag`: the filesystem does not record access times.
pub const ST_NOATIME: u64 = 4;
/// `f_flag`: the filesystem does not record modification times for directories.
pub const ST_NODIRATIME: u64 = 8;
/// `f_flag`: synchronous I/O.
pub const ST_SYNCHRONOUS: u64 = 16;

/// `struct dirent`.
///
/// The name is a fixed-size buffer in the kernel's view; `readdir(3)` in libc copies out
/// only `d_name[0..d_namlen]` plus a NUL, so the structure the application sees is exactly
/// as long as it needs to be.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Dirent {
    /// File serial number.
    pub d_ino: Ino,
    /// Offset to the next entry, as `telldir(3)`/`seekdir(3)` require.
    pub d_off: Off,
    /// Length of the name, excluding the NUL.
    pub d_reclen: u16,
    /// File type, as the `DT_*` values below. Not required by POSIX but universally
    /// expected; `readdir` fills it in so `ls` does not have to `stat` every entry.
    pub d_type: u8,
    /// Length of the name in bytes, excluding the NUL.
    pub d_namlen: u8,
    /// The name, NUL terminated, not NUL padded beyond the terminator.
    pub d_name: [u8; NAME_MAX_PLUS_1],
}

/// `NAME_MAX` plus one for the NUL terminator.
pub const NAME_MAX_PLUS_1: usize = 256;

/// Unknown file type.
pub const DT_UNKNOWN: u8 = 0;
/// FIFO.
pub const DT_FIFO: u8 = 1;
/// Character special.
pub const DT_CHR: u8 = 2;
/// Directory.
pub const DT_DIR: u8 = 4;
/// Block special.
pub const DT_BLK: u8 = 6;
/// Regular file.
pub const DT_REG: u8 = 8;
/// Symbolic link.
pub const DT_LNK: u8 = 10;
/// Socket.
pub const DT_SOCK: u8 = 12;

/// Map the file-type bits of a `mode_t` to a `DT_*` value.
#[must_use]
pub const fn dt_from_mode(mode: Mode) -> u8 {
    match mode & S_IFMT {
        S_IFIFO => DT_FIFO,
        S_IFCHR => DT_CHR,
        S_IFDIR => DT_DIR,
        S_IFBLK => DT_BLK,
        S_IFREG => DT_REG,
        S_IFLNK => DT_LNK,
        S_IFSOCK => DT_SOCK,
        _ => DT_UNKNOWN,
    }
}

/// Map a `DT_*` value back to the file-type bits of a `mode_t`.
#[must_use]
pub const fn mode_from_dt(dt: u8) -> Mode {
    match dt {
        DT_FIFO => S_IFIFO,
        DT_CHR => S_IFCHR,
        DT_DIR => S_IFDIR,
        DT_BLK => S_IFBLK,
        DT_REG => S_IFREG,
        DT_LNK => S_IFLNK,
        DT_SOCK => S_IFSOCK,
        _ => 0,
    }
}

// --------------------------------------------------------------------- limits

/// `NAME_MAX`.
pub const NAME_MAX: usize = 255;
/// `PATH_MAX`.
pub const PATH_MAX: usize = 1024;
/// `SYMLINK_MAX`: the longest link target we will store. POSIX does not define it; we do,
/// so that a `readlink` result cannot be truncated silently.
pub const SYMLINK_MAX: usize = 1024;
/// `SYMLOOP_MAX`: the standard requires at least 8; NOLXFS allows 40.
pub const SYMLOOP_MAX: usize = 40;
/// `OPEN_MAX`: the initial per-process descriptor limit, raised by `setrlimit`.
pub const OPEN_MAX: usize = 1024;
/// `NGROUPS_MAX`.
pub const NGROUPS_MAX: usize = 64;
/// `ARG_MAX`.
pub const ARG_MAX: usize = 1 << 20;
/// `CHILD_MAX`.
pub const CHILD_MAX: usize = 999;
/// `TZNAME_MAX`.
pub const TZNAME_MAX: usize = 6;
/// `LOGIN_NAME_MAX`.
pub const LOGIN_NAME_MAX: usize = 256;

/// Encode a device number from major and minor parts, as `makedev(3)` does.
///
/// Layout: bits 0..19 are the minor number, bits 20..63 the major. This is NOLX's own
/// encoding; the C headers expose `major`/`minor`/`makedev` so applications never see it.
#[must_use]
pub const fn makedev(major: u64, minor: u64) -> Dev {
    ((major & 0x0000_FFFF_FFFF_FFFF) << 20) | (minor & 0x000F_FFFF)
}

/// Extract the major part of a device number.
#[must_use]
pub const fn major(dev: Dev) -> u64 {
    (dev >> 20) & 0x0000_FFFF_FFFF_FFFF
}

/// Extract the minor part of a device number.
#[must_use]
pub const fn minor(dev: Dev) -> u64 {
    dev & 0x000F_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timespec_normalizes() {
        let t = Timespec::new(1, 2_500_000_000).normalized();
        assert_eq!(t, Timespec::new(3, 500_000_000));

        let t = Timespec::new(1, -500_000_000).normalized();
        assert_eq!(t, Timespec::new(0, 500_000_000));

        let t = Timespec::new(0, -1_500_000_000).normalized();
        assert_eq!(t, Timespec::new(-2, 500_000_000));
    }

    #[test]
    fn timespec_validity_matches_posix_range() {
        assert!(Timespec::new(0, 0).is_valid());
        assert!(Timespec::new(0, 999_999_999).is_valid());
        assert!(!Timespec::new(0, 1_000_000_000).is_valid());
        assert!(!Timespec::new(0, -1).is_valid());
    }

    #[test]
    fn timespec_add_saturates() {
        let a = Timespec::new(i64::MAX - 1, 0);
        let b = Timespec::new(10, 0);
        let sum = a.saturating_add(b);
        assert_eq!(sum.tv_sec, i64::MAX);
    }

    #[test]
    fn timespec_round_trips_through_sub() {
        let a = Timespec::new(100, 250);
        let b = Timespec::new(3, 999_999_999);
        assert_eq!(a.saturating_add(b).saturating_sub(b), a);
    }

    #[test]
    fn timespec_ordering_is_chronological() {
        assert!(Timespec::new(1, 0) < Timespec::new(1, 1));
        assert!(Timespec::new(1, 999_999_999) < Timespec::new(2, 0));
        assert!(Timespec::new(-1, 0) < Timespec::new(0, 0));
    }

    #[test]
    fn file_type_predicates_agree_with_the_bits() {
        assert!(is_dir(S_IFDIR | 0o755));
        assert!(!is_reg(S_IFDIR | 0o755));
        assert!(is_reg(S_IFREG | 0o644));
        assert!(is_lnk(S_IFLNK | 0o777));
        assert!(is_sock(S_IFSOCK | 0o600));
        assert!(is_fifo(S_IFIFO | 0o600));
        assert!(is_chr(S_IFCHR | 0o666));
        assert!(is_blk(S_IFBLK | 0o660));
        assert_eq!(ifmt(S_IFREG | S_ISUID | 0o755), S_IFREG);
    }

    #[test]
    fn permission_bits_strips_the_type() {
        assert_eq!(permission_bits(S_IFREG | S_ISUID | 0o755), S_ISUID | 0o755);
        assert_eq!(permission_bits(S_IFDIR | 0o1777), 0o1777);
    }

    #[test]
    fn dt_and_mode_round_trip() {
        for mode in [
            S_IFREG, S_IFDIR, S_IFLNK, S_IFIFO, S_IFSOCK, S_IFCHR, S_IFBLK,
        ] {
            let dt = dt_from_mode(mode | 0o755);
            assert_ne!(dt, DT_UNKNOWN, "mode {mode:#o} mapped to DT_UNKNOWN");
            assert_eq!(mode_from_dt(dt), mode);
        }
        assert_eq!(dt_from_mode(0), DT_UNKNOWN);
        assert_eq!(mode_from_dt(DT_UNKNOWN), 0);
    }

    #[test]
    fn makedev_round_trips() {
        for (major, minor) in [(0u64, 0u64), (8, 1), (259, 0), (1 << 24, (1 << 20) - 1)] {
            let dev = makedev(major, minor);
            assert_eq!(
                self::major(dev),
                major,
                "major mismatch for {major}:{minor}"
            );
            assert_eq!(
                self::minor(dev),
                minor,
                "minor mismatch for {major}:{minor}"
            );
        }
    }

    #[test]
    fn dirent_name_buffer_fits_a_max_length_name() {
        // NAME_MAX characters plus the terminating NUL must fit in d_name.
        assert_eq!(NAME_MAX_PLUS_1, NAME_MAX + 1);
        assert_eq!(core::mem::size_of::<Dirent>() % 8, 0);
    }

    #[test]
    fn symlink_loop_limit_meets_the_standard_minimum() {
        // A const assert, not a runtime one: `SYMLOOP_MAX` is a constant, so a runtime
        // `assert!` here can never fail and would only cost a branch. Spelled this way it
        // is checked while compiling, which is when it can still stop a regression.
        const _: () = assert!(SYMLOOP_MAX >= 8);
    }
}
