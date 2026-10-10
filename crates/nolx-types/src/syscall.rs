//! NOLX syscall numbers.
//!
//! ## Reservation policy
//!
//! A syscall number, once assigned, is **never** reused and never renumbered. If a call is
//! removed, its number becomes a tombstone that returns `ENOSYS` forever. This is the rule
//! that keeps a statically linked binary from silently calling something else after an
//! upgrade, and it is why the table below is append-only.
//!
//! ## Numbering
//!
//! Numbers are grouped by subsystem so a gap is informative rather than random:
//!
//! | Range | Subsystem |
//! |---|---|
//! | 0..99 | process, credentials, and the basic lifecycle |
//! | 100..199 | files and directories |
//! | 200..249 | extended attributes and file metadata |
//! | 250..299 | time |
//! | 300..349 | signals |
//! | 350..399 | interprocess communication |
//! | 400..449 | memory |
//! | 450..499 | sockets and networking |
//! | 500..549 | real-time and scheduling |
//! | 550..599 | dynamic loading, locale, and miscellany |
//! | 600..699 | NOLX-native executive calls with no POSIX equivalent |
//!
//! The 600s are where the Windows-shaped kernel shows through: object manager and port
//! calls that the POSIX subsystem uses internally and that a POSIX program never issues.
//! They are part of the ABI and are documented, but they are not POSIX surface and carry
//! no conformance obligations.

/// The type of a syscall number.
pub type Number = u16;

/// Highest assigned number. Anything above this returns `ENOSYS`.
pub const HIGHEST: Number = 606;

// --------------------------------------------------------------------- 0..99 process

/// `nxposix_exit`: terminate the calling process. Never returns.
pub const SYS_EXIT: Number = 0;
/// `nxposix_fork`.
pub const SYS_FORK: Number = 1;
/// `nxposix_vfork`.
pub const SYS_VFORK: Number = 2;
/// `nxposix_execve`.
pub const SYS_EXECVE: Number = 3;
/// `nxposix_waitid`, which `wait`, `waitpid`, and `waitid` are all built on.
pub const SYS_WAITID: Number = 4;
/// `nxposix_getpid`.
pub const SYS_GETPID: Number = 5;
/// `nxposix_getppid`.
pub const SYS_GETPPID: Number = 6;
/// `nxposix_getuid`.
pub const SYS_GETUID: Number = 7;
/// `nxposix_setuid`.
pub const SYS_SETUID: Number = 8;
/// `nxposix_getgid`.
pub const SYS_GETGID: Number = 9;
/// `nxposix_setgid`.
pub const SYS_SETGID: Number = 10;
/// `nxposix_geteuid`.
pub const SYS_GETEUID: Number = 11;
/// `nxposix_getegid`.
pub const SYS_GETEGID: Number = 12;
/// `nxposix_setreuid`.
pub const SYS_SETREUID: Number = 13;
/// `nxposix_setregid`.
pub const SYS_SETREGID: Number = 14;
/// `nxposix_getgroups`.
pub const SYS_GETGROUPS: Number = 15;
/// `nxposix_setgroups`.
pub const SYS_SETGROUPS: Number = 16;
/// `nxposix_setpgid`.
pub const SYS_SETPGID: Number = 17;
/// `nxposix_getpgid`.
pub const SYS_GETPGID: Number = 18;
/// `nxposix_setsid`.
pub const SYS_SETSID: Number = 19;
/// `nxposix_getsid`.
pub const SYS_GETSID: Number = 20;
/// `nxposix_umask`.
pub const SYS_UMASK: Number = 21;
/// `nxposix_chdir`.
pub const SYS_CHDIR: Number = 22;
/// `nxposix_fchdir`.
pub const SYS_FCHDIR: Number = 23;
/// `nxposix_chroot`.
pub const SYS_CHROOT: Number = 24;
/// `nxposix_getrlimit`.
pub const SYS_GETRLIMIT: Number = 25;
/// `nxposix_setrlimit`.
pub const SYS_SETRLIMIT: Number = 26;
/// `nxposix_getrusage`.
pub const SYS_GETRUSAGE: Number = 27;
/// `nxposix_nice`.
pub const SYS_NICE: Number = 28;
/// `nxposix_getpriority`.
pub const SYS_GETPRIORITY: Number = 29;
/// `nxposix_setpriority`.
pub const SYS_SETPRIORITY: Number = 30;
/// `nxposix_posix_spawn`, the primitive `fork`+`exec` is also built on.
pub const SYS_POSIX_SPAWN: Number = 31;
/// `nxposix_exit_thread`: terminate only the calling thread.
pub const SYS_EXIT_THREAD: Number = 32;
/// `nxposix_gettid`: the kernel's thread id, not the pthread id.
pub const SYS_GETTID: Number = 33;
/// `nxposix_uname`.
pub const SYS_UNAME: Number = 34;

// --------------------------------------------------------------------- 100..199 files

