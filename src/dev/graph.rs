//! Reachability and ownership of every allocated metadata/data cluster.
use super::*;
use crate::helpers::{IntAlignment, Qcow2IoBuf};
use crate::meta::{L1Entry, TableEntry};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const MASK: u64 = 0x00ff_ffff_ffff_fe00;
pub(crate) fn invalid(message: impl Into<String>) -> crate::error::Qcow2Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into()).into()
}
pub(crate) fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes(b[i..i + 2].try_into().unwrap())
}
pub(crate) fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i..i + 4].try_into().unwrap())
}
pub(crate) fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(b[i..i + 8].try_into().unwrap())
}
pub(crate) fn put64(b: &mut [u8], i: usize, n: u64) {
    b[i..i + 8].copy_from_slice(&n.to_be_bytes());
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub name: String,
    pub l1_offset: u64,
    pub l1_entries: u32,
    pub disk_size: u64,
    pub vm_state_size: u64,
    pub date_seconds: u32,
    pub date_nanoseconds: u32,
    pub vm_clock_nanoseconds: u64,
    pub icount: i64,
    pub extra_data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefcountMismatch {
    pub host_cluster: u64,
    pub actual: u64,
    pub expected: u64,
}
#[derive(Debug, Default)]
pub struct CheckReport {
    pub mismatches: Vec<RefcountMismatch>,
    pub copied_flag_errors: Vec<u64>,
    pub dirty: bool,
    pub corrupt: bool,
}
impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
            && self.copied_flag_errors.is_empty()
            && !self.dirty
            && !self.corrupt
    }
}

