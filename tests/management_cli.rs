use std::process::Command;

#[test]
fn snapshot_and_zero_commands_roundtrip_disk_and_vm_state() {
    let d = tempfile::tempdir().unwrap();
    let image = d.path().join("disk.qcow2");
    qcow2_rs::utils::make_temp_qcow2_img(1 << 20, 16, 4)
        .persist(&image)
        .unwrap();
    let state = d.path().join("state.bin");
    let exported = d.path().join("export.bin");
    std::fs::write(&state, b"saved VM state").unwrap();
    let run = |args: &[&std::ffi::OsStr]| {
        let output = Command::new(env!("CARGO_BIN_EXE_rqcow2"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        output
    };
    run(&[
        "snapshot".as_ref(),
        image.as_os_str(),
        "create".as_ref(),
        "saved".as_ref(),
        "--vm-state".as_ref(),
        state.as_os_str(),
    ]);
    let output = run(&["snapshot".as_ref(), image.as_os_str(), "list".as_ref()]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("saved"));
    run(&[
        "zero".as_ref(),
        image.as_os_str(),
        "0".as_ref(),
        "512".as_ref(),
    ]);
    run(&[
        "snapshot".as_ref(),
        image.as_os_str(),
        "restore".as_ref(),
        "saved".as_ref(),
    ]);
    run(&[
        "snapshot".as_ref(),
        image.as_os_str(),
        "vm-state".as_ref(),
        "saved".as_ref(),
        exported.as_os_str(),
    ]);
    assert_eq!(std::fs::read(exported).unwrap(), b"saved VM state");
    run(&[
        "snapshot".as_ref(),
        image.as_os_str(),
        "delete".as_ref(),
        "saved".as_ref(),
    ]);
    run(&["check".as_ref(), image.as_os_str()]);
}

#[test]
fn phase4_creation_bitmaps_compressed_writes_and_secrets() {
    let d = tempfile::tempdir().unwrap();
    let secret = d.path().join("secret");
    std::fs::write(&secret, b"private passphrase").unwrap();
    let input = d.path().join("input.raw");
    std::fs::write(&input, vec![0xa7; 65536]).unwrap();
    let run = |args: &[&std::ffi::OsStr]| {
        let output = Command::new(env!("CARGO_BIN_EXE_rqcow2"))
            .arg("--secret-file")
            .arg(&secret)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        output
    };
    for encryption in [false, true] {
        let image = d.path().join(if encryption {
            "encrypted.qcow2"
        } else {
            "compressed.qcow2"
        });
        if encryption {
            run(&[
                "format".as_ref(),
                "--size".as_ref(),
                "1".as_ref(),
                "--luks".as_ref(),
                image.as_os_str(),
            ]);
        } else {
            run(&[
                "format".as_ref(),
                "--size".as_ref(),
                "1".as_ref(),
                "--compression".as_ref(),
                "zstd".as_ref(),
                image.as_os_str(),
            ]);
        }
        run(&[
            "bitmap".as_ref(),
            image.as_os_str(),
            "create".as_ref(),
            "tracking".as_ref(),
            "--granularity-bits".as_ref(),
            "9".as_ref(),
        ]);
        if encryption {
            run(&[
                "zero".as_ref(),
                image.as_os_str(),
                "4096".as_ref(),
                "4096".as_ref(),
            ]);
        } else {
            run(&["compress".as_ref(), image.as_os_str(), input.as_os_str()]);
        }
        let export = d.path().join(if encryption {
            "encrypted.bits"
        } else {
            "compressed.bits"
        });
        run(&[
            "bitmap".as_ref(),
            image.as_os_str(),
            "export".as_ref(),
            "tracking".as_ref(),
            export.as_os_str(),
        ]);
        let bits = std::fs::read(export).unwrap();
        if encryption {
            assert_eq!(bits[1], 0xff);
            assert_eq!(bits[0], 0);
        } else {
            assert!(bits[..16].iter().all(|b| *b == 0xff));
        }
        run(&["check".as_ref(), image.as_os_str()]);
        let info = run(&["bitmap".as_ref(), image.as_os_str(), "list".as_ref()]);
        assert!(String::from_utf8_lossy(&info.stdout).contains("tracking"));
        if encryption {
            let before = std::fs::read(&image).unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_rqcow2"))
                .arg("check")
                .arg(&image)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(!String::from_utf8_lossy(&output.stderr).contains("private passphrase"));
            assert_eq!(std::fs::read(&image).unwrap(), before);
        }
    }
}
