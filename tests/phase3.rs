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

#[tokio::test]
async fn extended_l2_subclusters_write_zero_discard_and_reopen() {
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
