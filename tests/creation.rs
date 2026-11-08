mod support;
use qcow2_rs::dev::*;
use qcow2_rs::helpers::Qcow2IoBuf;
use qcow2_rs::utils::qcow2_setup_dev_tokio;
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
async fn native_layouts_preallocation_and_capabilities() {
    for (version, extended, external) in [
        (2, false, false),
        (3, false, false),
        (3, true, false),
        (3, false, true),
        (3, true, true),
    ] {
        for mode in [
            Preallocation::Off,
            Preallocation::Metadata,
            Preallocation::Fallocate,
            Preallocation::Full,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("disk.qcow2");
            let mut options = CreateOptions {
                size: 1 << 20,
                version,
                extended_l2: extended,
                preallocation: mode,
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
            let caps = dev.capabilities().await;
            assert_eq!(caps.version, version);
            assert_eq!(caps.extended_l2, extended);
            assert_eq!(caps.external_data, external);
            dev.check().await.unwrap();
            let mut buf = Qcow2IoBuf::<u8>::new(512);
            buf.fill(0xa7);
            dev.write_at(&buf, 512).await.unwrap();
            dev.flush_meta().await.unwrap();
            buf.fill(0);
            dev.read_at(&mut buf, 512).await.unwrap();
            assert!(buf.iter().all(|b| *b == 0xa7));
            dev.check().await.unwrap();
            drop(dev);
            check(&path);
        }
    }
}
#[tokio::test]
async fn creation_rejects_invalid_options_and_existing_storage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    for options in [
        CreateOptions {
            version: 1,
            ..Default::default()
        },
        CreateOptions {
            version: 2,
            extended_l2: true,
            ..Default::default()
        },
        CreateOptions {
            cluster_bits: 8,
            ..Default::default()
        },
        CreateOptions {
            refcount_order: 7,
            ..Default::default()
        },
        CreateOptions {
            size: u64::MAX,
            ..Default::default()
        },
    ] {
        assert!(create_qcow2(&path, &options).await.is_err());
        assert!(!path.exists());
    }
    std::fs::write(dir.path().join("data.raw"), b"preserve").unwrap();
    let options = CreateOptions {
        data_file: Some(ExternalDataFile {
            path: "data.raw".into(),
            raw: true,
        }),
        ..Default::default()
    };
    assert!(create_qcow2(&path, &options).await.is_err());
    assert!(!path.exists());
    assert_eq!(
        std::fs::read(dir.path().join("data.raw")).unwrap(),
        b"preserve"
    );
    std::fs::write(&path, b"preserve").unwrap();
    assert!(create_qcow2(&path, &CreateOptions::default())
        .await
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"preserve");
}

#[tokio::test]
async fn native_small_clusters_wide_refcounts_backing_and_luks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wide.qcow2");
    let options = CreateOptions {
        size: 512 << 20,
        cluster_bits: 9,
        refcount_order: 6,
        ..Default::default()
    };
    create_qcow2(&path, &options).await.unwrap();
    check(&path);
    std::fs::write(dir.path().join("base.raw"), vec![0x5a; 1 << 20]).unwrap();
    for external in [false, true] {
        let path = dir.path().join(if external {
            "external.qcow2"
        } else {
            "luks.qcow2"
        });
        let mut options = CreateOptions {
            size: 1 << 20,
            extended_l2: true,
            backing: Some(BackingFile {
                path: "base.raw".into(),
                format: Some(BackingFormat::Raw),
            }),
            preallocation: Preallocation::Full,
            ..Default::default()
        };
        options.luks.iterations = 2000;
        options.set_luks_secret(b"creation password");
        if external {
            options.data_file = Some(ExternalDataFile {
                path: "encrypted.raw".into(),
                raw: false,
            });
        }
        create_qcow2(&path, &options).await.unwrap();
        let mut p = options.open_params.clone();
        p.set_encryption_secret(b"creation password");
        let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
        assert_eq!(dev.capabilities().await.encryption, EncryptionFormat::Luks1);
        let mut buf = Qcow2IoBuf::<u8>::new(512);
        dev.read_at(&mut buf, 0).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0x5a));
        dev.check().await.unwrap();
        drop(dev);
        std::fs::write(dir.path().join("secret"), b"creation password").unwrap();
        if !support::qemu_supports_image(&path) {
            continue;
        }
        let opts = format!(
            "driver=qcow2,file.filename={},encrypt.key-secret=sec",
            path.display()
        );
        let out = Command::new("qemu-img")
            .current_dir(dir.path())
            .args([
                "check",
                "--object",
                "secret,id=sec,file=secret",
                "--image-opts",
                &opts,
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }
}

#[tokio::test]
async fn private_preallocated_payload_is_reused_but_snapshot_data_is_copied() {
    for encrypted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk");
        let mut options = CreateOptions {
            size: 1 << 20,
            extended_l2: true,
            preallocation: Preallocation::Fallocate,
            ..Default::default()
        };
        if encrypted {
            options.set_luks_secret(b"secret");
            options.luks.iterations = 1000;
        }
        create_qcow2(&path, &options).await.unwrap();
        let mut params = options.open_params.clone();
        if encrypted {
            params.set_encryption_secret(b"secret");
        }
        let dev = qcow2_setup_dev_tokio(&path, &params).await.unwrap();
        // Preallocated zero subclusters do not expose their physical address
        // through get_mapping; inspect their on-disk descriptor instead.
        let host = || {
            let bytes = std::fs::read(&path).unwrap();
            let l1 = u64::from_be_bytes(bytes[40..48].try_into().unwrap()) as usize;
            let l2 = (u64::from_be_bytes(bytes[l1..l1 + 8].try_into().unwrap())
                & 0x00ff_ffff_ffff_fe00) as usize;
            u64::from_be_bytes(bytes[l2..l2 + 8].try_into().unwrap()) & 0x00ff_ffff_ffff_fe00
        };
        let before = host();
        assert_ne!(before, 0);
        let mut buf = Qcow2IoBuf::<u8>::new(512);
        buf.fill(0xa7);
        dev.write_at(&buf, 512).await.unwrap();
        assert_eq!(host(), before);
        dev.create_snapshot(
            SnapshotOptions {
                id: "saved".into(),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
        buf.fill(0x63);
        dev.write_at(&buf, 512).await.unwrap();
        assert_ne!(host(), before);
        dev.restore_snapshot("saved").await.unwrap();
        dev.read_at(&mut buf, 512).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0xa7));
        dev.check().await.unwrap();
    }
}
