//! Persistent dirty bitmaps. Enabled maps are saved before guest mutations so
//! interrupted guest writes may add conservative dirty bits, never omit them.
use super::graph::{invalid, put64, u16_at, u32_at, u64_at, MASK};
use super::*;
use std::collections::BTreeSet;
const EXTENSION: u32 = 0x23852875;
const MAX: usize = 32 << 20;

#[derive(Debug, Clone)]
pub struct BitmapInfo {
    pub name: String,
    pub granularity_bits: u8,
    pub enabled: bool,
    pub in_use: bool,
    pub consistent: bool,
    pub extra_data: Vec<u8>,
    pub extra_data_compatible: bool,
}
#[derive(Clone)]
pub(crate) struct StoredBitmap {
    pub info: BitmapInfo,
    data: Vec<u8>,
    directory_entry: u64,
}
fn bitmap_bytes(size: u64, bits: u8) -> Qcow2Result<usize> {
    if !(9..=31).contains(&bits) {
        return Err(invalid(
            "Bitmap granularity must be between 512 bytes and 2 GiB",
        ));
    }
    let bytes = size.div_ceil(1_u64 << bits).div_ceil(8);
    if bytes > MAX as u64 {
        return Err(invalid(
            "Bitmap data exceeds 32 MiB; use a larger granularity",
        ));
    }
    Ok(bytes as usize)
}
impl StoredBitmap {
    fn mark(&mut self, offset: u64, len: u64, size: u64) {
        let end = offset.saturating_add(len).min(size);
        let unit = 1_u64 << self.info.granularity_bits;
        if end <= offset {
            return;
        }
        for bit in offset / unit..end.div_ceil(unit) {
            self.data[bit as usize / 8] |= 1 << (bit % 8);
        }
    }
    fn resize(&mut self, size: u64) -> Qcow2Result<()> {
        self.data
            .resize(bitmap_bytes(size, self.info.granularity_bits)?, 0);
        let bits = size.div_ceil(1_u64 << self.info.granularity_bits);
        if bits % 8 != 0 {
            if let Some(last) = self.data.last_mut() {
                *last &= ((1_u16 << (bits % 8)) - 1) as u8;
            }
        }
        Ok(())
    }
}
impl<T: Qcow2IoOps> Qcow2Dev<T> {
    pub(crate) async fn has_bitmaps(&self) -> bool {
        self.header.read().await.extension_data(EXTENSION).is_some()
    }

