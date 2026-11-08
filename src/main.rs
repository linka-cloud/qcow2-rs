use clap::{Args, Parser, Subcommand};
use clap_num::maybe_hex;
use qcow2_rs::dev::{Qcow2Dev, Qcow2DevParams, Qcow2Info};
mod management;
use management::{ManagementArgs, OpenContext};
use qcow2_rs::error::Qcow2Result;
use qcow2_rs::helpers::Qcow2IoBuf;
use qcow2_rs::meta::{
    L1Table, L2Table, Qcow2FeatureType, Qcow2Header, RefBlock, RefTable, Table, TableEntry,
};
use qcow2_rs::ops::Qcow2IoOps;
use qcow2_rs::utils::qcow2_setup_dev_tokio;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::path::PathBuf;
use tokio::runtime::Runtime;

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// qcow2 image path
    file: PathBuf,
    /// Explicitly rebuild reference counts and clear dirty/corrupt state.
    #[clap(long)]
    repair: bool,
}

#[derive(Args, Debug)]
pub struct SnapshotArgs {
    #[command(subcommand)]
    action: SnapshotAction,
}

#[derive(Subcommand, Debug)]
pub enum SnapshotAction {
    /// List internal snapshots
    List { file: PathBuf },
    /// Save the active disk and optional VM-state payload
    Create {
        file: PathBuf,
        id: String,
        #[arg(long, default_value = "")]
        name: String,
        #[arg(long)]
        vm_state: Option<PathBuf>,
    },
    /// Delete an internal snapshot
    Delete { file: PathBuf, id: String },
    /// Restore the disk contents and virtual size of a snapshot
    Restore { file: PathBuf, id: String },
    /// Export saved VM-state bytes to a new file
    VmState {
        file: PathBuf,
        id: String,
        output: PathBuf,
    },
}

impl SnapshotAction {
    fn file(&self) -> &Path {
        match self {
            Self::List { file }
            | Self::Create { file, .. }
            | Self::Delete { file, .. }
            | Self::Restore { file, .. }
            | Self::VmState { file, .. } => file,
        }
    }
}

#[derive(Args, Debug)]
pub struct ZeroArgs {
    file: PathBuf,
    #[arg(value_parser=maybe_hex::<u64>)]
    offset: u64,
    #[arg(value_parser=maybe_hex::<usize>)]
    length: usize,
}

#[derive(Args, Debug)]
pub struct BitmapArgs {
    #[command(subcommand)]
    action: BitmapAction,
}

#[derive(Subcommand, Debug)]
pub enum BitmapAction {
    List {
        file: PathBuf,
    },
    Create {
        file: PathBuf,
        name: String,
        #[arg(long, default_value_t = 16)]
        granularity_bits: u8,
        #[arg(long)]
        disabled: bool,
    },
    Remove {
        file: PathBuf,
        name: String,
    },
    Clear {
        file: PathBuf,
        name: String,
    },
    Enable {
        file: PathBuf,
        name: String,
    },
    Disable {
        file: PathBuf,
        name: String,
    },
    Rename {
        file: PathBuf,
        name: String,
        new_name: String,
    },
    Merge {
        file: PathBuf,
        name: String,
        source: String,
    },
    /// Export bitmap bytes (least significant bit first within each byte)
    Export {
        file: PathBuf,
        name: String,
        output: PathBuf,
    },
}

impl BitmapAction {
    fn file(&self) -> &Path {
        match self {
            Self::List { file }
            | Self::Create { file, .. }
            | Self::Remove { file, .. }
            | Self::Clear { file, .. }
            | Self::Enable { file, .. }
            | Self::Disable { file, .. }
            | Self::Rename { file, .. }
            | Self::Merge { file, .. }
            | Self::Export { file, .. } => file,
        }
    }
}

#[derive(Args, Debug)]
pub struct CompressArgs {
    file: PathBuf,
    /// Raw bytes for complete guest clusters, at most 32 MiB
    input: PathBuf,
    #[arg(long, default_value_t = 0, value_parser=maybe_hex::<u64>)]
    offset: u64,
}

#[derive(Args, Debug)]
pub struct InfoArgs {
    /// qcow2 image path
    file: PathBuf,

