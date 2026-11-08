#![cfg(target_os = "linux")]
use qcow2_rs::{
    dev::{CompressionType, LuksOptions, Qcow2DevParams},
    helpers::Qcow2IoBuf,
    utils::{make_temp_qcow2_img, qcow2_setup_dev_uring},
};

#[test]
fn uring_direct_io_supports_codecs_encryption_and_bitmap_tracking() {
    for encrypted in [false, true] {
        let image = make_temp_qcow2_img(1 << 20, 16, 4);
        tokio_uring::start(async {
            let mut params = Qcow2DevParams::new(12, None, None, false, true);
            let dev = qcow2_setup_dev_uring(image.path(), &params).await.unwrap();
            if encrypted {
                dev.initialize_luks(b"uring secret", LuksOptions { iterations: 2000 })
                    .await
                    .unwrap();
            } else {
                dev.set_compression_type(CompressionType::Zstd)
                    .await
                    .unwrap();
            }
            dev.create_bitmap("tracking", 12, true).await.unwrap();
            let mut buf = Qcow2IoBuf::<u8>::new(65536);
            buf.fill(0xa7);
            if encrypted {
                dev.write_at(&buf, 0).await.unwrap();
            } else {
                dev.write_compressed_at(&buf, 0).await.unwrap();
            }
            buf.fill(0);
            dev.read_at(&mut buf, 0).await.unwrap();
            assert!(buf.iter().all(|b| *b == 0xa7));
            dev.clear_bitmap("tracking").await.unwrap();
            dev.write_zeroes(4096, 4096).await.unwrap();
            assert_eq!(dev.read_bitmap("tracking").await.unwrap()[0], 2);
            dev.check().await.unwrap();
            drop(dev);
            if encrypted {
                params.set_encryption_secret(b"uring secret");
            }
            let dev = qcow2_setup_dev_uring(image.path(), &params).await.unwrap();
            dev.read_at(&mut buf, 0).await.unwrap();
            assert!(buf[..4096].iter().all(|b| *b == 0xa7));
            assert!(buf[4096..8192].iter().all(|b| *b == 0));
            assert!(buf[8192..].iter().all(|b| *b == 0xa7));
            dev.check().await.unwrap();
        });
    }
}
