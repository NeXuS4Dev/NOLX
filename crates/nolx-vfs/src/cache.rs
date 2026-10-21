//! The Cache Manager: shared cache maps, copy-read/copy-write, read-ahead, lazy writer, and
//! `MmFlushSection`.
//!
//! Per `docs/architecture/04-io-manager-drivers.md`, NOLX caches by **file offset**, not only
//! by raw disk block. A cached file holds a [`SharedCacheMap`] indexed by `(NodeId, page_index)`:
//!
//! * **Buffered read (`CcCopyRead`)**: copies directly from cached 4 KiB pages; on a miss,
//!   faults the page in via a paging read (`PAGING_IO`) against the backing store. When it
//!   detects sequential access (`page_index == last_read_page + 1`), it prefetches up to
//!   `read_ahead_granularity` subsequent pages within `file_size`.
//! * **Buffered write (`CcCopyWrite`)**: copies user bytes into cached pages, faulting in
//!   partial uncached pages first so surrounding bytes are preserved, marks modified pages
//!   dirty, and extends the cached `file_size`.
//! * **Lazy Writer (`CcLazyWriteTick`)**: background pass that flushes up to a bounded number
//!   of dirty pages in deterministic `(NodeId, page_index)` order via paging writes.
//! * **`MmFlushSection`**: flushes all dirty pages of one file (or every cached file) for
//!   `fsync` / `msync` / clean unmount.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::io::IoStatus;
use crate::mount::NodeId;

/// Size in bytes of one Cache Manager page (4 KiB, matching the x86-64 base frame size).
pub const CACHE_PAGE_SIZE: usize = 4096;

/// Default number of sequential pages prefetched by read-ahead when sequential access is detected.
pub const DEFAULT_READ_AHEAD_PAGES: u32 = 2;

/// Backing store interface used by the Cache Manager when faulting in or flushing pages
/// (`iorb_flags::PAGING_IO`).
pub trait CacheBacking {
    /// Paging read of up to `buf.len()` bytes starting at `offset` in `node`.
    ///
    /// Returns the number of bytes read from the underlying file/volume (`0` at or past EOF).
    ///
    /// # Errors
    ///
    /// Returns an [`IoStatus`] if the underlying device or filesystem fails the paging read.
    fn paging_read(&mut self, node: NodeId, offset: u64, buf: &mut [u8])
        -> Result<usize, IoStatus>;

    /// Paging write of `data` at `offset` in `node`.
    ///
    /// # Errors
    ///
    /// Returns an [`IoStatus`] if the underlying device or filesystem fails the paging write.
    fn paging_write(&mut self, node: NodeId, offset: u64, data: &[u8]) -> Result<usize, IoStatus>;
}

/// One 4 KiB page inside a [`SharedCacheMap`].
#[derive(Clone, Debug)]
pub struct CachePage {
    /// The 4 KiB page payload.
    pub data: Vec<u8>,
    /// `true` if modified by `copy_write` and not yet written back by the lazy writer or
    /// `mm_flush_section`.
    pub dirty: bool,
    /// `true` if populated by sequential read-ahead before an explicit demand read.
    pub prefetched: bool,
}

impl CachePage {
    fn zeroed() -> Self {
        Self {
            data: vec![0u8; CACHE_PAGE_SIZE],
            dirty: false,
            prefetched: false,
        }
    }
}

/// Per-file `SharedCacheMap` connecting a VFS file object to its cached pages.
#[derive(Clone, Debug)]
pub struct SharedCacheMap {
    /// Cached file identifier.
    pub node: NodeId,
    /// Current logical file size in bytes.
    pub file_size: u64,
    /// Number of open file objects sharing this cache map.
    pub open_count: u32,
    /// Number of pages to prefetch when sequential reads are observed.
    pub read_ahead_granularity: u32,
    /// Last page index read by a demand `copy_read`, used for sequential detection.
    pub last_read_page: Option<u64>,
    /// Cached pages indexed by `offset / CACHE_PAGE_SIZE`.
    pages: BTreeMap<u64, CachePage>,
}

