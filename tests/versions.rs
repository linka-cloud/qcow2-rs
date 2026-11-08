use qcow2_rs::dev::Qcow2DevParams;
use qcow2_rs::helpers::Qcow2IoBuf;
use qcow2_rs::meta::Qcow2Header;
use qcow2_rs::utils::qcow2_setup_dev_tokio;
use std::io::{Seek, SeekFrom, Write};
use std::process::Command;

fn qemu(args: &[&str], path: &std::path::Path) {
    let output = Command::new("qemu-img")
        .args(args)
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test]
async fn v2_read_write_and_header_roundtrip() {
    let image = tempfile::NamedTempFile::new().unwrap();
    // Use QEMU's v2 layout rather than a library-generated fixture.
    let output = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o", "compat=0.10"])
        .arg(image.path())
        .arg("64M")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let bytes = std::fs::read(image.path()).unwrap();
    let mut header = Qcow2Header::from_buf(&bytes).unwrap();
    assert_eq!(header.version(), 2);
    assert_eq!(header.header_length(), 72);
    assert_eq!(header.refcount_order(), 4);
    let serialized = header.serialize_to_buf().unwrap();
    // Publish the serialized header without overwriting later image metadata.
    let mut file = image.reopen().unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&serialized).unwrap();

    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0x5a);
    dev.write_at(&buf, 512).await.unwrap();
    dev.flush_meta().await.unwrap();
    drop(dev);
    qemu(&["check", "-q"], image.path());

    let params = Qcow2DevParams::new(9, None, None, true, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    let mut read = Qcow2IoBuf::<u8>::new(1024);
    dev.read_at(&mut read, 0).await.unwrap();
    assert!(read[..512].iter().all(|b| *b == 0));
    assert_eq!(&read[512..], &buf[..]);
    let header = Qcow2Header::from_buf(&std::fs::read(image.path()).unwrap()).unwrap();
    assert_eq!(header.version(), 2);
}
