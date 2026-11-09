//! Native image creation and feature discovery.
use super::crypto::Secret;
use super::graph::invalid;
use super::*;
use std::io::Write;

#[derive(Debug, Clone)]
pub struct EncryptionSecret(Secret);
impl EncryptionSecret {
    pub fn new(bytes: impl AsRef<[u8]>) -> Self {
        Self(Secret::new(bytes.as_ref()))
    }
}
impl AsRef<[u8]> for EncryptionSecret {
    fn as_ref(&self) -> &[u8] {
        &self.0 .0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preallocation {
    Off,
    Metadata,
    Fallocate,
    Full,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingFormat {
    Raw,
    Qcow2,
}
impl BackingFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Qcow2 => "qcow2",
        }
    }
}
#[derive(Debug, Clone)]
pub struct BackingFile {
    pub path: PathBuf,
    pub format: Option<BackingFormat>,
}
#[derive(Debug, Clone)]
pub struct ExternalDataFile {
    pub path: PathBuf,
    pub raw: bool,
}
#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub size: u64,
    pub version: u32,
    pub cluster_bits: u8,
    pub refcount_order: u8,
    pub extended_l2: bool,
    pub backing: Option<BackingFile>,
    pub data_file: Option<ExternalDataFile>,
    pub compression: CompressionType,
    pub lazy_refcounts: bool,
    pub preallocation: Preallocation,
    pub luks: LuksOptions,
    /// Buffered Tokio parameters/credentials used during initialization.
    pub open_params: Qcow2DevParams,
    pub encryption: Option<EncryptionSecret>,
}
impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            size: 64 << 20,
            version: 3,
            cluster_bits: 16,
            refcount_order: 4,
            extended_l2: false,
            backing: None,
            data_file: None,
            compression: CompressionType::Deflate,
            lazy_refcounts: false,
            preallocation: Preallocation::Off,
            luks: LuksOptions::default(),
            open_params: Qcow2DevParams::new(9, None, None, false, false),
            encryption: None,
        }
    }
}
impl CreateOptions {
    pub fn set_luks_secret(&mut self, bytes: impl AsRef<[u8]>) {
        self.encryption = Some(EncryptionSecret::new(bytes.as_ref()));
    }
    pub fn validate(&self) -> Qcow2Result<()> {
        if !matches!(self.version, 2 | 3)
            || !(9..=21).contains(&self.cluster_bits)
            || self.refcount_order > 6
        {
            return Err(invalid(
                "Unsupported version, cluster size, or refcount width",
            ));
        }
        if self.extended_l2 && self.cluster_bits < 14 {
            return Err(invalid("Extended L2 requires clusters of at least 16 KiB"));
        }
        if self.version == 2
            && (self.refcount_order != 4
                || self.extended_l2
                || self.data_file.is_some()
                || self.lazy_refcounts
                || self.compression != CompressionType::Deflate)
        {
            return Err(invalid(
                "v2 creation requires 16-bit refcounts and basic Deflate layout",
            ));
        }
        if self.data_file.is_some() && self.compression != CompressionType::Deflate
            || self.encryption.is_some() && self.compression != CompressionType::Deflate
            || self.data_file.as_ref().is_some_and(|d| d.raw)
                && (self.backing.is_some() || self.encryption.is_some())
        {
            return Err(invalid(
                "Incompatible external/raw/compression/encryption creation options",
            ));
        }
        if self.open_params.external_data_file().is_some()
            || self.open_params.is_direct_io()
            || self.open_params.is_read_only()
            || !(9..=12).contains(&self.open_params.get_bs_bits())
            || self.open_params.get_bs_bits() > self.cluster_bits
        {
            return Err(invalid("Creation initialization requires writable buffered parameters with a valid block size"));
        }
        if self.encryption.is_some()
            && (self.size % 512 != 0
                || self.luks.iterations == 0
                || self.luks.iterations > 10_000_000)
        {
            return Err(invalid(
                "Encryption requires a sector-aligned size and supported KDF iterations",
            ));
        }
        for name in self
            .backing
            .as_ref()
            .map(|b| &b.path)
            .into_iter()
            .chain(self.data_file.as_ref().map(|d| &d.path))
        {
            let name = name.to_str().ok_or("Image names must be UTF-8")?;
            if name.is_empty() || name.len() > 1023 || name.as_bytes().contains(&0) {
                return Err(invalid("Invalid image filename"));
            }
        }
        Ok(())
    }
}

