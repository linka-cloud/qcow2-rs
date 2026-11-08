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

fn management_run(args: &[&str]) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_rqcow2"))
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    output
}

#[test]
fn native_creation_resize_amend_rebase_commit_and_preallocation() {
    let d = tempfile::tempdir().unwrap();
    let image = d.path().join("overlay");
    let base = d.path().join("base");
    let image = image.to_str().unwrap();
    let base = base.to_str().unwrap();
    std::fs::write(base, vec![0x63; 1 << 20]).unwrap();
    management_run(&[
        "format",
        image,
        "--size",
        "1",
        "--extended-l2",
        "--backing",
        "base",
        "--backing-format",
        "raw",
        "--lazy-refcounts",
        "--preallocation",
        "metadata",
    ]);
    management_run(&["snapshot", image, "create", "saved"]);
    management_run(&["zero", image, "0", "512"]);
    management_run(&["resize", image, "2097152"]);
    let failed = Command::new(env!("CARGO_BIN_EXE_rqcow2"))
        .args(["resize", image, "1048576"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    management_run(&["resize", image, "1048576", "--shrink"]);
    management_run(&[
        "amend",
        image,
        "--extended-l2",
        "off",
        "--refcount-order",
        "5",
        "--compression",
        "zstd",
        "--lazy-refcounts",
        "off",
    ]);
    management_run(&["preallocate", image, "full"]);
    management_run(&["commit", image]);
    assert_eq!(
        &std::fs::read(base).unwrap()[..1024],
        &[vec![0; 512], vec![0x63; 512]].concat()
    );
    management_run(&["snapshot", image, "restore", "saved"]);
    management_run(&["commit", image, "--keep-overlay"]);
    assert_eq!(&std::fs::read(base).unwrap()[..1024], &[0x63; 1024]);
    management_run(&["rebase", image]);
    management_run(&[
        "rebase",
        image,
        "--backing",
        "base",
        "--backing-format",
        "raw",
    ]);
    management_run(&["check", image]);
    let out = management_run(&["info", image]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Capabilities:"));
    // Creation must never overwrite an existing image.
    let before = std::fs::read(image).unwrap();
    let failed = Command::new(env!("CARGO_BIN_EXE_rqcow2"))
        .args(["format", image])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert_eq!(std::fs::read(image).unwrap(), before);
}

#[test]
fn keyslot_commands_and_distinct_backing_credentials() {
    let d = tempfile::tempdir().unwrap();
    let path = |name| d.path().join(name).to_str().unwrap().to_string();
    let base = path("base");
    let image = path("overlay");
    let first = path("first");
    let second = path("second");
    let third = path("third");
    std::fs::write(&first, b"first").unwrap();
    std::fs::write(&second, b"second").unwrap();
    std::fs::write(&third, b"third").unwrap();
    management_run(&[
        "--secret-file",
        &first,
        "format",
        &base,
        "--luks",
        "--version",
        "2",
        "--size",
        "1",
    ]);
    let override_arg = format!("{base}={first}");
    management_run(&[
        "--secret-file",
        &second,
        "--image-secret-file",
        &override_arg,
        "format",
        &image,
        "--luks",
        "--backing",
        "base",
        "--size",
        "1",
    ]);
    management_run(&[
        "--secret-file",
        &second,
        "--image-secret-file",
        &override_arg,
        "keyslot",
        &image,
        "add",
        "1",
        &third,
        "--iterations",
        "1000",
    ]);
    management_run(&[
        "--secret-file",
        &second,
        "--image-secret-file",
        &override_arg,
        "keyslot",
        &image,
        "change",
        "0",
        &third,
        "--iterations",
        "1000",
    ]);
    management_run(&[
        "--secret-file",
        &third,
        "--image-secret-file",
        &override_arg,
        "keyslot",
        &image,
        "remove",
        "1",
    ]);
    let out = management_run(&[
        "--secret-file",
        &third,
        "--image-secret-file",
        &override_arg,
        "keyslot",
        &image,
        "list",
    ]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("2\ttrue\t1000"));
    management_run(&[
        "--secret-file",
        &third,
        "--image-secret-file",
        &override_arg,
        "commit",
        &image,
    ]);
    management_run(&[
        "--secret-file",
        &third,
        "--image-secret-file",
        &override_arg,
        "check",
        &image,
    ]);
}

#[test]
fn external_creation_and_renamed_payload_amendment() {
    let d = tempfile::tempdir().unwrap();
    let path = |name| d.path().join(name).to_str().unwrap().to_string();
    let image = path("disk");
    let data = path("data");
    let renamed = path("renamed");
    management_run(&[
        "format",
        &image,
        "--size",
        "1",
        "--data-file",
        "data",
        "--data-file-raw",
        "--extended-l2",
        "--preallocation",
        "falloc",
    ]);
    std::fs::rename(&data, &renamed).unwrap();
    management_run(&[
        "--external-data-file",
        &renamed,
        "amend",
        &image,
        "--data-file-name",
        "renamed",
    ]);
    management_run(&["resize", &image, "2097152"]);
    management_run(&["check", &image]);
    assert_eq!(std::fs::metadata(&renamed).unwrap().len(), 2 << 20);
    let empty = path("empty");
    management_run(&["format", &empty, "--size", "0", "--cluster-bits", "9"]);
    management_run(&["info", &empty]);
}
