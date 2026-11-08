use super::graph::{invalid, put64, u64_at, MASK};
use super::*;

#[derive(Debug, Default, Clone)]
pub struct SnapshotOptions {
    pub id: String,
    pub name: String,
    pub vm_clock_nanoseconds: u64,
    pub icount: Option<i64>,
}

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    pub async fn list_snapshots(&self) -> Qcow2Result<Vec<Snapshot>> {
        let _io = self.io_lock.read().await;
        Ok(self.snapshot_directory().await?.0)
    }

    async fn replace_snapshot_directory(&self, snapshots: &[Snapshot]) -> Qcow2Result<()> {
        let mut bytes = Vec::new();
        for s in snapshots {
            if s.id.len() > u16::MAX as usize || s.name.len() > u16::MAX as usize {
                return Err(invalid("Snapshot ID/name too long"));
            }
            let mut entry = vec![0; 40];
            put64(&mut entry, 0, s.l1_offset);
            entry[8..12].copy_from_slice(&s.l1_entries.to_be_bytes());
            entry[12..14].copy_from_slice(&(s.id.len() as u16).to_be_bytes());
            entry[14..16].copy_from_slice(&(s.name.len() as u16).to_be_bytes());
            entry[16..20].copy_from_slice(&s.date_seconds.to_be_bytes());
            entry[20..24].copy_from_slice(&s.date_nanoseconds.to_be_bytes());
            put64(&mut entry, 24, s.vm_clock_nanoseconds);
            entry[32..36]
                .copy_from_slice(&(s.vm_state_size.min(u32::MAX as u64) as u32).to_be_bytes());
            entry[36..40].copy_from_slice(&(s.extra_data.len() as u32).to_be_bytes());
            bytes.extend(entry);
            bytes.extend(&s.extra_data);
            bytes.extend(s.id.as_bytes());
            bytes.extend(s.name.as_bytes());
            bytes.resize(bytes.len().div_ceil(8) * 8, 0);
        }
        let offset = self.allocate_object(&bytes).await?;
        self.header
            .write()
            .await
            .set_snapshot_table(offset, snapshots.len() as u32);
        self.rebuild_refcounts().await
    }

    pub async fn create_snapshot(
        &self,
        options: SnapshotOptions,
        vm_state: &[u8],
    ) -> Qcow2Result<Snapshot> {
        let _io = self.io_lock.write().await;
        if self.info.external_data() {
            return Err(invalid(
                "Snapshots are incompatible with external data files",
            ));
        }
        if self.info.refcount_order() == 0 {
            return Err(invalid("Snapshots require refcounts wider than one bit"));
        }
        if options.id.is_empty()
            || options.id.len() > u16::MAX as usize
            || options.name.len() > u16::MAX as usize
        {
            return Err(invalid("Invalid snapshot ID/name"));
        }
        let mut snapshots = self.snapshot_directory().await?.0;
        if snapshots.len() >= 65535 || snapshots.iter().any(|s| s.id == options.id) {
            return Err(invalid("Duplicate or excessive snapshots"));
        }
        self.begin_transaction().await?;
        let result = async {
            let cs = self.info.cluster_size();
            let coverage = (cs * self.info.l2_entries()) as u64;
            let h = self.header.read().await;
            let size = h.size();
            let mut l1 = self
                .read_bytes(h.l1_table_offset(), h.l1_table_entries() * 8)
                .await?;
            drop(h);
            let first = size.div_ceil(coverage) as usize;
            if !vm_state.is_empty() {
                let entries = first + vm_state.len().div_ceil(coverage as usize);
                if entries * 8 > Qcow2Header::MAX_L1_SIZE as usize {
                    return Err(invalid("VM state exceeds snapshot L1 capacity"));
                }
                l1.resize(l1.len().max(entries * 8), 0);
                let mut done = 0;
                for index in first..entries {
                    let mut l2 = vec![0; cs];
                    for slot in 0..self.info.l2_entries() {
                        if done == vm_state.len() {
                            break;
                        }
                        let n = cs.min(vm_state.len() - done);
                        let mut data = vec![0; cs];
                        data[..n].copy_from_slice(&vm_state[done..done + n]);
                        let host = self
                            .allocate_payload(
                                &data,
                                (index * self.info.l2_entries() + slot) as u64 * cs as u64,
                            )
                            .await?;
                        put64(&mut l2, slot * self.info.l2_entry_bytes(), (1 << 63) | host);
                        if self.info.extended_l2() {
                            put64(&mut l2, slot * 16 + 8, u32::MAX as u64);
                        }
                        done += n;
                    }
                    let host = self.allocate_object(&l2).await?;
                    put64(&mut l1, index * 8, (1 << 63) | host);
                }
            }
            let host = self.allocate_object(&l1).await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| invalid("Clock before epoch"))?;
            let mut extra = vec![0; 24];
            put64(&mut extra, 0, vm_state.len() as u64);
            put64(&mut extra, 8, size);
            put64(&mut extra, 16, options.icount.unwrap_or(-1) as u64);
            let snapshot = Snapshot {
                id: options.id,
                name: options.name,
                l1_offset: host,
                l1_entries: (l1.len() / 8) as u32,
                disk_size: size,
                vm_state_size: vm_state.len() as u64,
                date_seconds: now.as_secs() as u32,
                date_nanoseconds: now.subsec_nanos(),
                vm_clock_nanoseconds: options.vm_clock_nanoseconds,
                icount: options.icount.unwrap_or(-1),
                extra_data: extra,
            };
            snapshots.push(snapshot.clone());
            self.replace_snapshot_directory(&snapshots).await?;
            Ok(snapshot)
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }

    pub async fn delete_snapshot(&self, id: &str) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        let mut snapshots = self.snapshot_directory().await?.0;
        let index = snapshots
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| invalid("Snapshot not found"))?;
        self.begin_transaction().await?;
        snapshots.remove(index);
        let result = self.replace_snapshot_directory(&snapshots).await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }

    pub async fn restore_snapshot(&self, id: &str) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        let snapshot = self
            .snapshot_directory()
            .await?
            .0
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| invalid("Snapshot not found"))?;
        self.begin_transaction().await?;
        let result = async {
            let bytes = self
                .read_bytes(snapshot.l1_offset, snapshot.l1_entries as usize * 8)
                .await?;
            let offset = self.allocate_object(&bytes).await?;
            let mut h = self.header.write().await;
            h.set_l1_table(offset, snapshot.l1_entries as usize)?;
            h.set_size(snapshot.disk_size);
            drop(h);
            self.rebuild_refcounts().await
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }

    /// Stream saved VM-state bytes without imposing a whole-state buffer limit.
    pub async fn read_snapshot_vm_state(
        &self,
        id: &str,
        offset: u64,
        buf: &mut [u8],
    ) -> Qcow2Result<usize> {
        let _io = self.io_lock.read().await;
        self.ensure_usable()?;
        let snapshot = self
            .snapshot_directory()
            .await?
            .0
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| invalid("Snapshot not found"))?;
        let len = snapshot
            .vm_state_size
            .saturating_sub(offset)
            .min(buf.len() as u64) as usize;
        let cs = self.info.cluster_size();
        let first = snapshot
            .disk_size
            .div_ceil((cs * self.info.l2_entries()) as u64);
        let mut done = 0;
        while done < len {
            let state = offset + done as u64;
            let cluster = state / cs as u64;
            let in_cluster = state as usize % cs;
            let unit = if self.info.extended_l2() { cs / 32 } else { cs };
            let n = (unit - in_cluster % unit).min(len - done);
            let l1_index = first + cluster / self.info.l2_entries() as u64;
            if l1_index >= snapshot.l1_entries as u64 {
                return Err(invalid("VM state exceeds snapshot L1 table"));
            }
            let root = self
                .read_bytes(snapshot.l1_offset + l1_index * 8, 8)
                .await?;
            let l2 = u64_at(&root, 0) & MASK;
            buf[done..done + n].fill(0);
            if l2 != 0 {
                let slot = cluster as usize % self.info.l2_entries();
                let value = self
                    .read_bytes(
                        l2 + (slot * self.info.l2_entry_bytes()) as u64,
                        self.info.l2_entry_bytes(),
                    )
                    .await?;
                let bitmap = if self.info.extended_l2() {
                    u64_at(&value, 8)
                } else {
                    0
                };
                let entry = self.validate_advanced_entry(
                    u64_at(&value, 0),
                    bitmap,
                    (first * self.info.l2_entries() as u64 + cluster) * cs as u64,
                )?;
                if entry.is_compressed() {
                    let (host, length) = entry
                        .compressed_range(self.info.cluster_bits() as u32)
                        .unwrap();
                    let mapping = crate::meta::Mapping {
                        source: crate::meta::MappingSource::Compressed,
                        cluster_offset: Some(host),
                        compressed_length: Some(length),
                        copied: false,
                    };
                    self.do_read_compressed(mapping, in_cluster, &mut buf[done..done + n])
                        .await?;
                } else if !entry.is_zero()
                    && entry.cluster_offset() != 0
                    && (!self.info.extended_l2() || bitmap & (1_u64 << (in_cluster / unit)) != 0)
                {
                    let data = self
                        .read_payload(
                            entry.cluster_offset() + in_cluster as u64,
                            (first * self.info.l2_entries() as u64 + cluster) * cs as u64
                                + in_cluster as u64,
                            n,
                        )
                        .await?;
                    buf[done..done + n].copy_from_slice(&data);
                }
            }
            done += n;
        }
        Ok(len)
    }
}
