//! Unsupported image features must fail before guest I/O or metadata changes.

use qcow2_rs::dev::Qcow2DevParams;
use qcow2_rs::utils::{make_temp_qcow2_img, qcow2_setup_dev_tokio};
use std::io::{Seek, SeekFrom, Write};
use std::process::Command;

#[tokio::test]
async fn encrypted_images_are_rejected_without_modification() {
    for method in [1_u32, 2, u32::MAX] {
        let image = make_temp_qcow2_img(1 << 20, 16, 4);
        let mut file = image.reopen().unwrap();
        file.seek(SeekFrom::Start(32)).unwrap();
        file.write_all(&method.to_be_bytes()).unwrap();
        let before = std::fs::read(image.path()).unwrap();

        for read_only in [true, false] {
            let params = Qcow2DevParams::new(9, None, None, read_only, false);
            let err = qcow2_setup_dev_tokio(image.path(), &params)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("encryption"), "{err}");
            assert_eq!(std::fs::read(image.path()).unwrap(), before);
        }

        #[cfg(not(target_os = "windows"))]
        {
            let params = Qcow2DevParams::new(9, None, None, true, false);
            let err = qcow2_rs::utils::qcow2_setup_dev_sync(image.path(), &params).unwrap_err();
            assert!(err.to_string().contains("encryption"), "{err}");
        }
    }
}

#[tokio::test]
async fn internal_snapshots_allow_reads_but_reject_writable_open() {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let output = Command::new("qemu-img")
        .args(["snapshot", "-c", "baseline"])
        .arg(image.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let before = std::fs::read(image.path()).unwrap();

    let params = Qcow2DevParams::new(9, None, None, false, false);
    let err = qcow2_setup_dev_tokio(image.path(), &params)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("snapshots"), "{err}");

    let params = Qcow2DevParams::new(9, None, None, true, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    let mut buf = qcow2_rs::helpers::Qcow2IoBuf::<u8>::new(512);
    buf.fill(0xa5);
    dev.read_at(&mut buf, 0).await.unwrap();
    assert!(buf.iter().all(|b| *b == 0));
    drop(dev);

    #[cfg(not(target_os = "windows"))]
    {
        let params = Qcow2DevParams::new(9, None, None, false, false);
        let err = qcow2_rs::utils::qcow2_setup_dev_sync(image.path(), &params).unwrap_err();
        assert!(err.to_string().contains("snapshots"), "{err}");
    }
    assert_eq!(std::fs::read(image.path()).unwrap(), before);
}

#[tokio::test]
async fn invalid_block_and_cache_geometry_returns_errors() {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let before = std::fs::read(image.path()).unwrap();
    let mut invalid = vec![
        Qcow2DevParams::new(8, None, None, false, false),
        Qcow2DevParams::new(13, None, None, false, false),
        Qcow2DevParams::new(u8::MAX, None, None, false, false),
    ];
    for cache in [(8, 4096), (17, 1 << 20), (u8::MAX, usize::MAX), (12, 4096)] {
        invalid.push(Qcow2DevParams::new(9, Some(cache), None, false, false));
        invalid.push(Qcow2DevParams::new(9, None, Some(cache), false, false));
    }
    for params in invalid {
        assert!(qcow2_setup_dev_tokio(image.path(), &params).await.is_err());
    }

    // A valid block size can still be too large for the image's clusters.
    let mut bytes = before.clone();
    bytes[20..24].copy_from_slice(&9_u32.to_be_bytes());
    bytes[36..40].copy_from_slice(&32_u32.to_be_bytes());
    let header = qcow2_rs::meta::Qcow2Header::from_buf(&bytes).unwrap();
    let params = Qcow2DevParams::new(12, None, None, false, false);
    assert!(qcow2_rs::dev::Qcow2Info::new(&header, &params).is_err());
    assert_eq!(std::fs::read(image.path()).unwrap(), before);
}