impl SharedCacheMap {
    /// Create a new shared cache map for `node` with initial `file_size`.
    #[must_use]
    pub const fn new(node: NodeId, file_size: u64) -> Self {
        Self {
            node,
            file_size,
            open_count: 1,
            read_ahead_granularity: DEFAULT_READ_AHEAD_PAGES,
            last_read_page: None,
            pages: BTreeMap::new(),
        }
    }

    /// Number of resident pages in this map.
    #[must_use]
    pub fn resident_pages(&self) -> usize {
        self.pages.len()
    }

    /// Number of dirty resident pages in this map.
    #[must_use]
    pub fn dirty_pages(&self) -> usize {
        self.pages.values().filter(|p| p.dirty).count()
    }

    /// `true` if `page_index` is currently resident in the cache map.
    #[must_use]
    pub fn is_resident(&self, page_index: u64) -> bool {
        self.pages.contains_key(&page_index)
    }
}

/// Summary returned by [`CacheManager::mm_flush_section`] and [`CacheManager::lazy_write_tick`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlushReport {
    /// Number of dirty 4 KiB pages written back to the backing store.
    pub pages_flushed: u64,
    /// Total valid file bytes written back across those pages.
    pub bytes_flushed: u64,
}

/// Cumulative statistics maintained by the [`CacheManager`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Demand page accesses satisfied directly from a resident [`CachePage`].
    pub page_hits: u64,
    /// Demand page accesses that faulted in from the [`CacheBacking`].
    pub page_misses: u64,
    /// Pages prefetched by sequential read-ahead.
    pub read_ahead_pages: u64,
    /// Pages dirtied by [`CacheManager::copy_write`].
    pub pages_dirtied: u64,
    /// Pages flushed by [`CacheManager::lazy_write_tick`].
    pub lazy_writer_flushes: u64,
    /// Pages flushed by [`CacheManager::mm_flush_section`].
    pub explicit_flushes: u64,
}

/// The executive Cache Manager.
#[derive(Default)]
pub struct CacheManager {
    maps: BTreeMap<NodeId, SharedCacheMap>,
    stats: CacheStats,
}

