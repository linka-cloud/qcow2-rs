//! Conversion of on-disk metadata representations.
use super::graph::{invalid, put64, u64_at, MASK};
use super::*;

#[derive(Debug, Clone, Default)]
pub struct AmendOptions {
    pub version: Option<u32>,
    pub refcount_order: Option<u8>,
    pub extended_l2: Option<bool>,
    pub compression: Option<CompressionType>,
    pub lazy_refcounts: Option<bool>,
}
impl<T: Qcow2IoOps> Qcow2Dev<T> {
    async fn convert_root(
        &self,
        root: &[u8],
        size: u64,
        vm_size: Option<u64>,
        extended: bool,
        version: u32,
    ) -> Qcow2Result<(u64, usize)> {
        let cs = self.info.cluster_size();
        let old_entries = self.info.l2_entries();
        let entries = cs / if extended { 16 } else { 8 };
        let old_vm = size.div_ceil((cs * old_entries) as u64) * (cs * old_entries) as u64;
        let new_vm = size.div_ceil((cs * entries) as u64) * (cs * entries) as u64;
        let mut l1 = vec![0; size.div_ceil((cs * entries) as u64) as usize * 8];
        if l1.len() > Qcow2Header::MAX_L1_SIZE as usize {
            return Err(invalid("Converted L1 exceeds capacity"));
        }
        let (mut index, mut table) = (None, vec![0; cs]);
        for (i, entry) in root.chunks_exact(8).enumerate() {
            let offset = u64_at(entry, 0) & MASK;
            if offset == 0 {
                continue;
            }
            let source = self.read_bytes(offset, cs).await?;
            for slot in 0..old_entries {
                let at = slot * self.info.l2_entry_bytes();
                let value = u64_at(&source, at);
                let bitmap = if self.info.extended_l2() {
                    u64_at(&source, at + 8)
                } else {
                    0
                };
                if value == 0 && bitmap == 0 {
                    continue;
                }
                let guest = (i * old_entries + slot) as u64 * cs as u64;
                let (destination, valid, fallback) = if guest < size {
                    (guest, cs.min((size - guest) as usize), true)
                } else if guest >= old_vm {
                    let relative = guest - old_vm;
                    if vm_size.is_some_and(|n| relative >= n) {
                        continue;
                    }
                    (
                        new_vm
                            .checked_add(relative)
                            .ok_or("VM-state relocation overflow")?,
                        vm_size
                            .map(|n| cs.min((n - relative) as usize))
                            .unwrap_or(cs),
                        false,
                    )
                } else {
                    continue;
                };
                let target_index = (destination / (cs * entries) as u64) as usize;
                if target_index >= Qcow2Header::MAX_L1_SIZE as usize / 8 {
                    return Err(invalid("Converted VM state exceeds L1 capacity"));
                }
                if index != Some(target_index) {
                    if let Some(previous) = index {
                        let host = self.allocate_object(&table).await?;
                        put64(&mut l1, previous * 8, (1 << 63) | host);
                    }
                    index = Some(target_index);
                    table.fill(0);
                    l1.resize(l1.len().max((target_index + 1) * 8), 0);
                }
                let entry = self.validate_advanced_entry(value, bitmap, guest)?;
                let pure_zero = if self.info.extended_l2() {
                    entry.cluster_offset() == 0
                        && !entry.is_copied()
                        && bitmap == (u32::MAX as u64) << 32
                } else {
                    value == 1
                };
                let (descriptor, bits) = if pure_zero && version == 3 {
                    (
                        if extended { 0 } else { 1 },
                        if extended { (u32::MAX as u64) << 32 } else { 0 },
                    )
                } else if pure_zero && !self.info.has_back_file() {
                    (0, 0)
                } else {
                    let mut data = self
                        .read_entry_cluster_mode(entry, bitmap, guest, valid, fallback)
                        .await?;
                    let host = self.stage_payload(&mut data, destination).await?;
                    ((1 << 63) | host, if extended { u32::MAX as u64 } else { 0 })
                };
                let target_slot = (destination / cs as u64) as usize % entries;
                put64(
                    &mut table,
                    target_slot * if extended { 16 } else { 8 },
                    descriptor,
                );
                if extended {
                    put64(&mut table, target_slot * 16 + 8, bits);
                }
            }
        }
        if let Some(previous) = index {
            let host = self.allocate_object(&table).await?;
            put64(&mut l1, previous * 8, (1 << 63) | host);
        }
        let host = self.allocate_object(&l1).await?;
        Ok((host, l1.len() / 8))
    }
    /// Convert version, refcount width, L2 layout, codec tag, and lazy policy.
    /// Payloads that need relocation are rewritten as ordinary clusters.
    /// Inactive snapshots and VM-state offsets are converted with the active root.
    pub async fn amend(&mut self, options: AmendOptions) -> Qcow2Result<()> {
        self.ensure_usable()?;
        if options.version.is_none()
            && options.refcount_order.is_none()
            && options.extended_l2.is_none()
            && options.compression.is_none()
            && options.lazy_refcounts.is_none()
        {
            return Ok(());
        }
        let h = self.header.read().await;
        let version = options.version.unwrap_or(h.version());
        let order = options.refcount_order.unwrap_or(if version == 2 {
            4
        } else {
            self.info.refcount_order()
        });
        let extended = options
            .extended_l2
            .unwrap_or(version != 2 && self.info.extended_l2());
        let codec = options.compression.unwrap_or(if version == 2 {
            CompressionType::Deflate
        } else if h.compression_type() == 1 {
            CompressionType::Zstd
        } else {
            CompressionType::Deflate
        });
        let lazy = options
            .lazy_refcounts
            .unwrap_or(version != 2 && h.compatible_features() & 1 != 0);
        let mut proposal = h.clone();
        proposal.amend_layout(
            version,
            order,
            extended,
            u8::from(codec == CompressionType::Zstd),
            lazy,
        )?;
        if self.info.external_data() && codec != CompressionType::Deflate
            || self.info.encrypted() && codec != CompressionType::Deflate
        {
            return Err(invalid(
                "Compression selection conflicts with encryption/external storage",
            ));
        }
        if order == 0 && h.nb_snapshots() != 0 {
            return Err(invalid("Snapshots require refcounts wider than one bit"));
        }
        proposal.serialize_to_buf()?;
        drop(h);
        let params = Qcow2DevParams::new(
            self.info.block_size_shift,
            Some((
                self.info.rb_slice_bits,
                (self.info.rb_cache_cnt as usize) << self.info.rb_slice_bits,
            )),
            Some((
                self.info.l2_slice_bits,
                (self.info.l2_cache_cnt as usize) << self.info.l2_slice_bits,
            )),
            self.info.is_read_only(),
            false,
        );
        let _io = self.io_lock.write().await;
        self.begin_transaction().await?;
        let result = async {
            let h = self.header.read().await;
            let root = self
                .read_bytes(h.l1_table_offset(), h.l1_table_entries() * 8)
                .await?;
            let size = h.size();
            drop(h);
            let (active, entries) = self
                .convert_root(
                    &root,
                    size,
                    if self.info.external_data() {
                        Some(0)
                    } else {
                        None
                    },
                    extended,
                    version,
                )
                .await?;
            let mut snapshots = self.snapshot_directory().await?.0;
            for snapshot in &mut snapshots {
                let root = self
                    .read_bytes(snapshot.l1_offset, snapshot.l1_entries as usize * 8)
                    .await?;
                let (host, entries) = self
                    .convert_root(
                        &root,
                        snapshot.disk_size,
                        Some(snapshot.vm_state_size),
                        extended,
                        version,
                    )
                    .await?;
                snapshot.l1_offset = host;
                snapshot.l1_entries = entries as u32;
            }
            let directory = self.stage_snapshot_directory(&snapshots).await?;
            let maps = self.load_bitmaps().await?;
            if !maps.is_empty() {
                self.stage_bitmaps(&maps).await?;
            }
            let mut h = self.header.write().await;
            // Preserve newly staged bitmap pointers and the allocator's latest RT.
            h.amend_layout(
                version,
                order,
                extended,
                u8::from(codec == CompressionType::Zstd),
                lazy,
            )?;
            h.set_l1_table(active, entries)?;
            h.set_snapshot_table(directory, snapshots.len() as u32);
            let info = Qcow2Info::new(&h, &params)?;
            drop(h);
            self.info = info;
            self.rebuild_refcounts().await
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
            self.reopen_required.store(true, Ordering::Release);
        }
        result
    }
    /// Change the recorded external filename after reopening with an override
    /// pointing to the same storage. Payload data is not moved by this method.
    pub async fn amend_data_file_name(&mut self, name: PathBuf) -> Qcow2Result<()> {
        if !self.info.external_data() {
            return Err(invalid("Image has no external data file"));
        }
        let text = name
            .to_str()
            .ok_or("Data filename must be UTF-8")?
            .to_owned();
        if text.is_empty() || text.len() > 1023 || text.as_bytes().contains(&0) {
            return Err(invalid("Invalid data filename"));
        }
        let resolved = if name.is_absolute() {
            name
        } else {
            self.path.parent().unwrap_or(Path::new(".")).join(name)
        };
        let current = self
            .data_path
            .as_ref()
            .ok_or("Supply the actual data path before amending its name")?;
        if !same_file::is_same_file(current, &resolved)? {
            return Err(invalid(
                "The new name does not identify the attached data file",
            ));
        }
        let _io = self.io_lock.write().await;
        self.begin_transaction().await?;
        let mut h = self.header.write().await;
        let previous = h.clone();
        h.set_extension(0x44415441, Some(text.as_bytes().to_vec()));
        if let Err(error) = self.commit_header(&mut h, |h| *h = previous).await {
            self.needs_repair.store(true, Ordering::Release);
            self.reopen_required.store(true, Ordering::Release);
            return Err(error);
        }
        drop(h);
        self.data_file_name = Some(text);
        let flags = self.header.read().await.incompatible_features();
        self.store_feature_flags(flags & !3).await
    }
}
