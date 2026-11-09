use qcow2_rs::{dev::Qcow2DevParams, helpers::Qcow2IoBuf, utils::qcow2_setup_dev_tokio};
use rand::{RngCore, SeedableRng};
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
async fn codec_reads_packed_output_fallback_snapshots_and_reopen_interoperate() {
    let help = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o", "help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    for codec in ["zlib", "zstd"] {
        if codec == "zstd" && !help.contains("compression_type") {
            continue;
        }
        for extended in [false, true] {
            if extended && !help.contains("extended_l2") {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let d = dir.path();
            let cs = 16384;
            let mut data = vec![0x5a; cs * 12];
            let mut rng = rand::rngs::StdRng::seed_from_u64(42);
            for i in 2..11 {
                rng.fill_bytes(&mut data[i * cs..i * cs + cs / 2]);
            }
            // Exercise Deflate references farther back than QEMU's encoder's
            // small window, and packed streams that cross host clusters.
            data.copy_within(5 * cs..5 * cs + cs / 2, 5 * cs + cs / 2);
            rng.fill_bytes(&mut data[11 * cs..]);
            std::fs::write(d.join("source.raw"), &data).unwrap();
            let mut options = format!("cluster_size={cs}");
            if help.contains("compression_type") {
                options.push_str(&format!(",compression_type={codec}"));
            }
            if help.contains("extended_l2") {
                options.push_str(&format!(
                    ",extended_l2={}",
                    if extended { "on" } else { "off" }
                ));
            }
            qemu(
                d,
                &[
                    "convert",
                    "-c",
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
            let p = Qcow2DevParams::new(9, None, None, false, false);
            let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
                .await
                .unwrap();
            let mut buf = Qcow2IoBuf::<u8>::new(data.len());
            dev.read_at(&mut buf, 0).await.unwrap();
            assert_eq!(&buf[..], data);
            // Native output packs adjacent streams, even within a sector.
            dev.write_compressed_at(&buf, 0).await.unwrap();
            let a = dev.get_mapping(0).await.unwrap();
            let b = dev.get_mapping(cs as u64).await.unwrap();
            assert_eq!(a.source, qcow2_rs::meta::MappingSource::Compressed);
            assert_eq!(
                a.cluster_offset.unwrap() / 512,
                b.cluster_offset.unwrap() / 512
            );
            assert_eq!(
                dev.get_mapping((11 * cs) as u64).await.unwrap().source,
                qcow2_rs::meta::MappingSource::DataFile
            );
            let mut crosses_boundary = false;
            for i in 2..11 {
                let mapping = dev.get_mapping((i * cs) as u64).await.unwrap();
                if let (Some(host), Some(length)) =
                    (mapping.cluster_offset, mapping.compressed_length)
                {
                    crosses_boundary |= (host as usize % cs) + length > cs;
                }
            }
            assert!(crosses_boundary);
            dev.check().await.unwrap();
            drop(dev);
            qemu(d, &["check", "-q", "disk.qcow2"]);
            qemu(
                d,
                &[
                    "compare",
                    "-f",
                    "qcow2",
                    "-F",
                    "raw",
                    "disk.qcow2",
                    "source.raw",
                ],
            );
            let dev = qcow2_setup_dev_tokio(&d.join("disk.qcow2"), &p)
                .await
                .unwrap();
            buf.fill(0);
            dev.read_at(&mut buf, 0).await.unwrap();
            assert_eq!(&buf[..], data);
        }
    }
}

#[tokio::test]
async fn native_zstd_selection_roundtrips_and_rejects_populated_images() {
    let image = qcow2_rs::utils::make_temp_qcow2_img(1 << 20, 16, 4);
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    dev.set_compression_type(qcow2_rs::dev::CompressionType::Zstd)
        .await
        .unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(65536);
    buf.fill(0x5a);
    dev.write_compressed_at(&buf, 0).await.unwrap();
    assert!(dev
        .set_compression_type(qcow2_rs::dev::CompressionType::Deflate)
        .await
        .is_err());
    buf.fill(0);
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0x5a));
    dev.check().await.unwrap();
    drop(dev);
    let help = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o", "help"])
        .output()
        .unwrap();
    if !String::from_utf8_lossy(&help.stdout).contains("compression_type") {
        return;
    }
    let out = Command::new("qemu-img")
        .args(["check", "-q"])
        .arg(image.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

#[tokio::test]
async fn compressed_packing_respects_narrow_refcount_capacity() {
    for order in 0..=2 {
        let image = qcow2_rs::utils::make_temp_qcow2_img(1 << 20, 16, order);
        let p = Qcow2DevParams::new(9, None, None, false, false);
        let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
        let mut buf = Qcow2IoBuf::<u8>::new(1 << 20);
        buf.fill(0x5a);
        dev.write_compressed_at(&buf, 0).await.unwrap();
        dev.check().await.unwrap();
        buf.fill(0);
        dev.read_at(&mut buf, 0).await.unwrap();
        assert!(buf.iter().all(|b| *b == 0x5a));
        drop(dev);
        let out = Command::new("qemu-img")
            .args(["check", "-q"])
            .arg(image.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "order {order}: {out:?}");
    }
}

#[tokio::test]
async fn overwriting_compressed_data_at_a_host_cluster_boundary_preserves_refcounts() {
    const CLUSTER_SIZE: usize = 512;
    let image = qcow2_rs::utils::make_temp_qcow2_img(1 << 20, 9, 4);
    let p = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &p).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(CLUSTER_SIZE);
    buf.fill(0x5a);
    dev.write_compressed_at(&buf, 0).await.unwrap();
    let mapping = dev.get_mapping(0).await.unwrap();
    assert_eq!(mapping.cluster_offset.unwrap() % CLUSTER_SIZE as u64, 0);
    assert_eq!(mapping.compressed_length, Some(CLUSTER_SIZE));
    dev.check().await.unwrap();

    buf.fill(0xa7);
    dev.write_at(&buf, 0).await.unwrap();
    dev.flush_meta().await.unwrap();
    dev.check().await.unwrap();
    buf.fill(0);
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|&byte| byte == 0xa7));
}
