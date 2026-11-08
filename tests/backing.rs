use qcow2_rs::dev::Qcow2DevParams;
use qcow2_rs::helpers::Qcow2IoBuf;
use qcow2_rs::utils::{make_temp_qcow2_img, qcow2_setup_dev_tokio};
use std::path::Path;
use std::process::Command;

fn overlay(path: &Path, backing: &str, format: &str, size: Option<&str>) {
    let mut command = Command::new("qemu-img");
    command
        .current_dir(path.parent().unwrap())
        .args(["create", "-f", "qcow2", "-F", format, "-b", backing])
        .arg(path.file_name().unwrap());
    if let Some(size) = size {
        command.arg(size);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn change_backing(path: &Path, name: &str) {
    let mut bytes = std::fs::read(path).unwrap();
    let offset = u64::from_be_bytes(bytes[8..16].try_into().unwrap()) as usize;
    bytes[16..20].copy_from_slice(&(name.len() as u32).to_be_bytes());
    bytes[offset..offset + name.len()].copy_from_slice(name.as_bytes());
    std::fs::write(path, bytes).unwrap();
}

async fn base(path: &Path, size: u64) {
    let image = make_temp_qcow2_img(size, 16, 4);
    std::fs::copy(image.path(), path).unwrap();
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(path, &params).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0x5a);
    dev.write_at(&buf, 0).await.unwrap();
    dev.flush_meta().await.unwrap();
    dev.fsync_range(0, 0).await.unwrap();
}

#[tokio::test]
async fn relative_multilevel_chains_resolve_from_each_image_directory() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir(root.join("layers")).unwrap();
    base(&root.join("base.qcow2"), 1 << 20).await;
    overlay(
        &root.join("layers/middle.qcow2"),
        "../base.qcow2",
        "qcow2",
        None,
    );
    overlay(
        &root.join("top.qcow2"),
        "layers/middle.qcow2",
        "qcow2",
        None,
    );
    let params = Qcow2DevParams::new(9, None, None, true, false);
    let dev = qcow2_setup_dev_tokio(&root.join("top.qcow2"), &params)
        .await
        .unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
    #[cfg(not(target_os = "windows"))]
    {
        let dev = qcow2_rs::utils::qcow2_setup_dev_sync(&root.join("top.qcow2"), &params).unwrap();
        dev.qcow2_prep_io().await.unwrap();
        buf.fill(0);
        dev.read_at(&mut buf, 0).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0x5a));
    }
}