/// `nxposix_open`.
pub const SYS_OPEN: Number = 100;
/// `nxposix_openat`.
pub const SYS_OPENAT: Number = 101;
/// `nxposix_creat`.
pub const SYS_CREAT: Number = 102;
/// `nxposix_close`.
pub const SYS_CLOSE: Number = 103;
/// `nxposix_read`.
pub const SYS_READ: Number = 104;
/// `nxposix_write`.
pub const SYS_WRITE: Number = 105;
/// `nxposix_pread`.
pub const SYS_PREAD: Number = 106;
/// `nxposix_pwrite`.
pub const SYS_PWRITE: Number = 107;
/// `nxposix_readv`.
pub const SYS_READV: Number = 108;
/// `nxposix_writev`.
pub const SYS_WRITEV: Number = 109;
/// `nxposix_lseek`.
pub const SYS_LSEEK: Number = 110;
/// `nxposix_dup`.
pub const SYS_DUP: Number = 111;
/// `nxposix_dup2`.
pub const SYS_DUP2: Number = 112;
/// `nxposix_dup3`.
pub const SYS_DUP3: Number = 113;
/// `nxposix_fcntl`.
pub const SYS_FCNTL: Number = 114;
/// `nxposix_ioctl`.
pub const SYS_IOCTL: Number = 115;
/// `nxposix_fsync`.
pub const SYS_FSYNC: Number = 116;
/// `nxposix_fdatasync`.
pub const SYS_FDATASYNC: Number = 117;
/// `nxposix_truncate`.
pub const SYS_TRUNCATE: Number = 118;
/// `nxposix_ftruncate`.
pub const SYS_FTRUNCATE: Number = 119;
/// `nxposix_stat`.
pub const SYS_STAT: Number = 120;
/// `nxposix_fstat`.
pub const SYS_FSTAT: Number = 121;
/// `nxposix_lstat`.
pub const SYS_LSTAT: Number = 122;
/// `nxposix_fstatat`.
pub const SYS_FSTATAT: Number = 123;
/// `nxposix_statvfs`.
pub const SYS_STATVFS: Number = 124;
/// `nxposix_fstatvfs`.
pub const SYS_FSTATVFS: Number = 125;
/// `nxposix_access`.
pub const SYS_ACCESS: Number = 126;
/// `nxposix_faccessat`.
pub const SYS_FACCESSAT: Number = 127;
/// `nxposix_chmod`.
pub const SYS_CHMOD: Number = 128;
/// `nxposix_fchmod`.
pub const SYS_FCHMOD: Number = 129;
/// `nxposix_fchmodat`.
pub const SYS_FCHMODAT: Number = 130;
/// `nxposix_chown`.
pub const SYS_CHOWN: Number = 131;
/// `nxposix_fchown`.
pub const SYS_FCHOWN: Number = 132;
/// `nxposix_fchownat`.
pub const SYS_FCHOWNAT: Number = 133;
/// `nxposix_lchown`.
pub const SYS_LCHOWN: Number = 134;
/// `nxposix_utimensat`.
pub const SYS_UTIMENSAT: Number = 135;
/// `nxposix_mkdir`.
pub const SYS_MKDIR: Number = 136;
/// `nxposix_mkdirat`.
pub const SYS_MKDIRAT: Number = 137;
/// `nxposix_rmdir`.
pub const SYS_RMDIR: Number = 138;
/// `nxposix_unlink`.
pub const SYS_UNLINK: Number = 139;
/// `nxposix_unlinkat`.
pub const SYS_UNLINKAT: Number = 140;
/// `nxposix_link`.
pub const SYS_LINK: Number = 141;
/// `nxposix_linkat`.
pub const SYS_LINKAT: Number = 142;
/// `nxposix_symlink`.
pub const SYS_SYMLINK: Number = 143;
/// `nxposix_symlinkat`.
pub const SYS_SYMLINKAT: Number = 144;
/// `nxposix_readlink`.
pub const SYS_READLINK: Number = 145;
/// `nxposix_readlinkat`.
pub const SYS_READLINKAT: Number = 146;
/// `nxposix_rename`.
pub const SYS_RENAME: Number = 147;
/// `nxposix_renameat`.
pub const SYS_RENAMEAT: Number = 148;
/// `nxposix_mkfifo`.
pub const SYS_MKFIFO: Number = 149;
/// `nxposix_mkfifoat`.
pub const SYS_MKFIFOAT: Number = 150;
/// `nxposix_mknod`.
pub const SYS_MKNOD: Number = 151;
/// `nxposix_mknodat`.
pub const SYS_MKNODAT: Number = 152;
/// `nxposix_getdents`: the raw directory-read primitive `readdir(3)` is built on.
pub const SYS_GETDENTS: Number = 153;
/// `nxposix_pipe`.
pub const SYS_PIPE: Number = 154;
/// `nxposix_pipe2`.
pub const SYS_PIPE2: Number = 155;
/// `nxposix_pathconf`.
pub const SYS_PATHCONF: Number = 156;
/// `nxposix_fpathconf`.
pub const SYS_FPATHCONF: Number = 157;

// --------------------------------------------------------------------- 200..249 xattr

