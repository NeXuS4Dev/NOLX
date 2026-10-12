//! The user/kernel ABI surface the C runtime and the kernel both compile against.
//!
//! Everything here is shared by `nolx-libc`, the embedded user programs, and the kernel's
//! POSIX subsystem: the initial-stack auxiliary vector, the `waitid` result record, and the
//! constants those two contracts are spelled with. A definition that appears anywhere else
//! is a bug, because it is then two definitions.
//!
//! # The initial stack
//!
//! A freshly created process finds this at `%rsp` (SysV ABI shape; `crt0` unwraps it):
//!
//! ```text
//! rsp -> argc
//!        argv[0], ..., argv[argc-1], NULL
//!        envp[0], ..., NULL
//!        auxv: (tag, value) pairs ending with AT_NULL
//! ```
//!
//! # The `waitid` record
//!
//! `waitid` writes a [`Siginfo`] for the waited-for child. Only the `SIGCHLD` payload is
//! defined today; the layout is fixed at [`Siginfo::SIZE`] bytes so libc can allocate one
//! without asking the kernel how big it is.

/// Auxiliary-vector tags carried on the initial user stack.
///
/// The values are the de-facto x86-64 ABI values every `crt0` and dynamic loader expects;
/// choosing different numbers would buy nothing and cost compatibility with every piece of
/// startup code ever written.
pub mod at {
    /// End of the auxiliary vector; `value` is zero.
    pub const NULL: u64 = 0;
    /// File descriptor of the executable, passed open. NOLX does not pass one yet.
    pub const IGNORE: u64 = 1;
    /// File descriptor of the executable, passed open. NOLX does not pass one yet.
    pub const EXECFD: u64 = 2;
    /// Program header table virtual address, for `PT_PHDR` walkers and dynamic loaders.
    pub const PHDR: u64 = 3;
    /// Size of one program header entry, in bytes.
    pub const PHENT: u64 = 4;
    /// Number of program header entries.
    pub const PHNUM: u64 = 5;
    /// Page size, in bytes.
    pub const PAGESZ: u64 = 6;
    /// Interpreter base address (`ET_DYN` load address of `PT_INTERP`, zero when static).
    pub const BASE: u64 = 7;
    /// ELF `e_flags`.
    pub const FLAGS: u64 = 8;
    /// Entry point of the program, with any `ET_DYN` slide applied.
    pub const ENTRY: u64 = 9;
    /// Non-ELF ABI, or zero.
    pub const NOTELF: u64 = 10;
    /// Real user id.
    pub const UID: u64 = 11;
    /// Effective user id.
    pub const EUID: u64 = 12;
    /// Real group id.
    pub const GID: u64 = 13;
    /// Effective group id.
    pub const EGID: u64 = 14;
    /// Platform string pointer ("x86_64").
    pub const PLATFORM: u64 = 15;
    /// Hardware capability hints; zero today.
    pub const HWCAP: u64 = 16;
    /// Clock frequency for `sysconf(_SC_CLK_TCK)`.
    pub const CLKTCK: u64 = 17;
    /// Security-mode flag: nonzero when the program runs with elevated privileges.
    pub const SECURE: u64 = 23;
    /// Pointer to 16 bytes of stack-randomisation entropy.
    pub const RANDOM: u64 = 25;
    /// Pointer to the executable path string.
    pub const EXECFN: u64 = 31;
}

/// `idtype_t` values for `waitid(2)`.
pub mod idtype {
    /// Wait for any child (`P_ALL`); the `id` argument is ignored.
    pub const P_ALL: i32 = 0;
    /// Wait for the child whose process id is `id` (`P_PID`).
    pub const P_PID: i32 = 1;
    /// Wait for any child in the process group `id` (`P_PGID`).
    pub const P_PGID: i32 = 2;
}

/// `waitid(2)` option bits.
pub mod waitopt {
    /// Report children that exited normally or by signal (`WEXITED`).
    pub const WEXITED: i32 = 4;
}

/// `si_code` values for a `SIGCHLD` record.
pub mod cld {
    /// The child exited normally; `status` is its exit status.
    pub const EXITED: i32 = 1;
    /// The child was killed by an uncaught signal; `status` is the signal number.
    pub const KILLED: i32 = 2;
    /// The child dumped core.
    pub const DUMPED: i32 = 3;
}

/// Signal number for "child state changed", the signal `waitid` reports through.
pub const SIGCHLD: i32 = 17;

