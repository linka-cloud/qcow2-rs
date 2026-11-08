//! Kill a separate process at instrumented I/O boundaries. This tests loss of
//! process state/destructors, not loss of the host filesystem's volatile cache.
mod support;
use qcow2_rs::{
    dev::*,
    error::Qcow2Result,
    helpers::Qcow2IoBuf,
    ops::Qcow2IoOps,
    tokio_io::Qcow2IoTokio,
    utils::{qcow2_alloc_dev, qcow2_setup_dev_tokio},
};
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    rc::Rc,
    time::{Duration, Instant},
};
const SIZE: usize = 128 << 10;
const CS: usize = 16 << 10;
struct Control {
    operation: String,
    point: String,
    marker: PathBuf,
    initial_l1: u64,
    publish: Cell<bool>,
    dirty: Cell<bool>,
    payload: Cell<bool>,
}
impl Control {
    async fn stop(&self) {
        std::fs::write(&self.marker, b"checkpoint reached").unwrap();
        std::future::pending::<()>().await;
    }
}
struct CrashIo {
    inner: Qcow2IoTokio,
    control: Option<Rc<Control>>,
}
impl Qcow2IoOps for CrashIo {
    async fn read_to(&self, off: u64, data: &mut [u8]) -> Qcow2Result<usize> {
        self.inner.read_to(off, data).await
    }
    async fn write_from(&self, off: u64, data: &[u8]) -> Qcow2Result<()> {
        if let Some(c) = &self.control {
            if off == 0 && data.len() >= 112 {
                let n = |at| u64::from_be_bytes(data[at..at + 8].try_into().unwrap());
                let publication = match c.operation.as_str() {
                    "resize" | "external-resize" => n(24) == (SIZE * 2) as u64,
                    "amend" => n(72) & 16 != 0,
                    "rebase" => data.windows(5).any(|b| b == b"b.raw"),
                    "commit" => c.payload.get() && n(40) != c.initial_l1,
                    "preallocate" => n(40) != c.initial_l1,
                    _ => unreachable!(),
                };
                if publication && c.point == "before" {
                    c.stop().await;
                }
                c.publish.set(publication);
                c.dirty.set(n(72) & 1 != 0);
            }
            if c.operation == "commit" && data.len() == CS && data[..512].iter().all(|b| *b == 0xa7)
            {
                c.payload.set(true);
                if c.point == "payload" {
                    self.inner.write_from(off, &data[..CS / 2]).await?;
                    c.stop().await;
                }
            }
        }
        self.inner.write_from(off, data).await
    }
    async fn fsync(&self, off: u64, len: usize, flags: u32) -> Qcow2Result<()> {
        self.inner.fsync(off, len, flags).await?;
        if let Some(c) = &self.control {
            if c.point == "after" && c.publish.get() || c.point == "dirty" && c.dirty.get() {
                c.stop().await;
            }
        }
        Ok(())
    }
    async fn fallocate(&self, off: u64, len: usize, flags: u32) -> Qcow2Result<()> {
        self.inner.fallocate(off, len, flags).await
    }
    async fn reserve(&self, off: u64, len: usize) -> Qcow2Result<()> {
        self.inner.reserve(off, len).await
    }
    async fn set_len(&self, len: u64) -> Qcow2Result<()> {
        self.inner.set_len(len).await
    }
}
async fn custom(path: &Path, control: Option<Rc<Control>>, ro: bool) -> Qcow2Dev<CrashIo> {
    let io = CrashIo {
        inner: Qcow2IoTokio::try_new(path, ro, false).await.unwrap(),
        control,
    };
    let params = Qcow2DevParams::new(9, None, None, ro, false);
    let (mut dev, _) = qcow2_alloc_dev(path, io, &params).await.unwrap();
    if dev.info.external_data() {
        let payload = path.parent().unwrap().join("payload.raw");
        dev.set_data_file_with_path(
            CrashIo {
                inner: Qcow2IoTokio::try_new(&payload, ro, false).await.unwrap(),
                control: None,
            },
            payload,
        )
        .unwrap();
    }
    dev.qcow2_prep_io().await.unwrap();
    dev
}
#[test]
fn crash_worker() {
    let Some(dir) = std::env::var_os("QCOW2_CRASH_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let operation = std::env::var("QCOW2_CRASH_OPERATION").unwrap();
    let point = std::env::var("QCOW2_CRASH_POINT").unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let disk = dir.join("disk");
        let controlled = if operation == "commit" {
            dir.join("base")
        } else {
            disk.clone()
        };
        let bytes = std::fs::read(&controlled).unwrap();
        let control = Rc::new(Control {
            operation: operation.clone(),
            point,
            marker: dir.join("checkpoint"),
            initial_l1: u64::from_be_bytes(bytes[40..48].try_into().unwrap()),
            publish: Cell::new(false),
            dirty: Cell::new(false),
            payload: Cell::new(false),
        });
        let mut dev = custom(
            &disk,
            (operation != "commit").then(|| control.clone()),
            false,
        )
        .await;
        if operation == "commit" {
            dev.set_backing_dev(Box::new(custom(&dir.join("base"), None, true).await));
        }
        match operation.as_str() {
            "resize" | "external-resize" => dev.resize((SIZE * 2) as u64, false).await.unwrap(),
            "amend" => dev
                .amend(AmendOptions {
                    extended_l2: Some(true),
                    refcount_order: Some(6),
                    ..Default::default()
                })
                .await
                .unwrap(),
            "preallocate" => dev.preallocate(Preallocation::Full).await.unwrap(),
            "rebase" => {
                let raw = CrashIo {
                    inner: Qcow2IoTokio::try_new(&dir.join("b.raw"), true, false)
                        .await
                        .unwrap(),
                    control: None,
                };
                dev.rebase(
                    Some(RebaseTarget {
                        file: BackingFile {
                            path: "b.raw".into(),
                            format: Some(BackingFormat::Raw),
                        },
                        source: Backing::raw(raw, SIZE as u64),
                    }),
                    true,
                )
                .await
                .unwrap();
            }
            "commit" => {
                let target = custom(&dir.join("base"), Some(control), false).await;
                dev.commit_to(&target, true).await.unwrap();
            }
            _ => unreachable!(),
        }
        panic!("Operation completed without reaching requested crash checkpoint");
    });
}
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn seed(dir: &Path, operation: &str) {
    let commit = operation == "commit";
    let external = operation == "external-resize";
    let mut options = CreateOptions {
        size: SIZE as u64,
        cluster_bits: 14,
        ..Default::default()
    };
    if external {
        options.extended_l2 = true;
        options.data_file = Some(ExternalDataFile {
            path: "payload.raw".into(),
            raw: true,
        });
    }
    let path = if commit {
        dir.join("base")
    } else {
        dir.join("disk")
    };
    create_qcow2(&path, &options).await.unwrap();
    let dev = qcow2_setup_dev_tokio(&path, &options.open_params)
        .await
        .unwrap();
    let mut data = Qcow2IoBuf::<u8>::new(SIZE);
    data.fill(0x31);
    dev.write_at(&data, 0).await.unwrap();
    dev.flush_meta().await.unwrap();
    if commit {
        dev.create_snapshot(
            SnapshotOptions {
                id: "base-old".into(),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
        drop(dev);
        options.backing = Some(BackingFile {
            path: "base".into(),
            format: Some(BackingFormat::Qcow2),
        });
        create_qcow2(&dir.join("disk"), &options).await.unwrap();
    } else {
        drop(dev);
    }
    let dev = qcow2_setup_dev_tokio(&dir.join("disk"), &options.open_params)
        .await
        .unwrap();
    if !external {
        dev.create_snapshot(
            SnapshotOptions {
                id: "old".into(),
                ..Default::default()
            },
            b"durable VM state",
        )
        .await
        .unwrap();
    }
    data[..512].fill(0xa7);
    dev.write_at(&data[..512], 0).await.unwrap();
    dev.flush_meta().await.unwrap();
    std::fs::write(dir.join("b.raw"), vec![0x63; SIZE]).unwrap();
}
async fn recover(path: &Path) -> Qcow2Dev<Qcow2IoTokio> {
    let mut params = Qcow2DevParams::new(9, None, None, false, false);
    params.set_repair_mode(true);
    let dev = qcow2_setup_dev_tokio(path, &params).await.unwrap();
    dev.repair_refcounts().await.unwrap();
    dev.check().await.unwrap();
    dev
}
fn qemu_check(path: &Path) {
    if support::qemu_supports_image(path) {
        let out = Command::new("qemu-img")
            .current_dir(path.parent().unwrap())
            .args(["check", "-q"])
            .arg(path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{path:?}: {out:?}");
    }
}
#[test]
fn killed_management_transactions_recover_data_snapshots_and_refcounts() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for operation in [
        "resize",
        "external-resize",
        "amend",
        "preallocate",
        "rebase",
        "commit",
    ] {
        let points: &[&str] = if operation == "commit" {
            &["payload", "before", "after"]
        } else {
            &["dirty", "before", "after"]
        };
        for point in points {
            let dir = tempfile::tempdir().unwrap();
            runtime.block_on(seed(dir.path(), operation));
            let mut child = KillOnDrop(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "crash_worker", "--nocapture"])
                    .env("QCOW2_CRASH_DIR", dir.path())
                    .env("QCOW2_CRASH_OPERATION", operation)
                    .env("QCOW2_CRASH_POINT", point)
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(30);
            while !dir.path().join("checkpoint").exists() {
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "{operation}/{point}: child exited before checkpoint"
                );
                assert!(
                    Instant::now() < deadline,
                    "{operation}/{point}: checkpoint timed out"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            child.0.kill().unwrap();
            assert!(!child.0.wait().unwrap().success());
            runtime.block_on(async {
                if operation == "commit" {
                    let base = recover(&dir.path().join("base")).await;
                    let mut data = Qcow2IoBuf::<u8>::new(SIZE);
                    base.read_at(&mut data, 0).await.unwrap();
                    assert!(
                        data[..512].iter().all(|b| *b == 0x31)
                            || data[..512].iter().all(|b| *b == 0xa7)
                    );
                    assert!(data[512..].iter().all(|b| *b == 0x31));
                    base.restore_snapshot("base-old").await.unwrap();
                    base.read_at(&mut data, 0).await.unwrap();
                    assert!(data.iter().all(|b| *b == 0x31));
                }
                let disk = dir.path().join("disk");
                let dev = recover(&disk).await;
                let size = dev.info.virtual_size() as usize;
                assert!(size == SIZE || operation.ends_with("resize") && size == SIZE * 2);
                let mut data = Qcow2IoBuf::<u8>::new(size);
                dev.read_at(&mut data, 0).await.unwrap();
                assert!(
                    data[..512].iter().all(|b| *b == 0xa7),
                    "{operation}/{point}"
                );
                assert!(data[512..SIZE].iter().all(|b| *b == 0x31));
                assert!(data[SIZE..].iter().all(|b| *b == 0));
                if operation == "external-resize" {
                    assert_eq!(
                        std::fs::read(dir.path().join("payload.raw")).unwrap(),
                        &data[..]
                    );
                } else {
                    let mut vm = [0; 16];
                    assert_eq!(
                        dev.read_snapshot_vm_state("old", 0, &mut vm).await.unwrap(),
                        16
                    );
                    assert_eq!(&vm, b"durable VM state");
                    dev.restore_snapshot("old").await.unwrap();
                    dev.read_at(&mut data[..SIZE], 0).await.unwrap();
                    assert!(data[..SIZE].iter().all(|b| *b == 0x31));
                }
                dev.check().await.unwrap();
                drop(dev);
                qemu_check(&disk);
            });
        }
    }
}