/// `nxposix_setxattr`.
pub const SYS_SETXATTR: Number = 200;
/// `nxposix_getxattr`.
pub const SYS_GETXATTR: Number = 201;
/// `nxposix_listxattr`.
pub const SYS_LISTXATTR: Number = 202;
/// `nxposix_removexattr`.
pub const SYS_REMOVEXATTR: Number = 203;
/// `nxposix_lsetxattr`.
pub const SYS_LSETXATTR: Number = 204;
/// `nxposix_lgetxattr`.
pub const SYS_LGETXATTR: Number = 205;

// --------------------------------------------------------------------- 250..299 time

/// `nxposix_clock_gettime`.
pub const SYS_CLOCK_GETTIME: Number = 250;
/// `nxposix_clock_settime`.
pub const SYS_CLOCK_SETTIME: Number = 251;
/// `nxposix_clock_getres`.
pub const SYS_CLOCK_GETRES: Number = 252;
/// `nxposix_clock_nanosleep`.
pub const SYS_CLOCK_NANOSLEEP: Number = 253;
/// `nxposix_nanosleep`.
pub const SYS_NANOSLEEP: Number = 254;
/// `nxposix_alarm`.
pub const SYS_ALARM: Number = 255;
/// `nxposix_gettimeofday`.
pub const SYS_GETTIMEOFDAY: Number = 256;
/// `nxposix_settimeofday`.
pub const SYS_SETTIMEOFDAY: Number = 257;
/// `nxposix_times`.
pub const SYS_TIMES: Number = 258;

// --------------------------------------------------------------------- 300..349 signals

/// `nxposix_kill`.
pub const SYS_KILL: Number = 300;
/// `nxposix_killpg`.
pub const SYS_KILLPG: Number = 301;
/// `nxposix_sigaction`.
pub const SYS_SIGACTION: Number = 302;
/// `nxposix_sigprocmask`.
pub const SYS_SIGPROCMASK: Number = 303;
/// `nxposix_sigpending`.
pub const SYS_SIGPENDING: Number = 304;
/// `nxposix_sigsuspend`.
pub const SYS_SIGSUSPEND: Number = 305;
/// `nxposix_sigwait`.
pub const SYS_SIGWAIT: Number = 306;
/// `nxposix_sigwaitinfo`.
pub const SYS_SIGWAITINFO: Number = 307;
/// `nxposix_sigtimedwait`.
pub const SYS_SIGTIMEDWAIT: Number = 308;
/// `nxposix_sigqueue`.
pub const SYS_SIGQUEUE: Number = 309;
/// `nxposix_sigaltstack`.
pub const SYS_SIGALTSTACK: Number = 310;
/// `nxposix_pthread_sigmask`.
pub const SYS_PTHREAD_SIGMASK: Number = 311;
/// `nxposix_pthread_kill`.
pub const SYS_PTHREAD_KILL: Number = 312;
/// `nxposix_sigreturn`, the trampoline's way back. Not callable directly by libc.
pub const SYS_SIGRETURN: Number = 313;

// --------------------------------------------------------------------- 350..399 IPC

/// `nxposix_select`.
pub const SYS_SELECT: Number = 350;
/// `nxposix_pselect`.
pub const SYS_PSELECT: Number = 351;
/// `nxposix_poll`.
pub const SYS_POLL: Number = 352;
/// `nxposix_ppoll`.
pub const SYS_PPOLL: Number = 353;
/// `nxposix_msgget` and the rest of System V message queues.
pub const SYS_MSGGET: Number = 354;
/// `nxposix_msgsnd`.
pub const SYS_MSGSND: Number = 355;
/// `nxposix_msgrcv`.
pub const SYS_MSGRCV: Number = 356;
/// `nxposix_msgctl`.
pub const SYS_MSGCTL: Number = 357;
/// `nxposix_semget`.
pub const SYS_SEMGET: Number = 358;
/// `nxposix_semop`.
pub const SYS_SEMOP: Number = 359;
/// `nxposix_semctl`.
pub const SYS_SEMCTL: Number = 360;
/// `nxposix_shmget`.
pub const SYS_SHMGET: Number = 361;
/// `nxposix_shmat`.
pub const SYS_SHMAT: Number = 362;
/// `nxposix_shmdt`.
pub const SYS_SHMDT: Number = 363;
/// `nxposix_shmctl`.
pub const SYS_SHMCTL: Number = 364;
/// `nxposix_mq_open`.
pub const SYS_MQ_OPEN: Number = 365;
/// `nxposix_mq_close`.
pub const SYS_MQ_CLOSE: Number = 366;
/// `nxposix_mq_unlink`.
pub const SYS_MQ_UNLINK: Number = 367;
/// `nxposix_mq_send`.
pub const SYS_MQ_SEND: Number = 368;
/// `nxposix_mq_receive`.
pub const SYS_MQ_RECEIVE: Number = 369;
/// `nxposix_mq_notify`.
pub const SYS_MQ_NOTIFY: Number = 370;
/// `nxposix_mq_getattr`.
pub const SYS_MQ_GETATTR: Number = 371;
/// `nxposix_mq_setattr`.
pub const SYS_MQ_SETATTR: Number = 372;
/// `nxposix_shm_open`.
pub const SYS_SHM_OPEN: Number = 373;
/// `nxposix_shm_unlink`.
pub const SYS_SHM_UNLINK: Number = 374;
/// `nxposix_sem_open`.
pub const SYS_SEM_OPEN: Number = 375;
/// `nxposix_sem_close`.
pub const SYS_SEM_CLOSE: Number = 376;
/// `nxposix_sem_unlink`.
pub const SYS_SEM_UNLINK: Number = 377;
/// `nxposix_sem_wait`.
pub const SYS_SEM_WAIT: Number = 378;
/// `nxposix_sem_trywait`.
pub const SYS_SEM_TRYWAIT: Number = 379;
/// `nxposix_sem_timedwait`.
pub const SYS_SEM_TIMEDWAIT: Number = 380;
/// `nxposix_sem_post`.
pub const SYS_SEM_POST: Number = 381;
/// `nxposix_sem_getvalue`.
pub const SYS_SEM_GETVALUE: Number = 382;