impl CacheManager {
    /// Create an empty Cache Manager.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            maps: BTreeMap::new(),
            stats: CacheStats {
                page_hits: 0,
                page_misses: 0,
                read_ahead_pages: 0,
                pages_dirtied: 0,
                lazy_writer_flushes: 0,
                explicit_flushes: 0,
            },
        }
    }

    /// Initialize or increment the reference count on `node`'s [`SharedCacheMap`]
    /// (`CcInitializeCacheMap`).
    pub fn initialize_cache_map(&mut self, node: NodeId, file_size: u64) -> &mut SharedCacheMap {
        let map = self
            .maps
            .entry(node)
            .and_modify(|m| {
                m.open_count = m.open_count.saturating_add(1);
                m.file_size = m.file_size.max(file_size);
            })
            .or_insert_with(|| SharedCacheMap::new(node, file_size));
        map
    }

    /// Inspect a file's [`SharedCacheMap`].
    #[must_use]
    pub fn map(&self, node: NodeId) -> Option<&SharedCacheMap> {
        self.maps.get(&node)
    }

    /// Cumulative Cache Manager counters.
    #[must_use]
    pub const fn stats(&self) -> CacheStats {
        self.stats
    }

    /// Buffered read (`CcCopyRead`) from `node` at `offset` into `buf`.
    ///
    /// Faults in missing pages via `backing.paging_read` and triggers sequential read-ahead
    /// when `page_index == last_read_page + 1`.
    pub fn copy_read(
        &mut self,
        node: NodeId,
        offset: u64,
        buf: &mut [u8],
        backing: &mut dyn CacheBacking,
    ) -> Result<usize, IoStatus> {
        let Some(file_size) = self.maps.get(&node).map(|m| m.file_size) else {
            return Err(IoStatus::InvalidParameter);
        };
        if offset >= file_size || buf.is_empty() {
            return Ok(0);
        }
        let max_avail = (file_size - offset).min(buf.len() as u64) as usize;
        let mut copied = 0usize;

        while copied < max_avail {
            let cur_off = offset + copied as u64;
            let page_idx = cur_off / (CACHE_PAGE_SIZE as u64);
            let page_off = (cur_off % (CACHE_PAGE_SIZE as u64)) as usize;
            let chunk = (CACHE_PAGE_SIZE - page_off).min(max_avail - copied);

            let sequential = self
                .maps
                .get(&node)
                .and_then(|m| m.last_read_page)
                .is_some_and(|prev| page_idx == prev.saturating_add(1));

            self.ensure_page_resident(node, page_idx, false, backing)?;
            if let Some(map) = self.maps.get_mut(&node) {
                map.last_read_page = Some(page_idx);
                let page = map.pages.get(&page_idx).unwrap();
                buf[copied..copied + chunk].copy_from_slice(&page.data[page_off..page_off + chunk]);
            }

            if sequential {
                self.trigger_read_ahead(node, page_idx, backing)?;
            }

            copied += chunk;
        }
        Ok(copied)
    }

    fn trigger_read_ahead(
        &mut self,
        node: NodeId,
        after_page: u64,
        backing: &mut dyn CacheBacking,
    ) -> Result<(), IoStatus> {
        let (file_size, granularity) = match self.maps.get(&node) {
            Some(m) => (m.file_size, m.read_ahead_granularity),
            None => return Ok(()),
        };
        let total_pages = file_size.div_ceil(CACHE_PAGE_SIZE as u64);
        for step in 1..=u64::from(granularity) {
            let target_page = after_page + step;
            if target_page >= total_pages {
                break;
            }
            let already = self
                .maps
                .get(&node)
                .is_some_and(|m| m.pages.contains_key(&target_page));
            if !already {
                self.ensure_page_resident(node, target_page, true, backing)?;
            }
        }
        Ok(())
    }

    fn ensure_page_resident(
        &mut self,
        node: NodeId,
        page_idx: u64,
        is_prefetch: bool,
        backing: &mut dyn CacheBacking,
    ) -> Result<(), IoStatus> {
        let already = self
            .maps
            .get(&node)
            .is_some_and(|m| m.pages.contains_key(&page_idx));
        if already {
            if !is_prefetch {
                self.stats.page_hits += 1;
            }
            return Ok(());
        }

        let mut page = CachePage::zeroed();
        page.prefetched = is_prefetch;
        let page_base = page_idx * (CACHE_PAGE_SIZE as u64);
        let read_bytes = backing.paging_read(node, page_base, &mut page.data)?;
        if read_bytes < CACHE_PAGE_SIZE {
            page.data[read_bytes..].fill(0);
        }

        if let Some(map) = self.maps.get_mut(&node) {
            map.pages.insert(page_idx, page);
        }
        if is_prefetch {
            self.stats.read_ahead_pages += 1;
        } else {
            self.stats.page_misses += 1;
        }
        Ok(())
    }

    /// Buffered write (`CcCopyWrite`) of `data` at `offset` in `node`.
    ///
    /// If `write_through` is `true`, dirty pages touched by this write are flushed to
    /// `backing` before returning.
    pub fn copy_write(
        &mut self,
        node: NodeId,
        offset: u64,
        data: &[u8],
        write_through: bool,
        backing: &mut dyn CacheBacking,
    ) -> Result<usize, IoStatus> {
        if !self.maps.contains_key(&node) {
            return Err(IoStatus::InvalidParameter);
        }
        if data.is_empty() {
            return Ok(0);
        }

        let mut written = 0usize;
        while written < data.len() {
            let cur_off = offset + written as u64;
            let page_idx = cur_off / (CACHE_PAGE_SIZE as u64);
            let page_off = (cur_off % (CACHE_PAGE_SIZE as u64)) as usize;
            let chunk = (CACHE_PAGE_SIZE - page_off).min(data.len() - written);

            let resident = self.maps[&node].pages.contains_key(&page_idx);
            let file_size = self.maps[&node].file_size;
            let page_base = page_idx * (CACHE_PAGE_SIZE as u64);

            if !resident {
                if chunk < CACHE_PAGE_SIZE && page_base < file_size {
                    // Partial overwrite of an existing uncached page: fault it in first.
                    self.ensure_page_resident(node, page_idx, false, backing)?;
                } else {
                    self.maps
                        .get_mut(&node)
                        .unwrap()
                        .pages
                        .insert(page_idx, CachePage::zeroed());
                }
            } else {
                self.stats.page_hits += 1;
            }

            let map = self.maps.get_mut(&node).unwrap();
            let page = map.pages.get_mut(&page_idx).unwrap();
            page.data[page_off..page_off + chunk].copy_from_slice(&data[written..written + chunk]);
            if !page.dirty {
                page.dirty = true;
                self.stats.pages_dirtied += 1;
            }
            let new_end = cur_off + chunk as u64;
            if new_end > map.file_size {
                map.file_size = new_end;
            }

            if write_through {
                self.flush_one_page(node, page_idx, false, backing)?;
            }

            written += chunk;
        }
        Ok(written)
    }

    /// Run one pass of the background Lazy Writer (`CcLazyWriteTick`), flushing up to
    /// `max_pages` dirty pages across all cached files in deterministic `(NodeId, page_idx)`
    /// order.
    pub fn lazy_write_tick(
        &mut self,
        max_pages: usize,
        backing: &mut dyn CacheBacking,
    ) -> Result<FlushReport, IoStatus> {
        if max_pages == 0 {
            return Ok(FlushReport::default());
        }
        let mut dirty_targets = Vec::new();
        for (&node, map) in &self.maps {
            for (&page_idx, page) in &map.pages {
                if page.dirty {
                    dirty_targets.push((node, page_idx));
                    if dirty_targets.len() >= max_pages {
                        break;
                    }
                }
            }
            if dirty_targets.len() >= max_pages {
                break;
            }
        }

        let mut report = FlushReport::default();
        for (node, page_idx) in dirty_targets {
            let bytes = self.flush_one_page(node, page_idx, true, backing)?;
            report.pages_flushed += 1;
            report.bytes_flushed += bytes as u64;
        }
        Ok(report)
    }

    /// Flush all dirty pages belonging to `node` (`MmFlushSection` / `CcFlushCache`).
    pub fn mm_flush_section(
        &mut self,
        node: NodeId,
        backing: &mut dyn CacheBacking,
    ) -> Result<FlushReport, IoStatus> {
        let Some(map) = self.maps.get(&node) else {
            return Err(IoStatus::InvalidParameter);
        };
        let dirty_indices: Vec<u64> = map
            .pages
            .iter()
            .filter_map(|(&idx, page)| if page.dirty { Some(idx) } else { None })
            .collect();

        let mut report = FlushReport::default();
        for page_idx in dirty_indices {
            let bytes = self.flush_one_page(node, page_idx, false, backing)?;
            report.pages_flushed += 1;
            report.bytes_flushed += bytes as u64;
        }
        Ok(report)
    }

    /// Truncate or purge cached pages for `node` beyond `new_size` (`CcPurgeCacheSection`).
    pub fn purge_section(&mut self, node: NodeId, new_size: u64) -> Result<(), IoStatus> {
        let map = self.maps.get_mut(&node).ok_or(IoStatus::InvalidParameter)?;
        map.file_size = new_size;
        let first_drop_page = new_size.div_ceil(CACHE_PAGE_SIZE as u64);
        let tail_keys: Vec<u64> = map
            .pages
            .range(first_drop_page..)
            .map(|(&k, _)| k)
            .collect();
        for k in tail_keys {
            map.pages.remove(&k);
        }
        if !new_size.is_multiple_of(CACHE_PAGE_SIZE as u64) {
            let last_page = new_size / (CACHE_PAGE_SIZE as u64);
            let keep = (new_size % (CACHE_PAGE_SIZE as u64)) as usize;
            if let Some(page) = map.pages.get_mut(&last_page) {
                page.data[keep..].fill(0);
            }
        }
        Ok(())
    }

    fn flush_one_page(
        &mut self,
        node: NodeId,
        page_idx: u64,
        from_lazy_writer: bool,
        backing: &mut dyn CacheBacking,
    ) -> Result<usize, IoStatus> {
        let (file_size, page_bytes) = {
            let map = self.maps.get(&node).ok_or(IoStatus::InvalidParameter)?;
            let page = map.pages.get(&page_idx).ok_or(IoStatus::InvalidParameter)?;
            if !page.dirty {
                return Ok(0);
            }
            (map.file_size, page.data.clone())
        };
        let page_base = page_idx * (CACHE_PAGE_SIZE as u64);
        if page_base >= file_size {
            if let Some(page) = self
                .maps
                .get_mut(&node)
                .and_then(|m| m.pages.get_mut(&page_idx))
            {
                page.dirty = false;
            }
            return Ok(0);
        }
        let valid_len = ((file_size - page_base) as usize).min(CACHE_PAGE_SIZE);
        let written = backing.paging_write(node, page_base, &page_bytes[..valid_len])?;
        if let Some(page) = self
            .maps
            .get_mut(&node)
            .and_then(|m| m.pages.get_mut(&page_idx))
        {
            page.dirty = false;
        }
        if from_lazy_writer {
            self.stats.lazy_writer_flushes += 1;
        } else {
            self.stats.explicit_flushes += 1;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockBacking {
        files: BTreeMap<NodeId, Vec<u8>>,
        reads: u64,
        writes: u64,
    }

    impl CacheBacking for MockBacking {
        fn paging_read(
            &mut self,
            node: NodeId,
            offset: u64,
            buf: &mut [u8],
        ) -> Result<usize, IoStatus> {
            self.reads += 1;
            let Some(data) = self.files.get(&node) else {
                return Ok(0);
            };
            let off = offset as usize;
            if off >= data.len() {
                return Ok(0);
            }
            let n = (data.len() - off).min(buf.len());
            buf[..n].copy_from_slice(&data[off..off + n]);
            Ok(n)
        }

        fn paging_write(
            &mut self,
            node: NodeId,
            offset: u64,
            data: &[u8],
        ) -> Result<usize, IoStatus> {
            self.writes += 1;
            let file = self.files.entry(node).or_default();
            let end = offset as usize + data.len();
            if file.len() < end {
                file.resize(end, 0);
            }
            file[offset as usize..end].copy_from_slice(data);
            Ok(data.len())
        }
    }

    #[test]
    fn sequential_read_triggers_read_ahead_and_serves_subsequent_hits() {
        let mut backing = MockBacking::default();
        let node = NodeId::new(1, 10);
        let mut raw = vec![0u8; CACHE_PAGE_SIZE * 4];
        for (i, b) in raw.iter_mut().enumerate() {
            *b = (i / CACHE_PAGE_SIZE) as u8 + 1;
        }
        backing.files.insert(node, raw);

        let mut cm = CacheManager::new();
        cm.initialize_cache_map(node, (CACHE_PAGE_SIZE * 4) as u64);

        let mut page_buf = [0u8; 64];
        // Page 0: demand miss (no read-ahead yet because no prior page).
        assert_eq!(cm.copy_read(node, 0, &mut page_buf, &mut backing), Ok(64));
        assert!(page_buf.iter().all(|&b| b == 1));
        assert_eq!(cm.stats().page_misses, 1);
        assert_eq!(cm.stats().read_ahead_pages, 0);

        // Page 1: sequential after page 0 -> faults page 1 AND prefetches pages 2 and 3!
        assert_eq!(
            cm.copy_read(node, CACHE_PAGE_SIZE as u64, &mut page_buf, &mut backing),
            Ok(64)
        );
        assert!(page_buf.iter().all(|&b| b == 2));
        assert_eq!(cm.stats().page_misses, 2);
        assert_eq!(cm.stats().read_ahead_pages, 2);
        assert!(cm.map(node).unwrap().is_resident(2));
        assert!(cm.map(node).unwrap().is_resident(3));

        // Reading pages 2 and 3 now hits the cache without touching the backing store.
        let reads_before = backing.reads;
        assert_eq!(
            cm.copy_read(
                node,
                (CACHE_PAGE_SIZE * 2) as u64,
                &mut page_buf,
                &mut backing
            ),
            Ok(64)
        );
        assert!(page_buf.iter().all(|&b| b == 3));
        assert_eq!(backing.reads, reads_before);
        assert_eq!(cm.stats().page_hits, 1);
    }

    #[test]
    fn copy_write_dirties_pages_and_lazy_writer_plus_mm_flush_section_persist() {
        let mut backing = MockBacking::default();
        let node = NodeId::new(1, 20);
        let mut cm = CacheManager::new();
        cm.initialize_cache_map(node, 0);

        // Write across page 0 and page 1.
        let payload_a = [0xAAu8; 128];
        let payload_b = [0xBBu8; 256];
        cm.copy_write(node, 0, &payload_a, false, &mut backing)
            .unwrap();
        cm.copy_write(
            node,
            CACHE_PAGE_SIZE as u64,
            &payload_b,
            false,
            &mut backing,
        )
        .unwrap();

        assert_eq!(backing.writes, 0);
        assert_eq!(cm.map(node).unwrap().dirty_pages(), 2);

        // Lazy writer flushes 1 page first.
        let lazy = cm.lazy_write_tick(1, &mut backing).unwrap();
        assert_eq!(lazy.pages_flushed, 1);
        assert_eq!(cm.map(node).unwrap().dirty_pages(), 1);

        // MmFlushSection flushes the remaining dirty page.
        let flushed = cm.mm_flush_section(node, &mut backing).unwrap();
        assert_eq!(flushed.pages_flushed, 1);
        assert_eq!(flushed.bytes_flushed, 256);
        assert_eq!(cm.map(node).unwrap().dirty_pages(), 0);

        let disk = &backing.files[&node];
        assert_eq!(&disk[0..128], &payload_a);
        assert_eq!(&disk[CACHE_PAGE_SIZE..CACHE_PAGE_SIZE + 256], &payload_b);
    }

    #[test]
    fn purge_section_drops_truncated_pages_and_zeroes_partial_tail() {
        let mut backing = MockBacking::default();
        let node = NodeId::new(1, 30);
        let mut cm = CacheManager::new();
        cm.initialize_cache_map(node, 0);

        let block = vec![0x5Au8; CACHE_PAGE_SIZE * 2];
        cm.copy_write(node, 0, &block, false, &mut backing).unwrap();
        assert_eq!(cm.map(node).unwrap().resident_pages(), 2);

        cm.purge_section(node, 100).unwrap();
        assert_eq!(cm.map(node).unwrap().resident_pages(), 1);
        assert_eq!(cm.map(node).unwrap().file_size, 100);

        let mut out = [0u8; 200];
        let n = cm.copy_read(node, 0, &mut out, &mut backing).unwrap();
        assert_eq!(n, 100);
        assert!(out[..100].iter().all(|&b| b == 0x5A));
    }
}
