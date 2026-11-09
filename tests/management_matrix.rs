//! Deterministic guest-byte model, independent QEMU readers, and management
//! round trips over every supported cluster size and refcount encoding.
mod support;
use qcow2_rs::{dev::*, helpers::Qcow2IoBuf, utils::qcow2_setup_dev_tokio};
use std::{path::Path, process::Command};
fn qemu(args: &[&str], directory: &Path) {
    let out = Command::new("qemu-img")
        .current_dir(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
}
async fn verify(dev: &Qcow2Dev<qcow2_rs::tokio_io::Qcow2IoTokio>, model: &[u8]) {
    let mut bytes = Qcow2IoBuf::<u8>::new(model.len());
    assert_eq!(dev.read_at(&mut bytes, 0).await.unwrap(), model.len());
    assert_eq!(&bytes[..], model);
    dev.flush_meta().await.unwrap();
    dev.check().await.unwrap();
}
fn independent_check(directory: &Path, model: &[u8]) {
    if !support::qemu_supports_image(&directory.join("disk")) {
        return;
    }
    std::fs::write(directory.join("expected.raw"), model).unwrap();
    qemu(&["check", "-q", "disk"], directory);
    qemu(
        &[
            "compare",
            "-f",
            "raw",
            "-F",
            "qcow2",
            "expected.raw",
            "disk",
        ],
        directory,
    );
    qemu(
        &["convert", "-f", "qcow2", "-O", "raw", "disk", "export.raw"],
        directory,
    );
    assert_eq!(std::fs::read(directory.join("export.raw")).unwrap(), model);
}
#[tokio::test]
async fn qemu_and_native_images_follow_the_guest_model_across_all_geometries() {
    let help = support::qemu_options();
    let mut configurations: Vec<_> = (9..=21)
        .map(|bits| (3, bits, (bits - 9) % 7, false))
        .collect();
    configurations.extend((14..=21).map(|bits| (3, bits, 1 + (bits - 14) % 6, true)));
    configurations.extend([(2, 9, 4, false), (2, 21, 4, false)]);
    for (case, (version, bits, order, extended)) in configurations.into_iter().enumerate() {
        eprintln!("geometry {case}: v{version}, cluster_bits={bits}, refcount_order={order}, extended_l2={extended}");
        let dir = tempfile::tempdir().unwrap();
        let cs = 1_usize << bits;
        let initial: Vec<u8> = (0..3 * cs)
            .map(|i| ((i / 512 * 17 + i % 251) % 256) as u8)
            .collect();
        std::fs::write(dir.path().join("base.raw"), &initial).unwrap();
        let options = CreateOptions {
            version,
            size: initial.len() as u64,
            cluster_bits: bits,
            refcount_order: order,
            extended_l2: extended,
            backing: Some(BackingFile {
                path: "base.raw".into(),
                format: Some(BackingFormat::Raw),
            }),
            ..Default::default()
        };
        let image = dir.path().join("disk");
        // Alternate origins. Gate only fixture generation when an old QEMU
        // lacks the feature; native layout checks still run for that case.
        if case % 2 == 0
            && (!extended || help.contains("extended_l2"))
            && (order == 4 || help.contains("refcount_bits"))
        {
            let opts = format!("compat={},cluster_size={cs},refcount_bits={},backing_file=base.raw,backing_fmt=raw{}", if version == 2 { "0.10" } else { "1.1" }, 1_u32 << order, if extended { ",extended_l2=on" } else { "" });
            qemu(
                &[
                    "create",
                    "-f",
                    "qcow2",
                    "-o",
                    &opts,
                    "disk",
                    &initial.len().to_string(),
                ],
                dir.path(),
            );
        } else {
            create_qcow2(&image, &options).await.unwrap();
        }
        let dev = qcow2_setup_dev_tokio(&image, &options.open_params)
            .await
            .unwrap();
        if order > 0 {
            dev.create_snapshot(
                SnapshotOptions {
                    id: "initial".into(),
                    ..Default::default()
                },
                b"matrix VM state",
            )
            .await
            .unwrap();
        }
        if version == 3 && help.contains("data_file") {
            dev.create_bitmap("tracking", 9, true).await.unwrap();
        }
        let mut model = initial.clone();
        let mut seed = 0x63bc_451a_u64 ^ case as u64;
        for step in 0..12 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let sector = seed as usize % (model.len() / 512);
            let off = sector * 512;
            if step % 4 == 3 {
                dev.write_zeroes(off as u64, 512).await.unwrap();
                model[off..off + 512].fill(0);
            } else {
                let mut bytes = Qcow2IoBuf::<u8>::new(512);
                bytes.fill((seed >> 32) as u8);
                dev.write_at(&bytes, off as u64).await.unwrap();
                model[off..off + 512].copy_from_slice(&bytes);
            }
            verify(&dev, &model).await;
        }
        // Discard exposes the backing again, including through shared roots.
        dev.discard(cs as u64, cs as u64).await.unwrap();
        model[cs..2 * cs].copy_from_slice(&initial[cs..2 * cs]);
        verify(&dev, &model).await;
        dev.resize((4 * cs + 512) as u64, false).await.unwrap();
        model.resize(4 * cs + 512, 0);
        dev.resize((cs + 512) as u64, true).await.unwrap();
        model.truncate(cs + 512);
        dev.resize((4 * cs + 512) as u64, false).await.unwrap();
        model.resize(4 * cs + 512, 0);
        let policy = [
            Preallocation::Metadata,
            Preallocation::Fallocate,
            Preallocation::Full,
        ][case % 3];
        dev.preallocate(policy).await.unwrap();
        verify(&dev, &model).await;
        drop(dev);
        independent_check(dir.path(), &model);
        let dev = qcow2_setup_dev_tokio(&image, &options.open_params)
            .await
            .unwrap();
        verify(&dev, &model).await;
        if order > 0 {
            let mut vm = [0; 15];
            assert_eq!(
                dev.read_snapshot_vm_state("initial", 0, &mut vm)
                    .await
                    .unwrap(),
                vm.len()
            );
            assert_eq!(&vm, b"matrix VM state");
            dev.restore_snapshot("initial").await.unwrap();
            verify(&dev, &initial).await;
            drop(dev);
            independent_check(dir.path(), &initial);
        }
    }
}