    /// verbose
    #[clap(long, short, default_value_t = false)]
    verbose: bool,
}

#[derive(Args, Debug)]
pub struct MapArgs {
    /// virtual address
    #[clap(short, long, value_parser=maybe_hex::<u64>)]
    addr: u64,

    /// map length, how many clusters
    #[clap(short, long, default_value_t = 1)]
    len: usize,

    /// qcow2 image path
    file: PathBuf,
}

#[derive(Args, Debug)]
pub struct FormatArgs {
    #[arg(long, default_value_t = 3)]
    version: u32,
    #[arg(long)]
    extended_l2: bool,
    #[arg(long)]
    data_file: Option<PathBuf>,
    #[arg(long, requires = "data_file")]
    data_file_raw: bool,
    #[arg(long)]
    backing: Option<PathBuf>,
    #[arg(long, requires = "backing", value_parser = ["raw", "qcow2"])]
    backing_format: Option<String>,
    #[arg(long)]
    lazy_refcounts: bool,
    #[arg(long, default_value = "off", value_parser = ["off", "metadata", "falloc", "full"])]
    preallocation: String,
    /// Compression format for compressed writes
    #[arg(long, default_value = "zlib", value_parser = ["zlib", "zstd"])]
    compression: String,
    /// Create a LUKS-encrypted image using --secret-file
    #[arg(long)]
    luks: bool,
    /// virtual size, unit is MB
    #[clap(long, short = 's', default_value_t = 64)]
    size: u32,

    /// Cluster bits, cluster size is 1 << cluster_bits
    #[clap(long, short = 'c', default_value_t = 16)]
    cluster_bits: usize,

    /// refcount order, entry size in refcount table is
    /// 1 << refcount_order
    #[clap(long, short = 'r', default_value_t = 4)]
    refcount_order: u8,

    /// qcow2 image path
    file: PathBuf,
}

#[derive(Args, Debug)]
pub struct DumpArgs {
    /// don't dump qcow2 header
    #[clap(long, short = 'n', default_value_t = false)]
    no_header: bool,

    /// dump refcount table
    #[clap(long, short = 'r', default_value_t = false)]
    rc_table: bool,

    /// dump refcount block
    #[clap(long, short = 'R', default_value_t = false, requires("rc_table"))]
    rc_block: bool,

    /// refcount block idx in refcount table
    #[clap(long, default_value_t = -1, requires("rc_block"))]
    rc_block_idx: i32,

    /// dump l1 table
    #[clap(long, short = 'l', default_value_t = false)]
    l1_table: bool,

    /// dump l2 block
    #[clap(long, short = 'L', default_value_t = false, requires("l1_table"))]
    l2_table: bool,

    /// l2 idx in l1 table
    #[clap(long, default_value_t = -1, requires("l2_table"))]
    l2_idx: i32,

    /// qcow2 image path
    file: PathBuf,
}

#[derive(Args, Debug)]
pub struct ConvertArgs {
    #[clap(long, short = 'f')]
    fmt: String,

    #[clap(long, short = 'O')]
    output_fmt: String,

    #[clap(long, short = 'o')]
    output_file: PathBuf,

    /// source image path
    file: PathBuf,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Resize, rebase, commit, amend, preallocate, or manage encryption keys
    #[command(flatten)]
    Manage(ManagementArgs),
    /// Dump all kinds of qcow2 meta data
    Dump(DumpArgs),

    /// Show qcow2 device info, and statistics for meta data
    Info(InfoArgs),

    /// Format into qcow2 image
    Format(FormatArgs),

    /// Check meta data integrity or cluster leak
    Check(CheckArgs),

    /// Manage internal snapshots and saved VM-state payloads
    Snapshot(SnapshotArgs),

    /// Write zeros to an aligned guest range
    Zero(ZeroArgs),

    /// Manage persistent dirty bitmaps
    Bitmap(BitmapArgs),

    /// Write raw input using the image's compression format
    Compress(CompressArgs),

    /// Map qcow2 virtual address into host cluster offset (for development only, may be removed in future)
    Map(MapArgs),

    /// Convert between raw and qcow2 image
    Convert(ConvertArgs),
}

