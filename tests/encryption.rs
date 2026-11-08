use qcow2_rs::{
    dev::{Qcow2DevParams, SnapshotOptions},
    helpers::Qcow2IoBuf,
    utils::qcow2_setup_dev_tokio,
};
use std::{path::Path, process::Command};

fn qemu(d: &Path, args: &[&str]) {
    let out = Command::new("qemu-img")
        .current_dir(d)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
}

#[tokio::test]
async fn qemu_encryption_payload_cow_snapshots_vm_state_and_wrong_keys() {
    let help = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o", "help"])
        .output()
        .unwrap();
    if !String::from_utf8_lossy(&help.stdout).contains("encrypt.format") {
        return;
    }
    for (format, extra) in [
        ("luks", ""),
        (
            "luks",
            ",encrypt.cipher-mode=cbc,encrypt.ivgen-alg=essiv,encrypt.ivgen-hash-alg=sha256",
        ),
        ("luks", ",extended_l2=on,cluster_size=16384"),
        ("aes", ""),
    ] {
        if extra.contains("extended_l2")
            && !String::from_utf8_lossy(&help.stdout).contains("extended_l2")
        {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let mut expected = vec![0x5a; 1 << 20];
        std::fs::write(d.join("source.raw"), &expected).unwrap();
        std::fs::write(d.join("secret"), b"test passphrase").unwrap();
        let object = "secret,id=sec,file=secret";
        let options = format!(
            "encrypt.format={format},encrypt.key-secret=sec{}{}",
            if format == "luks" {
                ",encrypt.iter-time=10"
            } else {
                ""
            },
            extra
        );
        qemu(
            d,
            &[
                "convert",
                "--object",
                object,
                "-f",
                "raw",
                "-O",
                "qcow2",
                "-o",
                &options,
                "source.raw",
                "disk.qcow2",
            ],
        );
        let path = d.join("disk.qcow2");
        let before = std::fs::read(&path).unwrap();
        let mut p = Qcow2DevParams::new(9, None, None, false, false);
        assert!(qcow2_setup_dev_tokio(&path, &p).await.is_err());
        if format == "luks" {
            p.set_encryption_secret(b"incorrect");
            let err = qcow2_setup_dev_tokio(&path, &p)
                .await
                .unwrap_err()
                .to_string();
            assert!(!err.contains("incorrect"));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        p.set_encryption_secret(b"test passphrase");
        assert!(!format!("{p:?}").contains("test passphrase"));
        let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(4096);
        dev.read_at(&mut buf, 512).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0x5a));
        dev.create_snapshot(
            SnapshotOptions {
                id: "saved".into(),
                ..Default::default()
            },
            b"encrypted VM state",
        )
        .await
        .unwrap();
        buf.fill(0xa7);
        dev.write_at(&buf, 512).await.unwrap();
        dev.write_zeroes(8192, 512).await.unwrap();
        dev.check().await.unwrap();
        let mut state = vec![0; 18];
        dev.read_snapshot_vm_state("saved", 0, &mut state)
            .await
            .unwrap();
        assert_eq!(state, b"encrypted VM state");
        dev.restore_snapshot("saved").await.unwrap();
        dev.read_at(&mut buf, 512).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0x5a));
        buf.fill(0xa7);
        dev.write_at(&buf, 512).await.unwrap();
        expected[512..4608].fill(0xa7);
        dev.delete_snapshot("saved").await.unwrap();
        dev.repair_refcounts().await.unwrap();
        dev.check().await.unwrap();
        drop(dev);
        std::fs::write(d.join("expected.raw"), &expected).unwrap();
        let opts = "driver=qcow2,file.filename=disk.qcow2,encrypt.key-secret=sec";
        qemu(d, &["check", "--object", object, "--image-opts", opts]);
        qemu(
            d,
            &[
                "compare",
                "--object",
                object,
                "--image-opts",
                opts,
                "driver=raw,file.filename=expected.raw",
            ],
        );
        let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
        dev.read_at(&mut buf, 512).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0xa7));
    }
}

