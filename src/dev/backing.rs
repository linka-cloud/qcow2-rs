//! Backing-chain management. Preserving rebases materialize every snapshot.
use super::graph::{invalid, put64, u64_at, MASK};
use super::*;
use crate::helpers::Qcow2IoBuf;
use crate::meta::{L2Entry, Mapping, MappingSource};

pub struct RebaseTarget<T> {
    pub file: BackingFile,
    pub source: Backing<T>,
}
impl<T> Backing<T> {
    pub fn raw(file: T, size: u64) -> Self {
        Self::Raw {
            file,
            size: AtomicU64::new(size),
        }
    }
    pub fn qcow2(dev: Qcow2Dev<T>) -> Self {
        Self::Qcow2(Box::new(dev))
    }
}
impl<T: Qcow2IoOps> Qcow2Dev<T> {
    /// The declared backing name and the format detected when the chain was opened.
    pub async fn backing_file(&self) -> Option<BackingFile> {
        let h = self.header.read().await;
        h.backing_filename().map(|name| BackingFile {
            path: PathBuf::from(name),
            format: self.backing_file.as_ref().map(|back| match back {
                Backing::Raw { .. } => BackingFormat::Raw,
                Backing::Qcow2(_) => BackingFormat::Qcow2,
            }),
        })
    }

