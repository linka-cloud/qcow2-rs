//! Linux image ownership using the same OFD permission bytes as QEMU.
use std::collections::{hash_map::Entry, HashMap};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

type FileId = (u64, u64);
type Region = (i32, i32);

// QEMU file-posix.c reserves bytes at 100 + bit for permissions in use and
// 200 + bit for permissions that must not be shared. The bits represent
// consistent read, write, write unchanged, and resize, in that order.
const READ_MARKERS: [Region; 2] = [(100, 1), (201, 3)];
const WRITE_MARKERS: [Region; 2] = [(101, 3), (200, 1)];
const READ_CONFLICTS: [Region; 2] = [(200, 1), (101, 3)];
const WRITE_CONFLICTS: [Region; 2] = [(200, 4), (100, 4)];

fn files() -> &'static Mutex<HashMap<FileId, LockedFile>> {
    static FILES: OnceLock<Mutex<HashMap<FileId, LockedFile>>> = OnceLock::new();
    FILES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn region_lock(fd: RawFd, command: i32, kind: i32, (offset, len): Region) -> io::Result<i32> {
    // SAFETY: flock consists of integer fields and accepts zero padding.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = offset as _;
    lock.l_len = len as _;
    loop {
        // SAFETY: the OFD commands take a pointer to the matching flock type.
        if unsafe { libc::fcntl(fd, command, &mut lock) } != -1 {
            return Ok(lock.l_type.into());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn contention(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "Could not lock image '{}'. Is another process using the image?",
            path.display()
        ),
    )
}

struct LockedFile {
    file: File,
    handles: usize,
    writer: bool,
}

impl LockedFile {
    fn mark(&self, regions: &[Region]) -> io::Result<()> {
        for &region in regions {
            region_lock(
                self.file.as_raw_fd(),
                libc::F_OFD_SETLK,
                libc::F_RDLCK,
                region,
            )?;
        }
        Ok(())
    }

    fn check(&self, regions: &[Region], path: &Path) -> io::Result<()> {
        for &region in regions {
            let kind = region_lock(
                self.file.as_raw_fd(),
                libc::F_OFD_GETLK,
                libc::F_WRLCK,
                region,
            )?;
            if kind != libc::F_UNLCK {
                return Err(contention(path));
            }
        }
        Ok(())
    }

    fn clear(&self, regions: &[Region]) {
        for &region in regions {
            let _ = region_lock(
                self.file.as_raw_fd(),
                libc::F_OFD_SETLK,
                libc::F_UNLCK,
                region,
            );
        }
    }

    fn upgrade(&mut self, path: &Path) -> io::Result<()> {
        if self.writer {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Image '{}' is already open for writing in this process",
                    path.display()
                ),
            ));
        }
        if let Err(error) = self
            .mark(&WRITE_MARKERS)
            .and_then(|()| self.check(&WRITE_CONFLICTS, path))
        {
            self.clear(&WRITE_MARKERS);
            return Err(error);
        }
        self.writer = true;
        Ok(())
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        // The backend may still have a duplicate of this open description.
        self.clear(&[(100, 4), (200, 4)]);
    }
}

/// A QEMU-compatible advisory lock for a Linux image file.
///
/// Keep this guard for the lifetime of the corresponding I/O handle. Local
/// readers and one local writer share an OFD lock so a commit can open its
/// backing image for writing. Callers still coordinate I/O between handles.
#[derive(Debug)]
pub struct ImageLock {
    id: FileId,
    writer: bool,
}

impl ImageLock {
    pub fn acquire(file: &File, path: &Path, read_only: bool) -> io::Result<Self> {
        Self::acquire_fd(file.as_raw_fd(), path, read_only)
    }

    pub(crate) fn acquire_fd(fd: RawFd, path: &Path, read_only: bool) -> io::Result<Self> {
        Self::acquire_inner(fd, path, read_only).map_err(|error| match error.raw_os_error() {
            Some(libc::EACCES | libc::EAGAIN) => contention(path),
            Some(_) => io::Error::new(
                error.kind(),
                format!("Could not lock image '{}': {error}", path.display()),
            ),
            None => error,
        })
    }

    fn acquire_inner(fd: RawFd, path: &Path, read_only: bool) -> io::Result<Self> {
        // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor owned by this call.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: duplicate is a valid descriptor and ownership is transferred.
        let file = unsafe { File::from_raw_fd(duplicate) };
        let meta = file.metadata()?;
        let id = (meta.dev(), meta.ino());
        let mut files = files().lock().unwrap();
        match files.entry(id) {
            Entry::Occupied(mut entry) => {
                let state = entry.get_mut();
                if !read_only {
                    state.upgrade(path)?;
                }
                state.handles += 1;
            }
            Entry::Vacant(entry) => {
                let mut state = LockedFile {
                    file,
                    handles: 1,
                    writer: false,
                };
                // Publish all markers before checking for conflicts, as QEMU does.
                state.mark(&READ_MARKERS)?;
                if read_only {
                    state.check(&READ_CONFLICTS, path)?;
                } else {
                    state.upgrade(path)?;
                }
                entry.insert(state);
            }
        }
        Ok(Self {
            id,
            writer: !read_only,
        })
    }
}

impl Drop for ImageLock {
    fn drop(&mut self) {
        let mut files = files().lock().unwrap();
        let state = files.get_mut(&self.id).expect("image lock is registered");
        state.handles -= 1;
        if state.handles == 0 {
            files.remove(&self.id);
        } else if self.writer {
            state.clear(&WRITE_MARKERS);
            state.writer = false;
        }
    }
}
