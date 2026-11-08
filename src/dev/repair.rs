use super::graph::{invalid, put64, u64_at};
use super::*;
use crate::helpers::IntAlignment;
use crate::meta::RefBlock;
use std::collections::{BTreeMap, BTreeSet};

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    pub(crate) async fn store_feature_flags(&self, flags: u64) -> Qcow2Result<()> {
        let mut header = self.header.write().await;
        if header.version() == 2 {
            return Ok(());
        }
        let old = header.incompatible_features();
        header.set_incompatible_features(flags);
        self.commit_header(&mut header, |h| h.set_incompatible_features(old))
            .await
    }

    pub(crate) async fn mark_image_dirty(&self) -> Qcow2Result<()> {
        let flags = self.header.read().await.incompatible_features();
        self.store_feature_flags(flags | 1).await
    }

    pub(crate) async fn reload_metadata(&self) -> Qcow2Result<()> {
        self.l2cache.invalidate();
        self.refblock_cache.invalidate();
        self.new_cluster.write().await.clear();
        let h = self.header.read().await;
        self.info.set_virtual_size(h.size());
        self.info.set_encrypted(h.crypt_method() != 0);
        let l1_size = (h.l1_table_entries() * 8)
            .align_up(self.info.cluster_size())
            .unwrap()
            .max(1 << self.info.block_size_shift);
        *self.l1table.write().await = L1Table::new(
            None,
            l1_size,
            h.l1_table_entries() as u32,
            self.info.block_size_shift,
        );
        *self.reftable.write().await = RefTable::new(
            None,
            h.reftable_clusters() * self.info.cluster_size(),
            self.info.block_size_shift,
        );
        drop(h);
        self.mark_need_flush(false);
        self.free_cluster_offset.store(0, Ordering::Relaxed);
        self.__qcow2_prep_io().await
    }

    async fn update_copied_flags(&self, counts: &BTreeMap<u64, u64>) -> Qcow2Result<()> {
        let h = self.header.read().await;
        let (offset, entries) = (h.l1_table_offset(), h.l1_table_entries());
        drop(h);
        let cs = self.info.cluster_size() as u64;
        let mut l1 = self.read_bytes(offset, entries * 8).await?;
        for i in 0..entries {
            let value = u64_at(&l1, i * 8);
            let off = value & super::graph::MASK;
            if off == 0 {
                put64(&mut l1, i * 8, 0);
                continue;
            }
            put64(
                &mut l1,
                i * 8,
                off | if counts.get(&(off / cs)) == Some(&1) {
                    1 << 63
                } else {
                    0
                },
            );
            let mut l2 = self.read_bytes(off, cs as usize).await?;
            for entry in l2.chunks_exact_mut(self.info.l2_entry_bytes()) {
                let value = u64_at(entry, 0);
                let data = value & super::graph::MASK;
                let copied = if self.info.external_data() {
                    data != 0 || value & (1 << 63) != 0
                } else {
                    data != 0 && value & (1 << 62) == 0 && counts.get(&(data / cs)) == Some(&1)
                };
                put64(
                    entry,
                    0,
                    (value & !(1 << 63)) | if copied { 1 << 63 } else { 0 },
                );
            }
            self.write_bytes(off, &l2).await?;
        }
        self.write_bytes(offset, &l1).await
    }

    /// Rebuild using fresh metadata, never allocating from suspect refcounts.
    pub(crate) async fn rebuild_refcounts(&self) -> Qcow2Result<()> {
        self.rebuild_refcounts_state(false).await
    }

    pub(crate) async fn rebuild_refcounts_state(&self, keep_dirty: bool) -> Qcow2Result<()> {
        let graph = self.graph().await?;
        let start = graph.counts.keys().next_back().copied().unwrap_or(0) + 1;
        let mut base = graph.counts;
        for c in graph.ref_metadata {
            base.remove(&c);
        }
        let cs = self.info.cluster_size();
        let mut table_clusters = 1;
        let mut block_count = 1;
        let (counts, groups) = loop {
            let mut counts = base.clone();
            for c in start..start + (table_clusters + block_count) as u64 {
                counts.insert(c, 1);
            }
            let groups: BTreeSet<u64> = counts
                .keys()
                .map(|c| *c / self.info.rb_entries() as u64)
                .collect();
            let entries = groups.last().copied().unwrap_or(0) + 1;
            let bytes = entries
                .checked_mul(8)
                .ok_or_else(|| invalid("Refcount table overflow"))?;
            if bytes > u64::from(Qcow2Header::MAX_REFCOUNT_TABLE_SIZE) {
                return Err(invalid("Repaired refcount table exceeds supported size"));
            }
            let needed = (bytes as usize).div_ceil(cs);
            if needed == table_clusters && groups.len() == block_count {
                break (counts, groups);
            }
            table_clusters = needed;
            block_count = groups.len();
        };
        let mut table = vec![0; table_clusters * cs];
        for (i, group) in groups.into_iter().enumerate() {
            let offset = (start + table_clusters as u64 + i as u64)
                .checked_mul(cs as u64)
                .filter(|n| *n < 1_u64 << 56)
                .ok_or_else(|| invalid("Repair allocation exceeds host offset limit"))?;
            put64(&mut table, group as usize * 8, offset);
            let mut block = RefBlock::new(self.info.refcount_order(), cs, Some(offset));
            for (&cluster, &value) in counts.range(
                group * self.info.rb_entries() as u64..(group + 1) * self.info.rb_entries() as u64,
            ) {
                block.set_count((cluster % self.info.rb_entries() as u64) as usize, value)?;
            }
            self.flush_table(&block, 0, cs).await?;
        }
        let offset = start * cs as u64;
        self.write_bytes(offset, &table).await?;
        let mut h = self.header.write().await;
        let (old, old_clusters) = (h.reftable_offset(), h.reftable_clusters());
        h.set_reftable(offset, table_clusters)?;
        self.commit_header(&mut h, |h| {
            h.set_reftable(old, old_clusters).unwrap();
        })
        .await?;
        drop(h);
        self.update_copied_flags(&counts).await?;
        self.call_fsync(0, 0, 0).await?;
        self.reload_metadata().await?;
        if self.info.raw_data() && self.needs_repair.load(Ordering::Acquire) {
            self.reconcile_external_raw().await?;
        }
        if !keep_dirty {
            let flags = self.header.read().await.incompatible_features();
            self.store_feature_flags(flags & !3).await?;
            self.needs_repair.store(false, Ordering::Release);
        }
        Ok(())
    }

    async fn reconcile_external_raw(&self) -> Qcow2Result<()> {
        let data = self.data_file.as_ref().ok_or("Missing raw external data")?;
        let mut buf = crate::helpers::Qcow2IoBuf::<u8>::new(self.info.cluster_size());
        for offset in (0..self.info.virtual_size()).step_by(buf.len()) {
            self.advanced_read_at(&mut buf, offset).await?;
            data.write_from(offset, &buf).await?;
        }
        if let Err(error) = data.set_len(self.info.virtual_size()).await {
            if error.get_inner().kind() != std::io::ErrorKind::Unsupported {
                return Err(error);
            }
        }
        data.fsync(0, 0, 0).await
    }

    /// Explicitly reconstruct refcounts. Structural overlaps are never repaired.
    pub async fn repair_refcounts(&self) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        if self.info.is_read_only() {
            return Err(invalid("Repair requires a writable image"));
        }
        if !self.needs_repair.load(Ordering::Acquire) {
            self.flush_meta_locked().await?;
        } else {
            // A failed transaction can have unpublished header fields in RAM.
            // Recovery always follows the last header actually stored on disk.
            let bytes = self.read_bytes(0, self.info.cluster_size()).await?;
            *self.header.write().await = Qcow2Header::from_buf(&bytes)?;
            self.reload_metadata().await?;
        }
        self.graph().await?; // Validate structure before making any changes.
        self.mark_image_dirty().await?;
        self.needs_repair.store(true, Ordering::Release);
        self.rebuild_refcounts().await
    }
}