    pub(crate) async fn read_entry_cluster(
        &self,
        entry: L2Entry,
        bitmap: u64,
        guest: u64,
        valid: usize,
    ) -> Qcow2Result<Qcow2IoBuf<u8>> {
        self.read_entry_cluster_mode(entry, bitmap, guest, valid, true)
            .await
    }
    pub(crate) async fn read_entry_cluster_mode(
        &self,
        entry: L2Entry,
        bitmap: u64,
        guest: u64,
        valid: usize,
        backing_fallback: bool,
    ) -> Qcow2Result<Qcow2IoBuf<u8>> {
        let cs = self.info.cluster_size();
        let mut data = Qcow2IoBuf::<u8>::new(cs);
        data.fill(0);
        if entry.is_compressed() {
            let (host, len) = entry
                .compressed_range(self.info.cluster_bits() as u32)
                .ok_or("Missing compressed extent")?;
            self.do_read_compressed(
                Mapping {
                    source: MappingSource::Compressed,
                    cluster_offset: Some(host),
                    compressed_length: Some(len),
                    copied: false,
                },
                0,
                &mut data,
            )
            .await?;
        } else if self.info.extended_l2() {
            let unit = cs / 32;
            if backing_fallback && (bitmap as u32 | (bitmap >> 32) as u32) != u32::MAX {
                self.read_backing_data(&mut data, guest).await?;
            }
            for i in 0..32 {
                let from = i * unit;
                if from >= valid {
                    break;
                }
                let to = from + unit;
                if bitmap & (1_u64 << (i + 32)) != 0 {
                    data[from..to].fill(0);
                } else if bitmap & (1_u64 << i) != 0 {
                    let bytes = self
                        .read_payload(
                            entry.cluster_offset() + from as u64,
                            guest + from as u64,
                            unit.min(valid.saturating_sub(from)),
                        )
                        .await?;
                    data[from..from + bytes.len()].copy_from_slice(&bytes);
                }
            }
        } else if !entry.is_zero() {
            if entry.cluster_offset() != 0 || self.info.external_data() && entry.is_copied() {
                let bytes = self
                    .read_payload(entry.cluster_offset(), guest, valid)
                    .await?;
                data[..valid].copy_from_slice(&bytes);
            } else if backing_fallback {
                self.read_backing_data(&mut data, guest).await?;
            }
        }
        data[valid..].fill(0);
        Ok(data)
    }
    pub(crate) async fn materialize_root(
        &self,
        root: &[u8],
        size: u64,
    ) -> Qcow2Result<(u64, usize)> {
        let cs = self.info.cluster_size();
        let mut l1 = root.to_vec();
        let coverage = (cs * self.info.l2_entries()) as u64;
        for index in 0..size.div_ceil(coverage) as usize {
            let old = u64_at(root, index * 8) & MASK;
            let mut l2 = if old == 0 {
                vec![0; cs]
            } else {
                self.read_bytes(old, cs).await?
            };
            for slot in 0..self.info.l2_entries() {
                let guest = (index * self.info.l2_entries() + slot) as u64 * cs as u64;
                if guest >= size {
                    break;
                }
                let at = slot * self.info.l2_entry_bytes();
                let bitmap = if self.info.extended_l2() {
                    u64_at(&l2, at + 8)
                } else {
                    0
                };
                let entry = self.validate_advanced_entry(u64_at(&l2, at), bitmap, guest)?;
                let mut data = self
                    .read_entry_cluster(entry, bitmap, guest, cs.min((size - guest) as usize))
                    .await?;
                let host = self.stage_payload(&mut data, guest).await?;
                put64(&mut l2, at, (1 << 63) | host);
                if self.info.extended_l2() {
                    put64(&mut l2, at + 8, u32::MAX as u64);
                }
            }
            let host = self.allocate_object(&l2).await?;
            put64(&mut l1, index * 8, (1 << 63) | host);
        }
        let host = self.allocate_object(&l1).await?;
        Ok((host, l1.len() / 8))
    }
    pub(crate) async fn materialize_all_roots(&self) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        self.materialize_all_roots_locked().await
    }
    async fn materialize_all_roots_locked(&self) -> Qcow2Result<()> {
        self.preallocate_locked(Preallocation::Full).await?;
        let mut snapshots = self.snapshot_directory().await?.0;
        if snapshots.is_empty() {
            return Ok(());
        }
        self.begin_transaction().await?;
        let result = async {
            for s in &mut snapshots {
                let root = self
                    .read_bytes(s.l1_offset, s.l1_entries as usize * 8)
                    .await?;
                let (offset, entries) = self.materialize_root(&root, s.disk_size).await?;
                s.l1_offset = offset;
                s.l1_entries = entries as u32;
            }
            self.replace_snapshot_directory(&snapshots).await
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }
    async fn storage_paths(&self) -> Qcow2Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        let mut current = self;
        loop {
            paths.push(current.path.clone());
            if let Some(data) = &current.data_path {
                paths.push(data.clone());
            }
            match &current.backing_file {
                Some(Backing::Qcow2(back)) => {
                    if !back.info.is_read_only() {
                        return Err(invalid("Backing sources must be read-only"));
                    }
                    current = back;
                }
                Some(Backing::Raw { .. }) => {
                    let h = current.header.read().await;
                    if let Some(name) = h.backing_filename() {
                        let path = PathBuf::from(name);
                        paths.push(if path.is_absolute() {
                            path
                        } else {
                            current.path.parent().unwrap_or(Path::new(".")).join(path)
                        });
                    }
                    break;
                }
                None => break,
            }
        }
        Ok(paths)
    }
    async fn validate_new_backing(&self, target: &RebaseTarget<T>) -> Qcow2Result<()> {
        let paths = match &target.source {
            Backing::Qcow2(back) => {
                if !back.info.is_read_only() {
                    return Err(invalid("Rebase target must be read-only"));
                }
                back.ensure_usable()?;
                let named = if target.file.path.is_absolute() {
                    target.file.path.clone()
                } else {
                    self.path
                        .parent()
                        .unwrap_or(Path::new("."))
                        .join(&target.file.path)
                };
                if !same_file::is_same_file(&named, &back.path)? {
                    return Err(invalid(
                        "Backing name does not identify the supplied device",
                    ));
                }
                back.storage_paths().await?
            }
            Backing::Raw { .. } => vec![if target.file.path.is_absolute() {
                target.file.path.clone()
            } else {
                self.path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join(&target.file.path)
            }],
        };
        for path in paths {
            for own in std::iter::once(&self.path).chain(self.data_path.iter()) {
                if same_file::is_same_file(&path, own)? {
                    return Err(invalid("Rebase would create a cycle or storage alias"));
                }
            }
        }
        Ok(())
    }
    /// Change backing storage. Preserving mode retains active and inactive
    /// snapshot bytes; non-preserving mode explicitly changes fallback data.
    pub async fn rebase(
        &mut self,
        mut target: Option<RebaseTarget<T>>,
        preserve_data: bool,
    ) -> Qcow2Result<()> {
        self.ensure_usable()?;
        if self.info.is_read_only() {
            return Err(invalid("Read-only image"));
        }
        if self.info.raw_data() && target.is_some() {
            return Err(invalid("Raw external data cannot have backing storage"));
        }
        if let Some(target) = &mut target {
            self.validate_new_backing(target).await?;
            let actual = match target.source {
                Backing::Raw { .. } => BackingFormat::Raw,
                Backing::Qcow2(_) => BackingFormat::Qcow2,
            };
            if target.file.format.is_some_and(|f| f != actual) {
                return Err(invalid("Backing format does not match supplied source"));
            }
            target.file.format = Some(actual);
        }
        let name = target
            .as_ref()
            .map(|t| {
                t.file
                    .path
                    .to_str()
                    .map(str::to_owned)
                    .ok_or("Backing name must be UTF-8")
            })
            .transpose()?;
        if name
            .as_ref()
            .is_some_and(|n| n.is_empty() || n.len() > 1023 || n.as_bytes().contains(&0))
        {
            return Err(invalid("Invalid backing name"));
        }
        let format = target
            .as_ref()
            .and_then(|t| t.file.format)
            .map(|f| f.as_str().to_owned());
        let mut proposal = self.header.read().await.clone();
        proposal.set_backing(name.clone(), format.clone());
        proposal.serialize_to_buf()?;
        if preserve_data {
            self.materialize_all_roots().await?;
        }
        let _io = self.io_lock.write().await;
        if !preserve_data {
            self.track_dirty_bitmaps(0, self.info.virtual_size())
                .await?;
        }
        self.begin_transaction().await?;
        let result = async {
            let mut h = self.header.write().await;
            let previous = h.clone();
            h.set_backing(name, format.clone());
            if let Err(error) = self.commit_header(&mut h, |h| *h = previous).await {
                self.reopen_required.store(true, Ordering::Release);
                return Err(error);
            }
            self.info.refresh_header_flags(&h);
            drop(h);
            if let Some(RebaseTarget {
                source: Backing::Qcow2(back),
                ..
            }) = &target
            {
                back.info.mark_used_as_backing();
            }
            self.backing_file = target.map(|t| t.source);
            self.backing_format = format;
            let flags = self.header.read().await.incompatible_features();
            self.store_feature_flags(flags & !3).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }
}

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    fn named_backing_path(&self, name: &str) -> PathBuf {
        let path = PathBuf::from(name);
        if path.is_absolute() {
            path
        } else {
            self.path.parent().unwrap_or(Path::new(".")).join(path)
        }
    }
    async fn empty_active_locked(&self) -> Qcow2Result<()> {
        self.begin_transaction().await?;
        let h = self.header.read().await;
        let mut l1 = self
            .read_bytes(h.l1_table_offset(), h.l1_table_entries() * 8)
            .await?;
        drop(h);
        let coverage = (self.info.cluster_size() * self.info.l2_entries()) as u64;
        for i in 0..self.info.virtual_size().div_ceil(coverage) as usize {
            put64(&mut l1, i * 8, 0);
        }
        let host = self.allocate_object(&l1).await?;
        self.header.write().await.set_l1_table(host, l1.len() / 8)?;
        self.rebuild_refcounts().await
    }
    /// Commit the active view into its immediate QCOW2 backing image.
    /// Snapshot fallback is materialized first. A failed commit retains the
    /// complete source view; close/reopen before retrying against the base.
    pub async fn commit_to<U: Qcow2IoOps>(
        &self,
        target: &Qcow2Dev<U>,
        empty_source: bool,
    ) -> Qcow2Result<()> {
        self.ensure_usable()?;
        target.ensure_usable()?;
        if self.info.is_read_only() || target.info.is_read_only() {
            return Err(invalid("Commit requires writable source and target images"));
        }
        let name = self
            .header
            .read()
            .await
            .backing_filename()
            .cloned()
            .ok_or("Image has no backing file")?;
        if !matches!(self.backing_file, Some(Backing::Qcow2(_)))
            || !same_file::is_same_file(self.named_backing_path(&name), &target.path)?
        {
            return Err(invalid(
                "Commit target must be the immediate QCOW2 backing file",
            ));
        }
        for path in target.storage_paths().await? {
            for own in std::iter::once(&self.path).chain(self.data_path.iter()) {
                if same_file::is_same_file(&path, own)? {
                    return Err(invalid("Commit storage aliases its source"));
                }
            }
        }
        let (_first, _second) = if self.path < target.path {
            (self.io_lock.write().await, target.io_lock.write().await)
        } else {
            (target.io_lock.write().await, self.io_lock.write().await)
        };
        let result = async {
            target.flush_meta_locked().await?;
            if let Some(Backing::Qcow2(back)) = &self.backing_file {
                let old = back.header.read().await;
                let new = target.header.read().await;
                if old.cluster_bits() != new.cluster_bits()
                    || old.refcount_order() != new.refcount_order()
                    || old.crypt_method() != new.crypt_method()
                    || old.incompatible_features() & 20 != new.incompatible_features() & 20
                    || old.backing_filename() != new.backing_filename()
                    || old.extension_data(0x44415441) != new.extension_data(0x44415441)
                {
                    return Err(invalid("Backing layout changed; reopen the source image"));
                }
                drop(old);
                let header = new.clone();
                drop(new);
                *back.header.write().await = header;
                back.reload_metadata().await?;
                back.needs_repair.store(false, Ordering::Release);
            }
            self.materialize_all_roots_locked().await?;
            let size = self.info.virtual_size();
            let unit = 1_u64 << target.info.block_size_shift;
            let needed = size.div_ceil(unit) * unit;
            if target.info.virtual_size() < needed {
                target.resize_locked(needed, false).await?;
            }
            let alignment = 1_usize << self.info.block_size_shift.max(target.info.block_size_shift);
            for offset in (0..size).step_by(1 << 20) {
                let valid = ((size - offset) as usize).min(1 << 20);
                let length = valid.div_ceil(alignment) * alignment;
                let mut source = Qcow2IoBuf::<u8>::new(length);
                source.fill(0);
                self.advanced_read_at(&mut source, offset).await?;
                let target_len = valid.div_ceil(unit as usize) * unit as usize;
                let mut data = Qcow2IoBuf::<u8>::new(target_len);
                data.fill(0);
                target.advanced_read_at(&mut data, offset).await?;
                data[..valid].copy_from_slice(&source[..valid]);
                target
                    .advanced_modify(offset, target_len, super::advanced::Mutation::Write(&data))
                    .await?;
            }
            target.flush_meta_locked().await?;
            if let Some(Backing::Qcow2(back)) = &self.backing_file {
                *back.header.write().await = target.header.read().await.clone();
                back.reload_metadata().await?;
            }
            if empty_source {
                self.empty_active_locked().await?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            self.reopen_required.store(true, Ordering::Release);
        }
        result
    }
    /// Commit to the immediate raw backing file supplied by the caller.
    pub async fn commit_to_raw<U: Qcow2IoOps>(
        &self,
        target: &U,
        path: &Path,
        empty_source: bool,
    ) -> Qcow2Result<()> {
        self.ensure_usable()?;
        if self.info.is_read_only() {
            return Err(invalid("Commit requires a writable source"));
        }
        let name = self
            .header
            .read()
            .await
            .backing_filename()
            .cloned()
            .ok_or("Image has no backing file")?;
        if !matches!(self.backing_file, Some(Backing::Raw { .. }))
            || !same_file::is_same_file(self.named_backing_path(&name), path)?
            || same_file::is_same_file(&self.path, path)?
        {
            return Err(invalid(
                "Commit target must be the immediate raw backing file",
            ));
        }
        let _io = self.io_lock.write().await;
        let original = std::fs::metadata(path)?.len();
        let result = async {
            self.materialize_all_roots_locked().await?;
            let size = self.info.virtual_size();
            let unit = 1_usize << self.info.block_size_shift;
            for offset in (0..size).step_by(1 << 20) {
                let valid = ((size - offset) as usize).min(1 << 20);
                let length = valid.div_ceil(unit) * unit;
                let mut data = Qcow2IoBuf::<u8>::new(length);
                data.fill(0);
                let _ = target.read_to(offset, &mut data).await?;
                let mut source = Qcow2IoBuf::<u8>::new(length);
                source.fill(0);
                self.advanced_read_at(&mut source, offset).await?;
                data[..valid].copy_from_slice(&source[..valid]);
                target.write_from(offset, &data).await?;
            }
            if let Err(error) = target.set_len(original.max(size)).await {
                if error.get_inner().kind() != std::io::ErrorKind::Unsupported {
                    return Err(error);
                }
            }
            target.fsync(0, 0, 0).await?;
            if let Some(Backing::Raw {
                size: backing_size, ..
            }) = &self.backing_file
            {
                backing_size.store(original.max(size), Ordering::Release);
            }
            if empty_source {
                self.empty_active_locked().await?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            self.reopen_required.store(true, Ordering::Release);
        }
        result
    }
}
