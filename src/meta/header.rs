use super::{RefBlock, RefTable, RefTableEntry, Table, TableEntry};
use crate::dev::Qcow2Info;
use crate::error::Qcow2Result;
use crate::helpers::IntAlignment;
use crate::numerical_enum;
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::mem::size_of;

#[derive(Debug, Default, Deserialize, Serialize)]
#[repr(Rust, packed)]
pub(crate) struct Qcow2RawHeader {
    /// QCOW magic string ("QFI\xfb")
    magic: u32,

    /// Version number (valid values are 2 and 3)
    version: u32,

    /// Offset into the image file at which the backing file name
    /// is stored (NB: The string is not null terminated). 0 if the
    /// image doesn't have a backing file.
    ///
    /// Note: backing files are incompatible with raw external data
    /// files (auto-clear feature bit 1).
    backing_file_offset: u64,

    /// Length of the backing file name in bytes. Must not be
    /// longer than 1023 bytes. Undefined if the image doesn't have
    /// a backing file.
    backing_file_size: u32,

    /// Number of bits that are used for addressing an offset
    /// within a cluster (1 << cluster_bits is the cluster size).
    /// Must not be less than 9 (i.e. 512 byte clusters).
    ///
    /// Note: qemu as of today has an implementation limit of 2 MB
    /// as the maximum cluster size and won't be able to open images
    /// with larger cluster sizes.
    ///
    /// Note: if the image has Extended L2 Entries then cluster_bits
    /// must be at least 14 (i.e. 16384 byte clusters).
    cluster_bits: u32,

    /// Virtual disk size in bytes.
    ///
    /// Note: qemu has an implementation limit of 32 MB as
    /// the maximum L1 table size.  With a 2 MB cluster
    /// size, it is unable to populate a virtual cluster
    /// beyond 2 EB (61 bits); with a 512 byte cluster
    /// size, it is unable to populate a virtual size
    /// larger than 128 GB (37 bits).  Meanwhile, L1/L2
    /// table layouts limit an image to no more than 64 PB
    /// (56 bits) of populated clusters, and an image may
    /// hit other limits first (such as a file system's
    /// maximum size).
    size: u64,

    /// 0 for no encryption
    /// 1 for AES encryption
    /// 2 for LUKS encryption
    crypt_method: u32,

    /// Number of entries in the active L1 table
    l1_size: u32,

    /// Offset into the image file at which the active L1 table
    /// starts. Must be aligned to a cluster boundary.
    l1_table_offset: u64,

    /// Offset into the image file at which the refcount table
    /// starts. Must be aligned to a cluster boundary.
    refcount_table_offset: u64,

    /// Number of clusters that the refcount table occupies
    refcount_table_clusters: u32,

    /// Number of snapshots contained in the image
    nb_snapshots: u32,

    /// Offset into the image file at which the snapshot table
    /// starts. Must be aligned to a cluster boundary.
    snapshots_offset: u64,

    // The following fields are only valid for version >= 3
    /// Bitmask of incompatible features. An implementation must
    /// fail to open an image if an unknown bit is set.
    ///
    /// Bit 0:      Dirty bit.  If this bit is set then refcounts
    /// may be inconsistent, make sure to scan L1/L2
    /// tables to repair refcounts before accessing the
    /// image.
    ///
    /// Bit 1:      Corrupt bit.  If this bit is set then any data
    /// structure may be corrupt and the image must not
    /// be written to (unless for regaining
    /// consistency).
    ///
    /// Bit 2:      External data file bit.  If this bit is set, an
    /// external data file is used. Guest clusters are
    /// then stored in the external data file. For such
    /// images, clusters in the external data file are
    /// not refcounted. The offset field in the
    /// Standard Cluster Descriptor must match the
    /// guest offset and neither compressed clusters
    /// nor internal snapshots are supported.
    ///
    /// An External Data File Name header extension may
    /// be present if this bit is set.
    ///
    /// Bit 3:      Compression type bit.  If this bit is set,
    /// a non-default compression is used for compressed
    /// clusters. The compression_type field must be
    /// present and not zero.
    ///
    /// Bit 4:      Extended L2 Entries.  If this bit is set then
    /// L2 table entries use an extended format that
    /// allows subcluster-based allocation. See the
    /// Extended L2 Entries section for more details.
    ///
    /// Bits 5-63:  Reserved (set to 0)
    incompatible_features: u64,

    /// Bitmask of compatible features. An implementation can
    /// safely ignore any unknown bits that are set.
    ///
    /// Bit 0:      Lazy refcounts bit.  If this bit is set then
    /// lazy refcount updates can be used.  This means
    /// marking the image file dirty and postponing
    /// refcount metadata updates.
    ///
    /// Bits 1-63:  Reserved (set to 0)
    compatible_features: u64,

