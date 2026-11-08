use qcow2_rs::dev::{Qcow2Dev, Qcow2DevParams};
use qcow2_rs::error::Qcow2Result;
use qcow2_rs::helpers::Qcow2IoBuf;
use qcow2_rs::ops::Qcow2IoOps;
use qcow2_rs::tokio_io::Qcow2IoTokio;
use qcow2_rs::utils::{make_temp_qcow2_img, qcow2_alloc_dev, qcow2_setup_dev_tokio};
use std::cell::{Cell, RefCell};
use std::io::{Seek, SeekFrom, Write};
use std::process::Command;
use std::rc::Rc;

const FLAGS: u64 = 1 << 63;

fn flagged_image() -> tempfile::NamedTempFile {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let mut file = image.reopen().unwrap();
    file.seek(SeekFrom::Start(88)).unwrap();
    file.write_all(&FLAGS.to_be_bytes()).unwrap();
    image
}

#[tokio::test]
async fn reads_preserve_autoclear_flags_and_writes_clear_them() {
    let image = flagged_image();
    let before = std::fs::read(image.path()).unwrap();
    for read_only in [true, false] {
        let params = Qcow2DevParams::new(9, None, None, read_only, false);
        let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(512);
        dev.read_at(&mut buf, 0).await.unwrap();
        assert_eq!(std::fs::read(image.path()).unwrap(), before);
        if !read_only {
            buf.fill(0x5a);
            dev.write_at(&buf, 0).await.unwrap();
            dev.flush_meta().await.unwrap();
            let after = std::fs::read(image.path()).unwrap();
            assert_eq!(&after[88..96], &[0; 8]);
            // Clearing the flags must preserve the rest of the first block.
            assert_eq!(&after[..88], &before[..88]);
            assert_eq!(&after[96..512], &before[96..512]);
        }
    }
}

#[derive(Default)]
struct State {
    fail_write: Cell<bool>,
    fail_sync: Cell<bool>,
    events: RefCell<Vec<&'static str>>,
}

struct FaultIo {
    inner: Qcow2IoTokio,
    state: Rc<State>,
}

impl Qcow2IoOps for FaultIo {
    async fn read_to(&self, offset: u64, buf: &mut [u8]) -> Qcow2Result<usize> {
        self.inner.read_to(offset, buf).await
    }

    async fn write_from(&self, offset: u64, buf: &[u8]) -> Qcow2Result<()> {
        if offset == 0 {
            self.state.events.borrow_mut().push("header");
            if self.state.fail_write.replace(false) {
                return Err("injected header write failure".into());
            }
        } else {
            self.state.events.borrow_mut().push("data");
        }
        self.inner.write_from(offset, buf).await
    }

    async fn fsync(&self, offset: u64, len: usize, flags: u32) -> Qcow2Result<()> {
        self.state.events.borrow_mut().push("sync");
        if self.state.fail_sync.replace(false) {
            return Err("injected header sync failure".into());
        }
        self.inner.fsync(offset, len, flags).await
    }

    async fn fallocate(&self, offset: u64, len: usize, flags: u32) -> Qcow2Result<()> {
        self.state.events.borrow_mut().push("allocate");
        self.inner.fallocate(offset, len, flags).await
    }
}

async fn fault_device(path: &std::path::Path, state: Rc<State>) -> Qcow2Dev<FaultIo> {
    let io = FaultIo {
        inner: Qcow2IoTokio::new(path, false, false).await,
        state,
    };
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let (dev, _) = qcow2_alloc_dev(path, io, &params).await.unwrap();
    dev.qcow2_prep_io().await.unwrap();
    dev
}

#[tokio::test]
async fn autoclear_failures_prevent_guest_mutation_and_can_be_retried() {
    for sync_failure in [false, true] {
        let image = flagged_image();
        let before = std::fs::read(image.path()).unwrap();
        let state = Rc::new(State::default());
        state.fail_write.set(!sync_failure);
        state.fail_sync.set(sync_failure);
        let dev = fault_device(image.path(), state.clone()).await;
        let mut buf = Qcow2IoBuf::<u8>::new(512);
        buf.fill(0x5a);
        assert!(dev.write_at(&buf, 0).await.is_err());
        assert!(!dev.need_flush_meta());
        let events = state.events.borrow().clone();
        assert_eq!(
            events,
            if sync_failure {
                vec!["header", "sync"]
            } else {
                vec!["header"]
            }
        );
        if !sync_failure {
            assert_eq!(std::fs::read(image.path()).unwrap(), before);
        }

        state.events.borrow_mut().clear();
        dev.write_at(&buf, 0).await.unwrap();
        let events = state.events.borrow().clone();
        assert_eq!(&events[..2], &["header", "sync"]);
        dev.flush_meta().await.unwrap();
    }
}

#[cfg(not(target_os = "windows"))]
#[tokio::test]
async fn autoclear_updates_work_with_direct_io() {
    let image = flagged_image();
    let params = Qcow2DevParams::new(9, None, None, false, true);
    let dev = qcow2_rs::utils::qcow2_setup_dev_sync(image.path(), &params).unwrap();
    dev.qcow2_prep_io().await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0x5a);
    dev.write_at(&buf, 0).await.unwrap();
    dev.flush_meta().await.unwrap();
    assert_eq!(&std::fs::read(image.path()).unwrap()[88..96], &[0; 8]);
}

#[tokio::test]
async fn discard_invalidates_persistent_bitmap_metadata() {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(1 << 16);
    buf.fill(0x5a);
    dev.write_at(&buf, 0).await.unwrap();
    dev.flush_meta().await.unwrap();
    drop(dev);
    let output = Command::new("qemu-img")
        .args(["bitmap", "--add"])
        .arg(image.path())
        .arg("tracking")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let before = std::fs::read(image.path()).unwrap();
    assert_eq!(
        u64::from_be_bytes(before[88..96].try_into().unwrap()) & 1,
        1
    );

    let ro = Qcow2DevParams::new(9, None, None, true, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &ro).await.unwrap();
    assert!(dev.discard(0, 1 << 16).await.is_err());
    assert_eq!(std::fs::read(image.path()).unwrap(), before);
    assert!(!dev.need_flush_meta());
    drop(dev);

    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    dev.discard(0, 1 << 16).await.unwrap();
    dev.flush_meta().await.unwrap();
    let after = std::fs::read(image.path()).unwrap();
    assert_eq!(&after[88..96], &[0; 8]);
    assert_eq!(&after[..88], &before[..88]);
    assert_eq!(&after[96..512], &before[96..512]);
}
