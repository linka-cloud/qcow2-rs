//! Image utility management commands.
use crate::{image_params, FormatArgs};
use clap::{Args, Subcommand};
use qcow2_rs::dev::*;
use qcow2_rs::error::Qcow2Result;
use qcow2_rs::tokio_io::Qcow2IoTokio;
use qcow2_rs::utils::qcow2_setup_dev_tokio;
use std::path::PathBuf;
use zeroize::Zeroizing;

pub struct OpenContext {
    pub params: Qcow2DevParams,
    secret: Option<EncryptionSecret>,
}
impl Default for OpenContext {
    fn default() -> Self {
        Self {
            params: Qcow2DevParams::new(9, None, None, false, false),
            secret: None,
        }
    }
}
impl OpenContext {
    pub fn load(
        secret_file: Option<PathBuf>,
        external: Option<PathBuf>,
        image_secrets: Vec<String>,
    ) -> Qcow2Result<Self> {
        let mut context = Self::default();
        if let Some(path) = secret_file {
            let bytes = Zeroizing::new(std::fs::read(path)?);
            context.params.set_encryption_secret(&bytes);
            context.secret = Some(EncryptionSecret::new(&bytes));
        }
        if let Some(path) = external {
            context.params.set_external_data_file(path);
        }
        for item in image_secrets {
            let (image, password_file) = item
                .split_once('=')
                .ok_or("Expected IMAGE=PASSWORD_FILE for --image-secret-file")?;
            let bytes = Zeroizing::new(std::fs::read(password_file)?);
            context
                .params
                .set_image_encryption_secret(image.into(), &bytes)?;
        }
        Ok(context)
    }
}
fn mode(value: &str) -> Preallocation {
    match value {
        "metadata" => Preallocation::Metadata,
        "falloc" => Preallocation::Fallocate,
        "full" => Preallocation::Full,
        _ => Preallocation::Off,
    }
}
fn codec(value: &str) -> CompressionType {
    if value == "zstd" {
        CompressionType::Zstd
    } else {
        CompressionType::Deflate
    }
}
fn format(value: &str) -> BackingFormat {
    if value == "raw" {
        BackingFormat::Raw
    } else {
        BackingFormat::Qcow2
    }
}
pub async fn create(args: &FormatArgs, context: &OpenContext) -> Qcow2Result<()> {
    let mut options = CreateOptions {
        size: (args.size as u64) << 20,
        version: args.version,
        cluster_bits: args
            .cluster_bits
            .try_into()
            .map_err(|_| "Invalid cluster bits")?,
        refcount_order: args.refcount_order,
        extended_l2: args.extended_l2,
        backing: args.backing.clone().map(|path| BackingFile {
            path,
            format: args.backing_format.as_deref().map(format),
        }),
        data_file: args.data_file.clone().map(|path| ExternalDataFile {
            path,
            raw: args.data_file_raw,
        }),
        compression: codec(&args.compression),
        lazy_refcounts: args.lazy_refcounts,
        preallocation: mode(&args.preallocation),
        open_params: image_params(false, false, context),
        ..Default::default()
    };
    options.open_params.clear_external_data_file();
    if args.luks {
        options.encryption = Some(
            context
                .secret
                .clone()
                .ok_or("LUKS creation requires --secret-file")?,
        );
    }
    create_qcow2(&args.file, &options).await
}