    /// Bitmask of auto-clear features. An implementation may only
    /// write to an image with unknown auto-clear features if it
    /// clears the respective bits from this field first.
    ///
    /// Bit 0:      Bitmaps extension bit
    /// This bit indicates consistency for the bitmaps
    /// extension data.
    ///
    /// It is an error if this bit is set without the
    /// bitmaps extension present.
    ///
    /// If the bitmaps extension is present but this
    /// bit is unset, the bitmaps extension data must be
    /// considered inconsistent.
    ///
    /// Bit 1:      Raw external data bit
    /// If this bit is set, the external data file can
    /// be read as a consistent standalone raw image
    /// without looking at the qcow2 metadata.
    ///
    /// Setting this bit has a performance impact for
    /// some operations on the image (e.g. writing
    /// zeros requires writing to the data file instead
    /// of only setting the zero flag in the L2 table
    /// entry) and conflicts with backing files.
    ///
    /// This bit may only be set if the External Data
    /// File bit (incompatible feature bit 1) is also
    /// set.
    ///
    /// Bits 2-63:  Reserved (set to 0)
    autoclear_features: u64,

    /// Describes the width of a reference count block entry (width
    /// in bits: refcount_bits = 1 << refcount_order). For version 2
    /// images, the order is always assumed to be 4
    /// (i.e. refcount_bits = 16).
    /// This value may not exceed 6 (i.e. refcount_bits = 64).
    refcount_order: u32,

    /// Length of the header structure in bytes. For version 2
    /// images, the length is always assumed to be 72 bytes.
    /// For version 3 it's at least 104 bytes and must be a multiple
    /// of 8.
    header_length: u32,

    /// Additional fields
    compression_type: u8,
}

numerical_enum! {
    pub enum Qcow2HeaderExtensionType as u32 {
        End = 0,
        BackingFileFormat = 0xe2792aca,
        FeatureNameTable = 0x6803f857,
    }
}

impl Qcow2RawHeader {
    pub fn serialize_vec(&mut self) -> Qcow2Result<Vec<u8>> {
        if self.version == 2 {
            self.header_length = 72;
        }

        let bincode = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_big_endian();

        let mut header_buf = bincode.serialize(self)?;
        header_buf.resize(self.header_length as usize, 0);

        assert!(header_buf.len() == self.header_length as usize);

        Ok(header_buf)
    }
}

#[derive(Default, Deserialize, Serialize)]
#[repr(Rust, packed)]
struct Qcow2HeaderExtensionHeader {
    /// Type code of the header extension
    extension_type: u32,

    /// Data length
    length: u32,
}

numerical_enum! {
    #[derive(Hash)]
    pub enum Qcow2FeatureType as u8 {
        Incompatible = 0,
        Compatible = 1,
        Autoclear = 2,
    }
}

#[derive(Debug, Clone)]
pub enum Qcow2HeaderExtension {
    BackingFileFormat(String),
    FeatureNameTable(HashMap<(Qcow2FeatureType, u8), String>),
    Unknown { extension_type: u32, data: Vec<u8> },
}

#[derive(Debug)]
pub struct Qcow2Header {
    raw: Qcow2RawHeader,
    // Preserve optional fields introduced after the last field we understand.
    unknown_header_fields: Vec<u8>,
    backing_filename: Option<String>,
    extensions: Vec<Qcow2HeaderExtension>,
}

impl Qcow2Header {
    pub const QCOW2_MAGIC: u32 = 0x51_46_49_fb;
    pub const MAX_CLUSTER_SIZE: u32 = 2_u32 << 20;
    pub const MAX_L1_SIZE: u32 = 32_u32 << 20;
    pub const MAX_REFCOUNT_TABLE_SIZE: u32 = 8_u32 << 20;

