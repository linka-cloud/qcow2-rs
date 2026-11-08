mod support;
use qcow2_rs::{dev::*, helpers::Qcow2IoBuf, utils::qcow2_setup_dev_tokio};
use std::process::Command;
fn check(path: &std::path::Path) {
    if !support::qemu_supports_image(path) {
        return;
    }
    let out = Command::new("qemu-img")
        .current_dir(path.parent().unwrap())
        .args(["check", "-q"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}
#[tokio::test]
async fn resize_preserves_snapshots_zeros_regrowth_and_tracks_bitmaps() {
    for (version, extended, external) in [
        (2, false, false),
        (3, false, false),
        (3, true, false),
        (3, false, true),
        (3, true, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.qcow2");
        let mut options = CreateOptions {
            size: 1 << 20,
            version,
            extended_l2: extended,
            ..Default::default()
        };
        if external {
            options.data_file = Some(ExternalDataFile {
                path: "data.raw".into(),
                raw: true,
            });
        }
        create_qcow2(&path, &options).await.unwrap();
        let dev = qcow2_setup_dev_tokio(&path, &options.open_params)
            .await
            .unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(65536);
        buf.fill(0x5a);
        dev.write_at(&buf, 0).await.unwrap();
        if !external {
            dev.create_snapshot(
                SnapshotOptions {
                    id: "saved".into(),
                    ..Default::default()
                },
                b"VM",
            )
            .await
            .unwrap();
        }
        if version == 3 {
            dev.create_bitmap("track", 9, true).await.unwrap();
        }
        assert!(dev.resize(512, false).await.is_err());
        dev.resize(512, true).await.unwrap();
        dev.resize(2 << 20, false).await.unwrap();
        dev.read_at(&mut buf, 0).await.unwrap();
        assert!(buf[..512].iter().all(|b| *b == 0x5a));
        assert!(buf[512..].iter().all(|b| *b == 0));
        dev.read_at(&mut buf, 1 << 20).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0));
        dev.check().await.unwrap();
        if !external {
            dev.restore_snapshot("saved").await.unwrap();
            assert_eq!(dev.info.virtual_size(), 1 << 20);
            dev.read_at(&mut buf, 0).await.unwrap();
            assert!(buf.iter().all(|b| *b == 0x5a));
        }
        dev.check().await.unwrap();
        drop(dev);
        check(&path);
    }
}
#[tokio::test]
async fn growth_masks_larger_backing_and_handles_l1_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    std::fs::write(dir.path().join("base.raw"), vec![0x5a; 2 << 20]).unwrap();
    for version in [2, 3] {
        let path = path.with_extension(version.to_string());
        let options = CreateOptions {
            size: 512,
            version,
            cluster_bits: 9,
            backing: Some(BackingFile {
                path: "base.raw".into(),
                format: Some(BackingFormat::Raw),
            }),
            ..Default::default()
        };
        create_qcow2(&path, &options).await.unwrap();
        let dev = qcow2_setup_dev_tokio(&path, &options.open_params)
            .await
            .unwrap();
        dev.resize(1 << 20, false).await.unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(4096);
        dev.read_at(&mut buf, 0).await.unwrap();
        assert!(buf[..512].iter().all(|b| *b == 0x5a));
        assert!(buf[512..].iter().all(|b| *b == 0));
        dev.check().await.unwrap();
        drop(dev);
        check(&path);
    }
}

