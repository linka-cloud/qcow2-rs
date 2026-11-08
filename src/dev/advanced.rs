//! Serialized COW operations for subclusters and shared snapshot mappings.
use super::graph::{invalid, put64, u64_at};
use super::*;
use crate::helpers::Qcow2IoBuf;
use crate::meta::{L1Entry, L2Entry, Mapping, MappingSource, TableEntry};

pub(crate) enum Mutation<'a> {
    Write(&'a [u8]),
    Zero,
    Discard,
}

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    pub(crate) fn validate_advanced_entry(
        &self,
        value: u64,
        bitmap: u64,
        guest: u64,
    ) -> Qcow2Result<L2Entry> {
        let entry = L2Entry::try_from_plain(value, &self.info)?;
        if self.info.extended_l2() {
            if entry.is_compressed() {
                if bitmap != 0 {
                    return Err(invalid("Compressed extended L2 bitmap is not zero"));
                }
            } else {
                if entry.is_zero() || bitmap as u32 & (bitmap >> 32) as u32 != 0 {
                    return Err(invalid("Invalid extended L2 allocation/zero bitmap"));
                }
                if bitmap as u32 != 0 && entry.cluster_offset() == 0 && !self.info.external_data() {
                    return Err(invalid("Allocated subcluster has no host cluster"));
                }
            }
        }
        if self.info.external_data() {
            if entry.is_compressed() {
                return Err(invalid(
                    "External data files cannot contain compressed clusters",
                ));
            }
            if (entry.cluster_offset() != 0
                || entry.is_copied()
                || self.info.extended_l2() && bitmap as u32 != 0)
                && (entry.cluster_offset() != self.info.cluster_round_down(guest)
                    || !entry.is_copied())
            {
                return Err(invalid(
                    "External data mapping must equal its guest offset and be copied",
                ));
            }
        }
        Ok(entry)
    }

    pub(crate) async fn advanced_entry(
        &self,
        guest: u64,
    ) -> Qcow2Result<(usize, u64, L2Entry, u64)> {
        let index = (guest / (self.info.cluster_size() * self.info.l2_entries()) as u64) as usize;
        let h = self.header.read().await;
        if index >= h.l1_table_entries() {
            return Ok((index, 0, L2Entry(0), 0));
        }
        let value = self
            .read_bytes(h.l1_table_offset() + index as u64 * 8, 8)
            .await?;
        drop(h);
        let off = L1Entry::try_from_plain(u64_at(&value, 0), &self.info)?.l2_offset();
        if off == 0 {
            return Ok((index, 0, L2Entry(0), 0));
        }
        let slot = (guest / self.info.cluster_size() as u64) as usize % self.info.l2_entries();
        let value = self
            .read_bytes(
                off + (slot * self.info.l2_entry_bytes()) as u64,
                self.info.l2_entry_bytes(),
            )
            .await?;
        let bitmap = if self.info.extended_l2() {
            u64_at(&value, 8)
        } else {
            0
        };
        let entry = self.validate_advanced_entry(u64_at(&value, 0), bitmap, guest)?;
        Ok((index, off, entry, bitmap))
    }

    pub(crate) async fn advanced_mapping(&self, guest: u64) -> Qcow2Result<Mapping> {
        let (_, _, entry, bitmap) = self.advanced_entry(guest).await?;
        if entry.is_compressed() {
            let (off, len) = entry
                .compressed_range(self.info.cluster_bits() as u32)
                .unwrap();
            return Ok(Mapping {
                source: MappingSource::Compressed,
                cluster_offset: Some(off),
                compressed_length: Some(len),
                copied: false,
            });
        }
        let mut allocated =
            entry.cluster_offset() != 0 || self.info.external_data() && entry.is_copied();
        let mut zero = entry.is_zero();
        if self.info.extended_l2() {
            let subcluster =
                (self.info.in_cluster_offset(guest) * 32 / self.info.cluster_size()) as u32;
            allocated = bitmap & (1_u64 << subcluster) != 0;
            zero = bitmap & (1_u64 << (subcluster + 32)) != 0;
        }
        let source = if zero {
            MappingSource::Zero
        } else if allocated {
            MappingSource::DataFile
        } else if self.info.has_back_file() {
            MappingSource::Backing
        } else {
            MappingSource::Unallocated
        };
        let offset = if allocated {
            entry.cluster_offset()
        } else {
            self.info.cluster_round_down(guest)
        };
        Ok(Mapping {
            source,
            cluster_offset: Some(offset),
            compressed_length: None,
            copied: entry.is_copied() && allocated,
        })
    }

    pub(crate) async fn advanced_read_at(&self, buf: &mut [u8], offset: u64) -> Qcow2Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let bs = 1_usize << self.info.block_size_shift;
        offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| invalid("Read range overflow"))?;
        if offset % bs as u64 != 0 || buf.len() % bs != 0 {
            return Err(invalid("Unaligned read"));
        }
        let valid = self
            .info
            .virtual_size()
            .saturating_sub(offset)
            .min(buf.len() as u64) as usize;
        if valid == 0 && !self.info.is_back_file() {
            return Err(invalid("Read beyond virtual EOF"));
        }
        buf.fill(0);
        let unit = if self.info.extended_l2() {
            self.info.cluster_size() / 32
        } else {
            self.info.cluster_size()
        };
        let mut done = 0;
        while done < valid {
            let off = offset + done as u64;
            let n = (unit - off as usize % unit)
                .min(buf.len() - done)
                .min(valid - done);
            // Keep host I/O sector aligned at a partial virtual EOF.
            let io_len = n.div_ceil(bs) * bs;
            let mut tmp = Qcow2IoBuf::<u8>::new(io_len);
            tmp.fill(0);
            let mapping = self.advanced_mapping(off).await?;
            let count = match mapping.source {
                MappingSource::DataFile => {
                    let host =
                        mapping.cluster_offset.unwrap() + self.info.in_cluster_offset(off) as u64;
                    if self.info.external_data() {
                        self.data_file
                            .as_ref()
                            .ok_or_else(|| invalid("Missing external data file"))?
                            .read_to(host, &mut tmp)
                            .await?
                    } else {
                        self.call_read(host, &mut tmp).await?
                    }
                }
                MappingSource::Compressed => {
                    self.do_read_compressed(mapping, self.info.in_cluster_offset(off), &mut tmp)
                        .await?
                }
                MappingSource::Backing => self.read_backing_data(&mut tmp, off).await?,
                _ => io_len,
            };
            if count < n {
                return Err(invalid("Short guest payload read"));
            }
            buf[done..done + n].copy_from_slice(&tmp[..n]);
            done += n;
        }
        Ok(if self.info.is_back_file() {
            buf.len()
        } else {
            valid
        })
    }

    pub(crate) async fn allocate_object(&self, data: &[u8]) -> Qcow2Result<u64> {
        if data.is_empty() {
            return Ok(0);
        }
        let cs = self.info.cluster_size();
        let (offset, _) = self
            .allocate_clusters_exact(data.len().div_ceil(cs))
            .await?;
        let mut bytes = vec![0; data.len().div_ceil(cs) * cs];
        bytes[..data.len()].copy_from_slice(data);
        self.flush_refcount().await?;
        self.write_bytes(offset, &bytes).await?;
        self.call_fsync(0, 0, 0).await?;
        Ok(offset)
    }

    pub(crate) async fn begin_transaction(&self) -> Qcow2Result<()> {
        self.ensure_usable()?;
        if self.info.is_read_only() {
            return Err(invalid("Read-only image"));
        }
        self.flush_meta_locked().await?;
        if self.graph().await?.counts != self.actual_refcounts().await? {
            return Err(invalid("Repair refcounts before modifying shared metadata"));
        }
        self.prepare_writable().await?;
        self.mark_image_dirty().await
    }

    async fn copy_active_l1(&self) -> Qcow2Result<()> {
        let h = self.header.read().await;
        let (offset, entries) = (h.l1_table_offset(), h.l1_table_entries());
        drop(h);
        let bytes = self.read_bytes(offset, entries * 8).await?;
        let new = self.allocate_object(&bytes).await?;
        let mut h = self.header.write().await;
        h.set_l1_table(new, entries)?;
        self.commit_header(&mut h, |h| {
            h.set_l1_table(offset, entries).unwrap();
        })
        .await
    }

    pub(crate) async fn advanced_modify(
        &self,
        offset: u64,
        len: usize,
        action: Mutation<'_>,
    ) -> Qcow2Result<()> {
        if len == 0 {
            return Ok(());
        }
        let bs = 1_usize << self.info.block_size_shift;
        let end = offset
            .checked_add(len as u64)
            .filter(|n| *n <= self.info.virtual_size())
            .ok_or_else(|| invalid("Write beyond virtual EOF"))?;
        if offset % bs as u64 != 0 || len % bs != 0 {
            return Err(invalid("Unaligned write"));
        }
        self.begin_transaction().await?;
        let result = async {
            self.copy_active_l1().await?;
            let cs = self.info.cluster_size();
            let mut guest = self.info.cluster_round_down(offset);
            while guest < end {
                let from = offset.saturating_sub(guest) as usize;
                let to = (end - guest).min(cs as u64) as usize;
                let (l1_index, old_l2, entry, mut bitmap) = self.advanced_entry(guest).await?;
                let mut l2 = if old_l2 == 0 {
                    vec![0; cs]
                } else {
                    self.read_bytes(old_l2, cs).await?
                };
                let slot = ((guest / cs as u64) as usize % self.info.l2_entries())
                    * self.info.l2_entry_bytes();
                let mut descriptor = entry.0;
                let discard = matches!(action, Mutation::Discard);
                if discard {
                    if entry.is_compressed() {
                        guest += cs as u64;
                        continue;
                    }
                    if self.info.extended_l2() {
                        let unit = cs / 32;
                        for i in from.div_ceil(unit)..to / unit {
                            bitmap &= !(1_u64 << i);
                            bitmap &= !(1_u64 << (i + 32));
                        }
                        if bitmap as u32 == 0 {
                            descriptor = 0;
                        }
                    } else if from == 0 && to == cs {
                        descriptor = 0;
                    } else {
                        guest += cs as u64;
                        continue;
                    }
                    if self.info.raw_data() {
                        let mut zeros = Qcow2IoBuf::<u8>::new(to - from);
                        zeros.fill(0);
                        self.data_file
                            .as_ref()
                            .unwrap()
                            .write_from(guest + from as u64, &zeros)
                            .await?;
                    }
                } else {
                    let mut data = Qcow2IoBuf::<u8>::new(cs);
                    data.fill(0);
                    self.advanced_read_at(&mut data, guest).await?;
                    match action {
                        Mutation::Write(buf) => {
                            let at = (guest + from as u64 - offset) as usize;
                            data[from..to].copy_from_slice(&buf[at..at + to - from]);
                        }
                        Mutation::Zero => data[from..to].fill(0),
                        Mutation::Discard => unreachable!(),
                    }
                    let host = if self.info.external_data() {
                        self.data_file
                            .as_ref()
                            .ok_or_else(|| invalid("Missing external data file"))?
                            .write_from(
                                guest,
                                &data[..cs.min((self.info.virtual_size() - guest) as usize)],
                            )
                            .await?;
                        guest
                    } else {
                        self.allocate_object(&data).await?
                    };
                    descriptor = (1 << 63) | host;
                    if self.info.extended_l2() {
                        if entry.is_compressed() {
                            bitmap = u32::MAX as u64;
                        }
                        let unit = cs / 32;
                        for i in from / unit..to.div_ceil(unit) {
                            bitmap |= 1_u64 << i;
                            bitmap &= !(1_u64 << (i + 32));
                        }
                        if matches!(action, Mutation::Zero) {
                            for i in from.div_ceil(unit)..to / unit {
                                bitmap &= !(1_u64 << i);
                                bitmap |= 1_u64 << (i + 32);
                            }
                        }
                    }
                }
                put64(&mut l2, slot, descriptor);
                if self.info.extended_l2() {
                    put64(&mut l2, slot + 8, bitmap);
                }
                let new_l2 = self.allocate_object(&l2).await?;
                if let Some(file) = &self.data_file {
                    file.fsync(0, 0, 0).await?;
                }
                let h = self.header.read().await;
                self.write_bytes(
                    h.l1_table_offset() + l1_index as u64 * 8,
                    &((1_u64 << 63) | new_l2).to_be_bytes(),
                )
                .await?;
                drop(h);
                self.call_fsync(0, 0, 0).await?;
                guest += cs as u64;
            }
            self.rebuild_refcounts().await
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }

    /// Write zeros precisely, preserving snapshot data and backing semantics.
    pub async fn write_zeroes(&self, offset: u64, len: usize) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        self.advanced_modify(offset, len, Mutation::Zero).await
    }
}
