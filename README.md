# qcow2-rs

[![license](https://img.shields.io/badge/License-MIT-blue.svg)](https://github.com/ming1/qcow2-rs/blob/master/LICENSE-MIT)
[![license](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://github.com/ming1/qcow2-rs/blob/master/LICENSE-APACHE)

Library in qcow2-rs is for reading/writing data from qcow2 image, and
follows its features:

- async/await, support multiple io engines, verified on tokio-uring, raw
linux sync IO syscall, tokio and io-uring[^3] with smol[^2] runtime

- support both direct IO and buffered IO, for direct IO, it needs async
runtime support, such as tokio doesn't allow it.

- v2/v3 read/write, QCOW2/raw backing chains, and Deflate/Zstd compressed I/O

- internal snapshot management with disk/VM-state preservation, extended L2
subclusters, external payload files, and precise zero writes

- exact refcount checking and explicit repair, including dirty/lazy-refcount
recovery, snapshots, and persistent bitmap storage

- LUKS1 and legacy AES payload encryption, native LUKS creation, and persistent
dirty bitmap management and change tracking

- l2 table & refcount block load & store in slice way, and the minimized
slice size is block size, and the maximized size is cluster size

- block device like interface, minimized read/write unit is aligned with
block size of the FS qcow2 image

- cross-platform support, verified on linux(Fedora/ubuntu), freebsd and windows

This project is based on qcow2 implementation from `rsd`[^1]

Motivation of this project is for supporting ublk-qcow2[^4], but turns out it
becomes one generic async qcow2 library. Attributed to Rust async/.await,
the lib is well designed & implemented, and easy to extend(add new features,
improve, ...)

One utility is included in this project, which can dump qcow2 meta,
show any meta related statistics of the image, check image meta integrity &
host cluster leak, format qcow2 image and convert between qcow2 and raw.

The setup helpers resolve relative backing names from each image's directory,
honor declared `raw`/`qcow2` formats, and probe the format when no declaration is
present. They reject cycles, missing files, unsupported formats, and chains
longer than 256 images. Reads beyond a shorter backing image return zeros;
partial writes copy unchanged backing data into the overlay.

Setup helpers return opening errors. The built-in IO backends also expose
fallible `try_new` constructors; their existing `new` constructors retain their
original return types. Custom backends using the setup macros must provide
`try_new` with the same arguments and a `Qcow2Result<Self>` return type.

Call `flush_meta()` to persist metadata after writes; dropping the device does
not flush dirty caches. See [PLAN.md](PLAN.md) for implemented features, current
limits, validation results, and remaining work.

## Feature support and management

| Feature | Read/write | Create/manage | Check/repair |
| --- | --- | --- | --- |
| Basic v2/v3 images, raw/QCOW2 backing | Yes | Basic v3 formatting | Yes |
| Internal snapshots and VM state | Yes, shared-cluster COW | List/create/delete/restore; VM-state read | Yes |
| Extended L2 subclusters | Yes, including compressed reads | Open QEMU-created images | Yes |
| External data files, including raw mode | Yes | Open QEMU-created images; caller path override | Metadata refcounts |
| Lazy refcounts | Yes, durable dirty-state tracking | Open existing images | Explicit recovery |
| Deflate/Zstd compression | Strict reads; ordinary or compressed writes | Native codec selection on empty v3 images | Packed extent accounting |
| LUKS1 encryption | AES-XTS/CBC, plain/plain64/ESSIV IVs | Native AES-256-XTS creation; existing keyslots preserved | Encryption storage accounted for |
| Legacy AES encryption | Compatibility reads/writes | No native creation default | No password verifier in this format |
| Persistent bitmaps | Durable dirty tracking, including snapshot restore | List/create/clear/enable/disable/rename/merge/remove/export | Storage and consistency checked |

`dev.write_zeroes(offset, length)` guarantees zero reads. Advisory discard can
reveal backing bytes and skips compressed mappings. Extended-L2 discard rounds
inward to complete subclusters. Snapshot creation needs refcounts wider than one
bit; external data files cannot have internal snapshots.

Use `dev.create_snapshot(SnapshotOptions { id: "saved".into(),
name: "baseline".into(), ..Default::default() }, &vm_state).await?`, with
`SnapshotOptions` from `qcow2_rs::dev`. `list_snapshots`, `restore_snapshot`, and
`delete_snapshot` use snapshot IDs. `read_snapshot_vm_state(id, offset, buf)`
streams saved state; restoring a disk does not restore a running VM's state.

For an external payload path override, set
`params.set_external_data_file(path)` before opening. The declared path otherwise
resolves relative to the QCOW2 image. Custom backends can attach storage with
`dev.set_data_file(io)?` before `qcow2_prep_io()`. Setup helpers reject storage
path aliases across metadata, backing, and external files.

Checking does not modify the image. Flush pending caches before `dev.check()` or
`dev.check_report()`. To recover a dirty/corrupt image, open writable with
`params.set_repair_mode(true)` and call `dev.repair_refcounts().await?`. Guest I/O
is blocked until recovery succeeds. Repair reconstructs refcounts and copied
flags; malformed structures, ambiguous overlaps, and lost payload bytes require
other recovery tools.

The CLI exposes these operations:

```sh
rqcow2 check disk.qcow2
rqcow2 check --repair disk.qcow2
rqcow2 snapshot disk.qcow2 create saved --name baseline --vm-state state.bin
rqcow2 snapshot disk.qcow2 list
rqcow2 snapshot disk.qcow2 restore saved
rqcow2 snapshot disk.qcow2 vm-state saved exported-state.bin
rqcow2 snapshot disk.qcow2 delete saved
rqcow2 zero disk.qcow2 0 4096
```

Shared/snapshot, extended-L2, external, encrypted, compressed, and bitmap-tracked
writes currently serialize I/O and rebuild refcounts after each operation. This prioritizes durable metadata
ordering and can increase latency and sparse-file growth. Ordinary images keep
the existing cached write path. Flush excludes guest writes so lazy dirty state
is cleared only after active writes and metadata are synchronized.

## Compression, encryption, and bitmaps

`dev.set_compression_type(CompressionType::Zstd).await?` selects the codec before
allocating guest storage; `Deflate` is the default. Both types are exported from
`qcow2_rs::dev`. Use `write_compressed_at(buf, offset)` for whole clusters, or a
block-aligned final partial cluster at virtual EOF, in batches up to 32 MiB.
Streams share sectors and host clusters; packing respects the refcount width.
Incompressible clusters use ordinary storage. Ordinary `write_at` overwrites
compressed mappings with uncompressed COW storage. Compression is incompatible
with encrypted payloads and external data files.

For encrypted opening, call `params.set_encryption_secret(password_bytes)`.
`set_image_encryption_secret(path, password_bytes)?` supplies a distinct password
for an image in a backing chain. Secret copies are zeroized on drop and redacted
from parameter Debug output. For a new empty v3 image, call
`dev.initialize_luks(password_bytes, LuksOptions::default()).await?` before guest
writes. Native creation uses AES-256-XTS, SHA-256, 4000 AF stripes, and 200,000
PBKDF2 iterations. `LuksOptions` exposes the iteration count.

Existing LUKS1 AES-XTS (128/256-bit AES keys) and AES-CBC (128/192/256-bit keys)
are supported, including plain, plain64, and supported ESSIV IVs. Keyslot KDF/AF
hashes include SHA-1, SHA-224/256/384/512, and RIPEMD-160. Unsupported ciphers,
LUKS versions, and IV/hash combinations return errors. Header/key material is
limited to 32 MiB, AF stripes to 65,536, and PBKDF2 iterations to 10,000,000.
The existing eight keyslots are preserved; keyslot/password amendment is not a
native management interface. Legacy AES is provided for compatibility, has no
password verifier, and is not a creation default. Encryption covers payloads;
QCOW2 metadata stays plaintext.

`create_bitmap(name, granularity_bits, enabled)` creates a dirty bitmap. Other
methods are `list_bitmaps`, `read_bitmap`, `clear_bitmap`, `set_bitmap_enabled`,
`rename_bitmap`, `merge_bitmap`, and `remove_bitmap`. Bitmap bytes use least
significant bit first. Supported granularities are 9–31 bits (512 bytes–2 GiB),
with at most 32 MiB of data per bitmap. Compatible opaque extra data is preserved;
incompatible extra data prevents use of that bitmap.

Enabled bitmaps are saved before guest writes, zeroing, or discard. Snapshot
restore resizes every bitmap and marks the entire restored disk in enabled
bitmaps. Failed writes may leave conservative extra dirty bits. Interrupted
bitmap saves leave an `in_use` marker; inconsistent enabled bitmaps block guest
changes until explicitly cleared, disabled, or removed. Refcount repair preserves
this marker and does not manufacture missing bitmap contents. Clearing an
inconsistent bitmap explicitly starts a new tracking baseline.

```sh
rqcow2 format --compression zstd disk.qcow2
rqcow2 compress disk.qcow2 cluster-data.raw --offset 0
rqcow2 --secret-file passphrase.bin format --luks encrypted.qcow2
rqcow2 --secret-file passphrase.bin check encrypted.qcow2
rqcow2 bitmap disk.qcow2 create tracking --granularity-bits 16
rqcow2 bitmap disk.qcow2 list
rqcow2 bitmap disk.qcow2 export tracking tracking.bits
rqcow2 bitmap disk.qcow2 clear tracking
rqcow2 bitmap disk.qcow2 disable tracking
rqcow2 bitmap disk.qcow2 remove tracking
```

`--secret-file` is global and applies to check/repair, info/map, snapshots,
bitmaps, zero writes, and QCOW2-to-raw conversion. File bytes are used verbatim;
trailing newlines are part of the password. Bitmap tracking and compressed or
encrypted writes use the serialized COW/refcount rebuild path described above.

Building requires a C compiler and OpenSSL development libraries. On Debian or
Ubuntu, install `pkg-config libssl-dev`; on Windows/MSVC, install OpenSSL through
vcpkg (for example `openssl:x64-windows-static-md`) and set `VCPKG_ROOT` to its
installation. See the [OpenSSL build instructions](https://docs.rs/openssl/latest/openssl/#building)
for other platforms. Zstd is built through its Rust dependency. CI provisions native
OpenSSL for Linux/i686 and Windows; only the Linux execution was verified locally.

## Example

```Rust

    use qcow2_rs::qcow2_default_params;
    use qcow2_rs::utils::qcow2_setup_dev_uring;
    use qcow2_rs::helpers::Qcow2IoBuf;

    tokio_uring::start(async move {
        let params = Qcow2DevParams::new(9, None, None, false, false);
        let path = std::path::PathBuf::from("test.qcow2");

        // setup one qcow2 device
        let dev = qcow2_setup_dev_uring(&path, &params).await.unwrap();

        // create one slice like & aligned IO buffer
        let mut buf = Qcow2IoBuf::<u8>::new(4096);

        // read 4096 bytes to `buf` from virt offset 0 of `test.qcow2`
        let _ = dev.read_at(&mut buf, 0).await.unwrap();

        // write 4096 bytes from `buf` to virt offset 4096 of `test.qcow2`
        let _ = dev.write_at(&buf, 4096).await.unwrap();

        // flush meta data lazily, which is done in soft update style
        dev.flush_meta().await.unwrap();
    });

```

## Test

- for running the built-in test, `qemu-img` is required, so package of
`qemu-utils`(Debian/Ubuntu) or `qemu-img`(Fedora, RHEL, ...) needs to
be installed

- built CI covers 'carget test' on both ubuntu and windows


## License

This project is licensed under MIT OR Apache.

## Contributing

Any kinds of contributions are welcome!

## References

[^1]: <https://gitlab.com/hreitz/rsd/-/tree/main/src/node/qcow2?ref_type=heads>
[^2]: <https://docs.rs/smol>
[^3]: <https://docs.rs/io-uring>
[^4]: <https://github.com/ming1/rublk/tree/qcow2>