#[derive(Parser)]
#[command(version)]
struct Cli {
    /// File containing the encryption passphrase (read verbatim, without trimming)
    #[arg(long, global = true)]
    secret_file: Option<PathBuf>,
    /// Override the root image's external payload path
    #[arg(long, global = true)]
    external_data_file: Option<PathBuf>,
    /// Passphrase override IMAGE=PASSWORD_FILE; repeat for backing chains
    #[arg(long, global = true)]
    image_secret_file: Vec<String>,
    #[command(subcommand)]
    command: Commands,
}

fn dump_l1_table(
    f: &mut std::fs::File,
    info: &Qcow2Info,
    header: &Qcow2Header,
    arg: &DumpArgs,
) -> Qcow2Result<()> {
    let mut l1 = L1Table::new_empty(Some(header.l1_table_offset()), info.cluster_size());
    let l1_buf = unsafe { std::slice::from_raw_parts_mut(l1.as_mut_ptr(), l1.byte_size()) };
    assert!(l1_buf.as_ptr() == l1.as_ptr());

    //load to this refcount table from image
    f.seek(SeekFrom::Start(header.l1_table_offset()))?;
    let _bytes = f.read(l1_buf).unwrap();
    //assert!(bytes == l1.byte_size());

    println!(
        "L1Table: offset_in_image 0x{:<16x}",
        header.l1_table_offset()
    );
    for i in 0..header.l1_table_entries() {
        let e = l1.get(i);

        println!("\t idx {:<4} entry 0x{:<16x}", i, e.l2_offset());
    }

    if !arg.l2_table {
        return Ok(());
    }

    for i in 0..header.l1_table_entries() {
        let e = l1.get(i);
        let virt_l1_base = (i * info.l2_entries()) << info.cluster_bits();

        if arg.l2_idx >= 0 && (i != arg.l2_idx as usize) {
            continue;
        }

        if !e.is_zero() {
            let offset = e.l2_offset();
            let mut l2 = L2Table::new_empty(Some(offset), info.cluster_size());
            let l2_buf = unsafe { std::slice::from_raw_parts_mut(l2.as_mut_ptr(), l2.byte_size()) };
            assert!(l2_buf.as_ptr() == l2.as_ptr());

            //load to this refcount table from image
            f.seek(SeekFrom::Start(offset))?;
            let bytes = f.read(l2_buf).unwrap();
            assert!(bytes == l2.byte_size());

            println!(
                "L2Table: idx_in_table {} offset_in_image 0x{:<16x} ",
                i, offset
            );
            for j in 0..l2.entries() {
                let e = l2.get(j);

                let ol = match e.is_compressed() {
                    true => match e.compressed_range(info.cluster_bits().try_into().unwrap()) {
                        Some(v) => v,
                        _ => (u64::MAX, 0),
                    },
                    false => (e.cluster_offset(), info.cluster_size()),
                };

                if e.cluster_offset() != 0 {
                    println!(
                        "\t virt_addr 0x{:<16x} idx {:<4} entry {:<x} off 0x{:<16x} len {:<5} compressed {} copied {}",
                        virt_l1_base + (j << info.cluster_bits()),
                        j,
                        e.into_plain(),
                        ol.0, ol.1,
                        e.is_compressed(),
                        e.is_copied(),
                    );
                }
            }
        }
    }

    Ok(())
}