#[tokio::test]
async fn backing_cycles_and_missing_paths_return_errors() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    base(&root.join("base.qcow2"), 1 << 20).await;
    let path = root.join("cycle.qcow2");
    overlay(&path, "base.qcow2", "qcow2", None);
    let params = Qcow2DevParams::new(9, None, None, true, false);
    change_backing(&path, "cycle.qcow2");
    let before = std::fs::read(&path).unwrap();
    let err = qcow2_setup_dev_tokio(&path, &params).await.unwrap_err();
    assert!(err.to_string().contains("Cycle"), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    #[cfg(not(target_os = "windows"))]
    assert!(qcow2_rs::utils::qcow2_setup_dev_sync(&path, &params)
        .unwrap_err()
        .to_string()
        .contains("Cycle"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("cycle.qcow2", root.join("alias.qcow2")).unwrap();
        change_backing(&path, "alias.qcow2");
        assert!(qcow2_setup_dev_tokio(&path, &params)
            .await
            .unwrap_err()
            .to_string()
            .contains("Cycle"));
    }
    change_backing(&path, "missing.qcow2");
    let err = qcow2_setup_dev_tokio(&path, &params).await.unwrap_err();
    assert_eq!(err.get_inner().kind(), std::io::ErrorKind::NotFound);
}

fn compare_with_qemu(path: &Path, expected: &[u8]) {
    let raw = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(raw.path(), expected).unwrap();
    let output = Command::new("qemu-img")
        .args(["compare", "-f", "qcow2", "-F", "raw"])
        .arg(path)
        .arg(raw.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = Command::new("qemu-img")
        .args(["check", "-q"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[tokio::test]
async fn raw_backing_through_qcow2_layer_preserves_bytes_and_zero_fills_eof() {
    const CLUSTER: usize = 1 << 16;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let raw: Vec<u8> = (0..CLUSTER + 573).map(|i| (i % 251) as u8).collect();
    std::fs::write(root.join("base.raw"), &raw).unwrap();
    overlay(&root.join("middle.qcow2"), "base.raw", "raw", Some("128K"));
    let middle_before = std::fs::read(root.join("middle.qcow2")).unwrap();
    overlay(
        &root.join("top.qcow2"),
        "middle.qcow2",
        "qcow2",
        Some("256K"),
    );
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(&root.join("top.qcow2"), &params)
        .await
        .unwrap();
    let mut expected = vec![0; CLUSTER * 4];
    expected[..raw.len()].copy_from_slice(&raw);
    let mut buf = Qcow2IoBuf::<u8>::new(expected.len());
    buf.fill(0xa5);
    dev.read_at(&mut buf, 0).await.unwrap();
    assert_eq!(&buf[..], &expected);
    let mut written = Qcow2IoBuf::<u8>::new(512);
    written.fill(0xa7);
    for offset in [0, CLUSTER + 512, CLUSTER * 3] {
        dev.write_at(&written, offset as u64).await.unwrap();
        expected[offset..offset + 512].fill(0xa7);
    }
    dev.flush_meta().await.unwrap();
    drop(dev);
    assert_eq!(std::fs::read(root.join("base.raw")).unwrap(), raw);
    assert_eq!(
        std::fs::read(root.join("middle.qcow2")).unwrap(),
        middle_before
    );
    compare_with_qemu(&root.join("top.qcow2"), &expected);
    #[cfg(not(target_os = "windows"))]
    {
        let params = Qcow2DevParams::new(9, None, None, true, true);
        let dev =
            qcow2_rs::utils::qcow2_setup_dev_sync(&root.join("middle.qcow2"), &params).unwrap();
        dev.qcow2_prep_io().await.unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(CLUSTER * 2);
        buf.fill(0xa5);
        dev.read_at(&mut buf, 0).await.unwrap();
        assert_eq!(&buf[..raw.len()], &raw);
        assert!(buf[raw.len()..].iter().all(|b| *b == 0));
    }
}

#[tokio::test]
async fn declared_raw_backing_is_not_probed_as_qcow2() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut raw = vec![0x5a; 512];
    raw[..4].copy_from_slice(b"QFI\xfb");
    std::fs::write(root.join("base.raw"), &raw).unwrap();
    let path = root.join("top.qcow2");
    overlay(&path, "base.raw", "raw", Some("1M"));
    let params = Qcow2DevParams::new(9, None, None, true, false);
    let dev = qcow2_setup_dev_tokio(&path, &params).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    dev.read_at(&mut buf, 0).await.unwrap();
    assert_eq!(&buf[..], &raw);
}

fn change_format(path: &Path, format: Option<&str>) {
    let mut bytes = std::fs::read(path).unwrap();
    let mut offset = u32::from_be_bytes(bytes[100..104].try_into().unwrap()) as usize;
    loop {
        let kind = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let length = u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        assert_ne!(kind, 0, "missing format extension");
        if kind == 0xe2792aca {
            if let Some(format) = format {
                bytes[offset + 4..offset + 8].copy_from_slice(&(format.len() as u32).to_be_bytes());
                bytes[offset + 8..offset + 8 + format.len()].copy_from_slice(format.as_bytes());
            } else {
                bytes[offset..offset + 4].copy_from_slice(&0x12345678_u32.to_be_bytes());
            }
            break;
        }
        offset += 8 + length.div_ceil(8) * 8;
    }
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn backing_format_is_honored_or_probed_only_when_absent() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("base.raw"), vec![0x5a; 512]).unwrap();
    let path = root.join("top.qcow2");
    overlay(&path, "base.raw", "raw", Some("1M"));
    let params = Qcow2DevParams::new(9, None, None, true, false);
    change_format(&path, Some("vmdk"));
    let err = qcow2_setup_dev_tokio(&path, &params).await.unwrap_err();
    assert!(
        err.to_string().contains("Unsupported backing format"),
        "{err}"
    );
    change_format(&path, None);
    let dev = qcow2_setup_dev_tokio(&path, &params).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
}

#[tokio::test]
async fn smaller_qcow2_backing_zero_fills_partial_cluster_cow() {
    const CLUSTER: usize = 1 << 16;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = root.join("base.qcow2");
    base(&path, (CLUSTER + 512) as u64).await;
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(&path, &params).await.unwrap();
    let mut written = Qcow2IoBuf::<u8>::new(512);
    written.fill(0x66);
    dev.write_at(&written, CLUSTER as u64).await.unwrap();
    dev.flush_meta().await.unwrap();
    drop(dev);
    let before = std::fs::read(&path).unwrap();
    let top = root.join("top.qcow2");
    overlay(&top, "base.qcow2", "qcow2", Some("192K"));
    let dev = qcow2_setup_dev_tokio(&top, &params).await.unwrap();
    let mut expected = vec![0; CLUSTER * 3];
    expected[..512].fill(0x5a);
    expected[CLUSTER..CLUSTER + 512].fill(0x66);
    let mut read = Qcow2IoBuf::<u8>::new(expected.len());
    read.fill(0xa5);
    dev.read_at(&mut read, 0).await.unwrap();
    assert_eq!(&read[..], &expected);
    written.fill(0xa7);
    dev.write_at(&written, (CLUSTER + 1024) as u64)
        .await
        .unwrap();
    expected[CLUSTER + 1024..CLUSTER + 1536].fill(0xa7);
    dev.flush_meta().await.unwrap();
    drop(dev);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    compare_with_qemu(&top, &expected);
}

#[tokio::test]
async fn hard_linked_raw_backing_cannot_alias_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("disk.qcow2");
    let alias = dir.path().join("base.raw");
    let options = qcow2_rs::dev::CreateOptions {
        size: 1 << 20,
        backing: Some(qcow2_rs::dev::BackingFile {
            path: "base.raw".into(),
            format: Some(qcow2_rs::dev::BackingFormat::Raw),
        }),
        ..Default::default()
    };
    std::fs::write(&alias, vec![0; 512]).unwrap();
    qcow2_rs::dev::create_qcow2(&image, &options).await.unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::fs::hard_link(&image, &alias).unwrap();
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let before = std::fs::read(&image).unwrap();
    assert!(qcow2_setup_dev_tokio(&image, &params).await.is_err());
    assert_eq!(std::fs::read(&image).unwrap(), before);
}