/// The `siginfo_t` subset NOLX fills for `waitid(2)`.
///
/// The layout is the common x86-64 Linux/SysV shape, truncated to the fields the kernel
/// actually writes: `si_signo`, `si_errno`, `si_code`, the SIGCHLD payload (`si_pid`,
/// `si_uid`, `si_status`), and padding to the fixed [`Siginfo::SIZE`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Siginfo {
    /// Signal number: [`SIGCHLD`] for a waited-for child.
    pub signo: i32,
    /// `errno` associated with the signal, or zero.
    pub code_errno: i32,
    /// Why the child changed state: [`cld::EXITED`] and friends.
    pub code: i32,
    /// Explicit layout padding; zeroed by the kernel.
    pub padding: i32,
    /// Process id of the child.
    pub pid: i32,
    /// Real user id of the child.
    pub uid: i32,
    /// Exit status or terminating signal, depending on `code`.
    pub status: i32,
    /// Explicit layout padding; zeroed by the kernel.
    pub padding2: i32,
    /// Reserved remainder, keeping the record at [`Siginfo::SIZE`].
    pub reserved: [u64; 12],
}

impl Siginfo {
    /// The fixed size libc allocates: 128 bytes, the x86-64 `sizeof(siginfo_t)`.
    pub const SIZE: usize = 128;

    /// An empty record: no signal, no child.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            signo: 0,
            code_errno: 0,
            code: 0,
            padding: 0,
            pid: 0,
            uid: 0,
            status: 0,
            padding2: 0,
            reserved: [0; 12],
        }
    }
}

impl Default for Siginfo {
    fn default() -> Self {
        Self::empty()
    }
}

/// Maximum arguments the initial stack carries, matching `MAX_ARG_STRINGS` practice.
pub const MAX_ARG_COUNT: usize = 128;

/// Maximum bytes one argument or environment string may occupy on the initial stack.
pub const MAX_ARG_BYTES: usize = 4096;

/// Open file flags (`<fcntl.h>`).
pub mod open_flags {
    /// Open for reading only (`O_RDONLY`).
    pub const O_RDONLY: u32 = 0x0000;
    /// Open for writing only (`O_WRONLY`).
    pub const O_WRONLY: u32 = 0x0001;
    /// Open for reading and writing (`O_RDWR`).
    pub const O_RDWR: u32 = 0x0002;
    /// Mask for the file access modes (`O_ACCMODE`).
    pub const O_ACCMODE: u32 = 0x0003;
    /// Create file if it does not exist (`O_CREAT`).
    pub const O_CREAT: u32 = 0x0040;
    /// Exclusive use flag (`O_EXCL`).
    pub const O_EXCL: u32 = 0x0080;
    /// Do not assign controlling terminal (`O_NOCTTY`).
    pub const O_NOCTTY: u32 = 0x0100;
    /// Truncate flag (`O_TRUNC`).
    pub const O_TRUNC: u32 = 0x0200;
    /// Set append mode (`O_APPEND`).
    pub const O_APPEND: u32 = 0x0400;
    /// Non-blocking mode (`O_NONBLOCK`).
    pub const O_NONBLOCK: u32 = 0x0800;
    /// Write according to synchronized I/O data integrity completion (`O_DSYNC`).
    pub const O_DSYNC: u32 = 0x1000;
    /// Write according to synchronized I/O file integrity completion (`O_SYNC`).
    pub const O_SYNC: u32 = 0x2000;
    /// Synchronized read I/O operations (`O_RSYNC`).
    pub const O_RSYNC: u32 = 0x4000;
    /// Fail if not a directory (`O_DIRECTORY`).
    pub const O_DIRECTORY: u32 = 0x0001_0000;
    /// Do not follow symbolic links (`O_NOFOLLOW`).
    pub const O_NOFOLLOW: u32 = 0x0002_0000;
    /// Set `FD_CLOEXEC` atomically at open (`O_CLOEXEC`).
    pub const O_CLOEXEC: u32 = 0x0008_0000;
}

/// `whence` values for `lseek(2)` (`<unistd.h>`).
pub mod seek {
    /// Set file offset to `offset` (`SEEK_SET`).
    pub const SEEK_SET: i32 = 0;
    /// Set file offset to current plus `offset` (`SEEK_CUR`).
    pub const SEEK_CUR: i32 = 1;
    /// Set file offset to EOF plus `offset` (`SEEK_END`).
    pub const SEEK_END: i32 = 2;
}