#[tokio::test]
async fn native_luks_creation_and_direct_io_interoperate() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let path = d.join("disk.qcow2");
    qcow2_rs::utils::make_temp_qcow2_img(1 << 20, 16, 4)
        .persist(&path)
        .unwrap();
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
    dev.initialize_luks(
        b"native password",
        qcow2_rs::dev::LuksOptions { iterations: 2000 },
    )
    .await
    .unwrap();
    let mut expected = vec![0; 1 << 20];
    let mut buf = Qcow2IoBuf::<u8>::new(4096);
    buf.fill(0xa7);
    dev.write_at(&buf, 4096).await.unwrap();
    expected[4096..8192].fill(0xa7);
    buf.fill(0);
    dev.read_at(&mut buf, 4096).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0xa7));
    assert!(dev.write_compressed_at(&buf, 0).await.is_err());
    dev.check().await.unwrap();
    drop(dev);
    std::fs::write(d.join("secret"), b"native password").unwrap();
    std::fs::write(d.join("expected.raw"), &expected).unwrap();
    let opts = "driver=qcow2,file.filename=disk.qcow2,encrypt.key-secret=sec";
    let help = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o", "help"])
        .output()
        .unwrap();
    if String::from_utf8_lossy(&help.stdout).contains("encrypt.format") {
        qemu(
            d,
            &[
                "compare",
                "--object",
                "secret,id=sec,file=secret",
                "--image-opts",
                opts,
                "driver=raw,file.filename=expected.raw",
            ],
        );
        qemu(
            d,
            &[
                "check",
                "--object",
                "secret,id=sec,file=secret",
                "--image-opts",
                opts,
            ],
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut p = Qcow2DevParams::new(12, None, None, false, true);
        p.set_encryption_secret(b"native password");
        let dev = qcow2_rs::utils::qcow2_setup_dev_sync(&path, &p).unwrap();
        dev.qcow2_prep_io().await.unwrap();
        dev.read_at(&mut buf, 4096).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0xa7));
        dev.write_zeroes(4096, 4096).await.unwrap();
        dev.read_at(&mut buf, 4096).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0));
        dev.check().await.unwrap();
    }
}

#[tokio::test]
async fn malformed_luks_parameters_fail_before_mutation() {
    let image = qcow2_rs::utils::make_temp_qcow2_img(1 << 20, 16, 4);
    let mut p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    dev.initialize_luks(
        b"private credential",
        qcow2_rs::dev::LuksOptions { iterations: 2000 },
    )
    .await
    .unwrap();
    drop(dev);
    let original = std::fs::read(image.path()).unwrap();
    let mut at = u32::from_be_bytes(original[100..104].try_into().unwrap()) as usize;
    let header = loop {
        let kind = u32::from_be_bytes(original[at..at + 4].try_into().unwrap());
        if kind == 0x0537be77 {
            break u64::from_be_bytes(original[at + 8..at + 16].try_into().unwrap()) as usize;
        }
        assert_ne!(kind, 0);
        let len = u32::from_be_bytes(original[at + 4..at + 8].try_into().unwrap()) as usize;
        at += 8 + len.div_ceil(8) * 8;
    };
    p.set_encryption_secret(b"private credential");
    for (offset, value) in [
        (6, 2_u16.to_be_bytes().to_vec()),
        (8, b"bad\0".to_vec()),
        (104, 0_u32.to_be_bytes().to_vec()),
        (164, 0_u32.to_be_bytes().to_vec()),
        (248, u32::MAX.to_be_bytes().to_vec()),
        (252, u32::MAX.to_be_bytes().to_vec()),
    ] {
        let mut bytes = original.clone();
        bytes[header + offset..header + offset + value.len()].copy_from_slice(&value);
        std::fs::write(image.path(), &bytes).unwrap();
        let error = qcow2_setup_dev_tokio(image.path(), &p)
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("private credential"));
        assert_eq!(std::fs::read(image.path()).unwrap(), bytes);
    }
}

#[tokio::test]
async fn backing_chain_supports_distinct_per_image_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let base = d.join("base.qcow2");
    let top = d.join("top.qcow2");
    qcow2_rs::utils::make_temp_qcow2_img(1 << 20, 16, 4)
        .persist(&base)
        .unwrap();
    let mut p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(&base, &p).await.unwrap();
    dev.initialize_luks(
        b"base password",
        qcow2_rs::dev::LuksOptions { iterations: 2000 },
    )
    .await
    .unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(65536);
    buf.fill(0x5a);
    dev.write_at(&buf, 0).await.unwrap();
    drop(dev);
    let base_before = std::fs::read(&base).unwrap();
    qemu(
        d,
        &[
            "create",
            "-f",
            "qcow2",
            "-u",
            "-F",
            "qcow2",
            "-b",
            "base.qcow2",
            "top.qcow2",
            "1M",
        ],
    );
    p.set_encryption_secret(b"base password");
    let dev = qcow2_setup_dev_tokio(&top, &p).await.unwrap();
    dev.initialize_luks(
        b"top password",
        qcow2_rs::dev::LuksOptions { iterations: 2000 },
    )
    .await
    .unwrap();
    buf.fill(0xa7);
    dev.write_at(&buf[..512], 512).await.unwrap();
    drop(dev);
    p.set_encryption_secret(b"top password");
    assert!(qcow2_setup_dev_tokio(&top, &p).await.is_err());
    p.set_image_encryption_secret(base.clone(), b"base password")
        .unwrap();
    let dev = qcow2_setup_dev_tokio(&top, &p).await.unwrap();
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf[..512].iter().all(|b| *b == 0x5a));
    assert!(buf[512..1024].iter().all(|b| *b == 0xa7));
    assert!(buf[1024..].iter().all(|b| *b == 0x5a));
    dev.check().await.unwrap();
    drop(dev);
    assert_eq!(std::fs::read(&base).unwrap(), base_before);
}