    pub fn from_buf(header_buf: &[u8]) -> Qcow2Result<Self> {
        let bincode = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_big_endian();

        let prefix = header_buf.get(..8).ok_or("Truncated qcow2 header")?;
        let (magic, version): (u32, u32) = bincode.deserialize(prefix)?;
        if magic != Self::QCOW2_MAGIC {
            return Err("Not a qcow2 file".into());
        }

        if !matches!(version, 2 | 3) {
            return Err(format!("qcow2 v{version} is not supported").into());
        }

        // Deserialize only fields present in this version. In v2 the bytes
        // after offset 72 can already contain extensions or a backing name.
        let required = if version == 2 { 72 } else { 104 };
        let base = header_buf.get(..required).ok_or("Truncated qcow2 header")?;
        let mut raw_buf = vec![0; size_of::<Qcow2RawHeader>()];
        raw_buf[..required].copy_from_slice(base);
        let mut header: Qcow2RawHeader = bincode.deserialize(&raw_buf)?;
        if version == 2 {
            header.refcount_order = 4;
            header.header_length = 72;
        } else {
            let length = header.header_length as usize;
            if length < 104 || length & 7 != 0 || length > header_buf.len() {
                return Err("Invalid qcow2 header length".into());
            }
            if length > 104 {
                header.compression_type = header_buf[104];
            }
            if header.refcount_order > 6 {
                return Err("Invalid qcow2 refcount order".into());
            }
        }

        let cluster_bits = header.cluster_bits;
        if !(9..=30).contains(&cluster_bits) {
            return Err(format!("qcow2 cluster_bits {cluster_bits} is invalid").into());
        }

        let cluster_size = 1u64 << cluster_bits;
        if cluster_size > Self::MAX_CLUSTER_SIZE as u64 {
            return Err(format!("qcow2 cluster size {cluster_size} is too big").into());
        }
        if header.header_length as u64 > cluster_size {
            return Err("qcow2 header exceeds the first cluster".into());
        }
        let l1_bytes = u64::from(header.l1_size) * 8;
        let l2_width = if header.incompatible_features & 16 != 0 {
            16
        } else {
            8
        };
        if l2_width == 16 && cluster_bits < 14 {
            return Err("Extended L2 requires at least 16 KiB clusters".into());
        }
        let max_virtual =
            u64::from(Self::MAX_L1_SIZE / 8) * (cluster_size / l2_width) * cluster_size;
        if l1_bytes > u64::from(Self::MAX_L1_SIZE) || header.size > max_virtual {
            return Err("qcow2 L1 table or virtual size exceeds the supported limit".into());
        }
        if header
            .size
            .div_ceil((cluster_size / l2_width) * cluster_size)
            > u64::from(header.l1_size)
        {
            return Err("qcow2 L1 table is too small for the virtual size".into());
        }
        let rt_bytes = u64::from(header.refcount_table_clusters) * cluster_size;
        if rt_bytes == 0 || rt_bytes > u64::from(Self::MAX_REFCOUNT_TABLE_SIZE) {
            return Err("Invalid qcow2 refcount table size".into());
        }
        for (offset, bytes, name) in [
            (
                header.l1_table_offset,
                l1_bytes.div_ceil(cluster_size) * cluster_size,
                "L1",
            ),
            (header.refcount_table_offset, rt_bytes, "refcount"),
        ] {
            if bytes != 0
                && (offset < cluster_size
                    || offset
                        .checked_add(bytes)
                        .map(|end| end > 1_u64 << 56)
                        .unwrap_or(true))
            {
                return Err(format!("Invalid qcow2 {name} table range").into());
            }
        }
        if l1_bytes != 0 {
            let l1_end = header.l1_table_offset + l1_bytes.div_ceil(cluster_size) * cluster_size;
            let rt_end = header.refcount_table_offset + rt_bytes;
            if header.l1_table_offset < rt_end && header.refcount_table_offset < l1_end {
                return Err("qcow2 L1 and refcount tables overlap".into());
            }
        }
        if header.nb_snapshots != 0
            && (header.snapshots_offset < cluster_size
                || header.snapshots_offset & (cluster_size - 1) != 0
                || header
                    .snapshots_offset
                    .checked_add(u64::from(header.nb_snapshots) * 40)
                    .map(|end| end > 1_u64 << 56)
                    .unwrap_or(true))
        {
            return Err("Invalid qcow2 snapshot table range".into());
        }
        if header.incompatible_features & (1 << 3) == 0 && header.compression_type != 0 {
            return Err("qcow2 compression type requires its incompatible feature bit".into());
        }

        // the spec requires both tables to start at a cluster boundary,
        // and the cluster walk in check() relies on it
        let l1_table_offset = header.l1_table_offset;
        if l1_table_offset & (cluster_size - 1) != 0 {
            return Err(format!(
                "qcow2 L1 table offset {l1_table_offset:#x} is not cluster aligned"
            )
            .into());
        }

        let reftable_offset = header.refcount_table_offset;
        if reftable_offset & (cluster_size - 1) != 0 {
            return Err(format!(
                "qcow2 refcount table offset {reftable_offset:#x} is not cluster aligned"
            )
            .into());
        }

        let backing_filename = if header.backing_file_offset != 0 {
            let (offset, length) = (header.backing_file_offset, header.backing_file_size);
            if offset < header.header_length as u64 {
                return Err("Backing file name overlaps the qcow2 header".into());
            }
            if length == 0 || length > 1023 {
                return Err(format!(
                    "Backing file name is too long ({length}, must not exceed 1023)"
                )
                .into());
            }

            let end = offset
                .checked_add(length as u64)
                .ok_or("Backing file name offset is invalid (too high)")?;
            if end > cluster_size {
                return Err("Backing file name offset is invalid (too high)".into());
            }

            if end > header_buf.len() as u64 {
                return Err("header buffer is too small".into());
            }

            let backing_buf = header_buf[(end - (length as u64)) as usize..(end as usize)].to_vec();
            Some(
                String::from_utf8(backing_buf)
                    .map_err(|err| format!("Backing file name is invalid: {err}"))?,
            )
        } else {
            None
        };

        let mut ext_offset: u64 = header.header_length as u64;
        let ext_end = if header.backing_file_offset != 0 {
            header.backing_file_offset.min(cluster_size)
        } else {
            cluster_size
        };
        let mut extensions = Vec::<Qcow2HeaderExtension>::new();
        let mut extension_types = HashSet::new();
        while ext_offset < ext_end {
            let max_len = ext_offset + size_of::<Qcow2HeaderExtensionHeader>() as u64;
            if max_len > ext_end || max_len > header_buf.len() as u64 {
                return Err(
                    "Header extensions exceed the first cluster or header buffer is too small"
                        .into(),
                );
            }

            let ext_hdr_buf = &header_buf[ext_offset as usize
                ..ext_offset as usize + size_of::<Qcow2HeaderExtensionHeader>()];

            ext_offset += size_of::<Qcow2HeaderExtensionHeader>() as u64;

            let ext_hdr: Qcow2HeaderExtensionHeader = bincode.deserialize(ext_hdr_buf)?;
            let extension_type = ext_hdr.extension_type;
            if extension_type == Qcow2HeaderExtensionType::End as u32 && ext_hdr.length != 0 {
                return Err("qcow2 extension end marker has a payload".into());
            }
            if !extension_types.insert(extension_type) {
                return Err(format!("Duplicate qcow2 header extension {extension_type:#x}").into());
            }
            let max_len = ext_offset + ext_hdr.length as u64;
            if max_len > ext_end || max_len > header_buf.len() as u64 {
                return Err("Header extensions exceed the first cluster or buffer length".into());
            }

            let ext_data = header_buf
                [ext_offset as usize..ext_offset as usize + ext_hdr.length as usize]
                .to_vec();
            ext_offset += (ext_hdr.length as u64).align_up(8u64).unwrap();
            if ext_offset > ext_end || ext_offset > header_buf.len() as u64 {
                return Err("qcow2 extension padding exceeds the extension area".into());
            }

            let extension = match Qcow2HeaderExtension::from(ext_hdr.extension_type, ext_data)? {
                Some(ext) => ext,
                None => break,
            };

            extensions.push(extension);
        }

        let unknown_header_fields = if version == 3 && header.header_length > 105 {
            header_buf[105..header.header_length as usize].to_vec()
        } else {
            Vec::new()
        };
        let header = Qcow2Header {
            raw: header,
            unknown_header_fields,
            backing_filename,
            extensions,
        };
        if (header.crypt_method() == 2) != header.extension_data(0x0537be77).is_some() {
            return Err("qcow2 encryption header pointer must be present only for LUKS".into());
        }
        if header.raw.autoclear_features & 1 != 0
            && !header.extensions.iter().any(|extension| {
                matches!(
                    extension,
                    Qcow2HeaderExtension::Unknown {
                        extension_type: 0x23852875,
                        ..
                    }
                )
            })
        {
            return Err("qcow2 bitmap consistency flag has no bitmap extension".into());
        }
        if header.raw.autoclear_features & 2 != 0 && header.raw.incompatible_features & 4 == 0 {
            return Err("qcow2 raw data flag requires an external data file".into());
        }

        if header.raw.incompatible_features & 4 != 0
            && (header.nb_snapshots() != 0
                || header.raw.autoclear_features & 2 != 0 && header.backing_filename().is_some())
        {
            return Err("External data conflicts with snapshots or raw data backing files".into());
        }
        if header.raw.incompatible_features & 8 != 0
            && (header.header_length() < 112 || header.compression_type() != 1)
        {
            return Err("Unsupported qcow2 compression type (expected Zstd)".into());
        }
        if header.raw.incompatible_features & !31 != 0 {
            let feats = (0..64)
                .filter(|bit| header.raw.incompatible_features & (1u64 << bit) != 0)
                .map(|bit| {
                    if let Some(name) = header.feature_name(Qcow2FeatureType::Incompatible, bit) {
                        format!("{bit} ({name})")
                    } else {
                        format!("{bit}")
                    }
                })
                .collect::<Vec<String>>();

            return Err(
                format!("Unrecognized incompatible feature(s) {}", feats.join(", ")).into(),
            );
        }

        Ok(header)
    }

