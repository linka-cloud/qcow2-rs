# Complete QCOW2 Support

Assessment date: 2026-11-08.

## Summary and evidence

The library implements core QCOW2 v3 disk I/O, but does not yet support the
complete QCOW2 format. Some unsupported features are rejected; others are
accepted without the handling needed to use them safely.

This plan is based on source and existing-test inspection, compared with the
[QEMU QCOW2 specification](https://www.qemu.org/docs/master/interop/qcow2.html).
The initial findings were source-level observations, not runtime-confirmed results.
`cargo test --locked --offline --lib` could not start because `crypto-hash`
was missing from the local dependency cache.

### Implementation progress

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

The target is complete on-disk format support for QCOW2 versions 2 and 3,
including safe reads, writes, metadata preservation, and recovery where
applicable. QEMU implementation limits can remain where explicitly validated
and documented. Unsupported or invalid feature combinations must fail with
clear errors; full support does not mean accepting every combination.

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
      them using the correct layout. The current parser reads a v3 structure
      even for v2 images.
- [ ] Accept only supported format versions and validate header lengths,
      refcount orders, offsets, sizes, extension bounds, and reserved fields
      before using them. Return errors rather than panicking on malformed input.
      Version/header-length/refcount-order checks and extension record sizes,
      duplicates, end markers, padding, and backing-name bounds are implemented;
      broader table/address validation remains.
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

- [ ] Support raw backing files and honor the declared backing format instead
      of recursively treating every backing file as QCOW2.
- [ ] Resolve relative backing paths against the containing image directory.
- [ ] Detect backing cycles and return clear chain-opening errors.
- [ ] Test multi-level backing chains and zero filling beyond a shorter backing
      image, including partial-cluster reads and writes at its boundary.
- [ ] Build a metadata traversal that calculates exact expected refcounts,
      rather than only testing allocation/non-allocation and leaks.
- [x] Report invalid mappings as checker failures. Missing data refcounts now
      return `InvalidData` errors with guest/host offsets instead of only printing
      diagnostics. Allocated zero clusters and all compressed host clusters are
      included in validation and usage accounting.
- [ ] Cover compressed extents, allocated zero clusters, metadata overlaps,
      and all referenced metadata; extend traversal as later features land.
- [ ] Add explicit repair operations for reconstructing refcounts and resolving
      supported inconsistencies. Checking remains read-only by default.
- [ ] Implement dirty/corrupt state handling and durable dirty-state transitions.
      Currently every nonzero incompatible-feature bitmap is rejected.
- [ ] Implement lazy refcounts and recovery. Opening a clean image with the
      compatible lazy-refcounts flag is not equivalent to implementing lazy
      updates or dirty-image recovery.

Acceptance: checker results agree with QEMU for supported fixtures; repair
restores valid refcounts without changing guest data, including after injected
write/flush failures.

## Phase 3: Snapshots, extended L2, and external data files

### Internal snapshots

- [ ] Parse and preserve snapshot directory entries and their extra data.
- [ ] Add list/create/delete/restore operations with snapshot identity and size
      handling, plus VM-state metadata/payload support for complete coverage.
- [ ] Implement shared L2-table and data-cluster COW, preserving untouched bytes
      during partial writes and releasing old references correctly.
- [ ] Reconstruct copied flags when switching the active snapshot.
- [ ] Include snapshot directories, inactive L1/L2 tables, and VM-state storage
      in checking, allocation accounting, and repair.

### Extended L2 entries

- [ ] Support 128-bit entries and the resulting L1/L2 addressing geometry.
- [ ] Implement subcluster allocation and zero bitmaps, backing fallback,
      partial writes, and discard/zero operations.
- [ ] Preserve compressed-cluster behavior within extended-L2 images.
- [ ] Validate cluster-size constraints and invalid bitmap combinations.

### External data files

- [ ] Parse the data-file extension and allow callers to supply the data file.
- [ ] Route guest payload I/O to the separate file while keeping metadata in
      the QCOW2 file; apply fixed guest/host mapping and refcount rules.
- [ ] Implement raw external-data consistency, including zeroing and discard.
- [ ] Validate feature combinations and missing/unusable data files.

Acceptance: QEMU-created images can be read, modified, and reopened by both
implementations; snapshot data remains unchanged after active-image writes.

## Phase 4: Compression, encryption, and persistent bitmaps

### Compression

- [ ] Recognize the compression-type incompatible feature and implement Zstd
      decompression alongside the existing Deflate reader.
- [ ] Add Deflate and Zstd compressed output with packed extent allocation,
      correct refcounts, and uncompressed fallback when compression does not fit.
- [ ] Validate decompression completion/output size and malformed streams.
- [ ] Test packed extents sharing sectors and crossing host-cluster boundaries.

### Encryption

- [ ] Add explicit key/secret inputs and cryptographic payload I/O.
- [ ] Implement the LUKS header-pointer extension and associated header/key data.
- [ ] Implement legacy AES compatibility if claiming all specified encryption
      methods; document its legacy status and do not make it a creation default.
- [ ] Include encryption metadata in allocation accounting and checking.
- [ ] Validate wrong/missing keys and unsupported encryption parameters without
      exposing secrets in errors or logs.

### Persistent dirty bitmaps

- [ ] Parse/preserve bitmap directories, tables, data, flags, and extra data.
- [ ] Add bitmap lifecycle operations and change tracking for writes, zeros,
      discard, snapshot switching, and virtual-size changes as applicable.
- [ ] Implement `in_use`, enabled/auto, and autoclear consistency behavior.
- [ ] Include bitmap storage in refcount checking and repair.

Acceptance: codec output, encrypted payloads, and persisted bitmap contents
interoperate with QEMU; interrupted updates are detected through consistency
state rather than silently treated as valid.

## Public interfaces and documentation

- [ ] Expose supported/detected capabilities and actionable unsupported-feature
      errors; replace blanket incompatible-feature rejection incrementally as
      each feature becomes operational.
- [ ] Extend open/create options for backing formats, external data files,
      compression, encryption credentials, and format version.
- [ ] Add snapshot, bitmap, explicit zero-write, and check/repair interfaces as
      their implementations land; extend the CLI for those capabilities.
- [ ] Document flush/durability requirements and failure behavior. Dirty cache
      contents must not be described as automatically persisted on drop.
- [ ] Publish a feature matrix distinguishing read, write, create, management,
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

## Additional image-management parity

These operations are additional capabilities beyond implementing the on-disk
format. Track them separately after the relevant format foundations exist:

- [ ] Resize, including grow and safe shrink.
- [ ] Rebase and commit backing chains.
- [ ] Amend image options and convert supported metadata representations.
- [ ] Configurable metadata/data preallocation.

The existing resized-backing test invokes `qemu-img resize`; it does not provide
a library resize implementation.
