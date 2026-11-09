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

- LUKS1 and legacy AES payload encryption, native LUKS creation/keyslot management,
and persistent dirty bitmap management and change tracking

- native v2/v3 creation, resize, snapshot-preserving rebase/commit, metadata
conversion, capabilities, and configurable preallocation

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

On Linux, built-in file handles hold QEMU-compatible advisory OFD locks for
the image, backing chain, and external data. Conflicting opens fail with
`WouldBlock` and the path of the locked file. Read-only processes can share an
image, and a writer excludes other processes that use this locking protocol.
The last handle releases the lock. Custom file backends can acquire the same
lock with `locking::ImageLock`.

Call `flush_meta()` to persist metadata after writes; dropping the device does
not flush dirty caches. See [PLAN.md](PLAN.md) for implemented features, current
limits, validation results, and remaining work.

## Feature support and management

| Feature | Read/write | Create/manage | Check/repair |
| --- | --- | --- | --- |
| Basic v2/v3 images, raw/QCOW2 backing | Yes | Native creation, resize, rebase, commit, amend | Yes |
| Internal snapshots and VM state | Yes, shared-cluster COW | List/create/delete/restore; VM-state read | Yes |
| Extended L2 subclusters | Yes, including compressed reads | Native creation and conversion | Yes |
| External data files, including raw mode | Yes | Native creation, resize, rename/path override | Metadata refcounts; raw consistency recovery |
| Lazy refcounts | Yes, durable dirty-state tracking | Create and amend | Explicit recovery |
| Deflate/Zstd compression | Strict reads; ordinary or compressed writes | Native creation, empty-image selection, and metadata conversion | Packed extent accounting |
| LUKS1 encryption | AES-XTS/CBC, plain/plain64/ESSIV IVs | Native v2/v3 AES-256-XTS creation; keyslot add/rotate/revoke | Encryption storage accounted for |
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

Grouped CLI commands use `rqcow2 <command> <subcommand> <file> [arguments]`.
The image path comes after the action; earlier `<command> <file> <subcommand>`
invocations must be reordered.

The CLI exposes these operations:

```sh
rqcow2 check disk.qcow2
rqcow2 check --repair disk.qcow2
rqcow2 snapshot create disk.qcow2 saved --name baseline --vm-state state.bin
rqcow2 snapshot list disk.qcow2
rqcow2 snapshot restore disk.qcow2 saved
rqcow2 snapshot vm-state disk.qcow2 saved exported-state.bin
rqcow2 snapshot delete disk.qcow2 saved
rqcow2 zero disk.qcow2 0 4096
```

Shared/snapshot, extended-L2, external, encrypted, compressed, and bitmap-tracked
writes currently serialize I/O and rebuild refcounts after each operation. This prioritizes durable metadata
ordering and can increase latency and sparse-file growth. Ordinary images keep
the existing cached write path. Flush excludes guest writes so lazy dirty state
is cleared only after active writes and metadata are synchronized.

## Creation and image management

`create_qcow2(path, &CreateOptions).await?` creates new files without overwriting
existing storage. Options select version, virtual size in bytes, cluster/refcount
geometry, standard or extended L2, backing name/format, external payload/raw mode,
codec, lazy refcounts, LUKS credentials/KDF parameters, and preallocation. Backing
and data names resolve relative to the new metadata file. `open_params` supplies
buffered initialization parameters and backing credentials; external path overrides
are rejected during creation. Creation errors remove only files created by that call.

`dev.capabilities().await` reports the detected layout, encryption, bitmap/snapshot
presence, writable mode, recovery/reopen state, and supported operation flags.
`dev.backing_file().await` returns the immediate backing declaration and detected
format; `storage_paths().await?` enumerates the attached file-backed chain.

Management APIs use the same serialized transaction path as advanced writes:

- `resize(new_size, allow_shrink)` requires sector alignment and explicit shrink
  permission. Growth reads as zeros even over a larger backing; shrink/regrow does
  not reveal discarded bytes. Inactive snapshot sizes/data remain intact. Old v2
  snapshots without disk-size records require conversion to v3 before resize.
