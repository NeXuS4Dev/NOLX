//! POSIX `errno` values.
//!
//! The kernel returns errors as a negative `Errno` in `rax`; libc negates it, stores it in
//! the thread's `errno`, and returns `-1`. Nothing else in the system invents an error
//! number: the POSIX subsystem (`kernel/src/psx`) is the only place that maps an internal
//! executive status onto one of these.
//!
//! The set below is the union required by Issue 8. Values are NOLX's own; only the names
//! and their meanings are standardised.

#![allow(clippy::identity_op)]

/// The type of an error number. Always positive when stored in `errno`.
pub type Errno = i32;

pub use codes::*;

/// Every error code, in numeric order.
pub mod codes {
    /// No error. Never stored in `errno`; used as a sentinel.
    pub const EOK: i32 = 0;
    /// Argument list too long.
    pub const E2BIG: i32 = 1;
    /// Permission denied.
    pub const EACCES: i32 = 2;
    /// Address in use.
    pub const EADDRINUSE: i32 = 3;
    /// Address not available.
    pub const EADDRNOTAVAIL: i32 = 4;
    /// Address family not supported.
    pub const EAFNOSUPPORT: i32 = 5;
    /// Resource unavailable, try again.
    pub const EAGAIN: i32 = 6;
    /// Connection already in progress.
    pub const EALREADY: i32 = 7;
    /// Bad file descriptor.
    pub const EBADF: i32 = 8;
    /// Bad message.
    pub const EBADMSG: i32 = 9;

    /// Device or resource busy.
    ///
    /// `mount` on a busy mount point, `rmdir` on a working directory, `rename` onto a
    /// directory in use. Issue 8 specifies it for all three, which is why a filesystem
    /// cannot get far without it.
    pub const EBUSY: i32 = 10;
    /// Operation in progress.
    pub const EINPROGRESS: i32 = 11;
    /// Interrupted function.
    pub const EINTR: i32 = 12;
    /// Invalid argument.
    pub const EINVAL: i32 = 13;
    /// I/O error.
    pub const EIO: i32 = 14;
    /// Socket is connected.
    pub const EISCONN: i32 = 15;
    /// Is a directory.
    pub const EISDIR: i32 = 16;
    /// Too many levels of symbolic links.
    pub const ELOOP: i32 = 17;
    /// File descriptor value too large.
    pub const EMFILE: i32 = 18;
    /// Too many links.
    pub const EMLINK: i32 = 19;
    /// Message too large.
    pub const EMSGSIZE: i32 = 20;
    /// Filename too long.
    pub const ENAMETOOLONG: i32 = 21;
    /// Network is down.
    pub const ENETDOWN: i32 = 22;
    /// Connection aborted by network.
    pub const ENETRESET: i32 = 23;
    /// Network unreachable.
    pub const ENETUNREACH: i32 = 24;
    /// Too many files open in system.
    pub const ENFILE: i32 = 25;
    /// No buffer space available.
    pub const ENOBUFS: i32 = 26;
    /// No such device.
    pub const ENODEV: i32 = 27;
    /// No such file or directory.
    pub const ENOENT: i32 = 28;
    /// Executable file format error.
    pub const ENOEXEC: i32 = 29;
    /// No locks available.
    pub const ENOLCK: i32 = 30;
    /// Link has been severed.
    pub const ENOLINK: i32 = 31;
    /// Not enough space.
    pub const ENOMEM: i32 = 32;
    /// No message of the desired type.
    pub const ENOMSG: i32 = 33;
    /// Protocol not available.
    pub const ENOPROTOOPT: i32 = 34;
    /// No space left on device.
    pub const ENOSPC: i32 = 35;
    /// Function not supported.
    pub const ENOSYS: i32 = 36;
    /// The socket is not connected.
    pub const ENOTCONN: i32 = 37;
    /// Not a directory or a symbolic link to a directory.
    pub const ENOTDIR: i32 = 38;
    /// Directory not empty.
    pub const ENOTEMPTY: i32 = 39;
    /// State not recoverable.
    pub const ENOTRECOVERABLE: i32 = 40;
    /// Not a socket.
    pub const ENOTSOCK: i32 = 41;
    /// Operation not supported.
    pub const ENOTSUP: i32 = 42;
    /// Inappropriate I/O control operation.
    pub const ENOTTY: i32 = 43;
    /// No such device or address.
    pub const ENXIO: i32 = 44;
    /// Operation not supported on socket.
    pub const EOPNOTSUPP: i32 = 45;
    /// Value too large to be stored in data type.
    pub const EOVERFLOW: i32 = 46;
    /// Previous owner died.
    pub const EOWNERDEAD: i32 = 47;
    /// Operation not permitted.
    pub const EPERM: i32 = 48;
    /// Broken pipe.
    pub const EPIPE: i32 = 49;
    /// Protocol error.
    pub const EPROTO: i32 = 50;
    /// Protocol not supported.
    pub const EPROTONOSUPPORT: i32 = 51;
    /// Protocol wrong type for socket.
    pub const EPROTOTYPE: i32 = 52;
    /// Result too large.
    pub const ERANGE: i32 = 53;
    /// Read-only file system.
    pub const EROFS: i32 = 54;
    /// Invalid seek.
    pub const ESPIPE: i32 = 55;
    /// No such process.
    pub const ESRCH: i32 = 56;
    /// Reserved.
    pub const ESTALE: i32 = 57;
    /// Connection timed out.
    pub const ETIMEDOUT: i32 = 58;
    /// Text file busy.
    pub const ETXTBSY: i32 = 59;
    /// Operation would block. Same value as [`EAGAIN`], as the standard requires.
    pub const EWOULDBLOCK: i32 = EAGAIN;
    /// Operation canceled.
    pub const ECANCELED: i32 = 60;
    /// Operation not permitted on a stream (XSI streams compatibility).
    pub const EBADFD: i32 = 61;
    /// Identifier removed.
    pub const EIDRM: i32 = 62;
    /// Illegal byte sequence.
    pub const EILSEQ: i32 = 63;
    /// No data available.
    pub const ENODATA: i32 = 64;
    /// Device not a stream.
    pub const ENOSTR: i32 = 65;
    /// Timer expired.
    pub const ETIME: i32 = 66;
    /// Resource deadlock would occur.
    pub const EDEADLK: i32 = 67;
    /// Too many processes.
    pub const EPROCUNAVAIL: i32 = 68;
    /// Not a typewriter (terminal).
    pub const ENOTBLK: i32 = 69;
    /// Quota exceeded.
    pub const EDQUOT: i32 = 70;
    /// Too many levels of remote references (reserved).
    pub const EREMCHG: i32 = 71;
    /// Bad font file format (reserved).
    pub const ELIBBAD: i32 = 72;
    /// Highest defined error number. `sysconf(_SC_...)` does not expose this; it exists so
    /// the kernel can validate a user-supplied `errno` without a table walk.
    ///
    /// An alias rather than a literal: a literal would have to be updated every time a code
    /// is added or removed, and getting it wrong silently either rejects valid errors or
    /// accepts invented ones. The test `elast_tracks_the_table` is what catches a mismatch
    /// between this and [`TABLE`].
    pub const ELAST: i32 = ELIBBAD;
}

