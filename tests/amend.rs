use qcow2_rs::{dev::*, helpers::Qcow2IoBuf, utils::qcow2_setup_dev_tokio};
use std::process::Command;
fn check(path: &std::path::Path) {
    let out = Command::new("qemu-img")
        .current_dir(path.parent().unwrap())
        .args(["check", "-q"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}
#[tokio::test]
async fn metadata_conversions_preserve_snapshots_and_relocate_vm_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    let options = CreateOptions {
        size: 1 << 20,
        ..Default::default()
    };
    create_qcow2(&path, &options).await.unwrap();
    let mut dev = qcow2_setup_dev_tokio(&path, &options.open_params)
        .await
        .unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(65536);
    buf.fill(0x5a);
    dev.write_compressed_at(&buf, 0).await.unwrap();
    let vm: Vec<u8> = (0..70017).map(|i| (i % 251) as u8).collect();
    dev.create_snapshot(
        SnapshotOptions {
            id: "saved".into(),
            ..Default::default()
        },
        &vm,
    )
    .await
    .unwrap();
    buf.fill(0xa7);
    dev.write_at(&buf[..512], 512).await.unwrap();
    for (version, extended, order, codec) in [
        (3, true, 6, CompressionType::Zstd),
        (3, false, 1, CompressionType::Deflate),
        (2, false, 4, CompressionType::Deflate),
        (3, true, 4, CompressionType::Zstd),
    ] {
        dev.amend(AmendOptions {
            version: Some(version),
            extended_l2: Some(extended),
            refcount_order: Some(order),
            compression: Some(codec),
            ..Default::default()
        })
        .await
        .unwrap();
        let caps = dev.capabilities().await;
        assert_eq!(caps.version, version);
        assert_eq!(caps.extended_l2, extended);
        assert_eq!(caps.refcount_bits, 1 << order);
        dev.read_at(&mut buf, 0).await.unwrap();
        assert!(buf[..512].iter().all(|b| *b == 0x5a));
        assert!(buf[512..1024].iter().all(|b| *b == 0xa7));
        assert!(buf[1024..].iter().all(|b| *b == 0x5a));
        let mut saved = vec![0; vm.len()];
        dev.read_snapshot_vm_state("saved", 0, &mut saved)
            .await
            .unwrap();
        assert_eq!(saved, vm);
        dev.check().await.unwrap();
        check(&path);
    }
    dev.restore_snapshot("saved").await.unwrap();
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
    dev.check().await.unwrap();
}
#[tokio::test]
async fn refcount_conversion_preserves_bitmaps_and_validates_downgrades() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    let options = CreateOptions {
        size: 1 << 20,
        ..Default::default()
    };
    create_qcow2(&path, &options).await.unwrap();
    let mut dev = qcow2_setup_dev_tokio(&path, &options.open_params)
        .await
        .unwrap();
    dev.create_bitmap("tracking", 9, true).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0x5a);
    dev.write_at(&buf, 0).await.unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(dev
        .amend(AmendOptions {
            version: Some(2),
            ..Default::default()
        })
        .await
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    for order in 0..=6 {
        dev.amend(AmendOptions {
            refcount_order: Some(order),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(dev.read_bitmap("tracking").await.unwrap()[0], 1);
        dev.check().await.unwrap();
        check(&path);
    }
}

struct PublishFailure {
    inner: qcow2_rs::tokio_io::Qcow2IoTokio,
    headers: std::cell::Cell<usize>,
}
impl qcow2_rs::ops::Qcow2IoOps for PublishFailure {
    async fn read_to(&self, off: u64, buf: &mut [u8]) -> qcow2_rs::error::Qcow2Result<usize> {
        self.inner.read_to(off, buf).await
    }
    async fn write_from(&self, off: u64, buf: &[u8]) -> qcow2_rs::error::Qcow2Result<()> {
        if off == 0 {
            let n = self.headers.get() + 1;
            self.headers.set(n);
            if n == 2 {
                return Err("injected header publication failure".into());
            }
        }
        self.inner.write_from(off, buf).await
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
}
#[tokio::test]
async fn failed_conversion_does_not_overwrite_retired_high_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    let options = CreateOptions {
        size: 1 << 20,
        ..Default::default()
    };
    create_qcow2(&path, &options).await.unwrap();
    let cs = 65536;
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.resize(5 * cs, 0);
    bytes[40..48].copy_from_slice(&(4_u64 * cs as u64).to_be_bytes());
    bytes[2 * cs + 6..2 * cs + 8].fill(0);
    bytes[2 * cs + 8..2 * cs + 10].copy_from_slice(&1_u16.to_be_bytes());
    std::fs::write(&path, bytes).unwrap();
    check(&path);
    let io = PublishFailure {
        inner: qcow2_rs::tokio_io::Qcow2IoTokio::try_new(&path, false, false)
            .await
            .unwrap(),
        headers: std::cell::Cell::new(0),
    };
    let (mut dev, _) = qcow2_rs::utils::qcow2_alloc_dev(&path, io, &options.open_params)
        .await
        .unwrap();
    dev.qcow2_prep_io().await.unwrap();
    assert!(dev
        .amend(AmendOptions {
            refcount_order: Some(5),
            ..Default::default()
        })
        .await
        .is_err());
    assert!(dev.repair_refcounts().await.is_err());
    drop(dev);
    let mut p = options.open_params.clone();
    p.set_repair_mode(true);
    let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
    dev.repair_refcounts().await.unwrap();
    dev.check().await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0));
    drop(dev);
    check(&path);
}

