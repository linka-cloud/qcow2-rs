use qcow2_rs::{
    dev::{Qcow2DevParams, SnapshotOptions},
    helpers::Qcow2IoBuf,
    utils::{make_temp_qcow2_img, qcow2_setup_dev_tokio},
};
use std::process::Command;
fn bit(data: &[u8], n: usize) -> bool {
    data[n / 8] & (1 << (n % 8)) != 0
}
fn qemu(path: &std::path::Path, args: &[&str]) {
    let mut command = Command::new("qemu-img");
    if args[0] == "bitmap" {
        command
            .args(&args[..args.len() - 1])
            .arg(path)
            .arg(args[args.len() - 1]);
    } else {
        command.args(args).arg(path);
    }
    let out = command.output().unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
}
fn supports() -> bool {
    let help = Command::new("qemu-img").arg("--help").output().unwrap();
    String::from_utf8_lossy(&help.stdout).contains("bitmap")
}
#[tokio::test]
async fn bitmap_lifecycle_tracks_writes_zeros_discard_compression_and_snapshots() {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    dev.create_bitmap("fine", 9, true).await.unwrap();
    dev.create_bitmap("coarse", 16, false).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0x5a);
    dev.write_at(&buf, 512).await.unwrap();
    dev.write_zeroes(4096, 512).await.unwrap();
    dev.discard(65536, 65536).await.unwrap();
    let mut whole = Qcow2IoBuf::<u8>::new(65536);
    whole.fill(0xa7);
    dev.write_compressed_at(&whole, 131072).await.unwrap();
    let bits = dev.read_bitmap("fine").await.unwrap();
    for n in 0..2048 {
        assert_eq!(
            bit(&bits, n),
            n == 1 || n == 8 || (128..384).contains(&n),
            "bit {n}"
        );
    }
    assert!(dev
        .read_bitmap("coarse")
        .await
        .unwrap()
        .iter()
        .all(|b| *b == 0));
    dev.set_bitmap_enabled("coarse", true).await.unwrap();
    dev.write_at(&buf, 0).await.unwrap();
    dev.set_bitmap_enabled("coarse", false).await.unwrap();
    dev.clear_bitmap("fine").await.unwrap();
    dev.merge_bitmap("fine", "coarse").await.unwrap();
    let bits = dev.read_bitmap("fine").await.unwrap();
    assert!((0..128).all(|n| bit(&bits, n)));
    assert!((128..2048).all(|n| !bit(&bits, n)));
    dev.rename_bitmap("coarse", "renamed").await.unwrap();
    dev.create_snapshot(
        SnapshotOptions {
            id: "saved".into(),
            ..Default::default()
        },
        &[],
    )
    .await
    .unwrap();
    dev.clear_bitmap("fine").await.unwrap();
    dev.restore_snapshot("saved").await.unwrap();
    assert!(dev
        .read_bitmap("fine")
        .await
        .unwrap()
        .iter()
        .all(|b| *b == 0xff));
    dev.check().await.unwrap();
    drop(dev);
    qemu(image.path(), &["check", "-q"]);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    assert_eq!(dev.list_bitmaps().await.unwrap().len(), 2);
    assert!(dev
        .list_bitmaps()
        .await
        .unwrap()
        .iter()
        .all(|b| b.consistent && !b.in_use));
    dev.remove_bitmap("fine").await.unwrap();
    dev.remove_bitmap("renamed").await.unwrap();
    assert!(dev.list_bitmaps().await.unwrap().is_empty());
    dev.check().await.unwrap();
    drop(dev);
    qemu(image.path(), &["check", "-q"]);
}
#[tokio::test]
async fn qemu_bitmaps_and_guest_writes_interoperate() {
    if !supports() {
        return;
    }
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    qemu(
        image.path(),
        &["bitmap", "--add", "--enable", "-g", "512", "foreign"],
    );
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0x5a);
    dev.write_at(&buf, 512).await.unwrap();
    assert!(bit(&dev.read_bitmap("foreign").await.unwrap(), 1));
    dev.check().await.unwrap();
    drop(dev);
    let out = Command::new("qemu-io")
        .args(["-f", "qcow2", "-c", "write -P 0xa7 4608 512"])
        .arg(image.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    let bits = dev.read_bitmap("foreign").await.unwrap();
    assert!(bit(&bits, 1) && bit(&bits, 9));
    dev.create_bitmap("merged", 9, false).await.unwrap();
    drop(dev);
    qemu(image.path(), &["bitmap", "--merge", "foreign", "merged"]);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    let bits = dev.read_bitmap("merged").await.unwrap();
    assert!(bit(&bits, 1) && bit(&bits, 9));
    dev.check().await.unwrap();
}