/// Create a new metadata file and optional new external data file. Existing
/// files are never overwritten. Initialization errors remove only created files.
pub async fn create_qcow2(path: &Path, options: &CreateOptions) -> Qcow2Result<()> {
    options.validate()?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let resolve = |name: &Path| {
        if name.is_absolute() {
            name.to_path_buf()
        } else {
            parent.join(name)
        }
    };
    let mut options = options.clone();
    #[cfg(target_os = "linux")]
    let _backing_lock;
    if let Some(backing) = &mut options.backing {
        let back_path = resolve(&backing.path);
        let _backing_file = std::fs::File::open(&back_path)?;
        #[cfg(target_os = "linux")]
        {
            _backing_lock = crate::locking::ImageLock::acquire(&_backing_file, &back_path, true)?;
        }
        if backing.format.is_none() {
            backing.format = Some(if crate::utils::qcow2_backing_is_raw(&back_path, None)? {
                BackingFormat::Raw
            } else {
                BackingFormat::Qcow2
            });
        }
    }
    let bytes = Qcow2Header::initial_metadata(&options)?;
    let data_path = options.data_file.as_ref().map(|data| resolve(&data.path));
    let identity = |p: &Path| -> Qcow2Result<PathBuf> {
        let parent = p
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Ok(std::fs::canonicalize(parent)?.join(p.file_name().ok_or("Missing filename")?))
    };
    if let Some(data_path) = &data_path {
        if identity(path)? == identity(data_path)? {
            return Err(invalid("Metadata and data paths alias"));
        }
    }
    let mut created = Vec::new();
    let result = async {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        created.push(path.to_path_buf());
        if let Some(data_path) = &data_path {
            let data = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(data_path)?;
            created.push(data_path.clone());
            data.set_len(options.size)?;
            data.sync_all()?;
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        if options.encryption.is_some() || options.preallocation != Preallocation::Off {
            let dev = crate::utils::qcow2_setup_dev_tokio(path, &options.open_params).await?;
            if let Some(secret) = &options.encryption {
                dev.initialize_luks(secret.as_ref(), options.luks.clone())
                    .await?;
            }
            if options.preallocation != Preallocation::Off {
                dev.preallocate(options.preallocation).await?;
            }
            dev.flush_meta().await?;
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        for path in created.into_iter().rev() {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionFormat {
    None,
    LegacyAes,
    Luks1,
}
#[derive(Debug, Clone)]
pub struct ImageCapabilities {
    pub version: u32,
    pub cluster_size: usize,
    pub refcount_bits: u32,
    pub compression: CompressionType,
    pub encryption: EncryptionFormat,
    pub extended_l2: bool,
    pub external_data: bool,
    pub raw_external_data: bool,
    pub lazy_refcounts: bool,
    pub persistent_bitmaps: bool,
    pub snapshots: u32,
    pub writable: bool,
    pub needs_repair: bool,
    pub reopen_required: bool,
    pub snapshot_creation: bool,
    pub compressed_writes: bool,
    pub bitmap_management: bool,
}
impl<T: Qcow2IoOps> Qcow2Dev<T> {
    pub async fn capabilities(&self) -> ImageCapabilities {
        let _io = self.io_lock.read().await;
        let h = self.header.read().await;
        ImageCapabilities {
            version: h.version(),
            cluster_size: self.info.cluster_size(),
            refcount_bits: 1 << self.info.refcount_order(),
            compression: if h.compression_type() == 1 {
                CompressionType::Zstd
            } else {
                CompressionType::Deflate
            },
            encryption: match h.crypt_method() {
                1 => EncryptionFormat::LegacyAes,
                2 => EncryptionFormat::Luks1,
                _ => EncryptionFormat::None,
            },
            extended_l2: self.info.extended_l2(),
            external_data: self.info.external_data(),
            raw_external_data: self.info.raw_data(),
            lazy_refcounts: h.compatible_features() & 1 != 0,
            persistent_bitmaps: h.extension_data(0x23852875).is_some(),
            snapshots: h.nb_snapshots(),
            writable: !self.info.is_read_only(),
            needs_repair: self.needs_repair.load(Ordering::Acquire),
            reopen_required: self.reopen_required.load(Ordering::Acquire),
            snapshot_creation: !self.info.is_read_only()
                && !self.info.external_data()
                && self.info.refcount_order() != 0,
            compressed_writes: !self.info.is_read_only()
                && !self.info.external_data()
                && !self.info.encrypted(),
            bitmap_management: !self.info.is_read_only() && h.version() == 3,
        }
    }
}