    pub fn calculate_meta_params(
        size: u64,
        cluster_bits: usize,
        refcount_order: u8,
        block_size: usize,
    ) -> ((u64, u32), (u64, u32), (u64, u32)) {
        let cluster_size = 1usize << cluster_bits;

        // cluster 0 is for header, refcount_table starts from 1st cluster
        let rc_table_offset = cluster_size as u64;
        let rc_table_size =
            Qcow2Info::__max_refcount_table_size(size, cluster_size, refcount_order, block_size);
        let rc_table_clusters = rc_table_size.div_ceil(cluster_size);

        let rc_block_offset = rc_table_offset + ((rc_table_clusters as u64) << cluster_bits);
        let rc_block_clusters = 1;

        let l1_table_offset = rc_block_offset + cluster_size as u64;
        let l1_table_entries = Qcow2Info::get_max_l1_entries(size, cluster_bits);
        let l1_table_size = Qcow2Info::__max_l1_size(l1_table_entries, block_size);
        let l1_table_clusters = l1_table_size.div_ceil(cluster_size);

        let rc_table = (rc_table_offset, rc_table_clusters as u32);
        let rc_block = (rc_block_offset, rc_block_clusters);
        let l1_table = (l1_table_offset, l1_table_clusters as u32);

        (rc_table, rc_block, l1_table)
    }
    /// Format in-ram qcow2 image, for test purpose
    pub fn format_qcow2(
        buf: &mut [u8],
        size: u64,
        cluster_bits: usize,
        refcount_order: u8,
        block_size: usize,
    ) -> Qcow2Result<()> {
        let cluster_size = 1usize << cluster_bits;

        if buf.len() & (block_size - 1) != 0 {
            return Err("buffer isn't cluster aligned".into());
        }

        let (rc_table, rc_blk, l1_table) =
            Self::calculate_meta_params(size, cluster_bits, refcount_order, block_size);

        // don't take l1 table into account
        let clusters = 1 + rc_table.1 + rc_blk.1;
        if (buf.len() / cluster_size) < clusters as usize {
            return Err("buffer is too small".into());
        }

        let start = rc_table.0 as usize;
        let end = start + ((rc_table.1 as usize) << cluster_bits);
        let mut rc_t = RefTable::new_empty(Some(rc_table.0), end - start);

        let start = rc_blk.0 as usize;
        let end = start + ((rc_blk.1 as usize) << cluster_bits);
        let mut ref_b = RefBlock::new(refcount_order, end - start, Some(rc_blk.0));

        //header
        ref_b.increment(0)?;
        assert!(ref_b.get(0).into_plain() == 1);

        //refcount table
        let start = rc_table.0;
        let end = start + ((rc_table.1 as u64) << cluster_bits);
        for i in (start..end).step_by(cluster_size) {
            ref_b.increment((i >> cluster_bits) as usize)?;
        }

        //me
        ref_b.increment((rc_table.1 as usize) + 1)?;

        //l1 table
        let start = l1_table.0;
        let end = start + ((l1_table.1 as u64) << cluster_bits);
        for i in (start..end).step_by(cluster_size) {
            ref_b.increment((i >> cluster_bits) as usize)?;
        }

        rc_t.set(0, RefTableEntry(rc_blk.0));

        // commit meta into external buffer
        let buf_start = buf.as_mut_ptr() as u64;
        unsafe {
            libc::memcpy(
                (buf_start + rc_table.0) as *mut libc::c_void,
                rc_t.as_ptr() as *const libc::c_void,
                (rc_table.1 as usize) << cluster_bits,
            );
        }
        unsafe {
            libc::memcpy(
                (buf_start + rc_blk.0) as *mut libc::c_void,
                ref_b.as_ptr() as *const libc::c_void,
                (rc_blk.1 as usize) << cluster_bits,
            );
        }

        // We are empty image, so nothing is in l1 table, just zero the
        // 1st sector
        unsafe {
            libc::memset((buf_start + l1_table.0) as *mut libc::c_void, 0, block_size);
        }

        let l2_entries = (cluster_size as u64) / 8;
        let size_per_l1_entry = l2_entries << cluster_bits;
        let l1_entries = size.div_ceil(size_per_l1_entry);

        let mut h = Qcow2RawHeader {
            magic: Self::QCOW2_MAGIC,
            version: 3,
            cluster_bits: cluster_bits as u32,
            size,
            refcount_order: refcount_order as u32,
            header_length: 112,
            l1_table_offset: l1_table.0,
            l1_size: l1_entries.try_into().unwrap(),
            refcount_table_offset: rc_table.0,
            refcount_table_clusters: rc_table.1,
            ..Default::default()
        };

        let vec = h.serialize_vec()?;
        buf[..vec.len()].copy_from_slice(vec.as_slice());

        Ok(())
    }

