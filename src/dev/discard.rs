use super::*;
use crate::error::Qcow2Result;
use crate::meta::{L2Entry, SplitGuestOffset, Table};

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    /// Discard the guest range `[virtual_offset, virtual_offset + len)`.
    ///
    /// For every fully-covered guest cluster in the range, the L2 mapping
    /// is cleared to "unallocated", the host cluster's refcount is
    /// decremented, and the host file extent is punched only when no
    /// references remain. An unallocated cluster reads from the backing
    /// image when present, or as zero otherwise. Discard is advisory and
    /// does not guarantee zero reads when a backing image is present.
    ///
    /// Partial-cluster head/tail ranges (guest_offset not aligned to
    /// cluster_size, or guest_offset+len not aligned) are silently
    /// skipped — discard is advisory and a partial-cluster mutation
    /// would require allocating/keeping the cluster anyway. Callers
    /// wanting per-byte-precise behavior should issue a zero-write for
    /// the head/tail in addition to this call.
    ///
    /// Shared and compressed clusters are silently skipped. Releasing shared
    /// clusters would also require updating surviving mappings' copied flags.
    /// Compressed clusters can share host sectors with adjacent clusters, so
    /// punching the host extent risks corrupting a neighbor. The
    /// cluster stays referenced and subsequent reads still decompress
    /// correctly.
    ///
    /// Already-unallocated clusters and zero entries without an allocation
    /// are no-ops. Allocated zero entries can be released like ordinary data.
    ///
    /// Mapping removal is persisted before releasing references or punching
    /// data. Reference-count updates may remain cached: call `flush_meta()`
    /// to persist them. Interrupted operations can leave allocated leaks,
    /// but must not reclaim a cluster still referenced by a durable mapping.
    pub async fn discard(&self, virtual_offset: u64, len: u64) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        if self.info.encrypted()
            || self.info.extended_l2()
            || self.info.external_data()
            || self.header.read().await.nb_snapshots() != 0
        {
            let end = virtual_offset
                .saturating_add(len)
                .min(self.info.virtual_size());
            let unit = if self.info.extended_l2() {
                self.info.cluster_size() / 32
            } else {
                self.info.cluster_size()
            } as u64;
            let start = virtual_offset.div_ceil(unit).saturating_mul(unit);
            let end = end / unit * unit;
            if end <= start {
                return Ok(());
            }
            return self
                .advanced_modify(
                    start,
                    (end - start) as usize,
                    super::advanced::Mutation::Discard,
                )
                .await;
        }
        let info = &self.info;
        let cluster_size = info.cluster_size() as u64;

        if len == 0 {
            return Ok(());
        }

        // Clip to the device's virtual size — out-of-range bytes are a
        // no-op rather than an error.
        let virt_size = info.virtual_size();
        let end_unclipped = virtual_offset.saturating_add(len);
        let end = end_unclipped.min(virt_size);
        if virtual_offset >= end {
            return Ok(());
        }

        // Round inward to whole-cluster boundaries.
        let start = info.cluster_round_up(virtual_offset);
        let stop = info.cluster_round_down(end);
        if start >= stop {
            return Ok(());
        }
        self.prepare_writable().await?;

        log::trace!(
            "discard guest [{:x}, {:x}) -> whole-cluster [{:x}, {:x})",
            virtual_offset,
            end,
            start,
            stop
        );

        let mut guest = start;
        while guest < stop {
            self.__discard_one_cluster(guest).await?;
            guest += cluster_size;
        }

        Ok(())
    }

    /// Discard a single guest cluster at `guest_offset` (cluster-aligned).
    ///
    /// Already-unallocated, shared, compressed, and L2-slice-absent ranges
    /// are no-ops. Invalid refcounts and metadata/data I/O failures are
    /// returned before the affected allocation can be reclaimed.
    async fn __discard_one_cluster(&self, guest_offset: u64) -> Qcow2Result<()> {
        let info = &self.info;
        debug_assert_eq!(info.in_cluster_offset(guest_offset), 0);
        let split = SplitGuestOffset(guest_offset);

        // Fast path: no L2 slice exists for this region; nothing to free.
        let l1_e = self.get_l1_entry(&split).await?;
        if l1_e.is_zero() {
            return Ok(());
        }

        let l2_handle = self.get_l2_slice(&split).await?;
        let mut l2_table = l2_handle.value().write().await;

        let entry = l2_table.get_entry(info, &split);

        // Compressed clusters share host sectors; punching could corrupt
        // a neighbor. Leave them mapped.
        if entry.is_compressed() {
            return Ok(());
        }

        let allocation = entry.allocation(info.cluster_bits() as u32);
        let Some((host_cluster, host_count)) = allocation else {
            // Unallocated or zero-flagged-only entry — nothing to release.
            return Ok(());
        };

        match self.cluster_refcount(host_cluster).await? {
            0 => return Err("Discard mapping points to an unallocated host cluster".into()),
            1 => {}
            _ => return Ok(()), // Advisory discard cannot disturb shared mappings.
        }

        // Clear the mapping first, while its reference count is still intact.
        let idx = split.l2_slice_index(info);
        l2_table.set(idx, L2Entry(0));
        l2_handle.set_dirty(true);
        self.mark_need_flush(true);
        drop(l2_table);

        // A crash after freeing/punching must not leave an old mapping on
        // disk pointing at reclaimed data. io_lock excludes ordinary I/O
        // until the durable mapping removal and reclamation are complete.
        self.flush_meta_locked().await?;
        self.call_fsync(0, 0, 0).await?;

        // Refcount-release the host cluster(s). For ordinary (non-
        // compressed) entries this is always a single cluster, but we
        // pass `host_count` through to mirror the existing free_clusters
        // call sites in the COW path.
        self.free_clusters(host_cluster, host_count).await?;

        // Standard entries cover one host cluster. A shared cluster must
        // remain untouched for its surviving references. io_lock prevents
        // allocation from reusing a freed cluster before punching finishes.
        if self.cluster_refcount(host_cluster).await? == 0 {
            let punch_len = host_count * info.cluster_size();
            self.call_fallocate(host_cluster, punch_len, Qcow2OpsFlags::FALLOCATE_ZERO_RANGE)
                .await?;
        }

        Ok(())
    }
}