/// `cmd` values and descriptor flags for `fcntl(2)` (`<fcntl.h>`).
pub mod fcntl_cmd {
    /// Duplicate file descriptor (`F_DUPFD`).
    pub const F_DUPFD: i32 = 0;
    /// Get file descriptor flags (`F_GETFD`).
    pub const F_GETFD: i32 = 1;
    /// Set file descriptor flags (`F_SETFD`).
    pub const F_SETFD: i32 = 2;
    /// Get file status flags and file access modes (`F_GETFL`).
    pub const F_GETFL: i32 = 3;
    /// Set file status flags (`F_SETFL`).
    pub const F_SETFL: i32 = 4;
    /// Duplicate file descriptor with the close-on-exec flag set (`F_DUPFD_CLOEXEC`).
    pub const F_DUPFD_CLOEXEC: i32 = 1030;
    /// Close the file descriptor upon execution of an `exec` family function (`FD_CLOEXEC`).
    pub const FD_CLOEXEC: i32 = 1;
}

/// `name` values for `pathconf(2)` and `fpathconf(2)` (`<unistd.h>`).
pub mod pathconf_name {
    /// Maximum number of links to a single file (`_PC_LINK_MAX`).
    pub const PC_LINK_MAX: i32 = 1;
    /// Maximum number of bytes in a terminal canonical input line (`_PC_MAX_CANON`).
    pub const PC_MAX_CANON: i32 = 2;
    /// Minimum number of bytes for which space is available in a terminal input queue (`_PC_MAX_INPUT`).
    pub const PC_MAX_INPUT: i32 = 3;
    /// Maximum number of bytes in a filename (`_PC_NAME_MAX`).
    pub const PC_NAME_MAX: i32 = 4;
    /// Maximum number of bytes in a pathname, including the terminating NUL (`_PC_PATH_MAX`).
    pub const PC_PATH_MAX: i32 = 5;
    /// Maximum number of bytes that is guaranteed to be atomic when writing to a pipe (`_PC_PIPE_BUF`).
    pub const PC_PIPE_BUF: i32 = 6;
    /// The use of `chown` and `fchown` is restricted (`_PC_CHOWN_RESTRICTED`).
    pub const PC_CHOWN_RESTRICTED: i32 = 7;
    /// Pathname components longer than `NAME_MAX` generate an error (`_PC_NO_TRUNC`).
    pub const PC_NO_TRUNC: i32 = 8;
    /// Terminal special characters can be disabled using this character value (`_PC_VDISABLE`).
    pub const PC_VDISABLE: i32 = 9;
    /// Maximum number of bytes in a symbolic link (`_PC_SYMLINK_MAX`).
    pub const PC_SYMLINK_MAX: i32 = 10;
    /// Symbolic links can be created (`_PC_2_SYMLINKS`).
    pub const PC_2_SYMLINKS: i32 = 11;
    /// Minimum number of bits needed to represent the maximum file size (`_PC_FILESIZEBITS`).
    pub const PC_FILESIZEBITS: i32 = 12;
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn siginfo_is_the_fixed_128_byte_record() {
        assert_eq!(size_of::<Siginfo>(), Siginfo::SIZE);
        assert_eq!(align_of::<Siginfo>(), 8);
    }

    #[test]
    fn auxv_tags_are_the_de_facto_x86_64_values() {
        // If these move, every crt0 ever written breaks. Pin them.
        assert_eq!(at::NULL, 0);
        assert_eq!(at::PHDR, 3);
        assert_eq!(at::PHENT, 4);
        assert_eq!(at::PHNUM, 5);
        assert_eq!(at::PAGESZ, 6);
        assert_eq!(at::BASE, 7);
        assert_eq!(at::ENTRY, 9);
        assert_eq!(at::UID, 11);
        assert_eq!(at::EUID, 12);
        assert_eq!(at::GID, 13);
        assert_eq!(at::EGID, 14);
        assert_eq!(at::SECURE, 23);
        assert_eq!(at::RANDOM, 25);
        assert_eq!(at::EXECFN, 31);
    }

    #[test]
    fn wait_constants_match_posix() {
        assert_eq!(idtype::P_ALL, 0);
        assert_eq!(idtype::P_PID, 1);
        assert_eq!(waitopt::WEXITED, 4);
        assert_eq!(cld::EXITED, 1);
        assert_eq!(SIGCHLD, 17);
    }
}