    pub fn serialize_to_buf(&mut self) -> Qcow2Result<Vec<u8>> {
        let header_len = self.raw.header_length as usize;
        let mut header_exts = self.serialize_extensions()?;

        if let Some(backing) = self.backing_filename.as_ref() {
            self.raw.backing_file_offset = (header_len + header_exts.len()).try_into()?;
            self.raw.backing_file_size = backing.len().try_into()?;
        } else {
            self.raw.backing_file_offset = 0;
            self.raw.backing_file_size = 0;
        }

        let mut full_buf = self.raw.serialize_vec()?;
        if !self.unknown_header_fields.is_empty() {
            full_buf[105..header_len].copy_from_slice(&self.unknown_header_fields);
        }
        full_buf.append(&mut header_exts);
        if let Some(backing) = self.backing_filename.as_ref() {
            full_buf.extend_from_slice(backing.as_bytes());
        }

        if full_buf.len() > 1 << self.raw.cluster_bits {
            return Err(format!(
                "Header is too big to write ({}, larger than a cluster ({}))",
                full_buf.len(),
                1 << self.raw.cluster_bits
            )
            .into());
        }

        Ok(full_buf)
    }

    pub fn version(&self) -> u32 {
        self.raw.version
    }

    pub(crate) fn set_crypt_method(&mut self, method: u32) {
        self.raw.crypt_method = method;
    }
    pub(crate) fn set_extension(&mut self, kind: u32, data: Option<Vec<u8>>) {
        self.extensions.retain(|e| e.extension_type() != kind);
        if let Some(data) = data {
            self.extensions.push(Qcow2HeaderExtension::Unknown {
                extension_type: kind,
                data,
            });
        }
    }
    pub fn crypt_method(&self) -> u32 {
        self.raw.crypt_method
    }

    pub fn compression_type(&self) -> u8 {
        self.raw.compression_type
    }

