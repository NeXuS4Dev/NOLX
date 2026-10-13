//! The I/O Manager, Cache Manager, and VFS kernel integration.
//!
//! All core data structures and state machines live in [`nolx_vfs`] so they are host-tested
//! under `cargo test -p nolx-vfs` with `#![forbid(unsafe_code)]`. This module re-exports the
//! executive I/O primitives for the rest of the kernel and runs the Milestone 7 boot
//! self-tests during [`init`]:
//!
//! 1. **I/O Manager self-test**: registers filter and bottom drivers, builds a 2-layer device
//!    stack, dispatches a synchronous IORB, an asynchronous completion-port IORB, and a
//!    cancelled asynchronous IORB, and verifies reference-tracked driver unload.
//! 2. **Cache Manager self-test**: exercises `SharedCacheMap` `copy_write`, `copy_read`,
//!    sequential read-ahead prefetching, the background lazy writer, and `MmFlushSection`.
//! 3. **VFS and filesystems self-test**: mounts `tmpfs` (`/`), `devfs` (`/dev`), `initrd`
//!    (`/initrd`), and `nolxfs` (`/mnt/nolxfs`), exercising cross-mount pathname resolution,
//!    device nodes, `initrd` reads, `NOLXFS` persistence, and `tmpfs` read/write/unlink-while-open.

use core::sync::atomic::{AtomicU64, Ordering};

use nolx_hal_core::console::HalConsole;
use nolx_types::abi::{open_flags, seek};
use nolx_types::errno;
use nolx_types::scalars::{S_IFDIR, S_IFREG};
use nolx_vfs::cache::{CacheManager, CACHE_PAGE_SIZE};
use nolx_vfs::devfs::Devfs;
use nolx_vfs::initrd::{build_cpio_newc, InitrdFs};
use nolx_vfs::mount::NodeId;
use nolx_vfs::nolxfs_vfs::NolxfsVolume;
use nolx_vfs::path::Final;
use nolx_vfs::tmpfs::{Tmpfs, TMPFS_ROOT_INO};
use nolx_vfs::vfs::{FsInstance, ProcessVfsContext, VirtualFileSystem};

pub use nolx_vfs::io::{
    iorb_flags, major, minor, sl_control, CancelRoutine, CompletionDisposition, CompletionOutcome,
    CompletionPacket, CompletionRoutine, DeviceObject, DispatchFn, DriverObject, IoCompletionPort,
    IoManager, IoStackLocation, IoStatus, Iorb, UnloadFn, MAJOR_FUNCTION_COUNT,
};

static UNLOADED_DRIVER_ID: AtomicU64 = AtomicU64::new(0);

fn on_driver_unload(driver_id: u64) {
    UNLOADED_DRIVER_ID.store(driver_id, Ordering::SeqCst);
}

fn filter_completion(iorb: &mut Iorb, _depth: u8, bonus: u64) -> CompletionDisposition {
    iorb.information = iorb.information.saturating_add(bonus);
    CompletionDisposition::ContinueCompletion
}

fn filter_dispatch(iorb: &mut Iorb) -> IoStatus {
    iorb.copy_current_to_next();
    iorb.set_completion_routine(filter_completion, 10, sl_control::SL_INVOKE_ON_SUCCESS);
    let _ = iorb.push_down();
    IoStatus::Success
}

fn bottom_read_sync(iorb: &mut Iorb) -> IoStatus {
    iorb.information = iorb.current().map_or(0, |l| l.length);
    IoStatus::Success
}

fn bottom_cancel_cb(iorb: &mut Iorb) {
    iorb.information = 0xC4C0;
}

fn bottom_write_async(iorb: &mut Iorb) -> IoStatus {
    iorb.set_cancel_routine(Some(bottom_cancel_cb));
    IoStatus::Pending
}