fn dump_refcount_table(
    f: &mut std::fs::File,
    info: &Qcow2Info,
    header: &Qcow2Header,
    arg: &DumpArgs,
) -> Qcow2Result<()> {
    let mut rc_t = RefTable::new_empty(Some(header.reftable_offset()), info.cluster_size());
    let rc_t_buf = unsafe { std::slice::from_raw_parts_mut(rc_t.as_mut_ptr(), rc_t.byte_size()) };
    assert!(rc_t_buf.as_ptr() == rc_t.as_ptr());

    //load to this refcount table from image
    f.seek(SeekFrom::Start(header.reftable_offset()))?;
    let bytes = f.read(rc_t_buf).unwrap();
    assert!(bytes == rc_t.byte_size());

    println!(
        "RefTable: offset_in_image 0x{:<16x}",
        header.reftable_offset()
    );
    for i in 0..rc_t.entries() {
        let e = rc_t.get(i);

        if !e.is_zero() {
            println!("\t idx {:4} entry 0x{:<16x}", i, e.refblock_offset());
        }
    }

    if !arg.rc_block {
        return Ok(());
    }

    for i in 0..rc_t.entries() {
        let e = rc_t.get(i);

        if arg.rc_block_idx >= 0 && i != (arg.rc_block_idx as usize) {
            continue;
        }
        if !e.is_zero() {
            let offset = e.refblock_offset();
            let mut rc_b = RefBlock::new(info.refcount_order(), info.cluster_size(), Some(offset));
            let rc_b_buf =
                unsafe { std::slice::from_raw_parts_mut(rc_b.as_mut_ptr(), rc_b.byte_size()) };
            assert!(rc_b_buf.as_ptr() == rc_b.as_ptr());

            //load to this refcount table from image
            f.seek(SeekFrom::Start(offset))?;
            let bytes = f.read(rc_b_buf).unwrap();
            assert!(bytes == rc_t.byte_size());

            println!(
                "RefBlock: idx_in_table {} offset_in_image 0x{:x} ",
                i, offset
            );
            for j in 0..rc_b.entries() {
                let e = rc_b.get(j);

                let mut off = (info.rb_entries() as u64 * i as u64) << info.cluster_bits();
                off += (j as u64) << info.cluster_bits();

                if !e.is_zero() {
                    println!(
                        "\t idx {:4} entry {} for cluster 0x{:<16x}",
                        j,
                        e.into_plain(),
                        off
                    );
                }
            }
        }
    }

    Ok(())
}

fn __dump_header(f: &PathBuf, h: &Qcow2Header) {
    println!("Qcow2 Header: image {:?} length {}", f, h.header_length());
    println!("\t version\t {}", h.version());
    println!("\t virtual_size\t {} MB", h.size() >> 20);
    println!("\t cluster_size\t {} bytes", 1_u64 << h.cluster_bits());
    println!("\t refcount_order\t {}", h.refcount_order());
    println!(
        "\t crypt_method\t {}",
        match h.crypt_method() {
            0 => "no encryption".to_string(),
            1 => "AES encryption".to_string(),
            2 => "LUKS encryption".to_string(),
            x => format!("{} ?", x),
        }
    );
    if h.header_length() >= 104 {
        println!(
            "\t compression\t {}",
            match h.compression_type() {
                0 => "zlib".to_string(),
                1 => "zstd".to_string(),
                x => format!("{} ?", x),
            }
        );
    }
    println!(
        "\t backing_file\t name {:?} format {:?}",
        h.backing_filename(),
        h.backing_format()
    );
    println!(
        "\t refcount_table\t 0x{:x} - 0x{:x}",
        h.reftable_offset(),
        h.reftable_offset() + ((h.reftable_clusters() as u64) << h.cluster_bits())
    );
    println!(
        "\t l1_table\t offset 0x{:x}, entries {}",
        h.l1_table_offset(),
        h.l1_table_entries(),
    );
    println!(
        "\t snapshots\t offset 0x{:x} nb_snapshots {}",
        h.snapshots_offset(),
        h.nb_snapshots()
    );
    println!("\t features:");
    for i in 0..4 {
        if let Some(f) = h.feature_name(Qcow2FeatureType::Incompatible, i) {
            println!("\t\t {}", f);
        }
    }
    for i in 0..1 {
        if let Some(f) = h.feature_name(Qcow2FeatureType::Compatible, i) {
            println!("\t\t {}", f);
        }
    }
    for i in 0..2 {
        if let Some(f) = h.feature_name(Qcow2FeatureType::Autoclear, i) {
            println!("\t\t {}", f);
        }
    }
}

fn dump_header(p: &PathBuf) -> Qcow2Result<()> {
    let mut f = std::fs::OpenOptions::new().read(true).open(p).unwrap();
    let mut buf = vec![0_u8; 4096];
    let _ = f.read(&mut buf).unwrap();
    let header = Qcow2Header::from_buf(&buf).unwrap();

    __dump_header(p, &header);

    Ok(())
}

