use crate::dev::{Qcow2Dev, Qcow2DevParams};
use crate::error::Qcow2Result;
use crate::helpers::Qcow2IoBuf;
use crate::meta::Qcow2Header;
use crate::ops::*;
#[cfg(not(target_os = "windows"))]
use crate::sync_io::Qcow2IoSync;
#[cfg(target_os = "linux")]
use crate::uring::Qcow2IoUring;
use async_recursion::async_recursion;
use std::io::Write;
use std::path::{Path, PathBuf};

#[macro_export]
macro_rules! qcow2_default_params {
    ($ro: expr, $dio: expr) => {
        Qcow2DevParams::new(9, None, None, $ro, $dio)
    };
}

/// 4K is usually enough for holding generic qcow2 header
const DEF_HEADER_SIZE: usize = 4096;

/// Header extensions may occupy the entire first cluster.
const MAX_HEADER_SIZE: usize = Qcow2Header::MAX_CLUSTER_SIZE as usize;

/// Allocate one qcow2 device and qcow2 header needs to be parsed
/// for allocating the device.
pub fn qcow2_alloc_dev_sync<T: Qcow2IoOps>(
    path: &Path,
    io: T,
    params: &Qcow2DevParams,
) -> Qcow2Result<(Qcow2Dev<T>, Option<PathBuf>)> {
    fn read_header(path: &Path, bytes: usize) -> Qcow2Result<Vec<u8>> {
        use std::io::Read;
        let mut buf = Vec::new();
        std::fs::File::open(path)?
            .take(bytes as u64)
            .read_to_end(&mut buf)?;
        Ok(buf)
    }

    let buf = read_header(path, DEF_HEADER_SIZE)?;
    let header = match Qcow2Header::from_buf(&buf) {
        Ok(h) => h,
        Err(_) => {
            let buf = read_header(path, MAX_HEADER_SIZE)?;
            Qcow2Header::from_buf(&buf)?
        }
    };
    let back_path = header.backing_filename().map(|s| PathBuf::from(s.clone()));

    Ok((Qcow2Dev::new(path, header, params, io)?, back_path))
}

/// Allocate one qcow2 device and qcow2 header needs to be parsed
/// for allocating the device.
pub async fn qcow2_alloc_dev<T: Qcow2IoOps>(
    path: &Path,
    io: T,
    params: &Qcow2DevParams,
) -> Qcow2Result<(Qcow2Dev<T>, Option<PathBuf>)> {
    async fn read_header<T: Qcow2IoOps>(io: &T, bytes: usize) -> Qcow2Result<Vec<u8>> {
        let mut buf = Qcow2IoBuf::<u8>::new(bytes);
        let count = io.read_to(0, &mut buf).await?;
        Ok(buf[..count].to_vec())
    }
    let buf = read_header(&io, DEF_HEADER_SIZE).await?;
    let header = match Qcow2Header::from_buf(&buf) {
        Ok(h) => h,
        Err(_) => {
            let buf = read_header(&io, MAX_HEADER_SIZE).await?;
            Qcow2Header::from_buf(&buf)?
        }
    };
    let back_path = header.backing_filename().map(|s| PathBuf::from(s.clone()));

    Ok((Qcow2Dev::new(path, header, params, io)?, back_path))
}

/// Honor an explicit backing format; probe QCOW2 magic when it is absent.
pub fn qcow2_backing_is_raw(path: &Path, format: Option<&str>) -> Qcow2Result<bool> {
    match format {
        Some("raw") => Ok(true),
        Some("qcow2") => Ok(false),
        Some(other) => Err(format!("Unsupported backing format {other}").into()),
        None => {
            use std::io::Read;
            let mut magic = [0; 4];
            let mut file = std::fs::File::open(path)?;
            match file.read_exact(&mut magic) {
                Ok(()) => Ok(u32::from_be_bytes(magic) != Qcow2Header::QCOW2_MAGIC),
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(true),
                Err(err) => Err(err.into()),
            }
        }
    }
}