fn run_io_selftest(console: &mut dyn HalConsole) -> Result<(), &'static str> {
    UNLOADED_DRIVER_ID.store(0, Ordering::SeqCst);
    let mut mgr = IoManager::new();

    let mut filter_tbl: [Option<DispatchFn>; MAJOR_FUNCTION_COUNT] = [None; MAJOR_FUNCTION_COUNT];
    filter_tbl[major::READ] = Some(filter_dispatch);
    filter_tbl[major::WRITE] = Some(filter_dispatch);

    let mut bottom_tbl: [Option<DispatchFn>; MAJOR_FUNCTION_COUNT] = [None; MAJOR_FUNCTION_COUNT];
    bottom_tbl[major::READ] = Some(bottom_read_sync);
    bottom_tbl[major::WRITE] = Some(bottom_write_async);

    let bottom_drv = mgr.register_driver(
        "\\Driver\\Bottom",
        bottom_tbl,
        Some(on_driver_unload),
        false,
    );
    let filter_drv = mgr.register_driver("\\Driver\\Filter", filter_tbl, None, true);

    let bottom_dev = mgr
        .create_device(bottom_drv, "\\Device\\Bottom0", 4)
        .map_err(|_| "io selftest: failed to create bottom device")?;
    let filter_dev = mgr
        .create_device(filter_drv, "\\Device\\Filter0", 4)
        .map_err(|_| "io selftest: failed to create filter device")?;

    let stack_depth = mgr
        .attach_device(filter_dev, bottom_dev)
        .map_err(|_| "io selftest: failed to attach filter above bottom")?;
    if stack_depth != 2 {
        return Err("io selftest: unexpected stack depth after attach");
    }

    let port = mgr.create_completion_port(2, 8);
    mgr.bind_completion_port(port, 1, 0x7001)
        .map_err(|_| "io selftest: failed to bind completion port")?;

    // 1. Synchronous READ IORB through the 2-device stack.
    let mut sync_iorb = mgr
        .allocate_iorb(filter_dev, major::READ as u8, 1)
        .map_err(|_| "io selftest: failed to allocate sync IORB")?;
    sync_iorb.flags = iorb_flags::SYNCHRONOUS_API;
    sync_iorb.current_mut().unwrap().length = 64;
    let sync_status = mgr
        .submit_iorb(sync_iorb)
        .map_err(|_| "io selftest: sync IORB submission failed")?;
    if sync_status != IoStatus::Success {
        return Err("io selftest: sync IORB did not succeed");
    }
    let pkt1 = mgr
        .dequeue_completion(port)
        .map_err(|_| "io selftest: port dequeue failed")?
        .ok_or("io selftest: missing sync completion packet")?;
    if pkt1.key != 0x7001 || pkt1.status != IoStatus::Success || pkt1.information != 74 {
        return Err("io selftest: sync completion packet mismatch");
    }

    // 2. Asynchronous WRITE IORB that pends and then completes to the completion port.
    let async_iorb = mgr
        .allocate_iorb(filter_dev, major::WRITE as u8, 1)
        .map_err(|_| "io selftest: failed to allocate async IORB")?;
    let async_id = async_iorb.id;
    let pending = mgr
        .submit_iorb(async_iorb)
        .map_err(|_| "io selftest: async IORB submission failed")?;
    if pending != IoStatus::Pending {
        return Err("io selftest: async IORB did not return Pending");
    }
    let outcome = mgr
        .complete_pending_iorb(async_id, IoStatus::Success, 128)
        .map_err(|_| "io selftest: complete_pending_iorb failed")?;
    if outcome.status != IoStatus::Success || outcome.information != 138 {
        return Err("io selftest: async completion outcome mismatch");
    }
    let pkt2 = mgr
        .dequeue_completion(port)
        .map_err(|_| "io selftest: port dequeue 2 failed")?
        .ok_or("io selftest: missing async completion packet")?;
    if pkt2.iorb_id != async_id || pkt2.information != 138 {
        return Err("io selftest: async completion packet mismatch");
    }

    // 3. Cancelable asynchronous WRITE IORB cancelled while in flight.
    let cancel_iorb = mgr
        .allocate_iorb(filter_dev, major::WRITE as u8, 1)
        .map_err(|_| "io selftest: failed to allocate cancelable IORB")?;
    let cancel_id = cancel_iorb.id;
    if mgr.submit_iorb(cancel_iorb) != Ok(IoStatus::Pending) {
        return Err("io selftest: cancelable IORB did not pend");
    }
    let ran_cancel = mgr
        .cancel_pending_iorb(cancel_id)
        .map_err(|_| "io selftest: cancel_pending_iorb failed")?;
    if !ran_cancel {
        return Err("io selftest: cancel routine did not execute");
    }
    let pkt3 = mgr
        .dequeue_completion(port)
        .map_err(|_| "io selftest: port dequeue 3 failed")?
        .ok_or("io selftest: missing cancelled completion packet")?;
    if pkt3.status != IoStatus::Cancelled || pkt3.information != 0xC4C0 {
        return Err("io selftest: cancelled packet status/info mismatch");
    }

    // 4. Reference-tracked driver unload: Busy while attached/referenced, Ok once detached & closed.
    mgr.reference_device(bottom_dev)
        .map_err(|_| "io selftest: reference_device failed")?;
    if mgr.unload_driver(bottom_drv) != Err(IoStatus::Busy) {
        return Err("io selftest: unload succeeded while attached and referenced");
    }
    mgr.detach_device(filter_dev)
        .map_err(|_| "io selftest: detach_device failed")?;
    if mgr.unload_driver(bottom_drv) != Err(IoStatus::Busy) {
        return Err("io selftest: unload succeeded while device reference count > 0");
    }
    mgr.dereference_device(bottom_dev)
        .map_err(|_| "io selftest: dereference_device failed")?;
    mgr.unload_driver(bottom_drv)
        .map_err(|_| "io selftest: unload_driver failed after detach and dereference")?;
    if UNLOADED_DRIVER_ID.load(Ordering::SeqCst) != bottom_drv {
        return Err("io selftest: driver unload callback did not run");
    }

    let _ = writeln!(
        console,
        "NOLX: io selftest: 2 drivers, 2 stacked devices, 3 IORBs (1 sync, 1 async completion \
         port, 1 cancelled), unload busy->ok; audit ok"
    );
    Ok(())
}

