//! Offline image-management transactions.
use super::graph::{put64, u64_at, MASK};
use super::*;
use crate::helpers::Qcow2IoBuf;

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    /// Materialize the active image without changing its logical bytes.
    /// Metadata mode uses allocated zero clusters when possible; Fallocate
    /// reserves storage, falling back to full initialization on other backends.
    pub async fn preallocate(&self, mode: Preallocation) -> Qcow2Result<()> {
        if mode == Preallocation::Off {
            return Ok(());
        }
        let _io = self.io_lock.write().await;
        self.preallocate_locked(mode).await
    }
    pub(crate) async fn preallocate_locked(&self, mode: Preallocation) -> Qcow2Result<()> {
        self.begin_transaction().await?;
        let result = async {
            let h = self.header.read().await;
            let mut l1 = self
                .read_bytes(h.l1_table_offset(), h.l1_table_entries() * 8)
                .await?;
            drop(h);
            let cs = self.info.cluster_size();
            let coverage = (cs * self.info.l2_entries()) as u64;
            for index in 0..self.info.virtual_size().div_ceil(coverage) as usize {
                let old = u64_at(&l1, index * 8) & MASK;
                let mut l2 = if old == 0 {
                    vec![0; cs]
                } else {
                    self.read_bytes(old, cs).await?
                };
                for slot in 0..self.info.l2_entries() {
                    let guest = (index * self.info.l2_entries() + slot) as u64 * cs as u64;
                    if guest >= self.info.virtual_size() {
                        break;
                    }
                    let mut data = Qcow2IoBuf::<u8>::new(cs);
                    data.fill(0);
                    self.advanced_read_at(&mut data, guest).await?;
                    let sparse_zero = data.iter().all(|b| *b == 0)
                        && !self.info.encrypted()
                        && !self.info.is_v2()
                        && mode != Preallocation::Full;
                    let host = if self.info.external_data() {
                        if !sparse_zero {
                            if self.info.encrypted() {
                                self.crypto
                                    .read()
                                    .await
                                    .as_ref()
                                    .ok_or("Encryption not initialized")?
                                    .apply(guest, guest, &mut data, true)?;
                            }
                            self.data_file
                                .as_ref()
                                .ok_or("Missing data file")?
                                .write_from(guest, &data)
                                .await?;
                        }
                        guest
                    } else if sparse_zero {
                        let (host, _) = self.allocate_clusters_exact(1).await?;
                        self.flush_refcount().await?;
                        let zeros = vec![0; 1_usize << self.info.block_size_shift];
                        self.write_bytes(host + cs as u64 - zeros.len() as u64, &zeros)
                            .await?;
                        host
                    } else {
                        self.allocate_payload(&data, guest).await?
                    };
                    if mode == Preallocation::Fallocate {
                        let file = if self.info.external_data() {
                            self.data_file.as_ref().unwrap()
                        } else {
                            &self.file
                        };
                        if let Err(error) = file.reserve(host, cs).await {
                            if error.get_inner().kind() != std::io::ErrorKind::Unsupported {
                                return Err(error);
                            }
                            // Zero mappings hide stale free-cluster contents.
                            if sparse_zero {
                                let mut zeros = Qcow2IoBuf::<u8>::new(cs);
                                zeros.fill(0);
                                file.write_from(host, &zeros).await?;
                            }
                        }
                    }
                    put64(
                        &mut l2,
                        slot * self.info.l2_entry_bytes(),
                        (1 << 63) | host | u64::from(sparse_zero && !self.info.extended_l2()),
                    );
                    if self.info.extended_l2() {
                        put64(
                            &mut l2,
                            slot * 16 + 8,
                            if sparse_zero {
                                (u32::MAX as u64) << 32
                            } else {
                                u32::MAX as u64
                            },
                        );
                    }
                }
                let host = self.allocate_object(&l2).await?;
                put64(&mut l1, index * 8, (1 << 63) | host);
            }
            if let Some(data) = &self.data_file {
                data.fsync(0, 0, 0).await?;
            }
            let root = self.allocate_object(&l1).await?;
            self.header.write().await.set_l1_table(root, l1.len() / 8)?;
            self.rebuild_refcounts().await
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }
}

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    pub(crate) async fn stage_payload(&self, data: &mut [u8], guest: u64) -> Qcow2Result<u64> {
        if !self.info.external_data() {
            return self.allocate_payload(data, guest).await;
        }
        if self.info.encrypted() {
            self.crypto
                .read()
                .await
                .as_ref()
                .ok_or("Encryption not initialized")?
                .apply(guest, guest, data, true)?;
        }
        self.data_file
            .as_ref()
            .ok_or("Missing external data file")?
            .write_from(guest, data)
            .await?;
        Ok(guest)
    }

    /// Grow or explicitly shrink the virtual disk. New guest bytes are zero,
    /// including when a larger backing file or an old snapshot contains data.
    /// Inactive snapshots retain their original contents and disk sizes.
    pub async fn resize(&self, new_size: u64, allow_shrink: bool) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        self.resize_locked(new_size, allow_shrink).await
    }
    pub(crate) async fn resize_locked(&self, new_size: u64, allow_shrink: bool) -> Qcow2Result<()> {
        let old_size = self.info.virtual_size();
        if new_size == old_size {
            return Ok(());
        }
        if new_size % 512 != 0 || new_size < old_size && !allow_shrink {
            return Err(super::graph::invalid(
                "Resize requires sector alignment and explicit shrink permission",
            ));
        }
        let cs = self.info.cluster_size();
        let coverage = (cs * self.info.l2_entries()) as u64;
        let entries = new_size.div_ceil(coverage);
        if entries > u64::from(Qcow2Header::MAX_L1_SIZE) / 8
            || self.info.external_data() && new_size > 1_u64 << 56
        {
            return Err(super::graph::invalid(
                "Resized disk exceeds supported mapping capacity",
            ));
        }
        if self
            .snapshot_directory()
            .await?
            .0
            .iter()
            .any(|s| s.extra_data.len() < 16)
        {
            return Err(super::graph::invalid(
                "Resize requires snapshot disk sizes; amend to v3 first",
            ));
        }
        let mut maps = self.load_bitmaps().await?;
        self.begin_transaction().await?;
        let result = async {
            let h = self.header.read().await;
            let old = self
                .read_bytes(h.l1_table_offset(), h.l1_table_entries() * 8)
                .await?;
            drop(h);
            let mut l1 = vec![0; entries as usize * 8];
            let (mut zero_base, mut zero_left) = (0_u64, 0_usize);
            for index in 0..entries as usize {
                let old_l2 = if index * 8 < old.len() {
                    u64_at(&old, index * 8) & MASK
                } else {
                    0
                };
                let mut l2 = if old_l2 == 0 {
                    vec![0; cs]
                } else {
                    self.read_bytes(old_l2, cs).await?
                };
                for slot in 0..self.info.l2_entries() {
                    let guest = (index * self.info.l2_entries() + slot) as u64 * cs as u64;
                    let entry = slot * self.info.l2_entry_bytes();
                    if guest >= new_size {
                        l2[entry..entry + self.info.l2_entry_bytes()].fill(0);
                        continue;
                    }
                    if guest + cs as u64 <= old_size || new_size < old_size && guest < new_size {
                        continue;
                    }
                    let (descriptor, bitmap) = if guest < old_size {
                        let mut data = Qcow2IoBuf::<u8>::new(cs);
                        data.fill(0);
                        self.advanced_read_at(&mut data, guest).await?;
                        let host = self.stage_payload(&mut data, guest).await?;
                        ((1 << 63) | host, u32::MAX as u64)
                    } else {
                        if self.info.raw_data() {
                            let mut zeros = Qcow2IoBuf::<u8>::new(cs);
                            zeros.fill(0);
                            self.data_file
                                .as_ref()
                                .unwrap()
                                .write_from(guest, &zeros)
                                .await?;
                        }
                        if !self.info.has_back_file() {
                            (0, 0)
                        } else if self.info.extended_l2() {
                            (0, (u32::MAX as u64) << 32)
                        } else if !self.info.is_v2() {
                            (1, 0)
                        } else {
                            if zero_left == 0 {
                                zero_left = ((new_size - guest).div_ceil(cs as u64) as usize)
                                    .min((32 << 20) / cs);
                                let mut zeros = Qcow2IoBuf::<u8>::new(zero_left * cs);
                                zeros.fill(0);
                                zero_base = self.allocate_payload(&zeros, guest).await?;
                            }
                            let host = zero_base;
                            zero_base += cs as u64;
                            zero_left -= 1;
                            ((1 << 63) | host, 0)
                        }
                    };
                    put64(&mut l2, entry, descriptor);
                    if self.info.extended_l2() {
                        put64(&mut l2, entry + 8, bitmap);
                    }
                }
                if l2.iter().any(|b| *b != 0) {
                    let host = self.allocate_object(&l2).await?;
                    put64(&mut l1, index * 8, (1 << 63) | host);
                }
            }
            if let Some(data) = &self.data_file {
                data.fsync(0, 0, 0).await?;
            }
            let root = self.allocate_object(&l1).await?;
            let mut h = self.header.write().await;
            h.set_l1_table(root, entries as usize)?;
            h.set_size(new_size);
            drop(h);
            self.stage_bitmap_size_change(&mut maps, new_size).await?;
            self.rebuild_refcounts_state(true).await?;
            if self.info.external_data() {
                let data = self.data_file.as_ref().unwrap();
                if let Err(error) = data.set_len(new_size).await {
                    if error.get_inner().kind() != std::io::ErrorKind::Unsupported {
                        return Err(error);
                    }
                }
                data.fsync(0, 0, 0).await?;
            }
            let flags = self.header.read().await.incompatible_features();
            self.store_feature_flags(flags & !3).await?;
            self.needs_repair.store(false, Ordering::Release);
            Ok(())
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }
}