fn dump_qcow2(args: DumpArgs) -> Qcow2Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .open(&args.file)
        .unwrap();
    let mut buf = vec![0_u8; 4096];
    let _ = f.read(&mut buf).unwrap();

    let p = qcow2_rs::qcow2_default_params!(true, true);
    let header = Qcow2Header::from_buf(&buf).unwrap();
    let info = Qcow2Info::new(&header, &p).unwrap();

    if !args.no_header {
        __dump_header(&args.file, &header);
    }

    if args.rc_table {
        dump_refcount_table(&mut f, &info, &header, &args).unwrap();
    }

    if args.l1_table {
        dump_l1_table(&mut f, &info, &header, &args).unwrap();
    }

    Ok(())
}

fn __format_qcow2_buf(size: u64, cluster_bits: usize, refcount_order: u8, bs: usize) -> Vec<u8> {
    let (rc_t, rc_b, _) =
        Qcow2Header::calculate_meta_params(size, cluster_bits, refcount_order, bs);
    let clusters = 1 + rc_t.1 + rc_b.1;

    // just zero the 1st 512 sector of l1 table
    let img_size = ((clusters as usize) << cluster_bits) + bs;
    let mut buf = vec![0_u8; img_size];

    Qcow2Header::format_qcow2(&mut buf, size, cluster_bits, refcount_order, bs).unwrap();

    buf
}

fn format_qcow2(args: FormatArgs, context: &OpenContext) -> Qcow2Result<()> {
    Runtime::new()?.block_on(management::create(&args, context))?;
    dump_header(&args.file)?;
    Ok(())
}

fn map_qcow2(args: MapArgs, context: &OpenContext) -> Qcow2Result<()> {
    let rt = Runtime::new().unwrap();
    rt.block_on(async move {
        let p = image_params(false, false, context);
        let dev = qcow2_setup_dev_tokio(&args.file, &p).await.unwrap();
        let cls_bits = dev.info.cluster_bits();
        let mut start = args.addr & (!((1 << cls_bits) - 1) as u64);

        let mut i = 0;
        while i < args.len && start < dev.info.virtual_size() {
            let mapping = dev.get_mapping(start).await.unwrap();

            if mapping.allocated() {
                println!("virt_addr {:x} mapping {}", start, mapping);
                i += 1;
            }

            start += dev.info.cluster_size() as u64;
        }
    });

    Ok(())
}

fn info_qcow2(args: InfoArgs, context: &OpenContext) -> Qcow2Result<()> {
    let rt = Runtime::new().unwrap();
    rt.block_on(async move {
        let p = image_params(true, false, context);
        let dev = qcow2_setup_dev_tokio(&args.file, &p).await.unwrap();
        println!("Capabilities: {:?}", dev.capabilities().await);
        let info = &dev.info;
        println!("{:?}", dev);

        let total_clusters = info.virtual_size().div_ceil(info.cluster_size() as u64);
        println!(
            "virtual size {}MB / {} clusters, cluster size {} byte",
            info.virtual_size() >> 20,
            total_clusters,
            info.cluster_size(),
        );
        println!("Host clusters usage");
        println!(
            "\t{:<16}: alloc {:10} compressed {:10} used {:10}",
            "header", 1, 0, 1
        );
        if args.verbose {
            println!("\t\t {:<#016x} - 0x{:<#016x}", 0, 0);
        }

        let used = std::cell::RefCell::new(1);
        let mapped = std::cell::RefCell::new(1_u64);
        let meta = std::cell::RefCell::new(1_u64);
        let compressed = std::cell::RefCell::new(0_u64);

        dev.qcow2_cluster_usage(|info, ranges, allocated| {
            let mut sum = 0;
            for r in ranges {
                sum += r.end() - r.start() + 1;
            }
            let (alloc, comp) = match allocated {
                Some(a) => a,
                None => (sum.try_into().unwrap(), 0),
            };
            *used.borrow_mut() += sum;
            *mapped.borrow_mut() += alloc as u64;
            *compressed.borrow_mut() += comp as u64;
            if info != "data" {
                *meta.borrow_mut() += sum;
            }

            println!(
                "\t{:<16}: alloc {:10} compressed {:10} used {:10}",
                info, alloc, comp, sum
            );
            if args.verbose {
                for r in ranges {
                    println!(
                        "\t\t {:<#016x} - {:<#016x}",
                        r.start() << dev.info.cluster_bits(),
                        r.end() << dev.info.cluster_bits()
                    );
                }
            }
        })
        .await
        .unwrap();

        let mapped_cnt = *mapped.borrow();
        let used_ratio = *used.borrow() * 100 / total_clusters.max(1);
        let alloc_ratio = mapped_cnt * 100 / total_clusters.max(1);
        let compressed_ratio =
            *compressed.borrow() * 100 / (if mapped_cnt == 0 { 1 } else { mapped_cnt });
        println!(
            "Total {} meta {}: {}/{}% allocated: {}/{}% compressed, {}/{}% used",
            total_clusters,
            *meta.borrow(),
            *mapped.borrow(),
            alloc_ratio,
            *compressed.borrow(),
            compressed_ratio,
            *used.borrow(),
            used_ratio
        );
    });
    Ok(())
}