// --------------------------------------------------------------------- 400..449 memory

/// `nxposix_mmap`.
pub const SYS_MMAP: Number = 400;
/// `nxposix_munmap`.
pub const SYS_MUNMAP: Number = 401;
/// `nxposix_mprotect`.
pub const SYS_MPROTECT: Number = 402;
/// `nxposix_msync`.
pub const SYS_MSYNC: Number = 403;
/// `nxposix_madvise`.
pub const SYS_MADVISE: Number = 404;
/// `nxposix_mlock`.
pub const SYS_MLOCK: Number = 405;
/// `nxposix_munlock`.
pub const SYS_MUNLOCK: Number = 406;
/// `nxposix_mlockall`.
pub const SYS_MLOCKALL: Number = 407;
/// `nxposix_munlockall`.
pub const SYS_MUNLOCKALL: Number = 408;
/// `nxposix_mincore`.
pub const SYS_MINCORE: Number = 409;
/// `nxposix_brk`.
pub const SYS_BRK: Number = 410;

// --------------------------------------------------------------------- 450..499 network

/// `nxposix_socket`.
pub const SYS_SOCKET: Number = 450;
/// `nxposix_bind`.
pub const SYS_BIND: Number = 451;
/// `nxposix_listen`.
pub const SYS_LISTEN: Number = 452;
/// `nxposix_accept`.
pub const SYS_ACCEPT: Number = 453;
/// `nxposix_accept4`.
pub const SYS_ACCEPT4: Number = 454;
/// `nxposix_connect`.
pub const SYS_CONNECT: Number = 455;
/// `nxposix_send`.
pub const SYS_SEND: Number = 456;
/// `nxposix_sendto`.
pub const SYS_SENDTO: Number = 457;
/// `nxposix_sendmsg`.
pub const SYS_SENDMSG: Number = 458;
/// `nxposix_recv`.
pub const SYS_RECV: Number = 459;
/// `nxposix_recvfrom`.
pub const SYS_RECVFROM: Number = 460;
/// `nxposix_recvmsg`.
pub const SYS_RECVMSG: Number = 461;
/// `nxposix_shutdown`.
pub const SYS_SHUTDOWN: Number = 462;
/// `nxposix_getsockopt`.
pub const SYS_GETSOCKOPT: Number = 463;
/// `nxposix_setsockopt`.
pub const SYS_SETSOCKOPT: Number = 464;
/// `nxposix_getsockname`.
pub const SYS_GETSOCKNAME: Number = 465;
/// `nxposix_getpeername`.
pub const SYS_GETPEERNAME: Number = 466;
/// `nxposix_socketpair`.
pub const SYS_SOCKETPAIR: Number = 467;

// --------------------------------------------------------------------- 500..549 RT

/// `nxposix_sched_yield`.
pub const SYS_SCHED_YIELD: Number = 500;
/// `nxposix_sched_setscheduler`.
pub const SYS_SCHED_SETSCHEDULER: Number = 501;
/// `nxposix_sched_getscheduler`.
pub const SYS_SCHED_GETSCHEDULER: Number = 502;
/// `nxposix_sched_setparam`.
pub const SYS_SCHED_SETPARAM: Number = 503;
/// `nxposix_sched_getparam`.
pub const SYS_SCHED_GETPARAM: Number = 504;
/// `nxposix_sched_get_priority_max`.
pub const SYS_SCHED_GET_PRIORITY_MAX: Number = 505;
/// `nxposix_sched_get_priority_min`.
pub const SYS_SCHED_GET_PRIORITY_MIN: Number = 506;
/// `nxposix_sched_rr_get_interval`.
pub const SYS_SCHED_RR_GET_INTERVAL: Number = 507;
/// `nxposix_sched_setaffinity`.
pub const SYS_SCHED_SETAFFINITY: Number = 508;
/// `nxposix_sched_getaffinity`.
pub const SYS_SCHED_GETAFFINITY: Number = 509;
/// `nxposix_timer_create`.
pub const SYS_TIMER_CREATE: Number = 510;
/// `nxposix_timer_delete`.
pub const SYS_TIMER_DELETE: Number = 511;
/// `nxposix_timer_settime`.
pub const SYS_TIMER_SETTIME: Number = 512;
/// `nxposix_timer_gettime`.
pub const SYS_TIMER_GETTIME: Number = 513;
/// `nxposix_timer_getoverrun`.
pub const SYS_TIMER_GETOVERRUN: Number = 514;
/// `nxposix_aio_read`.
pub const SYS_AIO_READ: Number = 515;
/// `nxposix_aio_write`.
pub const SYS_AIO_WRITE: Number = 516;
/// `nxposix_aio_error`.
pub const SYS_AIO_ERROR: Number = 517;
/// `nxposix_aio_return`.
pub const SYS_AIO_RETURN: Number = 518;
/// `nxposix_aio_cancel`.
pub const SYS_AIO_CANCEL: Number = 519;
/// `nxposix_aio_suspend`.
pub const SYS_AIO_SUSPEND: Number = 520;
/// `nxposix_aio_fsync`.
pub const SYS_AIO_FSYNC: Number = 521;
/// `nxposix_lio_listio`.
pub const SYS_LIO_LISTIO: Number = 522;

