#![cfg(target_os = "linux")]

use qcow2_rs::dev::{create_qcow2, BackingFile, BackingFormat, CreateOptions, ExternalDataFile};
use qcow2_rs::error::Qcow2Error;
use qcow2_rs::utils::{qcow2_setup_dev_sync, qcow2_setup_dev_tokio, qcow2_setup_dev_uring};
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct QemuIo(Child);

impl Drop for QemuIo {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn hold_with_qemu(path: &Path, format: &str, read_only: bool) -> QemuIo {
    let mut command = Command::new("qemu-io");
    command.args(["-f", format]);
    if read_only {
        command.arg("-r");
    }
    let mut peer = QemuIo(
        command
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdout = peer.0.stdout.take().unwrap();
    let (ready, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut prompt = [0; 9];
        let result = stdout.read_exact(&mut prompt).map(|()| prompt);
        let _ = ready.send(result);
    });
    assert_eq!(
        &wait.recv_timeout(Duration::from_secs(10)).unwrap().unwrap(),
        b"qemu-io> "
    );
    peer
}

fn assert_locked(error: Qcow2Error, path: &Path) {
    assert_eq!(error.get_inner().kind(), ErrorKind::WouldBlock, "{error}");
    assert!(
        error.to_string().contains(&path.display().to_string()),
        "{error}"
    );
}

fn qemu_writer(path: &Path) -> std::process::Output {
    Command::new("qemu-io")
        .args(["-f", "qcow2", "-c", "read -q 0 512"])
        .arg(path)
        .output()
        .unwrap()
}

#[test]
fn linux_backends_reject_a_qemu_writer_and_release_on_drop() {
    tokio_uring::start(async {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk");
        let options = CreateOptions {
            size: 1 << 20,
            ..Default::default()
        };
        create_qcow2(&disk, &options).await.unwrap();
        let peer = hold_with_qemu(&disk, "qcow2", false);
        for read_only in [false, true] {
            let mut params = options.open_params.clone();
            params.set_read_only(read_only);
            assert_locked(
                qcow2_setup_dev_tokio(&disk, &params).await.unwrap_err(),
                &disk,
            );
            assert_locked(qcow2_setup_dev_sync(&disk, &params).unwrap_err(), &disk);
            assert_locked(
                qcow2_setup_dev_uring(&disk, &params).await.unwrap_err(),
                &disk,
            );
        }
        drop(peer);
        let writer = qcow2_setup_dev_uring(&disk, &options.open_params)
            .await
            .unwrap();
        let alias = dir.path().join("alias");
        std::fs::hard_link(&disk, &alias).unwrap();
        assert_locked(
            qcow2_setup_dev_tokio(&alias, &options.open_params)
                .await
                .unwrap_err(),
            &alias,
        );
        let out = qemu_writer(&disk);
        assert!(!out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("lock"),
            "{out:?}"
        );
        drop(writer);
        let out = qemu_writer(&disk);
        assert!(out.status.success(), "{out:?}");
    });
}

#[tokio::test]
async fn shared_readers_survive_a_failed_writer_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk");
    let options = CreateOptions {
        size: 1 << 20,
        ..Default::default()
    };
    create_qcow2(&disk, &options).await.unwrap();
    let peer = hold_with_qemu(&disk, "qcow2", true);
    let mut params = options.open_params.clone();
    params.set_read_only(true);
    let reader = qcow2_setup_dev_tokio(&disk, &params).await.unwrap();
    assert_locked(
        qcow2_setup_dev_tokio(&disk, &options.open_params)
            .await
            .unwrap_err(),
        &disk,
    );
    drop(peer);
    // A failed upgrade must leave the original read lock in place.
    let out = qemu_writer(&disk);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("lock"),
        "{out:?}"
    );
    let writer = qcow2_setup_dev_tokio(&disk, &options.open_params)
        .await
        .unwrap();
    drop(writer);
    assert!(!qemu_writer(&disk).status.success());
    drop(reader);
    assert!(qemu_writer(&disk).status.success());
}

#[tokio::test]
async fn locked_backing_and_external_payloads_block_image_setup() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("base.raw");
    std::fs::write(&raw, vec![0; 1 << 20]).unwrap();
    let overlay = dir.path().join("overlay");
    let options = CreateOptions {
        size: 1 << 20,
        backing: Some(BackingFile {
            path: "base.raw".into(),
            format: Some(BackingFormat::Raw),
        }),
        ..Default::default()
    };
    create_qcow2(&overlay, &options).await.unwrap();
    let peer = hold_with_qemu(&raw, "raw", false);
    let new_overlay = dir.path().join("new-overlay");
    assert_locked(
        create_qcow2(&new_overlay, &options).await.unwrap_err(),
        &raw,
    );
    assert!(!new_overlay.exists());
    assert_locked(
        qcow2_setup_dev_tokio(&overlay, &options.open_params)
            .await
            .unwrap_err(),
        &raw,
    );
    drop(peer);
    qcow2_setup_dev_tokio(&overlay, &options.open_params)
        .await
        .unwrap();

    let external = dir.path().join("external");
    let data = dir.path().join("data.raw");
    let options = CreateOptions {
        size: 1 << 20,
        data_file: Some(ExternalDataFile {
            path: "data.raw".into(),
            raw: true,
        }),
        ..Default::default()
    };
    create_qcow2(&external, &options).await.unwrap();
    let peer = hold_with_qemu(&data, "raw", false);
    assert_locked(
        qcow2_setup_dev_tokio(&external, &options.open_params)
            .await
            .unwrap_err(),
        &data,
    );
    drop(peer);
    qcow2_setup_dev_tokio(&external, &options.open_params)
        .await
        .unwrap();
}

#[tokio::test]
async fn cli_reports_locked_images_and_conversion_does_not_truncate_them() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk");
    let raw = dir.path().join("raw");
    let options = CreateOptions {
        size: 1 << 20,
        ..Default::default()
    };
    create_qcow2(&disk, &options).await.unwrap();
    std::fs::write(&raw, vec![0x5a; 4096]).unwrap();
    let peer = hold_with_qemu(&disk, "qcow2", false);
    let binary = env!("CARGO_BIN_EXE_rqcow2");
    for args in [&["info"][..], &["dump"], &["check"], &["map", "-a", "0"]] {
        let out = Command::new(binary).args(args).arg(&disk).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Is another process using the image?")
        );
    }
    drop(peer);

    for (input, in_format, output, out_format) in
        [(&raw, "raw", &disk, "qcow2"), (&disk, "qcow2", &raw, "raw")]
    {
        let before = std::fs::read(output).unwrap();
        let _peer = hold_with_qemu(output, out_format, false);
        let out = Command::new(binary)
            .args(["convert", "-f", in_format, "-O", out_format, "-o"])
            .arg(output)
            .arg(input)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Is another process using the image?")
        );
        assert_eq!(std::fs::read(output).unwrap(), before);
    }
}