    pub(crate) async fn load_bitmaps(&self) -> Qcow2Result<Vec<StoredBitmap>> {
        let h = self.header.read().await;
        let ext = h.extension_data(EXTENSION);
        let consistent = h.autoclear_features() & 1 != 0;
        let size = h.size();
        drop(h);
        let Some(ext) = ext else {
            return Ok(Vec::new());
        };
        if ext.len() != 24
            || u32_at(&ext, 0) == 0
            || u32_at(&ext, 0) > 65535
            || u32_at(&ext, 4) != 0
        {
            return Err(invalid("Invalid bitmap extension"));
        }
        let length = u64_at(&ext, 8);
        if length > MAX as u64 {
            return Err(invalid("Bitmap directory exceeds 32 MiB"));
        }
        let start = u64_at(&ext, 16);
        if start == 0 || start % self.info.cluster_size() as u64 != 0 {
            return Err(invalid("Invalid bitmap directory offset"));
        }
        let directory = self.read_bytes(start, length as usize).await?;
        let cs = self.info.cluster_size();
        let mut cursor = 0;
        let mut names = BTreeSet::new();
        let mut maps = Vec::new();
        for _ in 0..u32_at(&ext, 0) {
            let entry = directory
                .get(cursor..cursor + 24)
                .ok_or("Truncated bitmap directory")?;
            let flags = u32_at(entry, 12);
            let granularity = entry[17];
            let bytes = bitmap_bytes(size, granularity)?;
            let namesize = u16_at(entry, 18) as usize;
            let extra = u32_at(entry, 20) as usize;
            let end = cursor
                .checked_add(24)
                .and_then(|n| n.checked_add(namesize))
                .and_then(|n| n.checked_add(extra))
                .ok_or("Bitmap directory overflow")?;
            if end > directory.len() {
                return Err(invalid("Bitmap entry exceeds directory"));
            }
            let padded_end = end.div_ceil(8) * 8;
            if namesize == 0 || namesize > 1023 || flags & !7 != 0 || entry[16] != 1 {
                return Err(invalid("Unsupported bitmap type, flags, or name"));
            }
            let payload = directory
                .get(cursor + 24..end)
                .ok_or("Truncated bitmap name/extra data")?;
            if directory
                .get(end..padded_end)
                .ok_or("Truncated bitmap padding")?
                .iter()
                .any(|b| *b != 0)
            {
                return Err(invalid("Nonzero bitmap directory padding"));
            }
            let name = String::from_utf8(payload[extra..].to_vec())
                .map_err(|_| invalid("Invalid bitmap name"))?;
            if !names.insert(name.clone()) {
                return Err(invalid("Duplicate bitmap name"));
            }
            let table_bytes = (u32_at(entry, 8) as usize)
                .checked_mul(8)
                .ok_or("Bitmap table size overflow")?;
            if table_bytes > MAX || table_bytes / 8 < bytes.div_ceil(cs) {
                return Err(invalid("Invalid bitmap table capacity"));
            }
            let table_offset = u64_at(entry, 0);
            if table_offset % cs as u64 != 0 || table_bytes != 0 && table_offset == 0 {
                return Err(invalid("Invalid bitmap table offset"));
            }
            let table = self.read_bytes(table_offset, table_bytes).await?;
            let mut data = vec![0; bytes];
            for (index, chunk) in data.chunks_mut(cs).enumerate() {
                let value = u64_at(&table, index * 8);
                if value & !(MASK | 1) != 0
                    || value & MASK != 0 && value & 1 != 0
                    || (value & MASK) % cs as u64 != 0
                {
                    return Err(invalid("Invalid bitmap data descriptor"));
                }
                if value == 1 {
                    chunk.fill(0xff);
                } else if value != 0 {
                    chunk.copy_from_slice(&self.read_bytes(value, chunk.len()).await?);
                }
            }
            let mut map = StoredBitmap {
                info: BitmapInfo {
                    name,
                    granularity_bits: granularity,
                    enabled: flags & 2 != 0,
                    in_use: flags & 1 != 0,
                    consistent: consistent && flags & 1 == 0,
                    extra_data: payload[..extra].to_vec(),
                    extra_data_compatible: flags & 4 != 0,
                },
                data,
                directory_entry: start + cursor as u64,
            };
            map.resize(size)?;
            maps.push(map);
            cursor = padded_end;
        }
        if cursor != directory.len() {
            return Err(invalid("Trailing bitmap directory bytes"));
        }
        Ok(maps)
    }

    pub(crate) async fn validate_writable_bitmaps(&self) -> Qcow2Result<bool> {
        let maps = self.load_bitmaps().await?;
        for map in &maps {
            if map.info.enabled
                && (!map.info.consistent
                    || !map.info.extra_data.is_empty() && !map.info.extra_data_compatible)
            {
                return Err(invalid("Enabled bitmap is inconsistent or has incompatible extra data; explicitly clear, disable, or remove it"));
            }
        }
        Ok(!maps.is_empty() && self.header.read().await.autoclear_features() & 1 != 0)
    }