// --------------------------------------------------------------------- 550..599 misc

/// `nxposix_sysconf`: the tunable answers live in the Configuration Manager.
pub const SYS_SYSCONF: Number = 550;
/// `nxposix_confstr`.
pub const SYS_CONFSTR: Number = 551;
/// `nxposix_openpty`.
pub const SYS_OPENPTY: Number = 552;
/// `nxposix_ptsname` resolves through the object namespace, but the lookup needs a call.
pub const SYS_PTSNAME: Number = 553;
/// `nxposix_tcgetattr`.
pub const SYS_TCGETATTR: Number = 554;
/// `nxposix_tcsetattr`.
pub const SYS_TCSETATTR: Number = 555;
/// `nxposix_tcsetpgrp`.
pub const SYS_TCSETPGRP: Number = 556;
/// `nxposix_tcgetpgrp`.
pub const SYS_TCGETPGRP: Number = 557;
/// `nxposix_tcsendbreak`.
pub const SYS_TCSENDBREAK: Number = 558;
/// `nxposix_tcdrain`.
pub const SYS_TCDRAIN: Number = 559;
/// `nxposix_tcflush`.
pub const SYS_TCFLUSH: Number = 560;
/// `nxposix_tcflow`.
pub const SYS_TCFLOW: Number = 561;
/// `nxposix_isatty`.
pub const SYS_ISATTY: Number = 562;
/// `nxposix_ttyname`.
pub const SYS_TTYNAME: Number = 563;
/// `nxposix_fallocate`.
pub const SYS_FALLOCATE: Number = 564;

// --------------------------------------------------------------------- 600..699 native
//
// Not POSIX surface. These are the executive calls the POSIX subsystem, the window
// server, and user-mode drivers use. A POSIX program never issues one directly.

/// `nxobj_create`: create a named executive object.
pub const SYS_NXOBJ_CREATE: Number = 600;
/// `nxobj_open`: open an executive object by name.
pub const SYS_NXOBJ_OPEN: Number = 601;
/// `nxobj_close`: release a handle.
pub const SYS_NXOBJ_CLOSE: Number = 602;
/// `nxobj_duplicate`: duplicate a handle, possibly into another process.
pub const SYS_NXOBJ_DUPLICATE: Number = 603;
/// `nxobj_query`: query an object's name, type, and security descriptor.
pub const SYS_NXOBJ_QUERY: Number = 604;
/// `nxport_call`: synchronous request/reply over an IPC port.
pub const SYS_NXPORT_CALL: Number = 605;
/// `nxwait`: wait on one or more dispatcher objects.
pub const SYS_NXWAIT: Number = 606;