fn check_qcow2(args: CheckArgs, context: &OpenContext) -> Qcow2Result<()> {
    let rt = Runtime::new().unwrap();

    rt.block_on(async move {
        let mut p = image_params(!args.repair, false, context);
        p.set_repair_mode(args.repair);
        let dev = qcow2_setup_dev_tokio(&args.file, &p).await?;
        if args.repair {
            dev.repair_refcounts().await?;
        }
        dev.check().await
    })
}

async fn copy_from_qcow2<T: Qcow2IoOps>(
    dev: &Qcow2Dev<T>,
    dest: &mut std::fs::File,
    off: u64,
    bytes: usize,
) -> Qcow2Result<usize> {
    let mut buf = Qcow2IoBuf::<u8>::new(bytes.div_ceil(512) * 512);
    let res = dev.read_at(&mut buf, off).await?.min(bytes);
    if res == 0 {
        return Err("Unexpected EOF during conversion".into());
    }

    dest.seek(SeekFrom::Start(off))?;
    dest.write_all(&buf[..res])?;

    Ok(res)
}

async fn copy_to_qcow2<T: Qcow2IoOps>(
    dev: &Qcow2Dev<T>,
    src: &mut std::fs::File,
    off: u64,
    bytes: usize,
) -> Qcow2Result<usize> {
    let mut buf = Qcow2IoBuf::<u8>::new(bytes.div_ceil(512) * 512);
    buf.fill(0);
    src.seek(SeekFrom::Start(off))?;
    src.read_exact(&mut buf[..bytes])?;
    dev.write_at(&buf, off).await?;
    Ok(bytes)
}

async fn convert_from_qcow2_dev<T: Qcow2IoOps>(dev: &Qcow2Dev<T>, raw: &Path) -> Qcow2Result<()> {
    if raw.exists() {
        for source in dev.storage_paths().await? {
            if same_file::is_same_file(raw, source)? {
                return Err("Conversion output aliases image storage".into());
            }
        }
    }
    let mut file = std::fs::File::create(raw)?;
    let mut off: u64 = 0;
    let buf_size = 8 << 20;
    let total = dev.info.virtual_size();
    while off < total {
        let len = std::cmp::min(buf_size, total - off);

        let res = copy_from_qcow2(dev, &mut file, off, len as usize).await?;

        off += res as u64;
    }

    file.sync_all()?;
    Ok(())
}

async fn convert_to_qcow2_dev<T: Qcow2IoOps>(raw: &Path, dev: &Qcow2Dev<T>) -> Qcow2Result<()> {
    let file_orig_size = std::fs::metadata(raw).unwrap().len();
    let mut file = std::fs::OpenOptions::new().read(true).open(raw).unwrap();

    let mut off: u64 = 0;
    let buf_size = 8 << 20;
    let total = file_orig_size;
    while off < total {
        let len = std::cmp::min(buf_size, total - off);
        let res = copy_to_qcow2(dev, &mut file, off, len as usize).await?;
        off += res as u64;
    }
    dev.flush_meta().await?;

    Ok(())
}