fn be64(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
}
fn extension(bytes: &[u8]) -> usize {
    let mut at = u32::from_be_bytes(bytes[100..104].try_into().unwrap()) as usize;
    loop {
        let kind = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        if kind == 0x23852875 {
            return at + 8;
        }
        assert_ne!(kind, 0);
        let len = u32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        at += 8 + len.div_ceil(8) * 8;
    }
}
#[tokio::test]
async fn opaque_extra_data_and_inconsistent_bitmap_management() {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    dev.create_bitmap("opaque", 9, false).await.unwrap();
    drop(dev);
    let mut bytes = std::fs::read(image.path()).unwrap();
    let ext = extension(&bytes);
    let start = be64(&bytes, ext + 16) as usize;
    let len = be64(&bytes, ext + 8) as usize;
    bytes.copy_within(start + 24..start + len, start + 32);
    bytes[start + 24..start + 32].copy_from_slice(b"preserve");
    bytes[start + 20..start + 24].copy_from_slice(&8_u32.to_be_bytes());
    bytes[start + 12..start + 16].copy_from_slice(&4_u32.to_be_bytes());
    bytes[ext + 8..ext + 16].copy_from_slice(&((len + 8) as u64).to_be_bytes());
    std::fs::write(image.path(), &bytes).unwrap();
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    dev.create_bitmap("other", 12, true).await.unwrap();
    assert_eq!(dev.list_bitmaps().await.unwrap()[0].extra_data, b"preserve");
    dev.remove_bitmap("other").await.unwrap();
    dev.set_bitmap_enabled("opaque", true).await.unwrap();
    dev.check().await.unwrap();
    drop(dev);
    // The extension consistency bit is missing after an interrupted writer.
    let mut bytes = std::fs::read(image.path()).unwrap();
    bytes[88..96].fill(0);
    std::fs::write(image.path(), &bytes).unwrap();
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    assert_eq!(
        dev.check_report().await.unwrap().inconsistent_bitmaps,
        vec!["opaque"]
    );
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0x5a);
    assert!(dev.write_at(&buf, 0).await.is_err());
    assert_eq!(std::fs::read(image.path()).unwrap(), bytes);
    dev.set_bitmap_enabled("opaque", false).await.unwrap();
    assert!(dev.read_bitmap("opaque").await.is_err());
    dev.clear_bitmap("opaque").await.unwrap();
    assert_eq!(dev.list_bitmaps().await.unwrap()[0].extra_data, b"preserve");
    dev.check().await.unwrap();
}

#[tokio::test]
async fn readonly_and_invalid_bitmap_operations_preserve_image() {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    dev.create_bitmap("tracking", 9, true).await.unwrap();
    drop(dev);
    let before = std::fs::read(image.path()).unwrap();
    let mut p = p;
    p.set_read_only(true);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    assert!(dev.remove_bitmap("tracking").await.is_err());
    assert!(dev.clear_bitmap("tracking").await.is_err());
    assert!(dev.set_bitmap_enabled("tracking", false).await.is_err());
    assert!(dev.create_bitmap("new", 9, true).await.is_err());
    assert!(dev.create_bitmap("tracking", 9, true).await.is_err());
    assert!(dev.create_bitmap("bad", 64, true).await.is_err());
    assert_eq!(std::fs::read(image.path()).unwrap(), before);
}