    pub fn incompatible_features(&self) -> u64 {
        self.raw.incompatible_features
    }
    pub fn compatible_features(&self) -> u64 {
        self.raw.compatible_features
    }
    pub(crate) fn set_incompatible_features(&mut self, flags: u64) {
        self.raw.incompatible_features = flags;
    }
    pub(crate) fn set_snapshot_table(&mut self, offset: u64, count: u32) {
        self.raw.snapshots_offset = offset;
        self.raw.nb_snapshots = count;
    }
    pub(crate) fn set_size(&mut self, size: u64) {
        self.raw.size = size;
    }
    pub(crate) fn extension_data(&self, kind: u32) -> Option<Vec<u8>> {
        self.extensions.iter().find_map(|e| match e {
            Qcow2HeaderExtension::Unknown {
                extension_type,
                data,
            } if *extension_type == kind => Some(data.clone()),
            _ => None,
        })
    }

    pub fn autoclear_features(&self) -> u64 {
        self.raw.autoclear_features
    }

    pub(crate) fn set_autoclear_features(&mut self, flags: u64) {
        self.raw.autoclear_features = flags;
    }

    pub fn header_length(&self) -> u32 {
        self.raw.header_length
    }

    pub fn size(&self) -> u64 {
        self.raw.size
    }

    pub fn cluster_bits(&self) -> u32 {
        self.raw.cluster_bits
    }

    pub fn refcount_order(&self) -> u32 {
        self.raw.refcount_order
    }

    pub fn l1_table_offset(&self) -> u64 {
        self.raw.l1_table_offset
    }

    pub fn l1_table_entries(&self) -> usize {
        self.raw.l1_size as usize
    }

    pub fn set_l1_table(&mut self, offset: u64, entries: usize) -> Qcow2Result<()> {
        self.raw.l1_size = entries.try_into()?;
        self.raw.l1_table_offset = offset;
        Ok(())
    }

    pub fn nb_snapshots(&self) -> u32 {
        self.raw.nb_snapshots
    }

    pub fn snapshots_offset(&self) -> u64 {
        self.raw.snapshots_offset
    }

    pub fn reftable_offset(&self) -> u64 {
        self.raw.refcount_table_offset
    }

    pub fn reftable_clusters(&self) -> usize {
        self.raw.refcount_table_clusters as usize
    }

    pub fn set_reftable(&mut self, offset: u64, clusters: usize) -> Qcow2Result<()> {
        self.raw.refcount_table_clusters = clusters.try_into()?;
        self.raw.refcount_table_offset = offset;
        Ok(())
    }

    pub fn backing_filename(&self) -> Option<&String> {
        self.backing_filename.as_ref()
    }

    pub fn backing_format(&self) -> Option<&String> {
        for e in &self.extensions {
            if let Qcow2HeaderExtension::BackingFileFormat(fmt) = e {
                return Some(fmt);
            }
        }

        None
    }

    pub fn feature_name(&self, feat_type: Qcow2FeatureType, bit: u32) -> Option<&String> {
        for e in &self.extensions {
            if let Qcow2HeaderExtension::FeatureNameTable(names) = e {
                if let Some(name) = names.get(&(feat_type, bit as u8)) {
                    return Some(name);
                }
            }
        }

        None
    }

    fn serialize_extensions(&self) -> Qcow2Result<Vec<u8>> {
        let bincode = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_big_endian();

        let mut result = Vec::new();
        for e in &self.extensions {
            let mut data = e.serialize_data()?;
            let ext_hdr = Qcow2HeaderExtensionHeader {
                extension_type: e.extension_type(),
                length: data.len().try_into()?,
            };
            result.append(&mut bincode.serialize(&ext_hdr)?);
            result.append(&mut data);
            result.resize(result.len().align_up(8usize).unwrap(), 0);
        }

        let end_ext = Qcow2HeaderExtensionHeader {
            extension_type: Qcow2HeaderExtensionType::End as u32,
            length: 0,
        };
        result.append(&mut bincode.serialize(&end_ext)?);
        result.resize(result.len().align_up(8usize).unwrap(), 0);

        Ok(result)
    }
}

impl Qcow2HeaderExtension {
    /// Parse an extension from its type and data.  Unrecognized types are stored as `Unknown`
    /// extensions, encountering the end of extensions returns `Ok(None)`.
    fn from(ext_type: u32, data: Vec<u8>) -> Qcow2Result<Option<Self>> {
        let ext = if let Ok(ext_type) = Qcow2HeaderExtensionType::try_from(ext_type) {
            match ext_type {
                Qcow2HeaderExtensionType::End => return Ok(None),
                Qcow2HeaderExtensionType::BackingFileFormat => {
                    let fmt = String::from_utf8(data)
                        .map_err(|err| format!("Invalid backing file format: {err}"))?;
                    Qcow2HeaderExtension::BackingFileFormat(fmt)
                }
                Qcow2HeaderExtensionType::FeatureNameTable => {
                    if !data.chunks_exact(48).remainder().is_empty() {
                        return Err("Invalid qcow2 feature name table length".into());
                    }
                    let mut feats = HashMap::new();
                    for feat in data.chunks(48) {
                        let feat_type: Qcow2FeatureType = match feat[0].try_into() {
                            Ok(ft) => ft,
                            Err(_) => continue, // skip unrecognized entries
                        };
                        let feat_name = String::from(
                            String::from_utf8_lossy(&feat[2..]).trim_end_matches('\0'),
                        );

                        feats.insert((feat_type, feat[1]), feat_name);
                    }
                    Qcow2HeaderExtension::FeatureNameTable(feats)
                }
            }
        } else {
            Qcow2HeaderExtension::Unknown {
                extension_type: ext_type,
                data,
            }
        };

        Ok(Some(ext))
    }