fn convert_to_qcow2_prep(raw: &Path, qcow2: &Path) -> Qcow2Result<()> {
    let cluster_bits = 16;
    let cluster_size = 1 << cluster_bits;
    let file_orig_size = std::fs::metadata(raw).unwrap().len();
    let file_size = (file_orig_size + cluster_size - 1) & !(cluster_size - 1);

    let img_buf = __format_qcow2_buf(file_size, cluster_bits, 4, 4096);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(qcow2)
        .unwrap();
    f.write_all(&img_buf)?;
    f.sync_all()?;

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn convert_from_qcow2(qcow2: &Path, raw: &Path, context: &OpenContext) -> Qcow2Result<()> {
    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(async {
        let p = image_params(true, false, context);
        let dev = qcow2_rs::utils::qcow2_setup_dev_tokio(qcow2, &p).await?;
        convert_from_qcow2_dev(&dev, raw).await
    })
}

#[cfg(target_os = "linux")]
fn convert_from_qcow2(qcow2: &Path, raw: &Path, context: &OpenContext) -> Qcow2Result<()> {
    tokio_uring::start(async {
        let p = image_params(true, true, context);
        let dev = qcow2_rs::utils::qcow2_setup_dev_uring(qcow2, &p).await?;
        convert_from_qcow2_dev(&dev, raw).await
    })
}

#[cfg(not(target_os = "linux"))]
fn convert_to_qcow2(raw: &Path, qcow2: &Path) -> Qcow2Result<()> {
    convert_to_qcow2_prep(raw, qcow2)?;
    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(async {
        let p = image_params(false, false, &OpenContext::default());
        let dev = qcow2_rs::utils::qcow2_setup_dev_tokio(qcow2, &p).await?;
        convert_to_qcow2_dev(raw, &dev).await
    })
}

#[cfg(target_os = "linux")]
fn convert_to_qcow2(raw: &Path, qcow2: &Path) -> Qcow2Result<()> {
    convert_to_qcow2_prep(raw, qcow2)?;

    tokio_uring::start(async {
        let p = image_params(false, true, &OpenContext::default());
        let dev = qcow2_rs::utils::qcow2_setup_dev_uring(qcow2, &p).await?;
        convert_to_qcow2_dev(raw, &dev).await
    })
}

fn __convert_qcow2(args: ConvertArgs, context: &OpenContext) -> Qcow2Result<()> {
    let in_fmt = &args.fmt;
    let out_fmt = &args.output_fmt;

    if args.file == args.output_file
        || args.output_file.exists() && same_file::is_same_file(&args.file, &args.output_file)?
    {
        return Err("input and output point to same file".into());
    }

    if in_fmt == "qcow2" && out_fmt == "raw" {
        convert_from_qcow2(&args.file, &args.output_file, context)?;
    } else if in_fmt == "raw" && out_fmt == "qcow2" {
        convert_to_qcow2(&args.file, &args.output_file)?;
    } else {
        return Err("wrong input or output format".into());
    }

    Ok(())
}

fn convert_qcow2(args: ConvertArgs, context: &OpenContext) -> Qcow2Result<()> {
    __convert_qcow2(args, context)
}

fn snapshot_qcow2(args: SnapshotArgs, context: &OpenContext) -> Qcow2Result<()> {
    Runtime::new()?.block_on(async {
        let read_only = matches!(
            args.action,
            SnapshotAction::List { .. } | SnapshotAction::VmState { .. }
        );
        let params = image_params(read_only, false, context);
        let dev = qcow2_setup_dev_tokio(args.action.file(), &params).await?;
        match args.action {
            SnapshotAction::List { .. } => {
                println!("ID\tNAME\tDISK BYTES\tVM STATE BYTES");
                for s in dev.list_snapshots().await? {
                    println!("{}\t{}\t{}\t{}", s.id, s.name, s.disk_size, s.vm_state_size);
                }
            }
            SnapshotAction::Create {
                id, name, vm_state, ..
            } => {
                let state = vm_state.map(std::fs::read).transpose()?.unwrap_or_default();
                dev.create_snapshot(
                    qcow2_rs::dev::SnapshotOptions {
                        id,
                        name,
                        ..Default::default()
                    },
                    &state,
                )
                .await?;
            }
            SnapshotAction::Delete { id, .. } => dev.delete_snapshot(&id).await?,
            SnapshotAction::Restore { id, .. } => dev.restore_snapshot(&id).await?,
            SnapshotAction::VmState { id, output, .. } => {
                // Validate the snapshot before creating the output file.
                dev.read_snapshot_vm_state(&id, 0, &mut []).await?;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(output)?;
                let mut buf = vec![0; 1 << 20];
                let mut offset = 0;
                loop {
                    let n = dev.read_snapshot_vm_state(&id, offset, &mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    file.write_all(&buf[..n])?;
                    offset += n as u64;
                }
                file.sync_all()?;
            }
        }
        Ok(())
    })
}

fn zero_qcow2(args: ZeroArgs, context: &OpenContext) -> Qcow2Result<()> {
    Runtime::new()?.block_on(async {
        let params = image_params(false, false, context);
        let dev = qcow2_setup_dev_tokio(&args.file, &params).await?;
        dev.write_zeroes(args.offset, args.length).await
    })
}

fn bitmap_qcow2(args: BitmapArgs, context: &OpenContext) -> Qcow2Result<()> {
    Runtime::new()?.block_on(async {
        let read_only = matches!(
            args.action,
            BitmapAction::List { .. } | BitmapAction::Export { .. }
        );
        let params = image_params(read_only, false, context);
        let dev = qcow2_setup_dev_tokio(args.action.file(), &params).await?;
        match args.action {
            BitmapAction::List { .. } => {
                println!("NAME\tGRANULARITY BITS\tENABLED\tCONSISTENT\tIN USE");
                for bitmap in dev.list_bitmaps().await? {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        bitmap.name,
                        bitmap.granularity_bits,
                        bitmap.enabled,
                        bitmap.consistent,
                        bitmap.in_use
                    );
                }
            }
            BitmapAction::Create {
                name,
                granularity_bits,
                disabled,
                ..
            } => {
                dev.create_bitmap(&name, granularity_bits, !disabled)
                    .await?
            }
            BitmapAction::Remove { name, .. } => dev.remove_bitmap(&name).await?,
            BitmapAction::Clear { name, .. } => dev.clear_bitmap(&name).await?,
            BitmapAction::Enable { name, .. } => dev.set_bitmap_enabled(&name, true).await?,
            BitmapAction::Disable { name, .. } => dev.set_bitmap_enabled(&name, false).await?,
            BitmapAction::Rename { name, new_name, .. } => {
                dev.rename_bitmap(&name, &new_name).await?
            }
            BitmapAction::Merge { name, source, .. } => dev.merge_bitmap(&name, &source).await?,
            BitmapAction::Export { name, output, .. } => {
                let bytes = dev.read_bitmap(&name).await?;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(output)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
        }
        Ok(())
    })
}

fn compress_qcow2(args: CompressArgs, context: &OpenContext) -> Qcow2Result<()> {
    if std::fs::metadata(&args.input)?.len() > 32 << 20 {
        return Err("Compressed batch exceeds 32 MiB".into());
    }
    let bytes = std::fs::read(args.input)?;
    Runtime::new()?.block_on(async {
        let params = image_params(false, false, context);
        let dev = qcow2_setup_dev_tokio(&args.file, &params).await?;
        dev.write_compressed_at(&bytes, args.offset).await
    })
}

fn image_params(read_only: bool, direct: bool, context: &OpenContext) -> Qcow2DevParams {
    let mut params = context.params.clone();
    params.set_read_only(read_only);
    params.set_direct_io(direct);
    params
}

fn main() -> Qcow2Result<()> {
    let cli = Cli::parse();
    let context = OpenContext::load(
        cli.secret_file,
        cli.external_data_file,
        cli.image_secret_file,
    )?;

    env_logger::builder()
        .format_target(false)
        .format_timestamp(None)
        .init();

    match cli.command {
        Commands::Manage(arg) => Runtime::new()?.block_on(management::run(arg, &context)),
        Commands::Dump(arg) => dump_qcow2(arg),
        Commands::Format(arg) => format_qcow2(arg, &context),
        Commands::Map(arg) => map_qcow2(arg, &context),
        Commands::Info(arg) => info_qcow2(arg, &context),
        Commands::Check(arg) => check_qcow2(arg, &context),
        Commands::Snapshot(arg) => snapshot_qcow2(arg, &context),
        Commands::Zero(arg) => zero_qcow2(arg, &context),
        Commands::Bitmap(arg) => bitmap_qcow2(arg, &context),
        Commands::Compress(arg) => compress_qcow2(arg, &context),
        Commands::Convert(arg) => convert_qcow2(arg, &context),
    }
}
