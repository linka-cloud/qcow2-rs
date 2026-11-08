#![allow(dead_code)]
use std::{io::Read, path::Path, process::Command};

pub fn qemu_options() -> String {
    let help = Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o", "help"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&help.stdout).into_owned()
}

/// Skip only external checks unsupported by an older fixture tool. Native
/// API and consistency checks still run for every layout.
pub fn qemu_supports_image(path: &Path) -> bool {
    let mut bytes = [0; 112];
    let mut file = std::fs::File::open(path).unwrap();
    file.read_exact(&mut bytes).unwrap();
    let help = qemu_options();
    let version = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
    let features = if version == 3 {
        u64::from_be_bytes(bytes[72..80].try_into().unwrap())
    } else {
        0
    };
    (features & (1 << 2) == 0 || help.contains("data_file"))
        && (features & (1 << 3) == 0 || help.contains("compression_type"))
        && (features & (1 << 4) == 0 || help.contains("extended_l2"))
        && (bytes[20..24] != 2_u32.to_be_bytes() || help.contains("encrypt.format"))
}