    fn extension_type(&self) -> u32 {
        match self {
            Qcow2HeaderExtension::BackingFileFormat(_) => {
                Qcow2HeaderExtensionType::BackingFileFormat as u32
            }
            Qcow2HeaderExtension::FeatureNameTable(_) => {
                Qcow2HeaderExtensionType::FeatureNameTable as u32
            }
            Qcow2HeaderExtension::Unknown {
                extension_type,
                data: _,
            } => *extension_type,
        }
    }

    fn serialize_data(&self) -> Qcow2Result<Vec<u8>> {
        match self {
            Qcow2HeaderExtension::BackingFileFormat(fmt) => Ok(fmt.as_bytes().into()),
            Qcow2HeaderExtension::FeatureNameTable(map) => {
                let mut result = Vec::new();
                for (bit, name) in map {
                    result.push(bit.0 as u8);
                    result.push(bit.1);

                    let mut padded_name = vec![0; 46];
                    let name_bytes = name.as_bytes();
                    // Might truncate in the middle of a multibyte character, but getting that
                    // right is complicated and probably not worth it
                    let truncated_len = std::cmp::min(name_bytes.len(), 46);
                    padded_name[..truncated_len].copy_from_slice(&name_bytes[..truncated_len]);
                    result.extend_from_slice(&padded_name);
                }
                Ok(result)
            }
            Qcow2HeaderExtension::Unknown {
                extension_type: _,
                data,
            } => Ok(data.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::*;

    fn minimal_header(version: u32, length: u32) -> Vec<u8> {
        let mut buf = vec![0; 512];
        buf[..4].copy_from_slice(&Qcow2Header::QCOW2_MAGIC.to_be_bytes());
        buf[4..8].copy_from_slice(&version.to_be_bytes());
        buf[20..24].copy_from_slice(&9_u32.to_be_bytes());
        buf[48..56].copy_from_slice(&512_u64.to_be_bytes());
        buf[56..60].copy_from_slice(&1_u32.to_be_bytes());
        if version == 3 {
            buf[96..100].copy_from_slice(&4_u32.to_be_bytes());
            buf[100..104].copy_from_slice(&length.to_be_bytes());
        }
        buf
    }

    #[test]
    fn test_version_specific_headers() {
        for version in [2, 3] {
            let buf = minimal_header(version, 104);
            let header = Qcow2Header::from_buf(&buf).unwrap();
            assert_eq!(header.version(), version);
            assert_eq!(header.refcount_order(), 4);
            assert_eq!(header.header_length(), if version == 2 { 72 } else { 104 });
        }

        // A v2 backing name can immediately follow the 72-byte header.
        let mut buf = minimal_header(2, 0);
        buf[8..16].copy_from_slice(&72_u64.to_be_bytes());
        buf[16..20].copy_from_slice(&8_u32.to_be_bytes());
        buf[72..80].copy_from_slice(b"base.raw");
        let mut header = Qcow2Header::from_buf(&buf).unwrap();
        assert_eq!(header.backing_filename().unwrap(), "base.raw");
        let serialized = header.serialize_to_buf().unwrap();
        let header = Qcow2Header::from_buf(&serialized).unwrap();
        assert_eq!(header.header_length(), 72);
        assert_eq!(header.backing_filename().unwrap(), "base.raw");
    }

    #[test]
    fn test_invalid_header_layouts() {
        let buf = minimal_header(3, 104);
        for length in 0..104 {
            assert!(Qcow2Header::from_buf(&buf[..length]).is_err());
        }
        for version in [0, 1, 4, u32::MAX] {
            assert!(Qcow2Header::from_buf(&minimal_header(version, 104)).is_err());
        }
        for length in [0, 72, 103, 105, 520, u32::MAX] {
            assert!(Qcow2Header::from_buf(&minimal_header(3, length)).is_err());
        }
        let mut buf = minimal_header(3, 104);
        buf[96..100].copy_from_slice(&7_u32.to_be_bytes());
        assert!(Qcow2Header::from_buf(&buf).is_err());
    }

    #[test]
    fn test_malformed_header_extensions_return_errors() {
        for length in [1_u32, 2, 47, 49] {
            let mut buf = minimal_header(3, 104);
            buf[104..108].copy_from_slice(&0x6803f857_u32.to_be_bytes());
            buf[108..112].copy_from_slice(&length.to_be_bytes());
            assert!(Qcow2Header::from_buf(&buf).is_err());
        }

        // End markers cannot have a payload.
        let mut buf = minimal_header(3, 104);
        buf[108..112].copy_from_slice(&1_u32.to_be_bytes());
        assert!(Qcow2Header::from_buf(&buf).is_err());

        // Extension padding must not run into a backing name.
        let mut buf = minimal_header(3, 104);
        buf[8..16].copy_from_slice(&115_u64.to_be_bytes());
        buf[16..20].copy_from_slice(&1_u32.to_be_bytes());
        buf[104..108].copy_from_slice(&0xe2792aca_u32.to_be_bytes());
        buf[108..112].copy_from_slice(&1_u32.to_be_bytes());
        buf[112] = b'x';
        buf[115] = b'y';
        assert!(Qcow2Header::from_buf(&buf).is_err());

        // Defined extension types may only occur once.
        let mut buf = minimal_header(3, 104);
        for offset in [104, 120] {
            buf[offset..offset + 4].copy_from_slice(&0xe2792aca_u32.to_be_bytes());
            buf[offset + 4..offset + 8].copy_from_slice(&3_u32.to_be_bytes());
            buf[offset + 8..offset + 11].copy_from_slice(b"raw");
        }
        assert!(Qcow2Header::from_buf(&buf).is_err());
    }

    #[test]
    fn test_backing_name_bounds() {
        let mut buf = minimal_header(3, 104);
        buf[8..16].copy_from_slice(&504_u64.to_be_bytes());
        buf[16..20].copy_from_slice(&8_u32.to_be_bytes());
        buf[504..].copy_from_slice(b"base.raw");
        assert_eq!(
            Qcow2Header::from_buf(&buf)
                .unwrap()
                .backing_filename()
                .unwrap(),
            "base.raw"
        );
        buf[8..16].copy_from_slice(&32_u64.to_be_bytes());
        assert!(Qcow2Header::from_buf(&buf).is_err());
    }

    #[test]
    fn test_preserve_unknown_header_fields_and_extensions() {
        for length in [104, 112, 128] {
            let mut buf = minimal_header(3, length);
            if length > 112 {
                buf[112..length as usize].fill(0xa5);
            }
            let mut header = Qcow2Header::from_buf(&buf).unwrap();
            header.backing_filename = Some("base.qcow2".to_owned());
            header.extensions.push(Qcow2HeaderExtension::Unknown {
                extension_type: 0x12345678,
                data: vec![1, 2, 3],
            });
            header.set_l1_table(1024, 1).unwrap();
            let serialized = header.serialize_to_buf().unwrap();
            let header = Qcow2Header::from_buf(&serialized).unwrap();
            assert_eq!(header.header_length(), length);
            if length > 104 {
                assert_eq!(
                    &serialized[105..length as usize],
                    &buf[105..length as usize]
                );
            }
            assert_eq!(header.backing_filename().unwrap(), "base.qcow2");
            assert_eq!(header.l1_table_offset(), 1024);
            assert!(
                matches!(&header.extensions[0], Qcow2HeaderExtension::Unknown {
                extension_type: 0x12345678,
                data,
            } if data == &[1, 2, 3])
            );
        }
    }

    #[test]
    fn test_format() {
        fn __test_format(cluster_bits: usize, refcount_order: u8, size: u64) {
            let p = crate::qcow2_default_params!(true, true);
            let bs = 1 << p.get_bs_bits();
            let (rc_t, rc_b, _) =
                Qcow2Header::calculate_meta_params(size, cluster_bits, refcount_order, bs);
            let clusters = 1 + rc_t.1 + rc_b.1;
            let img_size = ((clusters as usize) << cluster_bits) + 512;
            let mut buf = vec![0u8; img_size];

            Qcow2Header::format_qcow2(&mut buf, size, cluster_bits, refcount_order, bs).unwrap();

            let header = Qcow2Header::from_buf(&buf).unwrap();
            let info = Qcow2Info::new(&header, &p).unwrap();

            assert!(info.cluster_bits() == cluster_bits);
            assert!(info.virtual_size() == size);
            assert!(info.refcount_order() == refcount_order);
        }

        let sizes = [64 << 20, 64 << 30, 2 << 40];

        for c in 13..21 {
            for r in 1..7 {
                for s in sizes {
                    __test_format(c, r, s);
                }
            }
        }
    }

    #[test]
    fn test_unaligned_table_offsets_rejected() {
        let cluster_bits = 16;
        let refcount_order = 4;
        let size = 64 << 20;
        let p = crate::qcow2_default_params!(true, true);
        let bs = 1 << p.get_bs_bits();
        let (rc_t, rc_b, _) =
            Qcow2Header::calculate_meta_params(size, cluster_bits, refcount_order, bs);
        let clusters = 1 + rc_t.1 + rc_b.1;
        let img_size = ((clusters as usize) << cluster_bits) + 512;
        let mut buf = vec![0u8; img_size];

        Qcow2Header::format_qcow2(&mut buf, size, cluster_bits, refcount_order, bs).unwrap();
        assert!(Qcow2Header::from_buf(&buf).is_ok());

        // corrupt the low byte of the big-endian l1_table_offset (bytes 40..48)
        let mut unaligned = buf.clone();
        unaligned[47] |= 0x2;
        assert!(Qcow2Header::from_buf(&unaligned).is_err());

        // corrupt the low byte of the big-endian refcount_table_offset (bytes 48..56)
        let mut unaligned = buf.clone();
        unaligned[55] |= 0x2;
        assert!(Qcow2Header::from_buf(&unaligned).is_err());
    }
}
