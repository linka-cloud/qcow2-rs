use qcow2_rs::dev::Qcow2DevParams;
use qcow2_rs::helpers::Qcow2IoBuf;
use qcow2_rs::utils::qcow2_setup_dev_tokio;
use std::path::Path;
use std::process::Command;

fn qemu(args: &[&str], directory: &Path) {
    let output = Command::new("qemu-img")
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
}
fn be64(bytes: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(bytes[i..i + 8].try_into().unwrap())
}

fn supports(option: &str) -> bool {
    let output = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o", "help"])
        .output()
        .unwrap();
    let yes = String::from_utf8_lossy(&output.stdout).contains(option);
    if !yes {
        eprintln!("QEMU fixture tool lacks {option}; skipping that fixture");
    }
    yes
}

#[tokio::test]
async fn extended_l2_subclusters_write_zero_discard_and_reopen() {
    if !supports("extended_l2") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let d = directory.path();
    let mut expected = vec![0x5a; 1 << 20];
    std::fs::write(d.join("base.raw"), &expected).unwrap();
    qemu(
        &[
            "create",
            "-f",
            "qcow2",
            "-o",
            "extended_l2=on,cluster_size=16384",
            "-F",
            "raw",
            "-b",
            "base.raw",
            "disk.qcow2",
        ],
        d,
    );
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
        .await
        .unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0xa7);
    dev.write_at(&buf, 512).await.unwrap();
    expected[512..1024].fill(0xa7);
    let bytes = std::fs::read(d.join("disk.qcow2")).unwrap();
    let l1 = be64(&bytes, 40) as usize;
    let l2 = (be64(&bytes, l1) & 0x00ff_ffff_ffff_fe00) as usize;
    assert_eq!(be64(&bytes, l2 + 8), 2);
    dev.write_zeroes(1024, 512).await.unwrap();
    expected[1024..1536].fill(0);
    dev.discard(512, 512).await.unwrap();
    expected[512..1024].fill(0x5a);
    dev.check().await.unwrap();
    drop(dev);
    std::fs::write(d.join("expected.raw"), &expected).unwrap();
    qemu(&["check", "-q", "disk.qcow2"], d);
    qemu(
        &[
            "compare",
            "-f",
            "qcow2",
            "-F",
            "raw",
            "disk.qcow2",
            "expected.raw",
        ],
        d,
    );
    let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
        .await
        .unwrap();
    let mut read = Qcow2IoBuf::<u8>::new(16384);
    dev.read_at(&mut read, 0).await.unwrap();
    assert_eq!(&read[..], &expected[..16384]);
}

#[tokio::test]
async fn extended_l2_compressed_clusters_survive_partial_overwrite() {
    if !supports("extended_l2") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let d = directory.path();
    let mut expected = vec![0x5a; 1 << 20];
    std::fs::write(d.join("source.raw"), &expected).unwrap();
    qemu(
        &[
            "convert",
            "-c",
            "-f",
            "raw",
            "-O",
            "qcow2",
            "-o",
            "extended_l2=on,cluster_size=16384",
            "source.raw",
            "disk.qcow2",
        ],
        d,
    );
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
        .await
        .unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0xa7);
    dev.write_at(&buf, 512).await.unwrap();
    expected[512..1024].fill(0xa7);
    dev.check().await.unwrap();
    drop(dev);
    std::fs::write(d.join("expected.raw"), expected).unwrap();
    qemu(&["check", "-q", "disk.qcow2"], d);
    qemu(
        &[
            "compare",
            "-f",
            "qcow2",
            "-F",
            "raw",
            "disk.qcow2",
            "expected.raw",
        ],
        d,
    );
}

