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
            rng.fill_bytes(&mut data[11 * cs..]);
            std::fs::write(d.join("source.raw"), &data).unwrap();
            let options = format!(
                "cluster_size={cs},compression_type={codec},extended_l2={}",
                if extended { "on" } else { "off" }
            );
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