struct FaultIo {
    inner: qcow2_rs::tokio_io::Qcow2IoTokio,
    fail_len: std::rc::Rc<std::cell::Cell<bool>>,
    fail_write: std::rc::Rc<std::cell::Cell<bool>>,
}
impl qcow2_rs::ops::Qcow2IoOps for FaultIo {
    async fn read_to(&self, off: u64, buf: &mut [u8]) -> qcow2_rs::error::Qcow2Result<usize> {
        self.inner.read_to(off, buf).await
    }
    async fn write_from(&self, off: u64, buf: &[u8]) -> qcow2_rs::error::Qcow2Result<()> {
        self.inner.write_from(off, buf).await?;
        if self.fail_write.replace(false) {
            return Err("injected external write failure".into());
        }
        Ok(())
    }
    async fn fsync(&self, off: u64, len: usize, flags: u32) -> qcow2_rs::error::Qcow2Result<()> {
        self.inner.fsync(off, len, flags).await
    }
    async fn fallocate(
        &self,
        off: u64,
        len: usize,
        flags: u32,
    ) -> qcow2_rs::error::Qcow2Result<()> {
        self.inner.fallocate(off, len, flags).await
    }
    async fn set_len(&self, len: u64) -> qcow2_rs::error::Qcow2Result<()> {
        if self.fail_len.replace(false) {
            return Err("injected data-file resize failure".into());
        }
        self.inner.set_len(len).await
    }
}
#[tokio::test]
async fn external_failures_retain_dirty_state_and_repair_raw_consistency() {
    use qcow2_rs::tokio_io::Qcow2IoTokio;
    use std::{cell::Cell, rc::Rc};
    for length_failure in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.qcow2");
        let data = dir.path().join("data.raw");
        let options = CreateOptions {
            size: 1 << 20,
            data_file: Some(ExternalDataFile {
                path: "data.raw".into(),
                raw: true,
            }),
            ..Default::default()
        };
        create_qcow2(&path, &options).await.unwrap();
        let root = FaultIo {
            inner: Qcow2IoTokio::try_new(&path, false, false).await.unwrap(),
            fail_len: Rc::new(Cell::new(false)),
            fail_write: Rc::new(Cell::new(false)),
        };
        let (mut dev, _) = qcow2_rs::utils::qcow2_alloc_dev(&path, root, &options.open_params)
            .await
            .unwrap();
        dev.set_data_file(FaultIo {
            inner: Qcow2IoTokio::try_new(&data, false, false).await.unwrap(),
            fail_len: Rc::new(Cell::new(length_failure)),
            fail_write: Rc::new(Cell::new(!length_failure)),
        })
        .unwrap();
        dev.qcow2_prep_io().await.unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(512);
        buf.fill(0xa7);
        if length_failure {
            assert!(dev.resize(2 << 20, false).await.is_err());
        } else {
            assert!(dev.write_at(&buf, 0).await.is_err());
        }
        let header = std::fs::read(&path).unwrap();
        assert_ne!(
            u64::from_be_bytes(header[72..80].try_into().unwrap()) & 1,
            0
        );
        assert!(dev.read_at(&mut buf, 0).await.is_err());
        dev.repair_refcounts().await.unwrap();
        dev.check().await.unwrap();
        assert!(std::fs::read(&data).unwrap().iter().all(|b| *b == 0));
    }
}

#[tokio::test]
async fn safe_rebase_preserves_snapshots_and_unsafe_rebase_tracks_changes() {
    use qcow2_rs::tokio_io::Qcow2IoTokio;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    let a = dir.path().join("a.raw");
    let b = dir.path().join("b.raw");
    std::fs::write(&a, vec![0x5a; 1 << 20]).unwrap();
    std::fs::write(&b, vec![0xa7; 1 << 20]).unwrap();
    let options = CreateOptions {
        size: 1 << 20,
        backing: Some(BackingFile {
            path: "a.raw".into(),
            format: Some(BackingFormat::Raw),
        }),
        ..Default::default()
    };
    create_qcow2(&path, &options).await.unwrap();
    let mut dev = qcow2_setup_dev_tokio(&path, &options.open_params)
        .await
        .unwrap();
    dev.create_snapshot(
        SnapshotOptions {
            id: "saved".into(),
            ..Default::default()
        },
        b"VM state",
    )
    .await
    .unwrap();
    dev.create_bitmap("tracking", 9, true).await.unwrap();
    let source = Qcow2IoTokio::try_new(&b, true, false).await.unwrap();
    dev.rebase(
        Some(RebaseTarget {
            file: BackingFile {
                path: "b.raw".into(),
                format: None,
            },
            source: Backing::raw(source, 1 << 20),
        }),
        true,
    )
    .await
    .unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
    assert!(dev
        .read_bitmap("tracking")
        .await
        .unwrap()
        .iter()
        .all(|b| *b == 0));
    dev.restore_snapshot("saved").await.unwrap();
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
    let mut state = vec![0; 8];
    dev.read_snapshot_vm_state("saved", 0, &mut state)
        .await
        .unwrap();
    assert_eq!(state, b"VM state");
    dev.rebase(None, true).await.unwrap();
    assert!(!dev.capabilities().await.raw_external_data);
    dev.check().await.unwrap();
    drop(dev);
    check(&path);
    let other = dir.path().join("unsafe.qcow2");
    create_qcow2(&other, &options).await.unwrap();
    let mut dev = qcow2_setup_dev_tokio(&other, &options.open_params)
        .await
        .unwrap();
    dev.create_bitmap("tracking", 9, true).await.unwrap();
    let source = Qcow2IoTokio::try_new(&b, true, false).await.unwrap();
    dev.rebase(
        Some(RebaseTarget {
            file: BackingFile {
                path: "b.raw".into(),
                format: None,
            },
            source: Backing::raw(source, 1 << 20),
        }),
        false,
    )
    .await
    .unwrap();
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0xa7));
    assert!(dev
        .read_bitmap("tracking")
        .await
        .unwrap()
        .iter()
        .all(|b| *b == 0xff));
    dev.check().await.unwrap();
}

