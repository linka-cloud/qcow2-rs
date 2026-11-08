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
        ("aes", ""),
    ] {
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
    assert!(dev.write_compressed_at(&buf, 0).await.is_err());
    dev.check().await.unwrap();
    drop(dev);
    std::fs::write(d.join("secret"), b"native password").unwrap();
    std::fs::write(d.join("expected.raw"), &expected).unwrap();
    let opts = "driver=qcow2,file.filename=disk.qcow2,encrypt.key-secret=sec";
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
