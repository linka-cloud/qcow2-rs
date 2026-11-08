use qcow2_rs::dev::Qcow2DevParams;
use qcow2_rs::helpers::Qcow2IoBuf;
use qcow2_rs::utils::{make_temp_qcow2_img, qcow2_setup_dev_tokio};

const CLUSTER: usize = 1 << 16;
const OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;

fn be64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

async fn fixture() -> (tempfile::NamedTempFile, Vec<u8>, usize, usize, usize) {
    let image = make_temp_qcow2_img(1 << 20, 16, 4);
    let params = Qcow2DevParams::new(9, None, None, false, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    let mut buf = Qcow2IoBuf::<u8>::new(CLUSTER);
    buf.fill(0x5a);
    dev.write_at(&buf, 0).await.unwrap();
    dev.flush_meta().await.unwrap();
    dev.fsync_range(0, 0).await.unwrap();
    drop(dev);

    let bytes = std::fs::read(image.path()).unwrap();
    let l2 = (be64(&bytes, be64(&bytes, 40) as usize) & OFFSET_MASK) as usize;
    let host = (be64(&bytes, l2) & OFFSET_MASK) as usize;
    let rb = be64(&bytes, be64(&bytes, 48) as usize) as usize;
    let ref_offset = rb + (host / CLUSTER) * 2;
    (image, bytes, l2, host, ref_offset)
}

async fn check_bytes(
    image: &tempfile::NamedTempFile,
    bytes: &[u8],
) -> qcow2_rs::error::Qcow2Result<()> {
    std::fs::write(image.path(), bytes).unwrap();
    let params = Qcow2DevParams::new(9, None, None, true, false);
    let dev = qcow2_setup_dev_tokio(image.path(), &params).await.unwrap();
    let result = dev.check().await;
    assert_eq!(
        std::fs::read(image.path()).unwrap(),
        bytes,
        "check must be read-only"
    );
    result
}

#[tokio::test]
async fn checker_rejects_missing_data_refcounts() {
    let (image, original, l2, host, ref_offset) = fixture().await;
    for entry in [
        (1_u64 << 63) | host as u64,
        (1_u64 << 63) | host as u64 | 1,
        (1_u64 << 62) | host as u64,
    ] {
        let mut bytes = original.clone();
        bytes[l2..l2 + 8].copy_from_slice(&entry.to_be_bytes());
        bytes[ref_offset..ref_offset + 2].fill(0);
        let err = check_bytes(&image, &bytes).await.unwrap_err();
        assert_eq!(err.get_inner().kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("non-allocated"), "{err}");
    }
}

#[tokio::test]
async fn checker_accepts_allocated_zero_clusters() {
    let (image, mut bytes, l2, host, _) = fixture().await;
    let entry = (1_u64 << 63) | host as u64 | 1;
    bytes[l2..l2 + 8].copy_from_slice(&entry.to_be_bytes());
    check_bytes(&image, &bytes).await.unwrap();
}

#[tokio::test]
async fn checker_covers_both_host_clusters_of_compressed_extent() {
    let (image, mut bytes, l2, host, ref_offset) = fixture().await;
    bytes.resize(host + CLUSTER * 2, 0);
    // Unaligned compressed storage spans the end of this host cluster and
    // the first sector of its neighbor. The payload need not be decompressed
    // for a metadata/refcount check.
    let entry = (1_u64 << 62) | (1_u64 << 54) | (host + CLUSTER - 256) as u64;
    bytes[l2..l2 + 8].copy_from_slice(&entry.to_be_bytes());
    let err = check_bytes(&image, &bytes).await.unwrap_err();
    assert!(err.to_string().contains("non-allocated"), "{err}");
    bytes[ref_offset + 2..ref_offset + 4].copy_from_slice(&1_u16.to_be_bytes());
    check_bytes(&image, &bytes).await.unwrap();
}

#[tokio::test]
async fn checker_does_not_hide_leak_after_aligned_compressed_extent() {
    let (image, mut bytes, l2, host, ref_offset) = fixture().await;
    bytes.resize(host + CLUSTER * 2, 0);
    let entry = (1_u64 << 62) | (host + CLUSTER - 512) as u64;
    bytes[l2..l2 + 8].copy_from_slice(&entry.to_be_bytes());
    // This neighboring cluster is allocated but outside the compressed range.
    bytes[ref_offset + 2..ref_offset + 4].copy_from_slice(&1_u16.to_be_bytes());
    let err = check_bytes(&image, &bytes).await.unwrap_err();
    assert!(err.to_string().contains("leak"), "{err}");
}