#[tokio::test]
async fn external_data_files_support_offset_zero_raw_consistency_and_override() {
    if !supports("data_file") {
        return;
    }
    for extended in [false, true] {
        if extended && !supports("extended_l2") {
            continue;
        }
        let directory = tempfile::tempdir().unwrap();
        let d = directory.path();
        let options = if extended {
            "data_file=data.raw,data_file_raw=on,extended_l2=on,cluster_size=16384"
        } else {
            "data_file=data.raw,data_file_raw=on"
        };
        qemu(
            &["create", "-f", "qcow2", "-o", options, "disk.qcow2", "1M"],
            d,
        );
        let p = Qcow2DevParams::new(9, None, None, false, false);
        let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
            .await
            .unwrap();
        let mut expected = vec![0; 1 << 20];
        let mut buf = Qcow2IoBuf::<u8>::new(512);
        buf.fill(0xa7);
        dev.write_at(&buf, 0).await.unwrap();
        expected[..512].fill(0xa7);
        dev.write_at(&buf, 65536).await.unwrap();
        expected[65536..66048].fill(0xa7);
        dev.write_zeroes(0, 512).await.unwrap();
        expected[..512].fill(0);
        let unit = if extended { 512 } else { 65536 };
        dev.discard(65536, unit).await.unwrap();
        expected[65536..65536 + unit as usize].fill(0);
        dev.check().await.unwrap();
        drop(dev);
        assert_eq!(std::fs::read(d.join("data.raw")).unwrap(), expected);
        std::fs::write(d.join("expected.raw"), expected).unwrap();
        qemu(&["check", "-q", "disk.qcow2"], d);
        qemu(
            &[
                "compare",
                "-f",
                "qcow2",
                "-F",
                "raw",
                "disk.qcow2",
                "expected.raw",
            ],
            d,
        );
        std::fs::rename(d.join("data.raw"), d.join("moved.raw")).unwrap();
        assert!(qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
            .await
            .is_err());
        let mut p = p.clone();
        p.set_external_data_file(d.join("moved.raw"));
        let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
            .await
            .unwrap();
        dev.read_at(&mut buf, 0).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0));
    }
}

#[tokio::test]
async fn snapshot_lifecycle_preserves_disk_and_vm_state() {
    use qcow2_rs::dev::SnapshotOptions;
    for (version, extended) in [(2, false), (3, false), (3, true)] {
        if extended && !supports("extended_l2") {
            continue;
        }
        let directory = tempfile::tempdir().unwrap();
        let d = directory.path();
        let options = if version == 2 {
            "compat=0.10,cluster_size=65536"
        } else if extended {
            "extended_l2=on,cluster_size=16384"
        } else {
            "cluster_size=65536"
        };
        qemu(
            &["create", "-f", "qcow2", "-o", options, "disk.qcow2", "1M"],
            d,
        );
        let p = Qcow2DevParams::new(9, None, None, false, false);
        let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
            .await
            .unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(512);
        buf.fill(0x5a);
        dev.write_at(&buf, 0).await.unwrap();
        let vm_state: Vec<u8> = (0..70017).map(|i| (i % 251) as u8).collect();
        let snapshot = dev
            .create_snapshot(
                SnapshotOptions {
                    id: "saved".into(),
                    name: "baseline".into(),
                    icount: Some(42),
                    ..Default::default()
                },
                &vm_state,
            )
            .await
            .unwrap();
        assert_eq!(snapshot.vm_state_size, vm_state.len() as u64);
        let mut state = vec![0; vm_state.len()];
        assert_eq!(
            dev.read_snapshot_vm_state("saved", 0, &mut state)
                .await
                .unwrap(),
            vm_state.len()
        );
        assert_eq!(state, vm_state);
        buf.fill(0xa7);
        dev.write_at(&buf, 0).await.unwrap();
        dev.check().await.unwrap();
        let snapshot_storage = std::cell::Cell::new(false);
        dev.qcow2_cluster_usage(|role, ranges, _| {
            if role == "snapshot directory" {
                snapshot_storage.set(!ranges.is_empty());
            }
        })
        .await
        .unwrap();
        assert!(snapshot_storage.get());
        drop(dev);
        qemu(&["check", "-q", "disk.qcow2"], d);
        qemu(&["snapshot", "-l", "disk.qcow2"], d);
        let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
            .await
            .unwrap();
        assert_eq!(dev.list_snapshots().await.unwrap()[0].icount, 42);
        dev.restore_snapshot("saved").await.unwrap();
        dev.read_at(&mut buf, 0).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0x5a));
        let mut tail = vec![0; 17];
        assert_eq!(
            dev.read_snapshot_vm_state("saved", 70000, &mut tail)
                .await
                .unwrap(),
            17
        );
        assert_eq!(tail, vm_state[70000..]);
        dev.delete_snapshot("saved").await.unwrap();
        assert!(dev.list_snapshots().await.unwrap().is_empty());
        dev.check().await.unwrap();
        drop(dev);
        qemu(&["check", "-q", "disk.qcow2"], d);
    }
}

