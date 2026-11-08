use qcow2_rs::dev::{Qcow2Dev, Qcow2DevParams};
use qcow2_rs::error::Qcow2Result;
use qcow2_rs::helpers::Qcow2IoBuf;
use qcow2_rs::ops::Qcow2IoOps;
use qcow2_rs::tokio_io::Qcow2IoTokio;
use qcow2_rs::utils::{make_temp_qcow2_img, qcow2_alloc_dev, qcow2_setup_dev_tokio};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use tokio::sync::Notify;

const CLUSTER: usize = 1 << 16;

fn be64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn check(path: &Path) {
    let out = Command::new("qemu-img")
        .args(["check", "-q"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

async fn populated_image() -> tempfile::NamedTempFile {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(CLUSTER);
    buf.fill(0x5a);
    dev.write_at(&buf, 0).await.unwrap();
    dev.flush_meta().await.unwrap();
    dev.fsync_range(0, 0).await.unwrap();
    image
}

#[tokio::test]
async fn discard_preserves_shared_host_cluster() {
    let image = populated_image().await;
    let mut bytes = std::fs::read(image.path()).unwrap();
    let l1 = be64(&bytes, 40) as usize;
    let l2 = (be64(&bytes, l1) & 0x00ff_ffff_ffff_fe00) as usize;
    let host = be64(&bytes, l2) & 0x00ff_ffff_ffff_fe00;
    // Two guest clusters share the same data, with copied flags clear.
    bytes[l2..l2 + 8].copy_from_slice(&host.to_be_bytes());
    bytes[l2 + 8..l2 + 16].copy_from_slice(&host.to_be_bytes());
    let rt = be64(&bytes, 48) as usize;
    let rb = be64(&bytes, rt) as usize;
    let ref_offset = rb + (host as usize / CLUSTER) * 2;
    assert!(
        ref_offset + 2 <= bytes.len(),
        "l1={l1:x} l2={l2:x} host={host:x} rt={rt:x} rb={rb:x}"
    );
    bytes[ref_offset..ref_offset + 2].copy_from_slice(&2_u16.to_be_bytes());
    std::fs::write(image.path(), bytes).unwrap();
    check(image.path());

    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    dev.discard(0, CLUSTER as u64).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(CLUSTER);
    dev.read_at(&mut buf, CLUSTER as u64).await.unwrap();
    assert!(
        buf.iter().all(|b| *b == 0x5a),
        "discard damaged a live alias"
    );
    dev.read_at(&mut buf, 0).await.unwrap();
    // Shared mappings are conservatively skipped by advisory discard.
    assert!(buf.iter().all(|b| *b == 0x5a));
    dev.flush_meta().await.unwrap();
    drop(dev);
    check(image.path());
}

#[tokio::test]
async fn discard_reveals_backing_data_without_modifying_backing() {
    let backing = populated_image().await;
    let before = std::fs::read(backing.path()).unwrap();
    let overlay = tempfile::NamedTempFile::new().unwrap();
    let out = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
        .arg(backing.path())
        .arg(overlay.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(overlay.path(), &params)
        .await
        .unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(CLUSTER);
    buf.fill(0xa5);
    dev.write_at(&buf, 0).await.unwrap();
    dev.discard(0, CLUSTER as u64).await.unwrap();
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
    dev.flush_meta().await.unwrap();
    drop(dev);
    assert_eq!(std::fs::read(backing.path()).unwrap(), before);
    check(overlay.path());
}

#[tokio::test]
async fn discard_on_read_only_device_preserves_mapping_and_data() {
    let image = populated_image().await;
    let before = std::fs::read(image.path()).unwrap();
    let params = Qcow2DevParams::new(9, None, None, true, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    assert!(dev.discard(0, CLUSTER as u64).await.is_err());
    let mut buf = Qcow2IoBuf::<u8>::new(CLUSTER);
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
    assert!(!dev.need_flush_meta());
    assert_eq!(std::fs::read(image.path()).unwrap(), before);
}

#[derive(Debug, PartialEq)]
enum Event {
    Read(u64),
    Write(u64),
    Sync,
    Punch(u64),
}

#[derive(Default)]
struct State {
    events: RefCell<Vec<Event>>,
    fail_write: Cell<bool>,
    fail_sync: Cell<bool>,
    pause_punch: Cell<bool>,
    punching: Notify,
    resume: Notify,
}

struct RecordingIo {
    inner: Qcow2IoTokio,
    state: Rc<State>,
}

impl Qcow2IoOps for RecordingIo {
    async fn read_to(&self, offset: u64, buf: &mut [u8]) -> Qcow2Result<usize> {
        self.state.events.borrow_mut().push(Event::Read(offset));
        self.inner.read_to(offset, buf).await
    }

    async fn write_from(&self, offset: u64, buf: &[u8]) -> Qcow2Result<()> {
        self.state.events.borrow_mut().push(Event::Write(offset));
        if self.state.fail_write.replace(false) {
            return Err("injected metadata write failure".into());
        }
        self.inner.write_from(offset, buf).await
    }

    async fn fsync(&self, offset: u64, len: usize, flags: u32) -> Qcow2Result<()> {
        self.state.events.borrow_mut().push(Event::Sync);
        if self.state.fail_sync.replace(false) {
            return Err("injected metadata sync failure".into());
        }
        self.inner.fsync(offset, len, flags).await
    }

    async fn fallocate(&self, offset: u64, len: usize, flags: u32) -> Qcow2Result<()> {
        self.state.events.borrow_mut().push(Event::Punch(offset));
        if self.state.pause_punch.replace(false) {
            self.state.punching.notify_one();
            self.state.resume.notified().await;
        }
        self.inner.fallocate(offset, len, flags).await
    }
}

async fn recording_device(path: &Path, state: Rc<State>) -> Qcow2Dev<RecordingIo> {
    let io = RecordingIo {
        inner: Qcow2IoTokio::new(path, false, false).await,
        state: state.clone(),
    };
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let (dev, _) = qcow2_alloc_dev(path, io, &params).await.unwrap();
    dev.qcow2_prep_io().await.unwrap();
    state.events.borrow_mut().clear();
    dev
}

#[tokio::test]
async fn discard_failure_does_not_release_or_punch_host_data() {
    for fail_sync in [false, true] {
        let image = populated_image().await;
        let before = std::fs::read(image.path()).unwrap();
        let l2 = (be64(&before, be64(&before, 40) as usize) & 0x00ff_ffff_ffff_fe00) as usize;
        let host = (be64(&before, l2) & 0x00ff_ffff_ffff_fe00) as usize;
        let rb = be64(&before, be64(&before, 48) as usize) as usize;
        let ref_offset = rb + (host / CLUSTER) * 2;
        let state = Rc::new(State::default());
        let dev = recording_device(image.path(), state.clone()).await;
        state.fail_sync.set(fail_sync);
        state.fail_write.set(!fail_sync);
        assert!(dev.discard(0, CLUSTER as u64).await.is_err());
        assert!(!state
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, Event::Punch(_))));
        // Even after a later cache flush, the failed discard must retain its
        // reference. A possible leaked allocation is safer than live data loss.
        dev.flush_meta().await.unwrap();
        dev.fsync_range(0, 0).await.unwrap();
        let after = std::fs::read(image.path()).unwrap();
        assert_eq!(&after[host..host + CLUSTER], &before[host..host + CLUSTER]);
        assert_eq!(&after[ref_offset..ref_offset + 2], &1_u16.to_be_bytes());
    }
}

#[tokio::test]
async fn failed_metadata_flush_can_be_retried() {
    let image = populated_image().await;
    let state = Rc::new(State::default());
    let dev = recording_device(image.path(), state.clone()).await;
    let mut buf = Qcow2IoBuf::<u8>::new(CLUSTER);
    buf.fill(0xa5);
    dev.write_at(&buf, CLUSTER as u64).await.unwrap();
    state.fail_write.set(true);
    assert!(dev.flush_meta().await.is_err());
    assert!(dev.need_flush_meta());
    dev.flush_meta().await.unwrap();
    dev.fsync_range(0, 0).await.unwrap();
    drop(dev);
    check(image.path());
    let params = Qcow2DevParams::new(9, None, None, true, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    let mut read = Qcow2IoBuf::<u8>::new(CLUSTER);
    dev.read_at(&mut read, CLUSTER as u64).await.unwrap();
    assert_eq!(&read[..], &buf[..]);
}

#[tokio::test]
async fn discard_blocks_io_until_durable_removal_and_reclamation_finish() {
    let image = populated_image().await;
    let state = Rc::new(State::default());
    let dev = recording_device(image.path(), state.clone()).await;
    state.pause_punch.set(true);
    let mut written = Qcow2IoBuf::<u8>::new(CLUSTER);
    written.fill(0xa5);
    let mut read = Qcow2IoBuf::<u8>::new(CLUSTER);

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (discarded, ()) = tokio::join!(dev.discard(0, CLUSTER as u64), async {
            state.punching.notified().await;
            let mut writer = std::pin::pin!(dev.write_at(&written, 0));
            let mut reader = std::pin::pin!(dev.read_at(&mut read, 0));
            let events_before = state.events.borrow().len();
            assert!(futures::poll!(&mut writer).is_pending());
            assert!(futures::poll!(&mut reader).is_pending());
            assert_eq!(state.events.borrow().len(), events_before);
            state.resume.notify_one();
            writer.await.unwrap();
            reader.await.unwrap();
        });
        discarded.unwrap();
    })
    .await
    .unwrap();

    {
        let events = state.events.borrow();
        let punch = events
            .iter()
            .position(|e| matches!(e, Event::Punch(_)))
            .unwrap();
        let last_write = events[..punch]
            .iter()
            .rposition(|e| matches!(e, Event::Write(_)))
            .unwrap();
        assert!(events[last_write + 1..punch].contains(&Event::Sync));
    }
    dev.flush_meta().await.unwrap();
    dev.read_at(&mut read, 0).await.unwrap();
    assert_eq!(&read[..], &written[..]);
    drop(dev);
    check(image.path());
}
