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
