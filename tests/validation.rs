use qcow2_rs::dev::Qcow2DevParams;
use qcow2_rs::helpers::Qcow2IoBuf;
use qcow2_rs::utils::{make_temp_qcow2_img, qcow2_setup_dev_tokio};

fn be64(bytes: &[u8], offset: usize) -> usize {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize
}

#[tokio::test]
async fn invalid_header_ranges_are_rejected_without_modification() {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let original = std::fs::read(image.path()).unwrap();
    let cases: Vec<(usize, Vec<u8>)> = vec![
        (36, u32::MAX.to_be_bytes().to_vec()),
        (36, 0_u32.to_be_bytes().to_vec()),
        (56, u32::MAX.to_be_bytes().to_vec()),
        (56, 0_u32.to_be_bytes().to_vec()),
        (40, 0_u64.to_be_bytes().to_vec()),
        (48, 0_u64.to_be_bytes().to_vec()),
        (40, original[48..56].to_vec()),
        (48, (!65535_u64).to_be_bytes().to_vec()),
        (24, u64::MAX.to_be_bytes().to_vec()),
        (104, vec![1]),
        (88, 1_u64.to_be_bytes().to_vec()),
    ];
    for (offset, value) in cases {
        let mut bytes = original.clone();
        bytes[offset..offset + value.len()].copy_from_slice(&value);
        std::fs::write(image.path(), &bytes).unwrap();
        for ro in [true, false] {
            let params = Qcow2DevParams::new(9, None, None, ro, false);
            assert!(
                qcow2_setup_dev_tokio(image.path(), &params).await.is_err(),
                "accepted field at {offset}"
            );
        }
        assert_eq!(std::fs::read(image.path()).unwrap(), bytes);
    }
}

#[tokio::test]
async fn malformed_top_table_entries_and_truncation_are_rejected() {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let original = std::fs::read(image.path()).unwrap();
    let l1 = be64(&original, 40);
    let rt = be64(&original, 48);
    let params = Qcow2DevParams::new(9, None, None, true, false);
    for (offset, value) in [(l1, 1_u64), (l1, 512), (rt, 1), (rt, 1_u64 << 63)] {
        let mut bytes = original.clone();
        bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
        std::fs::write(image.path(), &bytes).unwrap();
        assert!(qcow2_setup_dev_tokio(image.path(), &params).await.is_err());
        assert_eq!(std::fs::read(image.path()).unwrap(), bytes);
    }
    for length in [0, 7, 71, 103, rt + 256, l1 + 4] {
        std::fs::write(image.path(), &original[..length]).unwrap();
        assert!(
            qcow2_setup_dev_tokio(image.path(), &params).await.is_err(),
            "accepted truncation at {length}"
        );
    }
}

#[tokio::test]
async fn invalid_l2_entries_are_rejected_on_every_attempt() {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    buf.fill(0x5a);
    dev.write_at(&buf, 0).await.unwrap();
    dev.flush_meta().await.unwrap();
    dev.fsync_range(0, 0).await.unwrap();
    drop(dev);
    let original = std::fs::read(image.path()).unwrap();
    let l2 = be64(&original, be64(&original, 40)) & 0x00ff_ffff_ffff_fe00;
    for (value, version) in [(2_u64, 3_u32), (1_u64 << 63, 3), (1_u64 << 56, 3), (1, 2)] {
        let mut bytes = original.clone();
        bytes[4..8].copy_from_slice(&version.to_be_bytes());
        bytes[l2..l2 + 8].copy_from_slice(&value.to_be_bytes());
        std::fs::write(image.path(), &bytes).unwrap();
        let params = Qcow2DevParams::new(9, None, None, true, false);
        let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
        for _ in 0..2 {
            assert!(dev.read_at(&mut buf, 0).await.is_err());
        }
        assert_eq!(std::fs::read(image.path()).unwrap(), bytes);
    }
    std::fs::write(image.path(), &original[..l2 + 256]).unwrap();
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    for _ in 0..2 {
        assert!(dev.read_at(&mut buf, 0).await.is_err());
    }
}

#[tokio::test]
async fn empty_and_overflowing_guest_io_do_not_mutate_image() {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let before = std::fs::read(image.path()).unwrap();
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    assert_eq!(dev.read_at(&mut [], 0).await.unwrap(), 0);
    dev.write_at(&[], 0).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(512);
    assert!(dev.read_at(&mut buf, u64::MAX - 511).await.is_err());
    assert!(dev.write_at(&buf, u64::MAX - 511).await.is_err());
    assert!(!dev.need_flush_meta());
    assert_eq!(std::fs::read(image.path()).unwrap(), before);
}