/// The table of `(number, name)`, in numeric order.
///
/// This is the ABI's manifest. `nxsh` prints it, `strace`-equivalent tooling reads it,
/// and the test below checks that it is complete and ordered.
pub const TABLE: &[(Number, &str)] = &[
    (SYS_EXIT, "exit"),
    (SYS_FORK, "fork"),
    (SYS_VFORK, "vfork"),
    (SYS_EXECVE, "execve"),
    (SYS_WAITID, "waitid"),
    (SYS_GETPID, "getpid"),
    (SYS_GETPPID, "getppid"),
    (SYS_GETUID, "getuid"),
    (SYS_SETUID, "setuid"),
    (SYS_GETGID, "getgid"),
    (SYS_SETGID, "setgid"),
    (SYS_GETEUID, "geteuid"),
    (SYS_GETEGID, "getegid"),
    (SYS_SETREUID, "setreuid"),
    (SYS_SETREGID, "setregid"),
    (SYS_GETGROUPS, "getgroups"),
    (SYS_SETGROUPS, "setgroups"),
    (SYS_SETPGID, "setpgid"),
    (SYS_GETPGID, "getpgid"),
    (SYS_SETSID, "setsid"),
    (SYS_GETSID, "getsid"),
    (SYS_UMASK, "umask"),
    (SYS_CHDIR, "chdir"),
    (SYS_FCHDIR, "fchdir"),
    (SYS_CHROOT, "chroot"),
    (SYS_GETRLIMIT, "getrlimit"),
    (SYS_SETRLIMIT, "setrlimit"),
    (SYS_GETRUSAGE, "getrusage"),
    (SYS_NICE, "nice"),
    (SYS_GETPRIORITY, "getpriority"),
    (SYS_SETPRIORITY, "setpriority"),
    (SYS_POSIX_SPAWN, "posix_spawn"),
    (SYS_EXIT_THREAD, "exit_thread"),
    (SYS_GETTID, "gettid"),
    (SYS_UNAME, "uname"),
    (SYS_OPEN, "open"),
    (SYS_OPENAT, "openat"),
    (SYS_CREAT, "creat"),
    (SYS_CLOSE, "close"),
    (SYS_READ, "read"),
    (SYS_WRITE, "write"),
    (SYS_PREAD, "pread"),
    (SYS_PWRITE, "pwrite"),
    (SYS_READV, "readv"),
    (SYS_WRITEV, "writev"),
    (SYS_LSEEK, "lseek"),
    (SYS_DUP, "dup"),
    (SYS_DUP2, "dup2"),
    (SYS_DUP3, "dup3"),
    (SYS_FCNTL, "fcntl"),
    (SYS_IOCTL, "ioctl"),
    (SYS_FSYNC, "fsync"),
    (SYS_FDATASYNC, "fdatasync"),
    (SYS_TRUNCATE, "truncate"),
    (SYS_FTRUNCATE, "ftruncate"),
    (SYS_STAT, "stat"),
    (SYS_FSTAT, "fstat"),
    (SYS_LSTAT, "lstat"),
    (SYS_FSTATAT, "fstatat"),
    (SYS_STATVFS, "statvfs"),
    (SYS_FSTATVFS, "fstatvfs"),
    (SYS_ACCESS, "access"),
    (SYS_FACCESSAT, "faccessat"),
    (SYS_CHMOD, "chmod"),
    (SYS_FCHMOD, "fchmod"),
    (SYS_FCHMODAT, "fchmodat"),
    (SYS_CHOWN, "chown"),
    (SYS_FCHOWN, "fchown"),
    (SYS_FCHOWNAT, "fchownat"),
    (SYS_LCHOWN, "lchown"),
    (SYS_UTIMENSAT, "utimensat"),
    (SYS_MKDIR, "mkdir"),
    (SYS_MKDIRAT, "mkdirat"),
    (SYS_RMDIR, "rmdir"),
    (SYS_UNLINK, "unlink"),
    (SYS_UNLINKAT, "unlinkat"),
    (SYS_LINK, "link"),
    (SYS_LINKAT, "linkat"),
    (SYS_SYMLINK, "symlink"),
    (SYS_SYMLINKAT, "symlinkat"),
    (SYS_READLINK, "readlink"),
    (SYS_READLINKAT, "readlinkat"),
    (SYS_RENAME, "rename"),
    (SYS_RENAMEAT, "renameat"),
    (SYS_MKFIFO, "mkfifo"),
    (SYS_MKFIFOAT, "mkfifoat"),
    (SYS_MKNOD, "mknod"),
    (SYS_MKNODAT, "mknodat"),
    (SYS_GETDENTS, "getdents"),
    (SYS_PIPE, "pipe"),
    (SYS_PIPE2, "pipe2"),
    (SYS_PATHCONF, "pathconf"),
    (SYS_FPATHCONF, "fpathconf"),
    (SYS_SETXATTR, "setxattr"),
    (SYS_GETXATTR, "getxattr"),
    (SYS_LISTXATTR, "listxattr"),
    (SYS_REMOVEXATTR, "removexattr"),
    (SYS_LSETXATTR, "lsetxattr"),
    (SYS_LGETXATTR, "lgetxattr"),
    (SYS_CLOCK_GETTIME, "clock_gettime"),
    (SYS_CLOCK_SETTIME, "clock_settime"),
    (SYS_CLOCK_GETRES, "clock_getres"),
    (SYS_CLOCK_NANOSLEEP, "clock_nanosleep"),
    (SYS_NANOSLEEP, "nanosleep"),
    (SYS_ALARM, "alarm"),
    (SYS_GETTIMEOFDAY, "gettimeofday"),
    (SYS_SETTIMEOFDAY, "settimeofday"),
    (SYS_TIMES, "times"),
    (SYS_KILL, "kill"),
    (SYS_KILLPG, "killpg"),
    (SYS_SIGACTION, "sigaction"),
    (SYS_SIGPROCMASK, "sigprocmask"),
    (SYS_SIGPENDING, "sigpending"),
    (SYS_SIGSUSPEND, "sigsuspend"),
    (SYS_SIGWAIT, "sigwait"),
    (SYS_SIGWAITINFO, "sigwaitinfo"),
    (SYS_SIGTIMEDWAIT, "sigtimedwait"),
    (SYS_SIGQUEUE, "sigqueue"),
    (SYS_SIGALTSTACK, "sigaltstack"),
    (SYS_PTHREAD_SIGMASK, "pthread_sigmask"),
    (SYS_PTHREAD_KILL, "pthread_kill"),
    (SYS_SIGRETURN, "sigreturn"),
    (SYS_SELECT, "select"),
    (SYS_PSELECT, "pselect"),
    (SYS_POLL, "poll"),
    (SYS_PPOLL, "ppoll"),
    (SYS_MSGGET, "msgget"),
    (SYS_MSGSND, "msgsnd"),
    (SYS_MSGRCV, "msgrcv"),
    (SYS_MSGCTL, "msgctl"),
    (SYS_SEMGET, "semget"),
    (SYS_SEMOP, "semop"),
    (SYS_SEMCTL, "semctl"),
    (SYS_SHMGET, "shmget"),
    (SYS_SHMAT, "shmat"),
    (SYS_SHMDT, "shmdt"),
    (SYS_SHMCTL, "shmctl"),
    (SYS_MQ_OPEN, "mq_open"),
    (SYS_MQ_CLOSE, "mq_close"),
    (SYS_MQ_UNLINK, "mq_unlink"),
    (SYS_MQ_SEND, "mq_send"),
    (SYS_MQ_RECEIVE, "mq_receive"),
    (SYS_MQ_NOTIFY, "mq_notify"),
    (SYS_MQ_GETATTR, "mq_getattr"),
    (SYS_MQ_SETATTR, "mq_setattr"),
    (SYS_SHM_OPEN, "shm_open"),
    (SYS_SHM_UNLINK, "shm_unlink"),
    (SYS_SEM_OPEN, "sem_open"),
    (SYS_SEM_CLOSE, "sem_close"),
    (SYS_SEM_UNLINK, "sem_unlink"),
    (SYS_SEM_WAIT, "sem_wait"),
    (SYS_SEM_TRYWAIT, "sem_trywait"),
    (SYS_SEM_TIMEDWAIT, "sem_timedwait"),
    (SYS_SEM_POST, "sem_post"),
    (SYS_SEM_GETVALUE, "sem_getvalue"),
    (SYS_MMAP, "mmap"),
    (SYS_MUNMAP, "munmap"),
    (SYS_MPROTECT, "mprotect"),
    (SYS_MSYNC, "msync"),
    (SYS_MADVISE, "madvise"),
    (SYS_MLOCK, "mlock"),
    (SYS_MUNLOCK, "munlock"),
    (SYS_MLOCKALL, "mlockall"),
    (SYS_MUNLOCKALL, "munlockall"),
    (SYS_MINCORE, "mincore"),
    (SYS_BRK, "brk"),
    (SYS_SOCKET, "socket"),
    (SYS_BIND, "bind"),
    (SYS_LISTEN, "listen"),
    (SYS_ACCEPT, "accept"),
    (SYS_ACCEPT4, "accept4"),
    (SYS_CONNECT, "connect"),
    (SYS_SEND, "send"),
    (SYS_SENDTO, "sendto"),
    (SYS_SENDMSG, "sendmsg"),
    (SYS_RECV, "recv"),
    (SYS_RECVFROM, "recvfrom"),
    (SYS_RECVMSG, "recvmsg"),
    (SYS_SHUTDOWN, "shutdown"),
    (SYS_GETSOCKOPT, "getsockopt"),
    (SYS_SETSOCKOPT, "setsockopt"),
    (SYS_GETSOCKNAME, "getsockname"),
    (SYS_GETPEERNAME, "getpeername"),
    (SYS_SOCKETPAIR, "socketpair"),
    (SYS_SCHED_YIELD, "sched_yield"),
    (SYS_SCHED_SETSCHEDULER, "sched_setscheduler"),
    (SYS_SCHED_GETSCHEDULER, "sched_getscheduler"),
    (SYS_SCHED_SETPARAM, "sched_setparam"),
    (SYS_SCHED_GETPARAM, "sched_getparam"),
    (SYS_SCHED_GET_PRIORITY_MAX, "sched_get_priority_max"),
    (SYS_SCHED_GET_PRIORITY_MIN, "sched_get_priority_min"),
    (SYS_SCHED_RR_GET_INTERVAL, "sched_rr_get_interval"),
    (SYS_SCHED_SETAFFINITY, "sched_setaffinity"),
    (SYS_SCHED_GETAFFINITY, "sched_getaffinity"),
    (SYS_TIMER_CREATE, "timer_create"),
    (SYS_TIMER_DELETE, "timer_delete"),
    (SYS_TIMER_SETTIME, "timer_settime"),
    (SYS_TIMER_GETTIME, "timer_gettime"),
    (SYS_TIMER_GETOVERRUN, "timer_getoverrun"),
    (SYS_AIO_READ, "aio_read"),
    (SYS_AIO_WRITE, "aio_write"),
    (SYS_AIO_ERROR, "aio_error"),
    (SYS_AIO_RETURN, "aio_return"),
    (SYS_AIO_CANCEL, "aio_cancel"),
    (SYS_AIO_SUSPEND, "aio_suspend"),
    (SYS_AIO_FSYNC, "aio_fsync"),
    (SYS_LIO_LISTIO, "lio_listio"),
    (SYS_SYSCONF, "sysconf"),
    (SYS_CONFSTR, "confstr"),
    (SYS_OPENPTY, "openpty"),
    (SYS_PTSNAME, "ptsname"),
    (SYS_TCGETATTR, "tcgetattr"),
    (SYS_TCSETATTR, "tcsetattr"),
    (SYS_TCSETPGRP, "tcsetpgrp"),
    (SYS_TCGETPGRP, "tcgetpgrp"),
    (SYS_TCSENDBREAK, "tcsendbreak"),
    (SYS_TCDRAIN, "tcdrain"),
    (SYS_TCFLUSH, "tcflush"),
    (SYS_TCFLOW, "tcflow"),
    (SYS_ISATTY, "isatty"),
    (SYS_TTYNAME, "ttyname"),
    (SYS_FALLOCATE, "fallocate"),
    (SYS_NXOBJ_CREATE, "nxobj_create"),
    (SYS_NXOBJ_OPEN, "nxobj_open"),
    (SYS_NXOBJ_CLOSE, "nxobj_close"),
    (SYS_NXOBJ_DUPLICATE, "nxobj_duplicate"),
    (SYS_NXOBJ_QUERY, "nxobj_query"),
    (SYS_NXPORT_CALL, "nxport_call"),
    (SYS_NXWAIT, "nxwait"),
];