/// `true` if `code` is an error number this kernel can produce.
#[must_use]
pub const fn is_valid(code: i32) -> bool {
    code > 0 && code <= codes::ELAST
}

/// The human-readable string for an error number, as `strerror(3)` must return it.
///
/// Returns `None` for numbers outside the known range, which is what `strerror` uses to
/// produce its `"Unknown error %d"` form.
#[must_use]
pub const fn message(code: i32) -> Option<&'static str> {
    let mut i = 0;
    while i < TABLE.len() {
        if TABLE[i].0 == code {
            return Some(TABLE[i].1);
        }
        i += 1;
    }
    None
}

/// `(code, message)` for every error, in numeric order. `EWOULDBLOCK` is deliberately
/// absent: it shares a value with `EAGAIN` and must not produce a second row.
pub const TABLE: &[(i32, &str)] = &[
    (codes::E2BIG, "Argument list too long"),
    (codes::EACCES, "Permission denied"),
    (codes::EADDRINUSE, "Address already in use"),
    (codes::EADDRNOTAVAIL, "Cannot assign requested address"),
    (
        codes::EAFNOSUPPORT,
        "Address family not supported by protocol",
    ),
    (codes::EAGAIN, "Resource temporarily unavailable"),
    (codes::EALREADY, "Operation already in progress"),
    (codes::EBADF, "Bad file descriptor"),
    (codes::EBADMSG, "Bad message"),
    (codes::EBUSY, "Device or resource busy"),
    (codes::EBADFD, "File descriptor in bad state"),
    (codes::ECANCELED, "Operation canceled"),
    (codes::EDEADLK, "Resource deadlock avoided"),
    (codes::EDQUOT, "Disk quota exceeded"),
    (codes::EIDRM, "Identifier removed"),
    (
        codes::EILSEQ,
        "Invalid or incomplete multibyte or wide character",
    ),
    (codes::EINPROGRESS, "Operation now in progress"),
    (codes::EINTR, "Interrupted system call"),
    (codes::EINVAL, "Invalid argument"),
    (codes::EIO, "Input/output error"),
    (codes::EISCONN, "Transport endpoint is already connected"),
    (codes::EISDIR, "Is a directory"),
    (codes::ELIBBAD, "Accessing a corrupted shared library"),
    (codes::ELOOP, "Too many levels of symbolic links"),
    (codes::EMFILE, "Too many open files"),
    (codes::EMLINK, "Too many links"),
    (codes::EMSGSIZE, "Message too long"),
    (codes::ENAMETOOLONG, "File name too long"),
    (codes::ENETDOWN, "Network is down"),
    (codes::ENETRESET, "Network dropped connection on reset"),
    (codes::ENETUNREACH, "Network is unreachable"),
    (codes::ENFILE, "Too many open files in system"),
    (codes::ENOBUFS, "No buffer space available"),
    (codes::ENODATA, "No data available"),
    (codes::ENODEV, "No such device"),
    (codes::ENOENT, "No such file or directory"),
    (codes::ENOEXEC, "Exec format error"),
    (codes::ENOLCK, "No locks available"),
    (codes::ENOLINK, "Link has been severed"),
    (codes::ENOMEM, "Cannot allocate memory"),
    (codes::ENOMSG, "No message of desired type"),
    (codes::ENOPROTOOPT, "Protocol not available"),
    (codes::ENOSPC, "No space left on device"),
    (codes::ENOSTR, "Device not a stream"),
    (codes::ENOSYS, "Function not implemented"),
    (codes::ENOTBLK, "Block device required"),
    (codes::ENOTCONN, "Transport endpoint is not connected"),
    (codes::ENOTDIR, "Not a directory"),
    (codes::ENOTEMPTY, "Directory not empty"),
    (codes::ENOTRECOVERABLE, "State not recoverable"),
    (codes::ENOTSOCK, "Socket operation on non-socket"),
    (codes::ENOTSUP, "Operation not supported"),
    (codes::ENOTTY, "Inappropriate ioctl for device"),
    (codes::ENXIO, "No such device or address"),
    (
        codes::EOPNOTSUPP,
        "Operation not supported on transport endpoint",
    ),
    (codes::EOVERFLOW, "Value too large for defined data type"),
    (codes::EOWNERDEAD, "Owner died"),
    (codes::EPERM, "Operation not permitted"),
    (codes::EPIPE, "Broken pipe"),
    (codes::EPROCUNAVAIL, "Process unavailable"),
    (codes::EPROTO, "Protocol error"),
    (codes::EPROTONOSUPPORT, "Protocol not supported"),
    (codes::EPROTOTYPE, "Protocol wrong type for socket"),
    (codes::ERANGE, "Numerical result out of range"),
    (codes::EREMCHG, "Remote address changed"),
    (codes::EROFS, "Read-only file system"),
    (codes::ESPIPE, "Illegal seek"),
    (codes::ESRCH, "No such process"),
    (codes::ESTALE, "Stale file handle"),
    (codes::ETIME, "Timer expired"),
    (codes::ETIMEDOUT, "Connection timed out"),
    (codes::ETXTBSY, "Text file busy"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_in_numeric_order() {
        let mut prev = i32::MIN;
        for (code, _) in TABLE {
            assert!(*code > prev, "table out of order at {code}");
            prev = *code;
        }
    }

    #[test]
    fn every_code_has_a_message() {
        for code in 1..=codes::ELAST {
            let msg =
                message(code).unwrap_or_else(|| panic!("errno {code} has no strerror message"));
            assert!(!msg.is_empty(), "errno {code} has an empty message");
        }
    }

    #[test]
    fn no_duplicate_codes_in_table() {
        let mut seen = [false; (codes::ELAST + 1) as usize];
        for (code, _) in TABLE {
            assert!(!seen[*code as usize], "duplicate errno {code}");
            seen[*code as usize] = true;
        }
    }

    #[test]
    fn every_table_entry_is_a_valid_code() {
        for (code, _) in TABLE {
            assert!(is_valid(*code), "{code} is not a valid errno");
        }
    }

    #[test]
    fn unknown_codes_have_no_message() {
        assert_eq!(message(0), None);
        assert_eq!(message(-1), None);
        assert_eq!(message(codes::ELAST + 1), None);
    }

    #[test]
    fn elast_tracks_the_table() {
        // ELAST is an alias for the highest-numbered code, and TABLE holds one row per
        // code from 1 to ELAST. If either changes without the other, `is_valid` starts
        // lying: it would accept an errno no code defines, or reject a real one.
        let highest = TABLE.iter().map(|(code, _)| *code).max().unwrap();
        assert_eq!(
            codes::ELAST,
            highest,
            "ELAST must equal the highest code in TABLE"
        );
        assert_eq!(
            TABLE.len(),
            codes::ELAST as usize,
            "TABLE must have exactly one row per code"
        );
        assert!(
            TABLE.windows(2).all(|w| w[0].0 < w[1].0),
            "TABLE must be in numeric order"
        );
        assert!(
            TABLE.windows(2).all(|w| w[0].0 + 1 == w[1].0),
            "codes must be contiguous"
        );
    }

    #[test]
    fn ewouldblock_and_eagain_share_a_value() {
        // The standard requires these to be interchangeable.
        assert_eq!(codes::EWOULDBLOCK, codes::EAGAIN);
    }

    #[test]
    fn eok_is_zero_and_not_a_valid_error() {
        assert_eq!(codes::EOK, 0);
        assert!(!is_valid(codes::EOK));
    }

    #[test]
    fn opnotsupp_and_notsup_are_distinct() {
        // Issue 8 allows them to be equal; NOLX keeps them distinct so a socket error and
        // a file error are distinguishable in a bug report.
        assert_ne!(codes::EOPNOTSUPP, codes::ENOTSUP);
    }
}
