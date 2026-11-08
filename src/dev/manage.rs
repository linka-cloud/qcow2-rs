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
