use crate::error::Qcow2Result;
#[rustversion::before(1.75)]
use async_trait::async_trait;

pub struct Qcow2OpsFlags {}

impl Qcow2OpsFlags {
    pub const FALLOCATE_ZERO_RANGE: u32 = 1_u32 << 0;
}

/// Map qcow2 fallocate flags to a host hole-punch and issue the
/// synchronous fallocate call. Shared by every backend whose
/// underlying fd supports `FALLOC_FL_PUNCH_HOLE`; `tokio-uring` has no
/// async fallocate of its own, so it also goes through this path.
#[cfg(target_os = "linux")]
pub(crate) fn linux_punch_hole(fd: i32, offset: u64, len: usize, _flags: u32) -> Qcow2Result<()> {
    // PUNCH_HOLE deallocates the range (shrinking the host file's block
    // count) and makes it read back as zero, covering both the discard and
    // FALLOCATE_ZERO_RANGE contracts; it must be ORed with KEEP_SIZE so the
    // logical file size is preserved. This matches the macOS backend, which
    // always issues F_PUNCHHOLE regardless of flags. Combining PUNCH_HOLE
    // with ZERO_RANGE is rejected by the kernel, and ZERO_RANGE alone does
    // not free blocks on every filesystem, so neither reclaims space.
    let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    linux_fallocate(fd, mode, offset, len)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_fallocate(fd: i32, mode: i32, offset: u64, len: usize) -> std::io::Result<()> {
    #[cfg(not(all(target_env = "gnu", target_pointer_width = "32")))]
    use libc::fallocate as fallocate_at;
    #[cfg(all(target_env = "gnu", target_pointer_width = "32"))]
    use libc::fallocate64 as fallocate_at;

    // glibc's default off_t can be 32 bits on i686.
    let offset = offset.try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Allocation offset overflow",
        )
    })?;
    let len = len.try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Allocation length overflow",
        )
    })?;
    if unsafe { fallocate_at(fd, mode, offset, len) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// A freshly allocated, zero-filled IO buffer used by the discard fallback
/// path on platforms/fds without hole-punch support.
pub(crate) fn zeroed_io_buf(len: usize) -> crate::helpers::Qcow2IoBuf<u8> {
    let mut data = crate::helpers::Qcow2IoBuf::<u8>::new(len);
    data.zero_buf();
    data
}

/// Enable `O_DIRECT` on `fd`. A no-op on macOS, which has no `O_DIRECT`.
#[cfg(not(target_os = "windows"))]
pub(crate) fn set_direct_io(fd: i32) {
    #[cfg(not(target_os = "macos"))]
    // SAFETY: `F_SETFL` with `O_DIRECT` only updates the fd's status flags.
    unsafe {
        libc::fcntl(fd, libc::F_SETFL, libc::O_DIRECT);
    }
    #[cfg(target_os = "macos")]
    let _ = fd;
}

/// How read/write/discard are implemented, so that qcow2-rs can be
/// used with multiple io engine.
///
/// these methods are called for reading data from image, writing data
/// to image, and discarding range.
#[rustversion::attr(before(1.75), async_trait(?Send))]
#[rustversion::attr(since(1.75), allow(async_fn_in_trait))]
pub trait Qcow2IoOps {
    /// Adjust a separate data file's length after a durable metadata resize.
    async fn set_len(&self, _len: u64) -> Qcow2Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "File resizing is unavailable",
        )
        .into())
    }
    /// Reserve physical storage without discarding existing contents.
    async fn reserve(&self, _offset: u64, _len: usize) -> Qcow2Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Storage reservation is unavailable",
        )
        .into())
    }
    async fn read_to(&self, offset: u64, buf: &mut [u8]) -> Qcow2Result<usize>;
    async fn write_from(&self, offset: u64, buf: &[u8]) -> Qcow2Result<()>;
    async fn fallocate(&self, offset: u64, len: usize, flags: u32) -> Qcow2Result<()>;
    async fn fsync(&self, offset: u64, len: usize, flags: u32) -> Qcow2Result<()>;
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_reserve(fd: i32, offset: u64, len: usize) -> Qcow2Result<()> {
    match linux_fallocate(fd, 0, offset, len) {
        Err(error) if matches!(error.raw_os_error(), Some(libc::EOPNOTSUPP | libc::ENOSYS)) => {
            Err(std::io::Error::new(std::io::ErrorKind::Unsupported, error).into())
        }
        result => result.map_err(Into::into),
    }
}