    /// Stage COW bitmap objects; the caller publishes the extension and exact
    /// refcounts together through rebuild_refcounts while holding io_lock.
    pub(crate) async fn stage_bitmaps(&self, maps: &[StoredBitmap]) -> Qcow2Result<()> {
        let cs = self.info.cluster_size();
        let mut directory = Vec::new();
        for map in maps {
            let mut table = Vec::new();
            for chunk in map.data.chunks(cs) {
                let value = if chunk.iter().all(|b| *b == 0) {
                    0
                } else if chunk.len() == cs && chunk.iter().all(|b| *b == 0xff) {
                    1
                } else {
                    self.allocate_object(chunk).await?
                };
                table.extend(value.to_be_bytes());
            }
            let table_offset = self.allocate_object(&table).await?;
            let mut entry = vec![0; 24];
            put64(&mut entry, 0, table_offset);
            entry[8..12].copy_from_slice(&((table.len() / 8) as u32).to_be_bytes());
            let flags = u32::from(map.info.in_use || !map.info.consistent)
                | (u32::from(map.info.enabled) << 1)
                | (u32::from(map.info.extra_data_compatible) << 2);
            entry[12..16].copy_from_slice(&flags.to_be_bytes());
            entry[16] = 1;
            entry[17] = map.info.granularity_bits;
            entry[18..20].copy_from_slice(&(map.info.name.len() as u16).to_be_bytes());
            entry[20..24].copy_from_slice(&(map.info.extra_data.len() as u32).to_be_bytes());
            directory.extend(entry);
            directory.extend(&map.info.extra_data);
            directory.extend(map.info.name.as_bytes());
            directory.resize(directory.len().div_ceil(8) * 8, 0);
            if directory.len() > MAX {
                return Err(invalid("Bitmap directory exceeds 32 MiB"));
            }
        }
        let host = self.allocate_object(&directory).await?;
        let mut extension = vec![0; 24];
        extension[..4].copy_from_slice(&(maps.len() as u32).to_be_bytes());
        put64(&mut extension, 8, directory.len() as u64);
        put64(&mut extension, 16, host);
        let mut h = self.header.write().await;
        h.set_extension(EXTENSION, (!maps.is_empty()).then_some(extension));
        let flags = (h.autoclear_features() & !1) | u64::from(!maps.is_empty());
        h.set_autoclear_features(flags);
        Ok(())
    }