- `rebase(target, preserve_data)` accepts `Option<RebaseTarget<T>>`; `None` flattens.
  Preserving mode materializes active and inactive roots before changing backing.
  Non-preserving mode deliberately changes fallback contents and marks dirty maps.
- `commit_to(&backing_dev, empty_source)` and
  `commit_to_raw(&raw_io, path, empty_source)` copy into the immediate writable base,
  preserve its trailing bytes, and optionally empty the active overlay. Both source
  and destination snapshots remain intact. Partial failures can leave the base
  partly updated while the materialized source retains the complete disk view.
- `amend(AmendOptions)` converts v2/v3, all seven refcount widths, standard/extended
  L2, codec tags, and lazy policy. It preserves snapshots, bitmap objects, and VM-state
  bytes, relocating VM mappings where geometry changes. Compressed payloads become
  ordinary clusters during conversion. Invalid downgrades fail explicitly, including
  v2 with bitmaps/external data and one-bit refcounts with snapshots.
- `amend_data_file_name(path)` records an external-file rename after reopening
  with an override to the same storage. It does not move payload bytes.
- `preallocate(Preallocation::{Off, Metadata, Fallocate, Full})` materializes the
  active view. Metadata mode reserves mappings with sparse allocated-zero extents
  where possible; nonzero/backing/encrypted data must be copied. Fallocate reserves
  physical storage, using full initialization when the backend reports reservation
  unsupported. Full writes every payload cluster. Private extents are reused by
  subsequent writes; shared snapshots and metadata COW still need free space.

Run management with exclusive ownership of the image and affected backing chain.
Each device serializes its own operations. Linux allows one writer per file in
a process and shares its lock with local readers for backing commits. Coordinate
I/O between those handles. Preserving rebase, commit, and
metadata conversion can copy the complete disk and every snapshot, requiring
substantial time and temporary space. They do not reclaim the sparse file's high
water mark. A failed layout/backing publication can require closing and reopening
before explicit repair; recovery flags and `capabilities` expose that requirement.
A refcount repair cannot reconstruct lost payload bytes or a torn header.

```sh
rqcow2 format disk.qcow2 --version 3 --extended-l2 --preallocation metadata
rqcow2 format overlay.qcow2 --backing base.raw --backing-format raw
rqcow2 format external.qcow2 --data-file payload.raw --data-file-raw
rqcow2 resize disk.qcow2 0x8000000
rqcow2 resize disk.qcow2 0x4000000 --shrink
rqcow2 preallocate disk.qcow2 falloc
rqcow2 rebase overlay.qcow2 --backing replacement.qcow2 --backing-format qcow2
rqcow2 commit overlay.qcow2
rqcow2 amend disk.qcow2 --extended-l2 off --refcount-order 5 --compression zstd
rqcow2 --external-data-file renamed.raw amend external.qcow2 --data-file-name renamed.raw
rqcow2 --secret-file current.bin keyslot list encrypted.qcow2
rqcow2 --secret-file current.bin keyslot change encrypted.qcow2 0 replacement.bin
```

CLI `format --size` retains MiB units; `resize` takes bytes and accepts hexadecimal.
`--image-secret-file IMAGE=PASSWORD_FILE` can be repeated for distinct backing
credentials. `--external-data-file` overrides only the opened root's payload path.
Passphrase file bytes are used verbatim. Raw-to-QCOW2 conversion rounds virtual size
to a cluster boundary and zero-pads the tail; QCOW2-to-raw preserves exact virtual
length. Conversion rejects output aliases to source metadata, payload, or backing.

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
from parameter Debug output. For a new empty v2/v3 image, call
`dev.initialize_luks(password_bytes, LuksOptions::default()).await?` before guest
writes. Native creation uses AES-256-XTS, SHA-256, 4000 AF stripes, and 200,000
PBKDF2 iterations. `LuksOptions` exposes the iteration count.