fn run_cache_selftest(console: &mut dyn HalConsole) -> Result<(), &'static str> {
    let mut backing = Tmpfs::new();
    let (ino, _) = backing
        .create_file(TMPFS_ROOT_INO, "cached.bin", 0o644, 0, 0, true)
        .map_err(|_| "cache selftest: failed to create backing file")?;
    let total_size = (CACHE_PAGE_SIZE * 4) as u64;
    backing
        .truncate(ino, total_size)
        .map_err(|_| "cache selftest: failed to size backing file")?;

    let node = NodeId::new(1, ino);
    let mut cm = CacheManager::new();
    cm.initialize_cache_map(node, total_size);

    let msg0 = b"NOLX-CACHE-PAGE-0";
    let msg1 = b"NOLX-CACHE-PAGE-1";
    cm.copy_write(node, 0, msg0, false, &mut backing)
        .map_err(|_| "cache selftest: copy_write page 0 failed")?;
    cm.copy_write(node, CACHE_PAGE_SIZE as u64, msg1, false, &mut backing)
        .map_err(|_| "cache selftest: copy_write page 1 failed")?;

    if cm.map(node).map_or(0, |m| m.dirty_pages()) != 2 {
        return Err("cache selftest: expected 2 dirty pages after 2 copy-writes");
    }

    // Sequential copy_read across page 0 and page 1 triggers 2 read-ahead pages (pages 2 and 3).
    let mut rbuf = [0u8; 17];
    cm.copy_read(node, 0, &mut rbuf, &mut backing)
        .map_err(|_| "cache selftest: copy_read page 0 failed")?;
    if &rbuf != msg0 {
        return Err("cache selftest: page 0 copy_read mismatch");
    }
    cm.copy_read(node, CACHE_PAGE_SIZE as u64, &mut rbuf, &mut backing)
        .map_err(|_| "cache selftest: copy_read page 1 failed")?;
    if &rbuf != msg1 {
        return Err("cache selftest: page 1 copy_read mismatch");
    }
    cm.copy_read(node, (CACHE_PAGE_SIZE * 2) as u64, &mut rbuf, &mut backing)
        .map_err(|_| "cache selftest: copy_read page 2 failed")?;

    let stats_mid = cm.stats();
    if stats_mid.read_ahead_pages != 2 {
        return Err("cache selftest: sequential read did not prefetch 2 pages");
    }

    let lazy = cm
        .lazy_write_tick(1, &mut backing)
        .map_err(|_| "cache selftest: lazy_write_tick failed")?;
    let explicit = cm
        .mm_flush_section(node, &mut backing)
        .map_err(|_| "cache selftest: mm_flush_section failed")?;
    if lazy.pages_flushed + explicit.pages_flushed != 2 {
        return Err("cache selftest: expected lazy writer + MmFlushSection to flush 2 pages");
    }

    let mut verify_buf = [0u8; 17];
    backing
        .read(ino, 0, &mut verify_buf)
        .map_err(|_| "cache selftest: backing read 0 failed")?;
    if &verify_buf != msg0 {
        return Err("cache selftest: backing store page 0 mismatch after flush");
    }
    backing
        .read(ino, CACHE_PAGE_SIZE as u64, &mut verify_buf)
        .map_err(|_| "cache selftest: backing read 1 failed")?;
    if &verify_buf != msg1 {
        return Err("cache selftest: backing store page 1 mismatch after flush");
    }

    let _ = writeln!(
        console,
        "NOLX: cache selftest: 1 shared map, 2 copy-writes, 3 copy-reads, {} read-ahead pages, \
         lazy writer + MmFlushSection flushed {} dirty pages; audit ok",
        cm.stats().read_ahead_pages,
        lazy.pages_flushed + explicit.pages_flushed,
    );
    Ok(())
}