#[tokio::test]
async fn qemu_snapshots_allow_safe_writes_and_size_restore() {
    let directory = tempfile::tempdir().unwrap();
    let d = directory.path();
    std::fs::write(d.join("source.raw"), vec![0x5a; 1 << 20]).unwrap();
    qemu(
        &[
            "convert",
            "-f",
            "raw",
            "-O",
            "qcow2",
            "source.raw",
            "disk.qcow2",
        ],
        d,
    );
    qemu(&["snapshot", "-c", "foreign", "disk.qcow2"], d);
    qemu(&["resize", "disk.qcow2", "2M"], d);
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
        .await
        .unwrap();
    let snapshots = dev.list_snapshots().await.unwrap();
    let id = &snapshots[0].id;
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0xa7);
    dev.write_at(&buf, 512).await.unwrap();
    dev.restore_snapshot(id).await.unwrap();
    assert_eq!(dev.info.virtual_size(), 1 << 20);
    dev.read_at(&mut buf, 512).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
    dev.check().await.unwrap();
    drop(dev);
    qemu(&["check", "-q", "disk.qcow2"], d);
    qemu(
        &[
            "compare",
            "-f",
            "qcow2",
            "-F",
            "raw",
            "disk.qcow2",
            "source.raw",
        ],
        d,
    );
}

// Subclusters can be smaller than the backend's required transfer alignment.
#[cfg(not(target_os = "windows"))]
#[tokio::test]
async fn extended_l2_with_4k_direct_io_reads_compressed_and_backing_subclusters() {
    if !supports("extended_l2") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let d = directory.path();
    let expected = vec![0x5a; 1 << 20];
    std::fs::write(d.join("source.raw"), &expected).unwrap();
    qemu(
        &[
            "convert",
            "-c",
            "-f",
            "raw",
            "-O",
            "qcow2",
            "-o",
            "extended_l2=on,cluster_size=16384",
            "source.raw",
            "compressed.qcow2",
        ],
        d,
    );
    qemu(
        &[
            "create",
            "-f",
            "qcow2",
            "-o",
            "extended_l2=on,cluster_size=16384",
            "-F",
            "raw",
            "-b",
            "source.raw",
            "backed.qcow2",
        ],
        d,
    );
    for image in ["compressed.qcow2", "backed.qcow2"] {
        let p = Qcow2DevParams::new(12, None, None, false, true);
        let dev = qcow2_rs::utils::qcow2_setup_dev_sync(&d.join(image), &p).unwrap();
        dev.qcow2_prep_io().await.unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(4096);
        dev.read_at(&mut buf, 12288).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0x5a));
        buf.fill(0xa7);
        dev.write_at(&buf, 4096).await.unwrap();
        dev.read_at(&mut buf, 12288).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0x5a));
        dev.read_at(&mut buf, 4096).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0xa7));
        dev.check().await.unwrap();
        drop(dev);
        qemu(&["check", "-q", image], d);
    }
}