#[tokio::test]
async fn commit_to_qcow2_preserves_source_and_target_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.qcow2");
    let overlay = dir.path().join("overlay.qcow2");
    let options = CreateOptions {
        size: 1 << 20,
        ..Default::default()
    };
    create_qcow2(&base, &options).await.unwrap();
    let target = qcow2_setup_dev_tokio(&base, &options.open_params)
        .await
        .unwrap();
    let mut whole = Qcow2IoBuf::<u8>::new(1 << 20);
    whole.fill(0x5a);
    target.write_at(&whole, 0).await.unwrap();
    target
        .create_snapshot(
            SnapshotOptions {
                id: "base-old".into(),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
    let options = CreateOptions {
        backing: Some(BackingFile {
            path: "base.qcow2".into(),
            format: Some(BackingFormat::Qcow2),
        }),
        ..options
    };
    create_qcow2(&overlay, &options).await.unwrap();
    let source = qcow2_setup_dev_tokio(&overlay, &options.open_params)
        .await
        .unwrap();
    source
        .create_snapshot(
            SnapshotOptions {
                id: "source-old".into(),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
    whole[..512].fill(0xa7);
    source.write_at(&whole[..512], 512).await.unwrap();
    source.commit_to(&target, true).await.unwrap();
    source.read_at(&mut whole, 0).await.unwrap();
    assert!(whole[..512].iter().all(|b| *b == 0x5a));
    assert!(whole[512..1024].iter().all(|b| *b == 0xa7));
    target.read_at(&mut whole, 0).await.unwrap();
    assert!(whole[512..1024].iter().all(|b| *b == 0xa7));
    source.check().await.unwrap();
    target.check().await.unwrap();
    source.restore_snapshot("source-old").await.unwrap();
    source.read_at(&mut whole, 0).await.unwrap();
    assert!(whole.iter().all(|b| *b == 0x5a));
    drop(source);
    target.restore_snapshot("base-old").await.unwrap();
    target.read_at(&mut whole, 0).await.unwrap();
    assert!(whole.iter().all(|b| *b == 0x5a));
    drop(target);
    check(&base);
    check(&overlay);
}
#[tokio::test]
async fn raw_backing_commit_preserves_tail_and_refreshes_empty_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.raw");
    let overlay = dir.path().join("overlay.qcow2");
    std::fs::write(&base, vec![0x5a; 2 << 20]).unwrap();
    let options = CreateOptions {
        size: 1 << 20,
        backing: Some(BackingFile {
            path: "base.raw".into(),
            format: Some(BackingFormat::Raw),
        }),
        ..Default::default()
    };
    create_qcow2(&overlay, &options).await.unwrap();
    let source = qcow2_setup_dev_tokio(&overlay, &options.open_params)
        .await
        .unwrap();
    source
        .create_snapshot(
            SnapshotOptions {
                id: "old".into(),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0xa7);
    source.write_at(&buf, 512).await.unwrap();
    let raw = qcow2_rs::tokio_io::Qcow2IoTokio::try_new(&base, false, false)
        .await
        .unwrap();
    source.commit_to_raw(&raw, &base, true).await.unwrap();
    let bytes = std::fs::read(&base).unwrap();
    assert_eq!(bytes.len(), 2 << 20);
    assert!(bytes[..512].iter().all(|b| *b == 0x5a));
    assert!(bytes[512..1024].iter().all(|b| *b == 0xa7));
    assert!(bytes[1024..].iter().all(|b| *b == 0x5a));
    source.read_at(&mut buf, 512).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0xa7));
    source.restore_snapshot("old").await.unwrap();
    source.read_at(&mut buf, 512).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
    source.check().await.unwrap();
}