#[derive(Subcommand)]
pub enum ManagementArgs {
    /// Change virtual size in bytes; shrinking requires --shrink
    Resize {
        file: PathBuf,
        #[arg(value_parser = clap_num::maybe_hex::<u64>)]
        size: u64,
        #[arg(long)]
        shrink: bool,
    },
    /// Materialize the active disk with the requested allocation policy
    Preallocate {
        file: PathBuf,
        #[arg(value_parser = ["off", "metadata", "falloc", "full"])]
        mode: String,
    },
    /// Change backing; omit --backing to flatten. Preserves all snapshots by default
    Rebase {
        file: PathBuf,
        #[arg(long)]
        backing: Option<PathBuf>,
        #[arg(long, requires = "backing", value_parser = ["raw", "qcow2"])]
        backing_format: Option<String>,
        /// Change only the backing reference; guest data may change
        #[arg(long = "unsafe")]
        unsafe_rebase: bool,
    },
    /// Copy the active disk into its immediate backing, preserving snapshots
    Commit {
        file: PathBuf,
        /// Retain the active overlay allocations after copying
        #[arg(long)]
        keep_overlay: bool,
    },
    /// Convert metadata while preserving disk, snapshots, bitmaps, and VM state
    Amend(AmendArgs),
    /// Inspect or change LUKS1 credentials; passphrases are read verbatim
    Keyslot {
        file: PathBuf,
        #[command(subcommand)]
        action: KeyslotAction,
    },
}
#[derive(Args)]
pub struct AmendArgs {
    file: PathBuf,
    #[arg(long)]
    version: Option<u32>,
    #[arg(long)]
    refcount_order: Option<u8>,
    #[arg(long, value_parser = ["on", "off"])]
    extended_l2: Option<String>,
    #[arg(long, value_parser = ["zlib", "zstd"])]
    compression: Option<String>,
    #[arg(long, value_parser = ["on", "off"])]
    lazy_refcounts: Option<String>,
    /// Update a renamed external payload; use --external-data-file to open it
    #[arg(long, conflicts_with_all = ["version", "refcount_order", "extended_l2", "compression", "lazy_refcounts"])]
    data_file_name: Option<PathBuf>,
}
#[derive(Subcommand)]
pub enum KeyslotAction {
    List,
    Add {
        index: usize,
        password_file: PathBuf,
        #[arg(long, default_value_t = 200_000)]
        iterations: u32,
    },
    Remove {
        index: usize,
        /// Allow revocation of the last password (makes reopening impossible)
        #[arg(long)]
        allow_last: bool,
    },
    Change {
        index: usize,
        password_file: PathBuf,
        #[arg(long, default_value_t = 200_000)]
        iterations: u32,
    },
}
fn resolve(file: &std::path::Path, name: &std::path::Path) -> PathBuf {
    if name.is_absolute() {
        name.into()
    } else {
        file.parent()
            .unwrap_or(std::path::Path::new("."))
            .join(name)
    }
}
pub async fn run(args: ManagementArgs, context: &OpenContext) -> Qcow2Result<()> {
    match args {
        ManagementArgs::Resize { file, size, shrink } => {
            let dev = qcow2_setup_dev_tokio(&file, &image_params(false, false, context)).await?;
            dev.resize(size, shrink).await?;
        }
        ManagementArgs::Preallocate { file, mode: policy } => {
            let dev = qcow2_setup_dev_tokio(&file, &image_params(false, false, context)).await?;
            dev.preallocate(mode(&policy)).await?;
        }
        ManagementArgs::Rebase {
            file,
            backing,
            backing_format,
            unsafe_rebase,
        } => {
            let mut dev =
                qcow2_setup_dev_tokio(&file, &image_params(false, false, context)).await?;
            let target = if let Some(name) = backing {
                let path = resolve(&file, &name);
                let fmt = if let Some(fmt) = backing_format {
                    format(&fmt)
                } else {
                    use std::io::Read;
                    let mut magic = [0; 4];
                    let n = std::fs::File::open(&path)?.read(&mut magic)?;
                    if n == 4 && magic == *b"QFI\xfb" {
                        BackingFormat::Qcow2
                    } else {
                        BackingFormat::Raw
                    }
                };
                let source = if fmt == BackingFormat::Raw {
                    Backing::raw(
                        Qcow2IoTokio::try_new(&path, true, false).await?,
                        std::fs::metadata(&path)?.len(),
                    )
                } else {
                    let mut params = image_params(true, false, context);
                    params.clear_external_data_file();
                    params.mark_backing_dev(Some(true));
                    Backing::qcow2(qcow2_setup_dev_tokio(&path, &params).await?)
                };
                Some(RebaseTarget {
                    file: BackingFile {
                        path: name,
                        format: Some(fmt),
                    },
                    source,
                })
            } else {
                None
            };
            dev.rebase(target, !unsafe_rebase).await?;
        }
        ManagementArgs::Commit { file, keep_overlay } => {
            let dev = qcow2_setup_dev_tokio(&file, &image_params(false, false, context)).await?;
            let backing = dev
                .backing_file()
                .await
                .ok_or("Image has no backing file")?;
            let path = resolve(&file, &backing.path);
            if backing.format == Some(BackingFormat::Raw) {
                let target = Qcow2IoTokio::try_new(&path, false, false).await?;
                dev.commit_to_raw(&target, &path, !keep_overlay).await?;
            } else {
                let mut params = image_params(false, false, context);
                params.clear_external_data_file();
                let target = qcow2_setup_dev_tokio(&path, &params).await?;
                dev.commit_to(&target, !keep_overlay).await?;
            }
        }
        ManagementArgs::Amend(args) => {
            let mut dev =
                qcow2_setup_dev_tokio(&args.file, &image_params(false, false, context)).await?;
            if let Some(name) = args.data_file_name {
                dev.amend_data_file_name(name).await?;
            } else {
                dev.amend(AmendOptions {
                    version: args.version,
                    refcount_order: args.refcount_order,
                    extended_l2: args.extended_l2.map(|s| s == "on"),
                    compression: args.compression.as_deref().map(codec),
                    lazy_refcounts: args.lazy_refcounts.map(|s| s == "on"),
                })
                .await?;
            }
        }
        ManagementArgs::Keyslot { file, action } => {
            let read_only = matches!(action, KeyslotAction::List);
            let dev =
                qcow2_setup_dev_tokio(&file, &image_params(read_only, false, context)).await?;
            match action {
                KeyslotAction::List => {
                    println!("INDEX\tACTIVE\tITERATIONS\tSTRIPES");
                    for slot in dev.luks_keyslots().await? {
                        println!(
                            "{}\t{}\t{}\t{}",
                            slot.index, slot.active, slot.iterations, slot.stripes
                        );
                    }
                }
                KeyslotAction::Add {
                    index,
                    password_file,
                    iterations,
                } => {
                    let bytes = Zeroizing::new(std::fs::read(password_file)?);
                    dev.add_luks_keyslot(index, &bytes, LuksOptions { iterations })
                        .await?;
                }
                KeyslotAction::Remove { index, allow_last } => {
                    dev.remove_luks_keyslot(index, allow_last).await?
                }
                KeyslotAction::Change {
                    index,
                    password_file,
                    iterations,
                } => {
                    let bytes = Zeroizing::new(std::fs::read(password_file)?);
                    let replacement = dev
                        .change_luks_keyslot(index, &bytes, LuksOptions { iterations })
                        .await?;
                    println!("Replacement keyslot: {replacement}");
                }
            }
        }
    }
    Ok(())
}
