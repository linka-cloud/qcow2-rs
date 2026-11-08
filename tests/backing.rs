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