fn run_vfs_selftest(console: &mut dyn HalConsole) -> Result<(), &'static str> {
    let mut vfs = VirtualFileSystem::new();
    let root = vfs
        .mount_root(FsInstance::Tmpfs(Tmpfs::new()), "tmpfs")
        .map_err(|_| "vfs selftest: failed to mount root tmpfs")?;
    let mut ctx = ProcessVfsContext::new(root);

    vfs.mkdir(&ctx, b"/dev", 0o755)
        .map_err(|_| "vfs selftest: mkdir /dev failed")?;
    vfs.mkdir(&ctx, b"/initrd", 0o755)
        .map_err(|_| "vfs selftest: mkdir /initrd failed")?;
    vfs.mkdir(&ctx, b"/mnt", 0o755)
        .map_err(|_| "vfs selftest: mkdir /mnt failed")?;
    vfs.mkdir(&ctx, b"/mnt/nolxfs", 0o755)
        .map_err(|_| "vfs selftest: mkdir /mnt/nolxfs failed")?;

    // Mount devfs at /dev.
    vfs.mount_at(&ctx, b"/dev", FsInstance::Devfs(Devfs::new()), 0, "devfs")
        .map_err(|_| "vfs selftest: mount devfs failed")?;

    // Mount cpio initrd at /initrd.
    let cpio_bytes = build_cpio_newc(&[
        ("etc", S_IFDIR | 0o755, b""),
        ("etc/motd", S_IFREG | 0o644, b"NOLX initrd ok\n"),
    ]);
    let initrd_fs =
        InitrdFs::from_archive(&cpio_bytes).map_err(|_| "vfs selftest: cpio parse failed")?;
    vfs.mount_at(&ctx, b"/initrd", FsInstance::Initrd(initrd_fs), 0, "initrd")
        .map_err(|_| "vfs selftest: mount initrd failed")?;

    // Format and mount NOLXFS at /mnt/nolxfs.
    let nolx_vol = NolxfsVolume::format_and_mount(4096, 96, "nolx-boot")
        .map_err(|_| "vfs selftest: nolxfs format_and_mount failed")?;
    vfs.mount_at(
        &ctx,
        b"/mnt/nolxfs",
        FsInstance::Nolxfs(nolx_vol),
        0,
        "nolxfs",
    )
    .map_err(|_| "vfs selftest: mount nolxfs failed")?;

    // 1. Exercise tmpfs: open(O_CREAT|O_RDWR), write 32 bytes, unlink-while-open, lseek, read, close.
    let payload = b"NOLX M7 tmpfs kernel selftest!\n\0";
    let fd = vfs
        .open(
            &mut ctx,
            b"/tmp_verify.txt",
            open_flags::O_CREAT | open_flags::O_EXCL | open_flags::O_RDWR,
            0o644,
        )
        .map_err(|_| "vfs selftest: tmpfs open failed")?;
    let written = vfs
        .write(&ctx, fd, payload)
        .map_err(|_| "vfs selftest: tmpfs write failed")?;
    if written != payload.len() {
        return Err("vfs selftest: tmpfs short write");
    }
    vfs.unlink(&ctx, b"/tmp_verify.txt")
        .map_err(|_| "vfs selftest: tmpfs unlink failed")?;
    if vfs.stat(&ctx, b"/tmp_verify.txt", Final::Follow) != Err(errno::ENOENT) {
        return Err("vfs selftest: unlinked tmpfs file still visible by path");
    }
    vfs.lseek(&ctx, fd, 0, seek::SEEK_SET)
        .map_err(|_| "vfs selftest: tmpfs lseek failed")?;
    let mut read_back = [0u8; 32];
    let read_n = vfs
        .read(&ctx, fd, &mut read_back)
        .map_err(|_| "vfs selftest: tmpfs read failed")?;
    let tmpfs_verified = read_n == payload.len() && &read_back == payload;
    if !tmpfs_verified {
        return Err("vfs selftest: tmpfs read-back mismatch");
    }
    vfs.close(&mut ctx, fd)
        .map_err(|_| "vfs selftest: tmpfs close failed")?;

    // 2. Exercise devfs: /dev/null, /dev/zero, /dev/full.
    let fd_zero = vfs
        .open(&mut ctx, b"/dev/zero", open_flags::O_RDONLY, 0)
        .map_err(|_| "vfs selftest: open /dev/zero failed")?;
    let mut zbuf = [0xAAu8; 8];
    if vfs.read(&ctx, fd_zero, &mut zbuf) != Ok(8) || !zbuf.iter().all(|&b| b == 0) {
        return Err("vfs selftest: /dev/zero read failed");
    }
    vfs.close(&mut ctx, fd_zero)
        .map_err(|_| "vfs selftest: close /dev/zero failed")?;

    let fd_full = vfs
        .open(&mut ctx, b"/dev/full", open_flags::O_WRONLY, 0)
        .map_err(|_| "vfs selftest: open /dev/full failed")?;
    if vfs.write(&ctx, fd_full, b"x") != Err(errno::ENOSPC) {
        return Err("vfs selftest: /dev/full write did not return ENOSPC");
    }
    vfs.close(&mut ctx, fd_full)
        .map_err(|_| "vfs selftest: close /dev/full failed")?;

    // 3. Exercise initrd: read /initrd/etc/motd.
    let fd_motd = vfs
        .open(&mut ctx, b"/initrd/etc/motd", open_flags::O_RDONLY, 0)
        .map_err(|_| "vfs selftest: open /initrd/etc/motd failed")?;
    let mut motd_buf = [0u8; 15];
    if vfs.read(&ctx, fd_motd, &mut motd_buf) != Ok(15) || &motd_buf != b"NOLX initrd ok\n" {
        return Err("vfs selftest: /initrd/etc/motd content mismatch");
    }
    vfs.close(&mut ctx, fd_motd)
        .map_err(|_| "vfs selftest: close /initrd/etc/motd failed")?;

    // 4. Exercise NOLXFS: create, write, lseek, read, and cross-mount `..` traversal.
    let fd_nolx = vfs
        .open(
            &mut ctx,
            b"/mnt/nolxfs/boot.txt",
            open_flags::O_CREAT | open_flags::O_RDWR,
            0o644,
        )
        .map_err(|_| "vfs selftest: open /mnt/nolxfs/boot.txt failed")?;
    if vfs.write(&ctx, fd_nolx, b"nolxfs-v1-ok") != Ok(12) {
        return Err("vfs selftest: nolxfs write failed");
    }
    vfs.lseek(&ctx, fd_nolx, 0, seek::SEEK_SET)
        .map_err(|_| "vfs selftest: nolxfs lseek failed")?;
    let mut nbuf = [0u8; 12];
    if vfs.read(&ctx, fd_nolx, &mut nbuf) != Ok(12) || &nbuf != b"nolxfs-v1-ok" {
        return Err("vfs selftest: nolxfs read mismatch");
    }
    vfs.close(&mut ctx, fd_nolx)
        .map_err(|_| "vfs selftest: close nolxfs failed")?;

    // 5. Exercise the M8 NOLXFS operations through the VFS: hard links,
    // symlinks, rename, mkdir/rmdir and unlink, all COW-persisted.
    if vfs
        .stat(&ctx, b"/mnt/nolxfs/boot.txt", Final::Follow)
        .is_err()
    {
        return Err("vfs selftest: nolxfs stat before link failed");
    }
    vfs.link(&ctx, b"/mnt/nolxfs/boot.txt", b"/mnt/nolxfs/boot-link.txt")
        .map_err(|e| match e {
            errno::ENOENT => "vfs selftest: nolxfs link failed (ENOENT)",
            errno::EEXIST => "vfs selftest: nolxfs link failed (EEXIST)",
            errno::ENOSPC => "vfs selftest: nolxfs link failed (ENOSPC)",
            errno::EIO => "vfs selftest: nolxfs link failed (EIO)",
            _ => "vfs selftest: nolxfs link failed (other)",
        })?;
    let st = vfs
        .stat(&ctx, b"/mnt/nolxfs/boot.txt", Final::Follow)
        .map_err(|_| "vfs selftest: nolxfs stat after link failed")?;
    if st.st_nlink != 2 {
        return Err("vfs selftest: nolxfs nlink != 2 after link");
    }
    vfs.rename(
        &ctx,
        b"/mnt/nolxfs/boot-link.txt",
        b"/mnt/nolxfs/boot-renamed.txt",
    )
    .map_err(|_| "vfs selftest: nolxfs rename failed")?;
    vfs.symlink(&ctx, b"boot.txt", b"/mnt/nolxfs/boot-sym")
        .map_err(|_| "vfs selftest: nolxfs symlink failed")?;
    let target = vfs
        .readlink(&ctx, b"/mnt/nolxfs/boot-sym")
        .map_err(|_| "vfs selftest: nolxfs readlink failed")?;
    if target != b"boot.txt" {
        return Err("vfs selftest: nolxfs readlink mismatch");
    }
    vfs.mkdir(&ctx, b"/mnt/nolxfs/sub", 0o755)
        .map_err(|_| "vfs selftest: nolxfs mkdir sub failed")?;
    vfs.rmdir(&ctx, b"/mnt/nolxfs/sub")
        .map_err(|_| "vfs selftest: nolxfs rmdir sub failed")?;
    vfs.unlink(&ctx, b"/mnt/nolxfs/boot-renamed.txt")
        .map_err(|_| "vfs selftest: nolxfs unlink renamed failed")?;
    vfs.unlink(&ctx, b"/mnt/nolxfs/boot-sym")
        .map_err(|_| "vfs selftest: nolxfs unlink sym failed")?;

    let _ = writeln!(
        console,
        "NOLX: vfs selftest: mounted tmpfs(/), devfs(/dev), initrd(/initrd), nolxfs(/mnt/nolxfs); \
         tmpfs write/read {written} bytes verified={tmpfs_verified}, devfs ok, initrd ok, nolxfs ok, nolxfs-m8 ok; audit ok",
    );
    Ok(())
}

/// Initialize the I/O Manager, Cache Manager, and VFS, running their boot self-tests.
pub fn init(console: &mut dyn HalConsole) {
    let _ = writeln!(
        console,
        "io: {} major codes, stack-loc {} B, IORB {} B",
        MAJOR_FUNCTION_COUNT,
        core::mem::size_of::<IoStackLocation>(),
        core::mem::size_of::<Iorb>(),
    );
    if let Err(reason) = run_io_selftest(console) {
        panic!("io selftest failed: {reason}");
    }
    if let Err(reason) = run_cache_selftest(console) {
        panic!("cache selftest failed: {reason}");
    }
    if let Err(reason) = run_vfs_selftest(console) {
        panic!("vfs selftest failed: {reason}");
    }
    let _ = writeln!(console, "NOLX: io online");
}