    async fn save_bitmaps(&self, maps: &[StoredBitmap]) -> Qcow2Result<()> {
        let result = async {
            self.stage_bitmaps(maps).await?;
            self.rebuild_refcounts().await
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }

    pub(crate) async fn track_dirty_bitmaps(&self, offset: u64, len: u64) -> Qcow2Result<()> {
        let mut maps = self.load_bitmaps().await?;
        if !maps.iter().any(|m| m.info.enabled) {
            return Ok(());
        }
        self.begin_transaction().await?;
        let result = async {
            for map in &mut maps {
                if !map.info.enabled {
                    continue;
                }
                // If the process stops before replacement metadata is published,
                // the old bitmap explicitly records that it may be incomplete.
                let flags = 1 | 2 | (u32::from(map.info.extra_data_compatible) << 2);
                self.write_bytes(map.directory_entry + 12, &flags.to_be_bytes())
                    .await?;
                map.mark(offset, len, self.info.virtual_size());
            }
            self.call_fsync(0, 0, 0).await?;
            self.save_bitmaps(&maps).await
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }

    pub(crate) async fn stage_bitmap_size_change(
        &self,
        maps: &mut [StoredBitmap],
        size: u64,
    ) -> Qcow2Result<()> {
        for map in maps.iter_mut() {
            map.resize(size)?;
            if map.info.enabled {
                map.mark(0, size, size);
            }
        }
        if !maps.is_empty() {
            self.stage_bitmaps(maps).await?;
        }
        Ok(())
    }

    pub async fn list_bitmaps(&self) -> Qcow2Result<Vec<BitmapInfo>> {
        let _io = self.io_lock.read().await;
        Ok(self
            .load_bitmaps()
            .await?
            .into_iter()
            .map(|m| m.info)
            .collect())
    }
    /// Raw bitmap bytes; bit zero is the least significant bit of byte zero.
    pub async fn read_bitmap(&self, name: &str) -> Qcow2Result<Vec<u8>> {
        let _io = self.io_lock.read().await;
        self.ensure_usable()?;
        let map = self
            .load_bitmaps()
            .await?
            .into_iter()
            .find(|m| m.info.name == name)
            .ok_or("Bitmap not found")?;
        if !map.info.consistent
            || !map.info.extra_data.is_empty() && !map.info.extra_data_compatible
        {
            return Err(invalid("Bitmap contents are inconsistent or incompatible"));
        }
        Ok(map.data)
    }
    pub async fn create_bitmap(
        &self,
        name: &str,
        granularity_bits: u8,
        enabled: bool,
    ) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        if name.is_empty() || name.len() > 1023 {
            return Err(invalid("Invalid bitmap name"));
        }
        if self.header.read().await.version() < 3 {
            return Err(invalid("Persistent bitmaps require QCOW2 v3"));
        }
        let mut maps = self.load_bitmaps().await?;
        if maps.len() >= 65535 || maps.iter().any(|m| m.info.name == name) {
            return Err(invalid("Duplicate or excessive bitmaps"));
        }
        let data = vec![0; bitmap_bytes(self.info.virtual_size(), granularity_bits)?];
        maps.push(StoredBitmap {
            info: BitmapInfo {
                name: name.into(),
                granularity_bits,
                enabled,
                in_use: false,
                consistent: true,
                extra_data: Vec::new(),
                extra_data_compatible: false,
            },
            data,
            directory_entry: 0,
        });
        self.begin_transaction().await?;
        self.save_bitmaps(&maps).await
    }

    pub async fn remove_bitmap(&self, name: &str) -> Qcow2Result<()> {
        self.change_bitmap(name, BitmapChange::Remove).await
    }
    pub async fn clear_bitmap(&self, name: &str) -> Qcow2Result<()> {
        self.change_bitmap(name, BitmapChange::Clear).await
    }
    pub async fn set_bitmap_enabled(&self, name: &str, enabled: bool) -> Qcow2Result<()> {
        self.change_bitmap(name, BitmapChange::Enable(enabled))
            .await
    }
    pub async fn rename_bitmap(&self, name: &str, new_name: &str) -> Qcow2Result<()> {
        self.change_bitmap(name, BitmapChange::Rename(new_name))
            .await
    }
    pub async fn merge_bitmap(&self, name: &str, source: &str) -> Qcow2Result<()> {
        self.change_bitmap(name, BitmapChange::Merge(source)).await
    }

    async fn change_bitmap(&self, name: &str, change: BitmapChange<'_>) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        let mut maps = self.load_bitmaps().await?;
        let index = maps
            .iter()
            .position(|m| m.info.name == name)
            .ok_or("Bitmap not found")?;
        if !matches!(change, BitmapChange::Remove | BitmapChange::Enable(false))
            && !maps[index].info.extra_data.is_empty()
            && !maps[index].info.extra_data_compatible
        {
            return Err(invalid("Bitmap has incompatible extra data"));
        }
        match change {
            BitmapChange::Remove => {
                maps.remove(index);
            }
            BitmapChange::Clear => {
                maps[index].data.fill(0);
                maps[index].info.in_use = false;
                maps[index].info.consistent = true;
            }
            BitmapChange::Enable(enabled) => {
                if enabled && !maps[index].info.consistent {
                    return Err(invalid("Clear inconsistent bitmap before enabling it"));
                }
                maps[index].info.enabled = enabled;
            }
            BitmapChange::Rename(new) => {
                if new.is_empty() || new.len() > 1023 || maps.iter().any(|m| m.info.name == new) {
                    return Err(invalid("Invalid or duplicate bitmap name"));
                }
                maps[index].info.name = new.into();
            }
            BitmapChange::Merge(source) => {
                let other = maps
                    .iter()
                    .find(|m| m.info.name == source)
                    .ok_or("Source bitmap not found")?
                    .clone();
                if !other.info.consistent
                    || !maps[index].info.consistent
                    || !other.info.extra_data.is_empty() && !other.info.extra_data_compatible
                {
                    return Err(invalid("Cannot merge inconsistent or incompatible bitmaps"));
                }
                let unit = 1_u64 << other.info.granularity_bits;
                for (byte, value) in other.data.iter().enumerate() {
                    for bit in 0..8 {
                        if value & (1 << bit) != 0 {
                            maps[index].mark(
                                (byte as u64 * 8 + bit) * unit,
                                unit,
                                self.info.virtual_size(),
                            );
                        }
                    }
                }
            }
        }
        // Explicit management must remain available to remove/disable an
        // inconsistent enabled bitmap. Its directory is validated above.
        self.ensure_usable()?;
        if self.info.is_read_only() {
            return Err(invalid("Read-only image"));
        }
        self.flush_meta_locked().await?;
        if self.graph().await?.counts != self.actual_refcounts().await? {
            return Err(invalid("Repair refcounts before bitmap management"));
        }
        let mut h = self.header.write().await;
        let old = h.autoclear_features();
        let preserve = old & (1 | if self.info.raw_data() { 2 } else { 0 });
        h.set_autoclear_features(preserve);
        self.commit_header(&mut h, |h| h.set_autoclear_features(old))
            .await?;
        drop(h);
        self.mark_image_dirty().await?;
        self.save_bitmaps(&maps).await
    }
}
enum BitmapChange<'a> {
    Remove,
    Clear,
    Enable(bool),
    Rename(&'a str),
    Merge(&'a str),
}