#[tokio::test]
async fn amend_rebase_and_commit_preserve_a_model_through_multiple_representations() {
    for extended in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk");
        let initial: Vec<u8> = (0..128 << 10).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.path().join("base.raw"), &initial).unwrap();
        let options = CreateOptions {
            size: initial.len() as u64,
            extended_l2: extended,
            backing: Some(BackingFile {
                path: "base.raw".into(),
                format: Some(BackingFormat::Raw),
            }),
            ..Default::default()
        };
        create_qcow2(&path, &options).await.unwrap();
        let mut dev = qcow2_setup_dev_tokio(&path, &options.open_params)
            .await
            .unwrap();
        dev.create_snapshot(
            SnapshotOptions {
                id: "old".into(),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
        let mut model = initial.clone();
        dev.write_zeroes(512, 512).await.unwrap();
        model[512..1024].fill(0);
        for (version, order, ext, codec) in [
            (3, 6, !extended, CompressionType::Zstd),
            (2, 4, false, CompressionType::Deflate),
            (3, 1, extended, CompressionType::Deflate),
        ] {
            dev.amend(AmendOptions {
                version: Some(version),
                refcount_order: Some(order),
                extended_l2: Some(ext),
                compression: Some(codec),
                ..Default::default()
            })
            .await
            .unwrap();
            verify(&dev, &model).await;
            drop(dev);
            independent_check(dir.path(), &model);
            dev = qcow2_setup_dev_tokio(&path, &options.open_params)
                .await
                .unwrap();
        }
        let new_base = dir.path().join("replacement.raw");
        std::fs::write(&new_base, vec![0x63; initial.len() * 2]).unwrap();
        let raw = qcow2_rs::tokio_io::Qcow2IoTokio::try_new(&new_base, true, false)
            .await
            .unwrap();
        dev.rebase(
            Some(RebaseTarget {
                file: BackingFile {
                    path: "replacement.raw".into(),
                    format: Some(BackingFormat::Raw),
                },
                source: Backing::raw(raw, (initial.len() * 2) as u64),
            }),
            true,
        )
        .await
        .unwrap();
        verify(&dev, &model).await;
        let target = qcow2_rs::tokio_io::Qcow2IoTokio::try_new(&new_base, false, false)
            .await
            .unwrap();
        dev.commit_to_raw(&target, &new_base, true).await.unwrap();
        let committed = std::fs::read(&new_base).unwrap();
        assert_eq!(&committed[..model.len()], &model);
        assert!(committed[model.len()..].iter().all(|b| *b == 0x63));
        verify(&dev, &model).await;
        drop(dev);
        drop(target);
        independent_check(dir.path(), &model);
        let mut dev = qcow2_setup_dev_tokio(&path, &options.open_params)
            .await
            .unwrap();
        dev.restore_snapshot("old").await.unwrap();
        dev.rebase(None, true).await.unwrap();
        verify(&dev, &initial).await;
        drop(dev);
        independent_check(dir.path(), &initial);
    }
}