#[tokio::test]
async fn external_data_backing_alias_rejection_and_small_raw_images() {
    if !supports("data_file") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let d = directory.path();
    std::fs::write(d.join("base.raw"), vec![0x5a; 1 << 20]).unwrap();
    qemu(
        &[
            "create",
            "-f",
            "qcow2",
            "-o",
            "data_file=data.bin",
            "-F",
            "raw",
            "-b",
            "base.raw",
            "disk.qcow2",
        ],
        d,
    );
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
        .await
        .unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0xa7);
    dev.write_at(&buf, 512).await.unwrap();
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
    dev.check().await.unwrap();
    drop(dev);
    qemu(&["check", "-q", "disk.qcow2"], d);
    let mut alias = p.clone();
    for name in ["disk.qcow2", "base.raw"] {
        alias.set_external_data_file(d.join(name));
        assert!(qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &alias)
            .await
            .is_err());
    }
    qemu(
        &[
            "create",
            "-f",
            "qcow2",
            "-o",
            "data_file=small.raw,data_file_raw=on",
            "small.qcow2",
            "512",
        ],
        d,
    );
    let dev = qcow2_setup_dev_tokio(&d.join("small.qcow2"), &p)
        .await
        .unwrap();
    // QEMU may pad a small data file to a whole cluster.
    let original_len = std::fs::metadata(d.join("small.raw")).unwrap().len();
    buf.fill(0xa7);
    dev.write_at(&buf, 0).await.unwrap();
    let raw = std::fs::read(d.join("small.raw")).unwrap();
    assert_eq!(raw.len() as u64, original_len);
    assert_eq!(&raw[..512], &buf[..]);
    dev.write_zeroes(0, 512).await.unwrap();
    let raw = std::fs::read(d.join("small.raw")).unwrap();
    assert_eq!(raw.len() as u64, original_len);
    assert!(raw[..512].iter().all(|b| *b == 0));
    dev.check().await.unwrap();
    drop(dev);
    qemu(&["check", "-q", "small.qcow2"], d);
}

#[tokio::test]
async fn readonly_snapshot_management_is_rejected_without_mutation() {
    let image = qcow2_rs::utils::make_temp_qcow2_img(1 << 20, 16, 4);
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    dev.create_snapshot(
        qcow2_rs::dev::SnapshotOptions {
            id: "saved".into(),
            ..Default::default()
        },
        &[],
    )
    .await
    .unwrap();
    drop(dev);
    let before = std::fs::read(image.path()).unwrap();
    let p = Qcow2DevParams::new(9, None, None, true, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    assert_eq!(dev.list_snapshots().await.unwrap().len(), 1);
    assert!(dev
        .create_snapshot(
            qcow2_rs::dev::SnapshotOptions {
                id: "new".into(),
                ..Default::default()
            },
            &[]
        )
        .await
        .is_err());
    assert!(dev.delete_snapshot("saved").await.is_err());
    assert!(dev.restore_snapshot("saved").await.is_err());
    assert!(dev.repair_refcounts().await.is_err());
    assert!(dev.write_zeroes(0, 512).await.is_err());
    assert_eq!(std::fs::read(image.path()).unwrap(), before);
}

#[tokio::test]
async fn invalid_extended_bitmaps_and_external_feature_combinations_are_rejected() {
    if !supports("extended_l2") || !supports("data_file") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let d = directory.path();
    qemu(
        &[
            "create",
            "-f",
            "qcow2",
            "-o",
            "extended_l2=on,cluster_size=16384",
            "extended.qcow2",
            "1M",
        ],
        d,
    );
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let path = d.join("extended.qcow2");
    let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0x5a);
    dev.write_at(&buf, 0).await.unwrap();
    drop(dev);
    let original = std::fs::read(&path).unwrap();
    let l1 = be64(&original, 40) as usize;
    let l2 = (be64(&original, l1) & 0x00ff_ffff_ffff_fe00) as usize;
    let mut malformed = original;
    malformed[l2 + 8..l2 + 16].copy_from_slice(&((1_u64 << 32) | 1).to_be_bytes());
    std::fs::write(&path, &malformed).unwrap();
    let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
    assert!(dev.read_at(&mut buf, 0).await.is_err());
    assert!(dev.check().await.is_err());
    assert!(dev.repair_refcounts().await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), malformed);

    std::fs::write(d.join("base.raw"), vec![0; 1 << 20]).unwrap();
    qemu(
        &[
            "create",
            "-f",
            "qcow2",
            "-o",
            "data_file=data.bin",
            "-F",
            "raw",
            "-b",
            "base.raw",
            "external.qcow2",
        ],
        d,
    );
    let original = std::fs::read(d.join("external.qcow2")).unwrap();
    for snapshots in [false, true] {
        let mut bytes = original.clone();
        if snapshots {
            bytes[60..64].copy_from_slice(&1_u32.to_be_bytes());
            bytes[64..72].copy_from_slice(&65536_u64.to_be_bytes());
        } else {
            bytes[88..96].copy_from_slice(&2_u64.to_be_bytes());
        }
        assert!(qcow2_rs::meta::Qcow2Header::from_buf(&bytes).is_err());
    }
}