pub(crate) struct Graph {
    pub counts: BTreeMap<u64, u64>,
    pub owners: BTreeMap<u64, (&'static str, u64)>,
    pub data_mappings: (usize, usize),
    pub ref_metadata: BTreeSet<u64>,
}
impl Graph {
    fn new() -> Self {
        Self {
            counts: BTreeMap::new(),
            owners: BTreeMap::new(),
            data_mappings: (0, 0),
            ref_metadata: BTreeSet::new(),
        }
    }
    fn claim(&mut self, role: &'static str, offset: u64, len: u64, cs: u64) -> Qcow2Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = offset
            .checked_add(len)
            .filter(|n| *n <= 1_u64 << 56)
            .ok_or_else(|| invalid("Metadata range overflow"))?;
        if offset % cs != 0 {
            return Err(invalid("Metadata range is not cluster aligned"));
        }
        for c in offset / cs..end.div_ceil(cs) {
            let identity = if role == "data" { c * cs } else { offset };
            if let Some(previous) = self.owners.insert(c, (role, identity)) {
                if previous != (role, identity) {
                    return Err(invalid(format!(
                        "Metadata overlap at cluster {c}: {} and {role}",
                        previous.0
                    )));
                }
            }
            let count = self.counts.entry(c).or_default();
            *count = count
                .checked_add(1)
                .ok_or_else(|| invalid("Refcount overflow"))?;
            if role == "refcount table" || role == "refcount block" {
                self.ref_metadata.insert(c);
            }
        }
        Ok(())
    }
}

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    pub(crate) async fn read_bytes(&self, offset: u64, len: usize) -> Qcow2Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        if len > 32 << 20 {
            return Err(invalid("Metadata object exceeds 32 MiB"));
        }
        let bs = 1_usize << self.info.block_size_shift;
        let base = offset & !(bs as u64 - 1);
        let pad = (offset - base) as usize;
        let bytes = (len + pad)
            .align_up(bs)
            .ok_or_else(|| invalid("Metadata length overflow"))?;
        offset
            .checked_add(len as u64)
            .filter(|n| *n <= 1_u64 << 56)
            .ok_or_else(|| invalid("Metadata address overflow"))?;
        let mut buf = Qcow2IoBuf::<u8>::new(bytes);
        if self.call_read(base, &mut buf).await? < len + pad {
            return Err(invalid("Truncated metadata object"));
        }
        Ok(buf[pad..pad + len].to_vec())
    }

    pub(crate) async fn write_bytes(&self, offset: u64, data: &[u8]) -> Qcow2Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let bs = 1_usize << self.info.block_size_shift;
        let base = offset & !(bs as u64 - 1);
        let pad = (offset - base) as usize;
        let bytes = (pad + data.len())
            .align_up(bs)
            .ok_or_else(|| invalid("Metadata length overflow"))?;
        let mut buf = Qcow2IoBuf::<u8>::new(bytes);
        buf.fill(0);
        if pad != 0 || bytes != data.len() {
            let _ = self.call_read(base, &mut buf).await?;
        }
        buf[pad..pad + data.len()].copy_from_slice(data);
        self.call_write(base, &buf).await
    }

    pub(crate) async fn snapshot_directory(&self) -> Qcow2Result<(Vec<Snapshot>, u64)> {
        let h = self.header.read().await;
        let (start, count, version, size) = (
            h.snapshots_offset(),
            h.nb_snapshots(),
            h.version(),
            h.size(),
        );
        drop(h);
        if count > 65535 {
            return Err(invalid("Too many snapshots"));
        }
        let mut cursor = start;
        let mut snapshots = Vec::new();
        let mut ids = BTreeSet::new();
        for _ in 0..count {
            let b = self.read_bytes(cursor, 40).await?;
            let extra_len = u32_at(&b, 36) as usize;
            if version == 3 && extra_len < 16 {
                return Err(invalid("Snapshot extra data is too short"));
            }
            let id_len = u16_at(&b, 12) as usize;
            let name_len = u16_at(&b, 14) as usize;
            let extra = self
                .read_bytes(cursor + 40, extra_len + id_len + name_len)
                .await?;
            let id = String::from_utf8(extra[extra_len..extra_len + id_len].to_vec())
                .map_err(|_| invalid("Invalid snapshot ID"))?;
            if !ids.insert(id.clone()) {
                return Err(invalid("Duplicate snapshot ID"));
            }
            let name = String::from_utf8(extra[extra_len + id_len..].to_vec())
                .map_err(|_| invalid("Invalid snapshot name"))?;
            let snapshot = Snapshot {
                id,
                name,
                l1_offset: u64_at(&b, 0),
                l1_entries: u32_at(&b, 8),
                disk_size: if extra_len >= 16 {
                    u64_at(&extra, 8)
                } else {
                    size
                },
                vm_state_size: if extra_len >= 8 {
                    u64_at(&extra, 0)
                } else {
                    u32_at(&b, 32) as u64
                },
                date_seconds: u32_at(&b, 16),
                date_nanoseconds: u32_at(&b, 20),
                vm_clock_nanoseconds: u64_at(&b, 24),
                icount: if extra_len >= 24 {
                    u64_at(&extra, 16) as i64
                } else {
                    -1
                },
                extra_data: extra[..extra_len].to_vec(),
            };
            let coverage = (self.info.cluster_size() * self.info.l2_entries()) as u64;
            let disk_entries = snapshot.disk_size.div_ceil(coverage);
            let state_entries = snapshot.vm_state_size.div_ceil(coverage);
            if snapshot.date_nanoseconds >= 1_000_000_000
                || snapshot.l1_entries as u64 * 8 > u64::from(Qcow2Header::MAX_L1_SIZE)
                || disk_entries.saturating_add(state_entries) > u64::from(snapshot.l1_entries)
            {
                return Err(invalid(
                    "Invalid snapshot size, VM-state capacity, or timestamp",
                ));
            }
            snapshots.push(snapshot);
            cursor = (cursor + 40 + extra.len() as u64)
                .align_up(8_u64)
                .ok_or_else(|| invalid("Snapshot directory overflow"))?;
            if cursor - start > 32 << 20 {
                return Err(invalid("Snapshot directory exceeds 32 MiB"));
            }
        }
        Ok((snapshots, cursor - start))
    }

    async fn graph_root(&self, graph: &mut Graph, offset: u64, entries: usize) -> Qcow2Result<()> {
        let cs = self.info.cluster_size() as u64;
        if entries > (32 << 20) / 8 {
            return Err(invalid("Snapshot L1 table is too large"));
        }
        graph.claim("L1", offset, entries as u64 * 8, cs)?;
        let l1 = self.read_bytes(offset, entries * 8).await?;
        for (l1_index, b) in l1.chunks_exact(8).enumerate() {
            let entry = L1Entry::try_from_plain(u64_at(b, 0), &self.info)?;
            let off = entry.l2_offset();
            if off == 0 {
                continue;
            }
            graph.claim("L2", off, cs, cs)?;
            let l2 = self.read_bytes(off, cs as usize).await?;
            for (l2_index, b) in l2.chunks_exact(self.info.l2_entry_bytes()).enumerate() {
                let guest = (l1_index * self.info.l2_entries() + l2_index) as u64 * cs;
                let bitmap = if self.info.extended_l2() {
                    u64_at(b, 8)
                } else {
                    0
                };
                let entry = self.validate_advanced_entry(u64_at(b, 0), bitmap, guest)?;
                if !self.info.external_data() {
                    if let Some((off, count)) = entry.allocation(self.info.cluster_bits() as u32) {
                        graph.claim("data", off, count as u64 * cs, cs)?;
                        graph.data_mappings.0 += 1;
                        graph.data_mappings.1 += usize::from(entry.is_compressed());
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn graph(&self) -> Qcow2Result<Graph> {
        let cs = self.info.cluster_size() as u64;
        let h = self.header.read().await;
        let (l1, entries, rt, rt_clusters, snap_offset) = (
            h.l1_table_offset(),
            h.l1_table_entries(),
            h.reftable_offset(),
            h.reftable_clusters(),
            h.snapshots_offset(),
        );
        let bitmap = h.extension_data(0x23852875);
        let encryption = h.extension_data(0x0537be77);
        drop(h);
        let mut graph = Graph::new();
        graph.claim("header", 0, cs, cs)?;
        graph.claim("refcount table", rt, rt_clusters as u64 * cs, cs)?;
        let table = self.read_bytes(rt, rt_clusters * cs as usize).await?;
        let mut blocks = BTreeSet::new();
        for b in table.chunks_exact(8) {
            let off = u64_at(b, 0);
            if off != 0 {
                if !blocks.insert(off) {
                    return Err(invalid("Aliased refcount blocks"));
                }
                graph.claim("refcount block", off, cs, cs)?;
            }
        }
        self.graph_root(&mut graph, l1, entries).await?;
        let (snapshots, bytes) = self.snapshot_directory().await?;
        graph.claim("snapshot directory", snap_offset, bytes, cs)?;
        for snapshot in snapshots {
            self.graph_root(&mut graph, snapshot.l1_offset, snapshot.l1_entries as usize)
                .await?;
        }
        if let Some(b) = bitmap {
            if b.len() != 24 || u32_at(&b, 0) == 0 || u32_at(&b, 0) > 65535 || u32_at(&b, 4) != 0 {
                return Err(invalid("Invalid bitmap extension"));
            }
            let size = usize::try_from(u64_at(&b, 8))?;
            let off = u64_at(&b, 16);
            if size > 32 << 20 {
                return Err(invalid("Bitmap directory exceeds 32 MiB"));
            }
            graph.claim("bitmap directory", off, size as u64, cs)?;
            let directory = self.read_bytes(off, size).await?;
            let mut cursor = 0;
            for _ in 0..u32_at(&b, 0) {
                let entry = directory
                    .get(cursor..cursor + 24)
                    .ok_or_else(|| invalid("Truncated bitmap directory"))?;
                let table_offset = u64_at(entry, 0);
                let table_bytes = u32_at(entry, 8) as usize * 8;
                if table_bytes > 32 << 20 {
                    return Err(invalid("Bitmap table exceeds 32 MiB"));
                }
                graph.claim("bitmap table", table_offset, table_bytes as u64, cs)?;
                let table = self.read_bytes(table_offset, table_bytes).await?;
                for entry in table.chunks_exact(8) {
                    let value = u64_at(entry, 0);
                    if value & !(MASK | 1) != 0 || value & MASK != 0 && value & 1 != 0 {
                        return Err(invalid("Invalid bitmap table entry"));
                    }
                    if value & MASK != 0 {
                        graph.claim("bitmap data", value & MASK, cs, cs)?;
                    }
                }
                cursor = (cursor + 24 + u16_at(entry, 18) as usize + u32_at(entry, 20) as usize)
                    .align_up(8_usize)
                    .ok_or_else(|| invalid("Bitmap directory overflow"))?;
                if cursor > directory.len() {
                    return Err(invalid("Bitmap directory exceeds its bounds"));
                }
            }
            if cursor != directory.len() {
                return Err(invalid("Bitmap directory has trailing data"));
            }
        }
        if let Some(b) = encryption {
            if b.len() != 16 {
                return Err(invalid("Invalid encryption header pointer"));
            }
            if u64_at(&b, 8) > 32 << 20 {
                return Err(invalid("Encryption header exceeds 32 MiB"));
            }
            graph.claim("encryption header", u64_at(&b, 0), u64_at(&b, 8), cs)?;
        }
        Ok(graph)
    }

    pub(crate) async fn actual_refcounts(&self) -> Qcow2Result<BTreeMap<u64, u64>> {
        let h = self.header.read().await;
        let table = self
            .read_bytes(
                h.reftable_offset(),
                h.reftable_clusters() * self.info.cluster_size(),
            )
            .await?;
        drop(h);
        let mut actual = BTreeMap::new();
        let bits = 1_usize << self.info.refcount_order();
        for (index, entry) in table.chunks_exact(8).enumerate() {
            let offset = u64_at(entry, 0);
            if offset == 0 {
                continue;
            }
            let block = self.read_bytes(offset, self.info.cluster_size()).await?;
            for i in 0..self.info.rb_entries() {
                let count = if bits < 8 {
                    (block[i * bits / 8] >> (i * bits % 8)) as u64 & ((1 << bits) - 1)
                } else {
                    block[i * bits / 8..(i + 1) * bits / 8]
                        .iter()
                        .fold(0_u64, |n, b| (n << 8) | u64::from(*b))
                };
                if count != 0 {
                    actual.insert((index * self.info.rb_entries() + i) as u64, count);
                }
            }
        }
        Ok(actual)
    }

    pub async fn check_report(&self) -> Qcow2Result<CheckReport> {
        let _io = self.io_lock.write().await;
        if self.need_flush_meta() {
            return Err(invalid("Flush metadata before checking"));
        }
        let graph = self.graph().await?;
        let actual = self.actual_refcounts().await?;
        let mut report = CheckReport::default();
        for c in graph
            .counts
            .keys()
            .chain(actual.keys())
            .copied()
            .collect::<BTreeSet<_>>()
        {
            let expected = graph.counts.get(&c).copied().unwrap_or(0);
            let stored = actual.get(&c).copied().unwrap_or(0);
            if expected != stored {
                report.mismatches.push(RefcountMismatch {
                    host_cluster: c,
                    actual: stored,
                    expected,
                });
            }
        }
        let flags = self.header.read().await.incompatible_features();
        let h = self.header.read().await;
        let (root, entries) = (h.l1_table_offset(), h.l1_table_entries());
        drop(h);
        let l1 = self.read_bytes(root, entries * 8).await?;
        let cs = self.info.cluster_size() as u64;
        for (i, b) in l1.chunks_exact(8).enumerate() {
            let value = u64_at(b, 0);
            let off = value & MASK;
            let copied = off != 0 && graph.counts.get(&(off / cs)) == Some(&1);
            if (value & (1 << 63) != 0) != copied {
                report.copied_flag_errors.push(root + i as u64 * 8);
            }
            if off == 0 {
                continue;
            }
            let l2 = self.read_bytes(off, cs as usize).await?;
            for (j, b) in l2.chunks_exact(self.info.l2_entry_bytes()).enumerate() {
                let value = u64_at(b, 0);
                let data = value & MASK;
                let copied = if self.info.external_data() {
                    data != 0 || value & (1 << 63) != 0
                } else {
                    data != 0
                        && value & (1 << 62) == 0
                        && graph.counts.get(&(data / cs)) == Some(&1)
                };
                if (value & (1 << 63) != 0) != copied {
                    report
                        .copied_flag_errors
                        .push(off + (j * self.info.l2_entry_bytes()) as u64);
                }
            }
        }
        report.dirty = flags & 1 != 0;
        report.corrupt = flags & 2 != 0;
        Ok(report)
    }
}
