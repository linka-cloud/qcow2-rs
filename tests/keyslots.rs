use qcow2_rs::{dev::*, helpers::Qcow2IoBuf, utils::qcow2_setup_dev_tokio};
use std::process::Command;
#[tokio::test]
async fn keyslots_add_rotate_revoke_and_interoperate_with_qemu() {
    let help = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o", "help"])
        .output()
        .unwrap();
    let modern = String::from_utf8_lossy(&help.stdout).contains("encrypt.format");
    for (version, cbc) in [(2, false), (3, false), (3, true)] {
        if cbc && !modern {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.qcow2");
        std::fs::write(dir.path().join("secret"), b"first password").unwrap();
        let mut options = CreateOptions {
            version,
            size: 1 << 20,
            ..Default::default()
        };
        options.luks.iterations = 2000;
        options.set_luks_secret(b"first password");
        if cbc {
            let out=Command::new("qemu-img").current_dir(dir.path()).args(["create","--object","secret,id=sec,file=secret","-f","qcow2","-o","encrypt.format=luks,encrypt.key-secret=sec,encrypt.cipher-alg=aes-256,encrypt.cipher-mode=cbc,encrypt.ivgen-alg=plain64,encrypt.hash-alg=sha512,encrypt.iter-time=10","disk.qcow2","1M"]).output().unwrap();
            assert!(out.status.success(), "{out:?}");
        } else {
            create_qcow2(&path, &options).await.unwrap();
        }
        let mut p = options.open_params.clone();
        p.set_encryption_secret(b"first password");
        let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(512);
        buf.fill(0xa7);
        dev.write_at(&buf, 0).await.unwrap();
        if version == 3 {
            dev.create_bitmap("tracking", 9, true).await.unwrap();
        }
        dev.add_luks_keyslot(1, b"second password", LuksOptions { iterations: 2000 })
            .await
            .unwrap();
        assert!(dev
            .add_luks_keyslot(1, b"overwrite", LuksOptions { iterations: 2000 })
            .await
            .is_err());
        let new = dev
            .change_luks_keyslot(0, b"third password", LuksOptions { iterations: 2000 })
            .await
            .unwrap();
        assert_eq!(new, 2);
        dev.remove_luks_keyslot(1, false).await.unwrap();
        assert!(dev.remove_luks_keyslot(2, false).await.is_err());
        let slots = dev.luks_keyslots().await.unwrap();
        assert_eq!(slots.iter().filter(|s| s.active).count(), 1);
        assert!(slots[2].active);
        if version == 3 {
            assert!(dev
                .read_bitmap("tracking")
                .await
                .unwrap()
                .iter()
                .all(|b| *b == 0));
        }
        dev.check().await.unwrap();
        drop(dev);
        assert!(qcow2_setup_dev_tokio(&path, &p).await.is_err());
        p.set_encryption_secret(b"third password");
        let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
        dev.read_at(&mut buf, 0).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0xa7));
        drop(dev);
        if modern {
            std::fs::write(dir.path().join("secret"), b"third password").unwrap();
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

struct WipeFailure {
    inner: qcow2_rs::tokio_io::Qcow2IoTokio,
    fail: std::cell::Cell<bool>,
}
impl qcow2_rs::ops::Qcow2IoOps for WipeFailure {
    async fn read_to(&self, off: u64, buf: &mut [u8]) -> qcow2_rs::error::Qcow2Result<usize> {
        self.inner.read_to(off, buf).await
    }
    async fn write_from(&self, off: u64, buf: &[u8]) -> qcow2_rs::error::Qcow2Result<()> {
        if buf.len() > 4096 && self.fail.replace(false) {
            return Err("injected keyslot wipe failure".into());
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
async fn repair_finishes_an_interrupted_keyslot_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    let mut options = CreateOptions {
        size: 1 << 20,
        ..Default::default()
    };
    options.luks.iterations = 2000;
    options.set_luks_secret(b"first");
    create_qcow2(&path, &options).await.unwrap();
    let mut p = options.open_params.clone();
    p.set_encryption_secret(b"first");
    let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
    dev.add_luks_keyslot(1, b"remaining", LuksOptions { iterations: 2000 })
        .await
        .unwrap();
    drop(dev);
    let io = WipeFailure {
        inner: qcow2_rs::tokio_io::Qcow2IoTokio::try_new(&path, false, false)
            .await
            .unwrap(),
        fail: std::cell::Cell::new(true),
    };
    let (dev, _) = qcow2_rs::utils::qcow2_alloc_dev(&path, io, &p)
        .await
        .unwrap();
    dev.qcow2_prep_io().await.unwrap();
    assert!(dev.remove_luks_keyslot(0, false).await.is_err());
    drop(dev);
    p.set_encryption_secret(b"remaining");
    p.set_repair_mode(true);
    let dev = qcow2_setup_dev_tokio(&path, &p).await.unwrap();
    dev.repair_refcounts().await.unwrap();
    dev.check().await.unwrap();
    assert!(!dev.luks_keyslots().await.unwrap()[0].active);
    drop(dev);
    let bytes = std::fs::read(&path).unwrap();
    let mut ext = u32::from_be_bytes(bytes[100..104].try_into().unwrap()) as usize;
    let host = loop {
        let kind = u32::from_be_bytes(bytes[ext..ext + 4].try_into().unwrap());
        assert_ne!(kind, 0);
        if kind == 0x0537be77 {
            break u64::from_be_bytes(bytes[ext + 8..ext + 16].try_into().unwrap()) as usize;
        }
        let len = u32::from_be_bytes(bytes[ext + 4..ext + 8].try_into().unwrap()) as usize;
        ext += 8 + len.div_ceil(8) * 8;
    };
    let start =
        u32::from_be_bytes(bytes[host + 248..host + 252].try_into().unwrap()) as usize * 512;
    let len = u32::from_be_bytes(bytes[host + 108..host + 112].try_into().unwrap()) as usize
        * u32::from_be_bytes(bytes[host + 252..host + 256].try_into().unwrap()) as usize;
    assert!(bytes[host + start..host + start + len]
        .iter()
        .all(|b| *b == 0));
}