#[tokio::test]
async fn encrypted_vm_state_survives_l2_and_version_conversion() {
    let help = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o", "help"])
        .output()
        .unwrap();
    for legacy in [false, true] {
        if legacy && !String::from_utf8_lossy(&help.stdout).contains("encrypt.format") {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.qcow2");
        let mut options = CreateOptions {
            size: 1 << 20,
            ..Default::default()
        };
        std::fs::write(dir.path().join("secret"), b"conversion password").unwrap();
        if legacy {
            let out = Command::new("qemu-img")
                .current_dir(dir.path())
                .args([
                    "create",
                    "--object",
                    "secret,id=sec,file=secret",
                    "-f",
                    "qcow2",
                    "-o",
                    "encrypt.format=aes,encrypt.key-secret=sec",
                    "disk.qcow2",
                    "1M",
                ])
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        } else {
            options.luks.iterations = 2000;
            options.set_luks_secret(b"conversion password");
            create_qcow2(&path, &options).await.unwrap();
        }
        let mut p = options.open_params.clone();
        p.set_encryption_secret(b"conversion password");
        let mut dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(512);
        buf.fill(0xa7);
        dev.write_at(&buf, 0).await.unwrap();
        dev.create_snapshot(
            SnapshotOptions {
                id: "saved".into(),
                ..Default::default()
            },
            b"encrypted state",
        )
        .await
        .unwrap();
        for (version, extended) in [(3, true), (2, false), (3, true)] {
            dev.amend(AmendOptions {
                version: Some(version),
                extended_l2: Some(extended),
                ..Default::default()
            })
            .await
            .unwrap();
            let mut state = vec![0; 15];
            dev.read_snapshot_vm_state("saved", 0, &mut state)
                .await
                .unwrap();
            assert_eq!(state, b"encrypted state");
            dev.read_at(&mut buf, 0).await.unwrap();
            assert!(buf.iter().all(|b| *b == 0xa7));
            dev.check().await.unwrap();
            if String::from_utf8_lossy(&help.stdout).contains("encrypt.format")
                && (!extended || String::from_utf8_lossy(&help.stdout).contains("extended_l2"))
            {
                let out = Command::new("qemu-img")
                    .current_dir(dir.path())
                    .args([
                        "check",
                        "--object",
                        "secret,id=sec,file=secret",
                        "--image-opts",
                        "driver=qcow2,file.filename=disk.qcow2,encrypt.key-secret=sec",
                    ])
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{out:?}");
            }
        }
    }
}

#[tokio::test]
async fn external_name_amendment_uses_the_attached_storage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    let options = CreateOptions {
        size: 1 << 20,
        data_file: Some(ExternalDataFile {
            path: "data.raw".into(),
            raw: true,
        }),
        ..Default::default()
    };
    create_qcow2(&path, &options).await.unwrap();
    std::fs::rename(dir.path().join("data.raw"), dir.path().join("moved.raw")).unwrap();
    let mut p = options.open_params.clone();
    p.set_external_data_file(dir.path().join("moved.raw"));
    let mut dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
    dev.amend_data_file_name("moved.raw".into()).await.unwrap();
    drop(dev);
    let dev = qcow2_setup_dev_tokio(&path, &options.open_params)
        .await
        .unwrap();
    dev.check().await.unwrap();
    drop(dev);
    check(&path);
}

#[tokio::test]
async fn old_v2_snapshots_gain_explicit_disk_sizes_before_resize() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old-v2");
    let options = CreateOptions {
        version: 2,
        size: 1 << 20,
        ..Default::default()
    };
    create_qcow2(&path, &options).await.unwrap();
    let dev = qcow2_setup_dev_tokio(&path, &options.open_params)
        .await
        .unwrap();
    dev.create_snapshot(
        SnapshotOptions {
            id: "old".into(),
            ..Default::default()
        },
        &[],
    )
    .await
    .unwrap();
    drop(dev);
    let mut bytes = std::fs::read(&path).unwrap();
    let offset = u64::from_be_bytes(bytes[64..72].try_into().unwrap()) as usize;
    // A v2 snapshot with no extra data inherits the current header's disk size.
    bytes[offset + 36..offset + 40].fill(0);
    bytes[offset + 40..offset + 43].copy_from_slice(b"old");
    std::fs::write(&path, &bytes).unwrap();
    let mut dev = qcow2_setup_dev_tokio(&path, &options.open_params)
        .await
        .unwrap();
    assert!(dev.resize(2 << 20, false).await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    dev.amend(AmendOptions {
        version: Some(3),
        ..Default::default()
    })
    .await
    .unwrap();
    dev.resize(2 << 20, false).await.unwrap();
    let snapshots = dev.list_snapshots().await.unwrap();
    assert_eq!(snapshots[0].disk_size, 1 << 20);
    assert!(snapshots[0].extra_data.len() >= 16);
    dev.restore_snapshot("old").await.unwrap();
    assert_eq!(dev.info.virtual_size(), 1 << 20);
    dev.check().await.unwrap();
    check(&path);
}
