# Complete QCOW2 Support

Assessment date: 2026-11-08.

Status: **Phases 1–4 implemented within the documented limits.** See the latest
Phase 4 checkpoint below for interoperability, validation, supported encryption
parameters, and remaining image-management work.

## Summary and evidence

The initial assessment found core QCOW2 v3 disk I/O and gaps in safe feature
admission. The implementation now covers Phases 1–4 within the limits documented
below. Algorithm/size limits and additional image-management parity remain explicit.

This plan is based on source and existing-test inspection, compared with the
[QEMU QCOW2 specification](https://www.qemu.org/docs/master/interop/qcow2.html).
The initial findings were source-level observations, not runtime-confirmed results.
`cargo test --locked --offline --lib` could not start because `crypto-hash`
was missing from the local dependency cache.

### Implementation progress

Entries below are chronological; the latest checkpoint supersedes earlier
limitations and test counts.

The first Phase 1 batch is implemented in separate commits:

| Commit | Change |
| --- | --- |
| `d129a0e` | Reject encrypted images before guest I/O; propagate device setup errors |
| `2b57774` | Reject writable internal-snapshot images while retaining read-only access |
| `82770c3` | Parse/serialize v2 headers and validate version-specific header layouts |
| `7be7ffa` | Preserve header lengths, unknown header fields, and extensions on rewrite |
| `521bd28` | Support default caches with small clusters and reject invalid geometry |

Dependencies are now available. Validation after this batch:

- `cargo test`: all 59 tests passed, including the Linux io_uring backend.
- New regressions cover unsupported-feature admission without file modification,
  malformed layouts, v2 backing names, unknown-header preservation, and invalid
  block/cache parameters.
- QEMU-created v2 and 512-byte through 4 KiB cluster fixtures pass writes,
  flush/reopen byte checks, and `qemu-img check`.
- `cargo fmt --check`: passed.
- `cargo clippy --lib --test admission --test versions --no-deps`: passed with
  warnings.
- `cargo clippy --all-targets --no-deps`: fails on existing unchecked read/write
  amounts in unchanged integration tests (`clippy::unused_io_amount`).

The first batch did not complete Phase 1. Autoclear consistency and discard
safety were addressed in the second batch below; broader malformed-metadata
validation and allocation growth remain open.

Second batch:

| Commit | Change |
| --- | --- |
| `711b259` | Clear autoclear flags durably before writes/discard, with retryable failure handling and aligned direct I/O |
| `cf697bb` | Persist mapping removal before discard reclamation; exclude concurrent reads/writes and skip shared clusters |
| `3809d4c` | Reject malformed/duplicate extensions, invalid end markers, padding overlap, and backing-name/header overlap |
| `a7493be` | Keep bitmap fixtures compatible with older QEMU and scope test borrows before awaits |

Second-batch validation: `cargo test` passes all 70 tests, including io_uring.
Formatting and Clippy for the library plus new regression targets pass (Clippy
emits warnings). The existing all-target Clippy failures noted above remain.
Failure injection verifies that autoclear write/fsync errors prevent guest
mutation and that discard metadata write/fsync errors do not free or punch the
affected host cluster. A gated reclamation test verifies that reads and writes
wait until discard completes. Shared-data and backing-image fixtures pass
`qemu-img check`; malformed feature-name records now return errors instead of
panicking.

Discard remains advisory: shared/compressed clusters are skipped, and clearing
an overlay mapping reveals backing data. It now takes an exclusive device I/O
lock and flushes mapping removal before reclaiming each cluster, trading discard
throughput for safety. Failures can leave allocated leaks for later repair.
Persistent bitmap contents are invalidated, not tracked or repaired. The bitmap
fixture test skips only when the installed QEMU lacks the bitmap subcommand;
the other autoclear tests still run.

Third batch:

| Commit | Change |
| --- | --- |
| `db4ec64` | Correct L1 growth byte sizing, zero initialization, entry counts, and complete relocation writes |
| `20ee405` | Require exact contiguous L1 allocations and release/retry short candidates |
| `bdea63a` | Return missing-refcount errors and account for allocated zero clusters and compressed extents correctly |
| `8ee0a05` | Relocate refcount tables without recursive locking, zero-length operations, incomplete writes, or unaligned header publication |

Third-batch validation: all 80 tests pass with `cargo test`, including io_uring;
formatting and Clippy for the library and regression targets pass with warnings.
New allocation regressions cover a fragmented refblock boundary, unsupported
exact-allocation sizes, and table relocation under buffered/direct I/O. Relocated
images pass `qemu-img check`, and reopen checks verify the old table was freed
and the replacement remains allocated. Checker regressions verify read-only
error reporting, allocated zero clusters, compressed storage spanning two host
clusters, and leak detection at an aligned compressed-range endpoint.

Large contiguous allocations spanning refblocks remain unsupported. Refcount
table growth still has the documented single-refblock allocation limit. The
checker does not yet reconstruct exact counts or traverse all optional metadata.
Header publication now flushes dependencies before an aligned header write and
syncs that write before old storage can be released; partial/failed header I/O
can still require on-disk recovery.

### Phase 1 completion / Phase 2 start

| Commit | Change |
| --- | --- |
| `f63e4b3` | Allocate contiguous metadata across refblocks and remove the single-refblock table-growth limit |
| `1437e24` | Validate metadata ranges, capacity, flag prerequisites, entries, and truncation; initialize complete metadata clusters |
| `849fb03`, `af2c862` | Retain failed metadata writes for retry and preserve ordering across failed synchronization |
| `d221803` | Add fallible backend opening and report short-write failures |
| `dc68fbe` | Resolve relative backing paths per image and reject canonical-path cycles |
| `47ef2ee` | Support raw backing files, honor declared formats, and zero-fill backing EOF during reads and COW |
| `2a6fb06`, `f05ebf9` | Reject overflowing writes and invalid backing-device/cache parameters without panics |
| `54a2225` | Verify the full supported 32 MiB contiguous metadata extent |

All Phase 1 checklist items are complete. The supported limits remain a 32 MiB
L1 table, 8 MiB refcount table, 2 MiB clusters, and 56-bit host offsets. Large
exact allocations use fresh storage beyond the old refcount table's coverage;
placement of refblocks outside the requested extent preserves contiguity.
Whole metadata clusters are initialized before publishing their pointers.

Phase 2 now opens raw/QCOW2 backing chains with relative paths, explicit format
selection or magic probing when absent, canonical-path cycle detection, and a
256-image chain limit. Missing files and unsupported formats return errors.
Reads and partial-cluster COW preserve backing bytes and zero-fill regions beyond
shorter raw or QCOW2 backings. Backing files are opened read-only.

Validation at this checkpoint: `cargo test --quiet` passes all 94 tests,
including io_uring. Formatting and Clippy for the library and new regression
targets pass with warnings; the previously recorded all-target lint failures
in older test helpers remain outside this change. New backing fixtures pass
`qemu-img compare` and `qemu-img check`; allocation fixtures also pass QEMU checks.
Regression tests cover truncated/invalid headers and tables, repeated invalid
slice reads, v2 zero flags, empty/overflowing I/O, failed write/fsync retries,
relative multi-level chains, symlink cycles, and unchanged backing data.

Public interface additions: each built-in backend has a fallible `try_new`
constructor. Existing `new` constructors retain their return type. Setup macros
now expect `try_new` from custom backends; setup helper signatures are unchanged.

The target is complete on-disk format support for QCOW2 versions 2 and 3,
including safe reads, writes, metadata preservation, and recovery where
applicable. QEMU implementation limits can remain where explicitly validated
and documented. Unsupported or invalid feature combinations must fail with
clear errors; full support does not mean accepting every combination.

### Phase 2 and Phase 3 completion

| Commit | Change |
| --- | --- |
| `051a6e8` | Reconstruct exact reachable refcounts, including snapshots and optional metadata |
| `10d2740` | Explicit refcount repair, dirty/corrupt recovery, and lazy-refcount dirty transitions |
| `6d23540` | Extended L2 subclusters, backing fallback, shared metadata/data COW, and precise zero writes |
| `0af35bf` | External data storage, caller overrides, and raw-data consistency |
| `ab7f78b` | Internal snapshot list/create/delete/restore, VM state, and copied-flag reconstruction |
| `eb1a80e` | Aligned subcluster payload reads, including 4 KiB direct I/O and compressed EOF |
| `8dd904d`, `5e49631` | Recovery failure injection, optional metadata preservation, all refcount widths, and interrupted lazy updates |
| `4d98892` | Snapshot, VM-state export, and zero-write CLI commands |
| `e74ace6` | Serialize flush against guest writes before clearing lazy dirty state |
| `58fc957` | Include inactive snapshots/VM state and optional metadata in usage; bound snapshot geometry and optional objects |

The checker traverses every active and inactive L1/L2 root, compressed extent,
allocated zero cluster, snapshot directory/VM-state mapping, refcount block,
and bitmap directory/table/data cluster. It detects storage-role overlaps,
exact count mismatches, and inaccurate active copied flags. Checking is read-only;
flush pending caches before checking or collecting usage statistics.

`check_report()` returns structured mismatches and consistency state. Explicit
`repair_refcounts()` writes replacement refcount metadata in fresh storage and
publishes it durably before releasing old references. Repair reconstructs
active copied flags and clears dirty/corrupt markers only after synchronization.
It rejects ambiguous structural overlaps and malformed mappings rather than
trying to invent missing guest data. Writable opening of dirty/corrupt images
requires `Qcow2DevParams::set_repair_mode(true)`; this permits repair, not guest
I/O before repair. Read-only metadata inspection remains available.

Snapshots preserve identity, disk size, timing, instruction count, opaque extra
data, and saved VM-state bytes. Native v2/v3 and QEMU-created snapshots are
supported. Restore switches disk contents/size; callers restore the exported VM
state in their VM separately. One-bit refcounts cannot represent snapshot
sharing and are rejected for snapshot creation. Snapshot directories, bitmap
directories/tables, and other individually read metadata objects have a 32 MiB
limit in addition to the existing L1/refcount-table limits.

Extended L2 supports 128-bit descriptors, allocation/zero subcluster bitmaps,
backing fallback, compressed reads/overwrite, and inward-rounded advisory
discard. External payload files use fixed guest offsets and no QCOW2 payload
refcounts. Helpers resolve their declared path relative to the metadata image;
callers can override it with `set_external_data_file` or attach backend storage
with `set_data_file` before I/O preparation. Raw mode synchronizes actual zero
bytes for zero/discard and preserves the raw-consistency flag. Missing files,
path aliases, external snapshots/compression, raw external backing, invalid
bitmaps, and invalid fixed-offset mappings are rejected.

Shared/snapshot, extended-L2, and external writes currently serialize guest I/O,
clone affected metadata, and rebuild refcounts after each operation. This favors
correctness over throughput and can grow the sparse host file. The native
formatter still creates basic v3 images; use QEMU to create extended-L2/external
fixtures. Persistent bitmaps are accounted for and preserved by repair, but their
consistency flag is invalidated before untracked guest changes; tracking and
bitmap lifecycle management remain Phase 4. Encrypted images and Zstd remain
unsupported. Torn header writes or missing payload bytes cannot be reconstructed
by a refcount-only repair.

Validation covers QEMU check/compare and reopen, ordinary/extended snapshots,
v2 snapshot lifecycle, saved state across cluster boundaries, size restoration,
external raw/non-raw data and backing, path overrides/aliases, 512-byte
subclusters with 4 KiB direct I/O, compressed overwrites, malformed bitmaps,
read-only management, all seven refcount widths, lazy interruption, and injected
snapshot-COW failure followed by repair. Full-suite and lint results are recorded
in the final validation checkpoint below.

### Phase 4 completion

| Commit | Change |
| --- | --- |
| `5f93b0e` | Add maintained Zstd, OpenSSL, and secret-zeroization dependencies |
| `b01d88a` | Strict Deflate/Zstd readers, packed compressed output, and incompressible fallback |
| `b8f561b` | LUKS1/legacy AES payload I/O, eight-slot unlocking, native LUKS creation, and secret inputs |
| `f19ae5a` | Persistent bitmap lifecycle, durable dirty tracking, consistency reporting, and failure recovery |
| `8e7aa20` | Native codec selection and compressed packing within narrow refcount capacities |
| `8a99112` | CLI encrypted/Zstd formatting, secret files, compressed writes, and bitmap management/export |
| `447c540` | Bound untrusted encryption/bitmap fields and verify size-changing bitmap restores |
| `4e6484a` | Provision OpenSSL development libraries in Linux/i686 and Windows CI |
| `8cd2193` | Verify direct io_uring, encrypted backing credentials, and packed boundary crossings |
| `608465d` | Validate pointer alignment and every active LUKS keyslot before credential acceptance |
| `030e425` | Preserve the distinction between bitmap extension consistency and per-bitmap in-use state |
| `d4d9f78` | Block bitmap content reads while image metadata recovery is required |

Deflate and Zstd decoding requires a complete stream and exactly one guest
cluster of output. Packed writes can share sectors and cross host clusters;
1-/2-/4-bit reference counts limit sharing instead of overflowing. Unsupported
codec values and compressed encryption/external combinations remain rejected.
`set_compression_type` selects the format on empty v3 images;
`write_compressed_at` accepts whole clusters (or a block-aligned final partial
cluster at virtual EOF), with a 32 MiB batch limit.

Encryption implements QCOW2 LUKS1 and legacy AES. Native initialization on empty
v3 images writes AES-256-XTS, SHA-256, 4000-stripe AF material, salts, a UUID, and
an encrypted keyslot before publishing its header pointer. Existing AES-XTS
(128/256-bit AES keys), AES-CBC (128/192/256-bit keys), plain/plain64 IVs, and
supported ESSIV hashes are accepted. The payload IV uses physical offsets for
LUKS and guest offsets for legacy AES. Snapshot VM-state storage uses the same
payload cryptography. All eight existing keyslots and their storage are preserved
by normal operations and repair. Wrong LUKS passwords fail before mutation;
legacy AES cannot verify passwords because its format stores no verifier.
Secret copies are zeroized and Debug output is redacted; per-image credentials
support backing chains with distinct passwords.

Supported LUKS KDF/AF hashes are SHA-1, SHA-224/256/384/512, and RIPEMD-160.
Other ciphers, versions (including LUKS2), and unsupported IV/hash combinations
fail explicitly. Header/key material is limited to 32 MiB, AF stripes to 65,536,
and PBKDF2 iterations to 10,000,000. Native creation defaults to 200,000 iterations
and exposes the iteration count through `LuksOptions`. Keyslot/password amendment
is not a native API. Legacy AES is compatibility-only, not a creation default.
Raw external-data consistency is incompatible with encrypted payloads.

Bitmap APIs implement list/create/read/clear/enable/disable/rename/merge/remove,
plus CLI byte export. Directory fields, tables, flags, padding, and names are
validated; compatible opaque extra data is preserved and incompatible data is
not used. Enabled bitmaps are persisted before guest mutations. Writes, zeros,
discard, and compressed writes mark attempted ranges conservatively; snapshot
restore resizes all maps and marks the entire new disk in enabled maps. Bitmap
updates use COW objects and set the old directory's `in_use` flags durably before
replacement. A failed update blocks further I/O until metadata recovery; bitmap
inconsistency remains visible after refcount repair and requires an explicit
reset/disable/removal. Native tracking preserves the bitmap autoclear flag.
QEMU-compatible bitmap granularity is limited to 9–31 bits and each bitmap's
expanded data to 32 MiB; directory/table limits remain 32 MiB.

Validation includes QEMU check/compare, ordinary/extended-L2 codec fixtures,
sector sharing and host-boundary crossings, long-distance Deflate references,
incompressible fallback, narrow refcounts, malformed/truncated/oversized streams,
QEMU/native LUKS headers, AES-XTS/CBC-ESSIV/legacy AES, encrypted snapshots/VM
state, malformed keyslot parameters, wrong/missing secrets, native/QEMU bitmap
merges and writes, opaque extra data, bitmap save failure, read-only management,
and size-changing snapshot restore. A combined io_uring test covers encrypted
and Zstd payloads with bitmap tracking under 4 KiB direct I/O and reopen.

The implementation follows the [QCOW2 format specification](https://www.qemu.org/docs/master/interop/qcow2.html),
with cryptographic interoperability checked against
[QEMU’s LUKS1 implementation](https://github.com/qemu/qemu/blob/master/crypto/block-luks.c)
and [legacy AES implementation](https://github.com/qemu/qemu/blob/master/crypto/block-qcow.c). OpenSSL supplies AES, hashes, PBKDF2, randomness, and constant-time
key-digest comparison.
Serialized COW/refcount rebuilding and eager bitmap persistence prioritize
ordering over throughput and can grow sparse images. Native advanced-layout
formatting, resize/rebase/commit/amend/preallocation, keyslot amendment, additional
cipher families, and a new non-Linux runtime validation matrix remain separate.

## Baseline feature coverage (before Phase 1)

| Feature | Current implementation |
| --- | --- |
| Basic reads/writes | Allocated and sparse clusters, partial-cluster writes, concurrent I/O |
| Standard L1/L2 mappings | Lookup, allocation, caching, persistence, and table growth |
| Refcounts | Allocation/freeing and encodings for 1, 2, 4, 8, 16, 32, and 64-bit entries |
| QCOW2 backing chains | Recursive opening, backing reads, and copy-on-write into the overlay |
| Deflate compression | Reads compressed clusters; overwrites become uncompressed clusters |
| Zero clusters | Recognizes zero flags and reads them as zeros |
| Discard | Whole ordinary clusters; skips partial clusters and compressed clusters |
| Creation/conversion | Creates basic v3 images; raw to QCOW2 and QCOW2 to raw conversion |
| Inspection | Header/table dumps, mappings, usage statistics, basic consistency/leak checks |
| I/O backends | Tokio, synchronous I/O, Linux io_uring; buffered/direct I/O depending on backend |

Key implementation references:

- `src/meta/header.rs`: header parsing, serialization, extensions, feature admission.
- `src/meta/l2.rs` and `src/dev/info.rs`: mapping representation and geometry.
- `src/dev/read.rs` and `src/dev/write.rs`: reads, writes, backing and compressed COW.
- `src/meta/refcount.rs` and `src/dev/alloc.rs`: refcount encodings and allocation.
- `src/dev/discard.rs`: guest discard and host extent reclamation.
- `src/dev/check.rs`: current checker and cluster usage traversal.
- `src/utils.rs`: device and backing-chain setup.
- `src/main.rs`: image utility commands.

Existing tests exercise basic I/O, compressed reads and overwrites, backing
reads and writes, partial and concurrent I/O, cache behavior, discard, host
allocation operations, and conversion. Their presence is not proof of full
format coverage.

## Phase 1: Correctness and safe feature admission

Address these before advertising broader feature support:

- [x] Reject encrypted images explicitly until cryptographic I/O is implemented.
      `crypt_method` is currently parsed/displayed, but payload reads and writes
      never encrypt or decrypt.
- [x] Reject writable snapshot-bearing images until shared metadata and data
      COW is safe. Existing L2 tables are reused without shared-table COW, and
      replacing a shared data mapping lacks a general path to preserve untouched
      bytes. Read-only access to the active image must remain separately tested.
- [x] Implement autoclear handling before writes can invalidate unsupported
      metadata. Clear flags durably before the first write/discard, retain them
      for reads, and prevent mutation if invalidation fails.
- [x] Fix discard semantics with backing files: clearing an L2 mapping exposes
      backing data and does not guarantee zero reads. Document discard behavior
      accurately and keep explicit zero-write semantics separate.
- [x] Reclaim host extents only after their refcount reaches zero. Discard leaves
      shared host data untouched.
- [x] Review discard ordering, concurrent writes/allocation, and failure handling
      so an extent cannot be reclaimed while still referenced or after reuse.
      Serialize discard against reads/writes, persist mapping removal first,
      and conservatively skip shared/compressed clusters. Failed operations can
      leave allocated leaks for future repair instead of reclaiming live data.
- [x] Parse v2 headers using their 72-byte layout and implied defaults; serialize
      them using the correct layout.
- [x] Accept only supported format versions and validate header lengths,
      refcount orders, offsets, sizes, extension bounds, and reserved fields
      before using them. Return errors rather than panicking on malformed input.
      Enforce supported table/virtual-size limits, table range/alignment and
      overlap checks, flag prerequisites, extension bounds, and reserved bits
      when loading L1/L2/refcount entries. Reject truncated metadata and invalid
      cached slices on every attempt. Load only declared L1 entries and zero
      padding before it can become active. Exact reference reconciliation and
      optional-metadata overlap traversal remain Phase 2 checker work.
- [x] Preserve unknown additional header fields when rewriting the header.
      Unknown extensions are already retained as opaque data; extend this
      preservation to unknown header bytes.
- [x] Fix cache geometry for clusters smaller than the default 4 KiB slice,
      and validate cluster/block-size combinations explicitly.
- [x] Complete contiguous allocation handling, including the larger-allocation
      TODO in refcount-table growth and the short-allocation FIXME in L1 growth.
      L1 clones now size storage in bytes, zero new entries, advertise the
      requested entry count, and mark every relocated block for writing.
      L1 relocation now requires an exact contiguous range, releases short
      candidates, and retries the next refblock. Large exact allocations use a
      fresh contiguous extent with refblocks placed outside it. Replacement
      refcount tables can span multiple refblocks; their self-accounting layout
      grows until it covers the table, all refblocks, and any reserved extent.
      Existing 32 MiB L1 / 8 MiB refcount-table implementation limits remain.

Acceptance: unsupported images fail before mutation; supported images preserve
unmodified bytes and metadata across writes, discard, flush, and reopen.

## Phase 2: Backing compatibility and complete checking

- [x] Support raw backing files and honor the declared backing format instead
      of recursively treating every backing file as QCOW2.
- [x] Resolve relative backing paths against the containing image directory.
- [x] Detect backing cycles and return clear chain-opening errors.
- [x] Test multi-level backing chains and zero filling beyond a shorter backing
      image, including partial-cluster reads and writes at its boundary.
- [x] Build a metadata traversal that calculates exact expected refcounts,
      rather than only testing allocation/non-allocation and leaks.
- [x] Report invalid mappings as checker failures. Missing data refcounts now
      return `InvalidData` errors with guest/host offsets instead of only printing
      diagnostics. Allocated zero clusters and all compressed host clusters are
      included in validation and usage accounting.
- [x] Cover compressed extents, allocated zero clusters, metadata overlaps,
      and all referenced metadata; extend traversal as later features land.
- [x] Add explicit repair operations for reconstructing refcounts and resolving
      supported inconsistencies. Checking remains read-only by default.
- [x] Implement dirty/corrupt state handling and durable dirty-state transitions.
      Dirty/corrupt images require explicit recovery before guest I/O.
- [x] Implement lazy refcounts and recovery using durable dirty transitions,
      cached updates, and explicit reachability-based recovery. Flush excludes
      concurrent writers before marking the image clean.

Acceptance: checker results agree with QEMU for supported fixtures; repair
restores valid refcounts without changing guest data, including after injected
write/flush failures.

## Phase 3: Snapshots, extended L2, and external data files

### Internal snapshots

- [x] Parse and preserve snapshot directory entries and their extra data.
- [x] Add list/create/delete/restore operations with snapshot identity and size
      handling, plus VM-state metadata/payload support for complete coverage.
- [x] Implement shared L2-table and data-cluster COW, preserving untouched bytes
      during partial writes and releasing old references correctly.
- [x] Reconstruct copied flags when switching the active snapshot.
- [x] Include snapshot directories, inactive L1/L2 tables, and VM-state storage
      in checking, allocation accounting, and repair.

### Extended L2 entries

- [x] Support 128-bit entries and the resulting L1/L2 addressing geometry.
- [x] Implement subcluster allocation and zero bitmaps, backing fallback,
      partial writes, and discard/zero operations.
- [x] Preserve compressed-cluster behavior within extended-L2 images.
- [x] Validate cluster-size constraints and invalid bitmap combinations.

### External data files

- [x] Parse the data-file extension and allow callers to supply the data file.
- [x] Route guest payload I/O to the separate file while keeping metadata in
      the QCOW2 file; apply fixed guest/host mapping and refcount rules.
- [x] Implement raw external-data consistency, including zeroing and discard.
- [x] Validate feature combinations and missing/unusable data files.

Acceptance: QEMU-created images can be read, modified, and reopened by both
implementations; snapshot data remains unchanged after active-image writes.

## Phase 4: Compression, encryption, and persistent bitmaps

### Compression

- [x] Recognize the compression-type incompatible feature and implement Zstd
      decompression alongside the existing Deflate reader.
- [x] Add Deflate and Zstd compressed output with packed extent allocation,
      correct refcounts, and uncompressed fallback when compression does not fit.
- [x] Validate decompression completion/output size and malformed streams.
- [x] Test packed extents sharing sectors and crossing host-cluster boundaries.

### Encryption

- [x] Add explicit key/secret inputs and cryptographic payload I/O.
- [x] Implement the LUKS header-pointer extension and associated header/key data.
- [x] Implement legacy AES compatibility if claiming all specified encryption
      methods; document its legacy status and do not make it a creation default.
- [x] Include encryption metadata in allocation accounting and checking.
- [x] Validate wrong/missing keys and unsupported encryption parameters without
      exposing secrets in errors or logs.

### Persistent dirty bitmaps

- [x] Parse/preserve bitmap directories, tables, data, flags, and extra data.
- [x] Add bitmap lifecycle operations and change tracking for writes, zeros,
      discard, snapshot switching, and virtual-size changes as applicable.
- [x] Implement `in_use`, enabled/auto, and autoclear consistency behavior.
- [x] Include bitmap storage in refcount checking and repair. Directory/table
      traversal and native tracking preserve consistent bitmap storage.

Acceptance: codec output, encrypted payloads, and persisted bitmap contents
interoperate with QEMU; interrupted updates are detected through consistency
state rather than silently treated as valid.

## Public interfaces and documentation

- [ ] Expose supported/detected capabilities and actionable unsupported-feature
      errors; replace blanket incompatible-feature rejection incrementally as
      each feature becomes operational.
- [ ] Extend open/create options for backing formats, external data files,
      compression, encryption credentials, and format version. External-path and
      secret options, native codec selection, and LUKS initialization are complete;
      native formatting remains basic v3 (advanced layout creation/version
      selection is still a separate interface task).
- [x] Add snapshot, bitmap, explicit zero-write, and check/repair interfaces as
      their implementations land; extend the CLI for those capabilities.
- [x] Document flush/durability requirements and failure behavior. Dirty cache
      contents must not be described as automatically persisted on drop.
- [x] Publish a feature matrix distinguishing read, write, create, management,
      recovery, and tested interoperability support.

Preserve existing ordinary-image APIs where practical. Add explicit operations
for feature management and repair rather than introducing implicit destructive
behavior during open.

## Validation strategy

- [x] Restore dependency availability and run library/integration tests before
      treating the source assessment as a verified baseline.
- [ ] Generate fixtures with QEMU for v2/v3, supported cluster sizes and all
      refcount widths, backing chains, zero/preallocated clusters, compression,
      snapshots, extended L2, external data files, encryption, and bitmaps.
- [ ] Compare guest bytes with QEMU after reads, partial/full writes, discard,
      zeroing, flush, close, and reopen; verify untouched neighboring data.
- [ ] Run `qemu-img check` on library-created and library-modified images and
      compare checker outcomes on deliberately inconsistent fixtures.
- [ ] Exercise valid feature combinations and rejection of invalid combinations.
- [ ] Inject short I/O, write failures, allocation failures, flush failures, and
      crashes at metadata publication boundaries; check recovery and leak safety.
- [ ] Add regression tests for each Phase 1 issue, especially shared clusters,
      backing-file discard, autoclear invalidation, and v2 header parsing.
- [ ] Exercise concurrent I/O and the applicable backend/platform matrix.

## Final Phase 2/3 validation checkpoint

- `cargo test --quiet`: **114 passed, 0 failed**, including Linux io_uring.
- `cargo fmt --check` and `git diff --check`: passed.
- `cargo clippy --lib --bin rqcow2 --test phase3 --test check --test autoclear
  --test management_cli --no-deps`: passed with warnings.
- QEMU checks/guest comparisons pass for the new interoperability fixtures.
  Extended-L2/external fixtures skip when the installed QEMU predates those
  features; persistent-bitmap fixtures similarly require QEMU's bitmap command.
- The previously recorded all-target Clippy failures in older unchanged test
  helpers remain outside this work. This checkpoint verifies the Linux runtime;
  it does not claim a new FreeBSD/macOS/Windows execution matrix.

## Final Phase 4 validation checkpoint

- `cargo test --quiet`: **130 passed, 0 failed**, including the combined
  io_uring/direct-I/O Phase 4 regression.
- `cargo fmt --check` and `git diff --check`: passed.
- `cargo clippy --lib --bin rqcow2 --test compression --test encryption
  --test bitmaps --test phase4_backends --test autoclear --test management_cli
  --no-deps`: passed with warnings.
- Local QEMU 8.2.2 check/compare, bitmap merge/write, and native LUKS/codec
  interoperability fixtures pass. Feature-dependent QEMU fixtures gate on the
  fixture tool's supported options so older tools can run the remaining tests.
- Linux Tokio, synchronous/direct I/O, and io_uring/direct I/O are exercised.
  Windows and i686 CI dependency provisioning is updated but was not executed
  locally. Earlier all-target Clippy failures in unchanged legacy test helpers
  remain outside this checkpoint.

## Additional image-management parity

These operations are additional capabilities beyond implementing the on-disk
format. Track them separately after the relevant format foundations exist:

- [ ] Resize, including grow and safe shrink.
- [ ] Rebase and commit backing chains.
- [ ] Amend image options and convert supported metadata representations.
- [ ] Configurable metadata/data preallocation.

The existing resized-backing test invokes `qemu-img resize`; it does not provide
a library resize implementation.
