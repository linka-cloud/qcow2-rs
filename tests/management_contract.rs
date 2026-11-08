use qcow2_rs::{
    dev::*,
    error::Qcow2Result,
    helpers::Qcow2IoBuf,
    ops::Qcow2IoOps,
    tokio_io::Qcow2IoTokio,
    utils::{qcow2_alloc_dev, qcow2_setup_dev_tokio},
};
use std::{cell::Cell, rc::Rc};
#[tokio::test]
async fn read_only_management_and_invalid_amendments_leave_all_storage_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("disk");
    let target_path = dir.path().join("target");
    let options = CreateOptions {
        size: 128 << 10,
        ..Default::default()
    };
    for path in [&image, &target_path] {
        create_qcow2(path, &options).await.unwrap();
    }
    let dev = qcow2_setup_dev_tokio(&image, &options.open_params)
        .await
        .unwrap();
    dev.create_snapshot(
        SnapshotOptions {
            id: "saved".into(),
            ..Default::default()
        },
        &[],
    )
    .await
    .unwrap();
    dev.create_bitmap("tracking", 9, true).await.unwrap();
    drop(dev);
    let before = std::fs::read(&image).unwrap();
    let target_before = std::fs::read(&target_path).unwrap();
    let mut ro = options.open_params.clone();
    ro.set_read_only(true);
    let mut dev = qcow2_setup_dev_tokio(&image, &ro).await.unwrap();
    let target = qcow2_setup_dev_tokio(&target_path, &options.open_params)
        .await
        .unwrap();
    assert!(dev.resize(256 << 10, false).await.is_err());
    assert!(dev.preallocate(Preallocation::Full).await.is_err());
    assert!(dev
        .amend(AmendOptions {
            refcount_order: Some(6),
            ..Default::default()
        })
        .await
        .is_err());
    assert!(dev.rebase(None, true).await.is_err());
    assert!(dev.commit_to(&target, true).await.is_err());
    drop(dev);
    let mut dev = qcow2_setup_dev_tokio(&image, &options.open_params)
        .await
        .unwrap();
    for amendment in [
        AmendOptions {
            version: Some(2),
            ..Default::default()
        },
        AmendOptions {
            refcount_order: Some(0),
            ..Default::default()
        },
        AmendOptions {
            refcount_order: Some(7),
            ..Default::default()
        },
        AmendOptions {
            version: Some(4),
            ..Default::default()
        },
    ] {
        assert!(dev.amend(amendment).await.is_err());
        assert_eq!(std::fs::read(&image).unwrap(), before);
    }
    assert!(dev.commit_to(&target, true).await.is_err());
    assert_eq!(std::fs::read(&image).unwrap(), before);
    assert_eq!(std::fs::read(&target_path).unwrap(), target_before);
    dev.check().await.unwrap();
}
struct NoSpace {
    inner: Qcow2IoTokio,
    reserve_attempts: Rc<Cell<usize>>,
}
impl Qcow2IoOps for NoSpace {
    async fn read_to(&self, off: u64, bytes: &mut [u8]) -> Qcow2Result<usize> {
        self.inner.read_to(off, bytes).await
    }
    async fn write_from(&self, off: u64, bytes: &[u8]) -> Qcow2Result<()> {
        self.inner.write_from(off, bytes).await
    }
    async fn fallocate(&self, off: u64, len: usize, flags: u32) -> Qcow2Result<()> {
        self.inner.fallocate(off, len, flags).await
    }
    async fn fsync(&self, off: u64, len: usize, flags: u32) -> Qcow2Result<()> {
        self.inner.fsync(off, len, flags).await
    }
    async fn reserve(&self, _: u64, _: usize) -> Qcow2Result<()> {
        self.reserve_attempts.set(self.reserve_attempts.get() + 1);
        // ENOSPC on Unix / ERROR_DISK_FULL on Windows, without filling the
        // machine's filesystem. This is not the Unsupported fallback.
        Err(std::io::Error::from_raw_os_error(if cfg!(windows) { 112 } else { 28 }).into())
    }
}
#[tokio::test]
async fn allocation_exhaustion_blocks_io_and_repairs_without_changing_guest_or_snapshot() {
    for extended in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk");
        let options = CreateOptions {
            size: 128 << 10,
            extended_l2: extended,
            ..Default::default()
        };
        create_qcow2(&path, &options).await.unwrap();
        let dev = qcow2_setup_dev_tokio(&path, &options.open_params)
            .await
            .unwrap();
        let mut model = Qcow2IoBuf::<u8>::new(128 << 10);
        model.fill(0x31);
        dev.write_at(&model, 0).await.unwrap();
        dev.flush_meta().await.unwrap();
        dev.create_snapshot(
            SnapshotOptions {
                id: "saved".into(),
                ..Default::default()
            },
            b"VM",
        )
        .await
        .unwrap();
        drop(dev);
        let attempts = Rc::new(Cell::new(0));
        let io = NoSpace {
            inner: Qcow2IoTokio::try_new(&path, false, false).await.unwrap(),
            reserve_attempts: attempts.clone(),
        };
        let (dev, _) = qcow2_alloc_dev(&path, io, &options.open_params)
            .await
            .unwrap();
        dev.qcow2_prep_io().await.unwrap();
        assert!(dev.preallocate(Preallocation::Fallocate).await.is_err());
        assert_eq!(attempts.get(), 1);
        assert!(dev.capabilities().await.needs_repair);
        assert!(dev.read_at(&mut model, 0).await.is_err());
        drop(dev);
        let mut params = options.open_params.clone();
        params.set_repair_mode(true);
        let dev = qcow2_setup_dev_tokio(&path, &params).await.unwrap();
        dev.repair_refcounts().await.unwrap();
        dev.read_at(&mut model, 0).await.unwrap();
        assert!(model.iter().all(|b| *b == 0x31));
        dev.restore_snapshot("saved").await.unwrap();
        dev.read_at(&mut model, 0).await.unwrap();
        assert!(model.iter().all(|b| *b == 0x31));
        let mut vm = [0; 2];
        dev.read_snapshot_vm_state("saved", 0, &mut vm)
            .await
            .unwrap();
        assert_eq!(&vm, b"VM");
        dev.check().await.unwrap();
    }
}