/// The name of a syscall number, or `None` if the number is unassigned.
///
/// `None` must produce `ENOSYS`, never a guess.
#[must_use]
pub fn name_of(number: Number) -> Option<&'static str> {
    // The table is sorted and small; a binary search keeps the trace path off the hot
    // path without pulling in a HashMap.
    let mut lo = 0usize;
    let mut hi = TABLE.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match TABLE[mid].0.cmp(&number) {
            core::cmp::Ordering::Equal => return Some(TABLE[mid].1),
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

/// The number of a syscall name, or `None` if no such call exists.
#[must_use]
pub fn number_of(name: &str) -> Option<Number> {
    TABLE.iter().find(|(_, n)| *n == name).map(|(nr, _)| *nr)
}

/// The subsystem a number belongs to, for diagnostics.
#[must_use]
pub const fn subsystem(number: Number) -> &'static str {
    match number {
        0..=99 => "process",
        100..=199 => "file",
        200..=249 => "xattr",
        250..=299 => "time",
        300..=349 => "signal",
        350..=399 => "ipc",
        400..=449 => "memory",
        450..=499 => "network",
        500..=549 => "realtime",
        550..=599 => "misc",
        600..=699 => "native",
        _ => "unassigned",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_unique() {
        let mut prev = None;
        for (number, name) in TABLE {
            if let Some(p) = prev {
                assert!(*number > p, "table not sorted: {number} follows {p}");
            }
            prev = Some(*number);
            assert!(!name.is_empty(), "syscall {number} has an empty name");
        }
        let mut names: Vec<&str> = TABLE.iter().map(|(_, n)| *n).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate syscall name in the table");
    }

    #[test]
    fn every_entry_is_reachable_by_binary_search() {
        for (number, name) in TABLE {
            assert_eq!(name_of(*number), Some(*name), "name_of({number}) failed");
            assert_eq!(number_of(name), Some(*number), "number_of({name}) failed");
        }
    }

    #[test]
    fn unassigned_numbers_have_no_name() {
        assert_eq!(name_of(HIGHEST + 1), None);
        assert_eq!(name_of(u16::MAX), None);
        // A gap inside the process range must also miss.
        assert_eq!(name_of(35), None);
    }

    #[test]
    fn highest_covers_the_whole_table() {
        let max = TABLE.iter().map(|(n, _)| *n).max().unwrap();
        assert_eq!(max, HIGHEST, "HIGHEST must equal the largest assigned number");
    }

    #[test]
    fn numbers_land_in_their_documented_subsystem_range() {
        for (number, name) in TABLE {
            let sub = subsystem(*number);
            assert_ne!(sub, "unassigned", "{name} ({number}) is outside every range");
        }
        assert_eq!(subsystem(SYS_READ), "file");
        assert_eq!(subsystem(SYS_KILL), "signal");
        assert_eq!(subsystem(SYS_MMAP), "memory");
        assert_eq!(subsystem(SYS_NXWAIT), "native");
    }

    #[test]
    fn native_calls_are_segregated_from_posix_numbers() {
        // A POSIX call must never be numbered in the native range, or a stray libc bug
        // could reach the executive directly.
        for (number, name) in TABLE {
            if *number >= 600 {
                assert!(name.starts_with("nx"), "{name} is in the native range");
            } else {
                assert!(!name.starts_with("nx"), "{name} looks native but is not");
            }
        }
    }

    #[test]
    fn the_posix_hot_path_is_present() {
        // The calls a shell needs to start. If one of these is missing, nothing else in
        // the milestone list matters.
        for name in [
            "fork", "execve", "waitid", "exit", "open", "close", "read", "write", "lseek",
            "dup2", "getpid", "getuid", "kill", "sigaction", "sigprocmask", "mmap",
            "ioctl", "fcntl", "getdents", "pipe", "select",
        ] {
            assert!(number_of(name).is_some(), "missing the {name} syscall");
        }
    }

    #[test]
    fn numbers_fit_in_the_register_convention() {
        // rax carries the number; u16 keeps the dispatch table a reasonable size and
        // leaves room for a 256-entry-per-subsystem table if we ever shard it.
        assert!(TABLE.iter().all(|(n, _)| *n <= u16::MAX));
        assert!(TABLE.len() < 4096, "dispatch table is getting large");
    }
}