Existing LUKS1 AES-XTS (128/256-bit AES keys) and AES-CBC (128/192/256-bit keys)
are supported, including plain, plain64, and supported ESSIV IVs. Keyslot KDF/AF
hashes include SHA-1, SHA-224/256/384/512, and RIPEMD-160. Unsupported ciphers,
LUKS versions, and IV/hash combinations return errors. Header/key material is
limited to 32 MiB, AF stripes to 65,536, and PBKDF2 iterations to 10,000,000.
Use `luks_keyslots`, `add_luks_keyslot`, `change_luks_keyslot`, and
`remove_luks_keyslot` to inspect, add, rotate, and revoke credentials. Rotation
publishes a replacement in a free slot before disabling the old slot, and returns
the replacement slot index. Removal refuses the final active slot unless
`allow_last` is explicit. Revocation disables the slot durably before scrubbing
its key material; repair finishes interrupted scrubbing. This does not promise
physical erasure from filesystem/storage snapshots or protect against torn sectors. Legacy AES is provided for compatibility, has no
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
rqcow2 bitmap create disk.qcow2 tracking --granularity-bits 16
rqcow2 bitmap list disk.qcow2
rqcow2 bitmap export disk.qcow2 tracking tracking.bits
rqcow2 bitmap clear disk.qcow2 tracking
rqcow2 bitmap disable disk.qcow2 tracking
rqcow2 bitmap remove disk.qcow2 tracking
```

`--secret-file` is global and applies to check/repair, info/map, snapshots,
bitmaps, zero writes, and QCOW2-to-raw conversion. File bytes are used verbatim;
trailing newlines are part of the password. Bitmap tracking and compressed or
encrypted writes use the serialized COW/refcount rebuild path described above.

Building requires a C compiler and OpenSSL development libraries. On Debian or
Ubuntu, install `pkg-config libssl-dev`; on Windows/MSVC, install OpenSSL through
vcpkg (for example `openssl:x64-windows-static-md`) and set `VCPKG_ROOT` to its
installation. See the [OpenSSL build instructions](https://docs.rs/openssl/latest/openssl/#building)
for other platforms. Zstd is built through its Rust dependency. Windows CI uses the
runner's preinstalled OpenSSL headers and static MSVC libraries.

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

The integration tests require `qemu-img` and `qemu-io` on `PATH`. On Debian or
Ubuntu, install `qemu-utils`. Windows CI installs the full QEMU package from MSYS2
UCRT64, which provides both tools.

## Management validation

The management APIs above have end-to-end coverage for native and QEMU-created
images. A deterministic byte model covers all supported cluster sizes and all
seven refcount widths, including writes, zeros, backing discard, resize,
preallocation, amendment, rebase/commit, and snapshot restoration. QEMU independently
checks metadata and compares/exports the resulting guest bytes.

```sh
cargo test --test management_matrix -- --nocapture
cargo test --test management_contract --test management_backends
cargo test --test crash_management -- --nocapture
```

The crash regression kills separate processes at 18 transaction checkpoints,
then verifies repair, guest bytes, snapshots/VM state, and raw external consistency.
Backend workflows run with plaintext/encrypted backing chains under Tokio,
synchronous buffered/direct I/O, and Linux io_uring buffered/direct I/O. The full
geometry matrix can take several minutes in either build profile.

CI runs debug/release tests on Linux, Windows, macOS, and a native FreeBSD VM.
The three hosted platforms also run all-target Clippy;
Windows requires a modern QEMU fixture tool so advanced-feature checks execute.
The new non-Linux jobs were configured here but have not been run locally.
Process termination tests retain the host filesystem cache; they do not establish
physical power-loss or torn-sector recovery guarantees. See [PLAN.md](PLAN.md)
for the current validation checkpoint and implementation limits.

## License

This project is licensed under MIT OR Apache.

## Contributing

Any kinds of contributions are welcome!

## References

[^1]: <https://gitlab.com/hreitz/rsd/-/tree/main/src/node/qcow2?ref_type=heads>
[^2]: <https://docs.rs/smol>
[^3]: <https://docs.rs/io-uring>
[^4]: <https://github.com/ming1/rublk/tree/qcow2>
