use qcow2_rs::{dev::*, helpers::Qcow2IoBuf, ops::Qcow2IoOps, utils::qcow2_setup_dev_tokio};
use std::path::Path;
async fn seed(directory: &Path, encrypted: bool) -> Qcow2DevParams {
    let base = directory.join("base");
    let mut options = CreateOptions {
        size: 128 << 10,
        ..Default::default()
    };
    if encrypted {
        options.set_luks_secret(b"management secret");
        options.luks.iterations = 1000;
    }
    create_qcow2(&base, &options).await.unwrap();
    let mut params = options.open_params.clone();
    if encrypted {
        params.set_encryption_secret(b"management secret");
    }
    let target = qcow2_setup_dev_tokio(&base, &params).await.unwrap();
    let mut bytes = Qcow2IoBuf::<u8>::new(128 << 10);
    bytes.fill(0x31);
    target.write_at(&bytes, 0).await.unwrap();
    target.flush_meta().await.unwrap();
    target
        .create_snapshot(
            SnapshotOptions {
                id: "base-old".into(),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
    drop(target);
    options.backing = Some(BackingFile {
        path: "base".into(),
        format: Some(BackingFormat::Qcow2),
    });
    options.open_params = params.clone();
    create_qcow2(&directory.join("disk"), &options)
        .await
        .unwrap();
    params
}
async fn exercise<T: Qcow2IoOps>(dev: &mut Qcow2Dev<T>, target: &Qcow2Dev<T>) {
    let mut bytes = Qcow2IoBuf::<u8>::new(128 << 10);
    dev.read_at(&mut bytes, 0).await.unwrap();
    assert!(bytes.iter().all(|b| *b == 0x31));
    dev.create_snapshot(
        SnapshotOptions {
            id: "saved".into(),
            ..Default::default()
        },
        b"backend VM state",
    )
    .await
    .unwrap();
    dev.preallocate(Preallocation::Metadata).await.unwrap();
    bytes[..4096].fill(0xa7);
    dev.write_at(&bytes[..4096], 0).await.unwrap();
    dev.resize(256 << 10, false).await.unwrap();
    dev.resize(128 << 10, true).await.unwrap();
    dev.amend(AmendOptions {
        extended_l2: Some(true),
        refcount_order: Some(6),
        ..Default::default()
    })
    .await
    .unwrap();
    dev.commit_to(target, true).await.unwrap();
    bytes.fill(0);
    dev.read_at(&mut bytes, 0).await.unwrap();
    assert!(bytes[..4096].iter().all(|b| *b == 0xa7));
    assert!(bytes[4096..].iter().all(|b| *b == 0x31));
    target.read_at(&mut bytes, 0).await.unwrap();
    assert!(bytes[..4096].iter().all(|b| *b == 0xa7));
    dev.rebase(None, true).await.unwrap();
    dev.preallocate(Preallocation::Fallocate).await.unwrap();
    dev.restore_snapshot("saved").await.unwrap();
    dev.read_at(&mut bytes, 0).await.unwrap();
    assert!(bytes.iter().all(|b| *b == 0x31));
    let mut vm = [0; 16];
    dev.read_snapshot_vm_state("saved", 0, &mut vm)
        .await
        .unwrap();
    assert_eq!(&vm, b"backend VM state");
    target.restore_snapshot("base-old").await.unwrap();
    target.read_at(&mut bytes, 0).await.unwrap();
    assert!(bytes.iter().all(|b| *b == 0x31));
    dev.check().await.unwrap();
    target.check().await.unwrap();
}
#[tokio::test]
async fn tokio_management_preserves_encrypted_and_plain_backing_snapshots() {
    for encrypted in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let params = seed(directory.path(), encrypted).await;
        let mut dev = qcow2_setup_dev_tokio(&directory.path().join("disk"), &params)
            .await
            .unwrap();
        let target = qcow2_setup_dev_tokio(&directory.path().join("base"), &params)
            .await
            .unwrap();
        exercise(&mut dev, &target).await;
    }
}
#[cfg(unix)]
#[tokio::test]
async fn sync_management_supports_buffered_and_direct_io() {
    use qcow2_rs::utils::qcow2_setup_dev_sync;
    for direct in [false, true] {
        for encrypted in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut params = seed(directory.path(), encrypted).await;
            params.set_direct_io(direct);
            if direct {
                params = {
                    let mut p = Qcow2DevParams::new(12, None, None, false, true);
                    if encrypted {
                        p.set_encryption_secret(b"management secret");
                    }
                    p
                };
            }
            let mut dev = qcow2_setup_dev_sync(&directory.path().join("disk"), &params).unwrap();
            let target = qcow2_setup_dev_sync(&directory.path().join("base"), &params).unwrap();
            dev.qcow2_prep_io().await.unwrap();
            target.qcow2_prep_io().await.unwrap();
            exercise(&mut dev, &target).await;
        }
    }
}
#[cfg(target_os = "linux")]
#[test]
fn uring_management_supports_buffered_and_direct_io() {
    use qcow2_rs::utils::qcow2_setup_dev_uring;
    for direct in [false, true] {
        for encrypted in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut params = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(seed(directory.path(), encrypted));
            if direct {
                params = Qcow2DevParams::new(12, None, None, false, true);
                if encrypted {
                    params.set_encryption_secret(b"management secret");
                }
            }
            tokio_uring::start(async {
                let mut dev = qcow2_setup_dev_uring(&directory.path().join("disk"), &params)
                    .await
                    .unwrap();
                let target = qcow2_setup_dev_uring(&directory.path().join("base"), &params)
                    .await
                    .unwrap();
                exercise(&mut dev, &target).await;
            });
        }
    }
}