#[macro_export]
macro_rules! qcow2_setup_dev_fn {
    ($type:ty, $fn_name: ident) => {
        pub async fn $fn_name(
            path: &Path,
            params: &Qcow2DevParams,
        ) -> Qcow2Result<Qcow2Dev<$type>> {
            #[async_recursion(?Send)]
            async fn open_chain(
                path: &Path,
                params: &Qcow2DevParams,
                ancestors: &mut std::collections::HashSet<PathBuf>,
            ) -> Qcow2Result<Qcow2Dev<$type>> {
                if ancestors.len() >= 256 {
                    return Err("Backing chain exceeds the supported depth of 256".into());
                }
                if !ancestors.insert(std::fs::canonicalize(path)?) {
                    return Err("Cycle in qcow2 backing chain".into());
                }
                let io =
                    <$type>::try_new(path, params.is_read_only(), params.is_direct_io()).await?;
                let (mut dev, backing) = qcow2_alloc_dev(&path, io, params).await?;
                match backing {
                    Some(back_path) => {
                        let back_path = if back_path.is_absolute() {
                            back_path
                        } else {
                            path.parent()
                                .unwrap_or_else(|| Path::new("."))
                                .join(back_path)
                        };
                        let mut p = params.clone();
                        p.mark_backing_dev(Some(true));
                        if qcow2_backing_is_raw(&back_path, dev.backing_format())? {
                            if ancestors.contains(&std::fs::canonicalize(&back_path)?) {
                                return Err("Cycle in qcow2 backing chain".into());
                            }
                            let size = std::fs::metadata(&back_path)?.len();
                            let file = <$type>::try_new(&back_path, true, p.is_direct_io()).await?;
                            dev.set_raw_backing(file, size);
                        } else {
                            let bdev = open_chain(&back_path, &p, ancestors).await?;
                            dev.set_backing_dev(Box::new(bdev));
                        }
                    }
                    _ => {}
                };

                dev.__qcow2_prep_io().await?;
                Ok(dev)
            }
            open_chain(path, params, &mut std::collections::HashSet::new()).await
        }
    };
}

#[cfg(target_os = "linux")]
qcow2_setup_dev_fn!(Qcow2IoUring, qcow2_setup_dev_uring);
qcow2_setup_dev_fn!(crate::tokio_io::Qcow2IoTokio, qcow2_setup_dev_tokio);

/// Build one helper which can setup one qcow2 device, and this helper
/// needn't be async/.await
///
/// The backing device is setup automatically in the built helper.
#[macro_export]
macro_rules! qcow2_setup_dev_fn_sync {
    ($type:ty, $fn_name: ident) => {
        pub fn $fn_name(path: &Path, params: &Qcow2DevParams) -> Qcow2Result<Qcow2Dev<$type>> {
            fn open_chain(
                path: &Path,
                params: &Qcow2DevParams,
                ancestors: &mut std::collections::HashSet<PathBuf>,
            ) -> Qcow2Result<Qcow2Dev<$type>> {
                if ancestors.len() >= 256 {
                    return Err("Backing chain exceeds the supported depth of 256".into());
                }
                if !ancestors.insert(std::fs::canonicalize(path)?) {
                    return Err("Cycle in qcow2 backing chain".into());
                }
                let io = <$type>::try_new(path, params.is_read_only(), params.is_direct_io())?;
                let (mut dev, backing) = qcow2_alloc_dev_sync(&path, io, params)?;
                match backing {
                    Some(back_path) => {
                        let back_path = if back_path.is_absolute() {
                            back_path
                        } else {
                            path.parent()
                                .unwrap_or_else(|| Path::new("."))
                                .join(back_path)
                        };
                        let mut p = params.clone();
                        p.mark_backing_dev(Some(true));
                        if qcow2_backing_is_raw(&back_path, dev.backing_format())? {
                            if ancestors.contains(&std::fs::canonicalize(&back_path)?) {
                                return Err("Cycle in qcow2 backing chain".into());
                            }
                            let size = std::fs::metadata(&back_path)?.len();
                            let file = <$type>::try_new(&back_path, true, p.is_direct_io())?;
                            dev.set_raw_backing(file, size);
                        } else {
                            let bdev = open_chain(&back_path, &p, ancestors)?;
                            dev.set_backing_dev(Box::new(bdev));
                        }
                    }
                    _ => {}
                };

                Ok(dev)
            }
            open_chain(path, params, &mut std::collections::HashSet::new())
        }
    };
}

#[cfg(not(target_os = "windows"))]
qcow2_setup_dev_fn_sync!(Qcow2IoSync, qcow2_setup_dev_sync);

fn make_qcow2_buf(cluster_bits: usize, refcount_order: u8, size: u64) -> Vec<u8> {
    let bs_shift = 9_u8;
    let bs = 1 << bs_shift;
    let (rc_t, rc_b, _) =
        Qcow2Header::calculate_meta_params(size, cluster_bits, refcount_order, bs);
    let clusters = 1 + rc_t.1 + rc_b.1;
    let img_size = ((clusters as usize) << cluster_bits) + 512;
    let mut buf = vec![0u8; img_size];

    Qcow2Header::format_qcow2(&mut buf, size, cluster_bits, refcount_order, bs).unwrap();

    buf
}

pub fn make_temp_qcow2_img(
    size: u64,
    cluster_bits: usize,
    refcount_order: u8,
) -> tempfile::NamedTempFile {
    let tmpfile = tempfile::NamedTempFile::new().unwrap();
    let mut file = std::fs::File::create(tmpfile.path()).unwrap();

    let buf = make_qcow2_buf(cluster_bits, refcount_order, size);
    file.write_all(&buf).unwrap();

    tmpfile
}
