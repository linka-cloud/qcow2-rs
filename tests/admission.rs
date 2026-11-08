//! Unsupported image features must fail before guest I/O or metadata changes.

use qcow2_rs::dev::Qcow2DevParams;
use qcow2_rs::utils::{make_temp_qcow2_img, qcow2_setup_dev_tokio};
use std::io::{Seek, SeekFrom, Write};

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
