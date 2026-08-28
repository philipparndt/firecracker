# Fork patches

This fork (`philipparndt/firecracker`) carries a small number of local patches
on top of upstream `firecracker-microvm/firecracker`. Every patch is listed here
with its rationale, its upstream reference, and — most importantly — the
**condition under which it should be deleted**. A patch with no drop condition
is a patch nobody will ever remove.

## Why the fork is pinned to `v1.16.1`

The shipped binary is built from the upstream `v1.16.1` tag, not from `main`.
This is a hard constraint, not a preference:

`SNAPSHOT_VERSION` is `10.0.0` on `v1.16.1` and `11.0.0` on upstream `main`. The
snapshot loader rejects a **major** version mismatch outright. Adopting `main`
would therefore make every existing base snapshot and every currently-suspended
instance unrestorable at once — a full fleet rebuild and drain, not a binary
swap. Any upstream change we want before that gap closes has to be backported.

## Branches

| Branch                            | Contents                                                                             |
| --------------------------------- | ------------------------------------------------------------------------------------ |
| `fix/v1.16.1-snapshot-deadlock`   | `v1.16.1` + patch 1. This is the branch the currently-shipped binary was built from. |
| `feat/v1.16.1-virtio-blk-discard` | `fix/v1.16.1-snapshot-deadlock` + patch 2.                                           |

Each new local patch gets its own branch off the previous one, so the provenance
of the shipped artifact stays reproducible and a rollback is a re-vendor of the
existing binary rather than a revert.

## Patch 1 — async-signal-safe handlers + vcpu seccomp rules

- **Commit**: `702a3c0f8` on `fix/v1.16.1-snapshot-deadlock`
- **Files**: `src/vmm/src/signal_handler.rs`,
  `resources/seccomp/x86_64-unknown-linux-musl.json`,
  `.github/workflows/build-firecracker-binary.yml`

**What it changes.** Two independent fixes in one commit:

1. *Signal handlers no longer log.* They `write(2)` a stack buffer and no longer
   take `METRICS.write()`, so a fault signal raised during snapshot cannot
   deadlock against the logger or the malloc lock.
1. *Two vcpu seccomp rules.* Allows musl mallocng's
   `mmap(MAP_ANONYMOUS|MAP_PRIVATE, PROT_NONE)` reservation and its
   `mprotect(PROT_READ|PROT_WRITE)` commit on the `vcpu` thread. Snapshot state
   serialization allocates on that thread; without these rules it takes a
   `SIGSYS`.

The workflow file is the build path for the vendored binary, not part of the
fix.

**Upstream reference.** Upstream fixed the logger half by a different route in
[#6086](https://github.com/firecracker-microvm/firecracker/pull/6086) (merged
2026-08-10). The seccomp half has no upstream equivalent.

**Drop condition.** The signal-handler half can be dropped when this fork moves
to a release containing #6086. **The vcpu seccomp rules cannot be dropped with
it** — they are still required on any musl build and have no upstream fix. Do
not delete this patch wholesale on the strength of #6086 alone; split it.

**Validated on** `runner-prod`: 6/6 clean snapshots, 0 stalls, 0 SIGSYS.

## Patch 2 — virtio-blk discard backport

- **Branch**: `feat/v1.16.1-virtio-blk-discard`
- **Upstream reference**:
  [#6142](https://github.com/firecracker-microvm/firecracker/pull/6142), a
  maintainer's rebase of community PR
  [#5908](https://github.com/firecracker-microvm/firecracker/pull/5908), against
  the long-running issue
  [#2708](https://github.com/firecracker-microvm/firecracker/issues/2708).
  Substantively approved upstream ("Otherwise the change seems ok", 2026-07-23)
  but **not merged**.

**What it changes.** Implements `VIRTIO_BLK_F_DISCARD` for the `Sync` IO engine:
the feature bit and the grown `ConfigSpace`, a `RequestType::Discard` arm with
range validation, and a backend that uses
`fallocate(FALLOC_FL_PUNCH_HOLE|FALLOC_FL_KEEP_SIZE)` for file-backed drives and
`BLKDISCARD` for block-device backings. Opt-in per drive via a new `discard`
field on the drive config; refused on read-only drives and with the `Async`
engine.

**Why we carry it.** Without discard, the guest's only way to tell the host a
block is garbage is to overwrite it with zeros. That workaround inflates the
overlay it exists to shrink (writing zeros into a reflinked extent breaks the
copy-on-write share and allocates), consumes real guest free space, can never
reclaim its own safety reserve, and costs ~31 GiB of writes per suspend per
instance for zero net data. With discard, the guest runs `fstrim`: metadata
only, no block data written, ranges chosen by the guest's own ext4.

**Backported, not adopted from `main`.** See "Why the fork is pinned" above.
#6142 was written against `main`, ~68 commits past `v1.16.1`, so it does not
apply cleanly here: 20 of its 25 files land clean, five were resolved by hand —
`CHANGELOG.md`, `block/virtio/device.rs`, `block/virtio/io/sync_io.rs`,
`tests/integration_tests/functional/test_api.py` and `test_drive_virtio.py`. The
two Rust conflicts are caused by upstream refactors *after* `v1.16.1` (notably a
122-line refactor of `device.rs`), not by patch 1. They were ported hunk by hunk
onto the shape `v1.16.1` actually has, deliberately **without** dragging in
those refactors. The two Python conflicts are test-framework drift: the test
bodies were ported onto `v1.16.1`'s fixtures (`uvm_plain`, no
`@pin_guest_kernel`). Patch 1's seccomp additions and this patch's do not
conflict — they touch different threads (`vcpu` vs `vmm`).

**Two upstream review findings that must survive any future rebase:**

1. The validated discard range is stored on the `Request`
   (`validate_discard_ranges` writes back `self.sector` / `self.data_len`) and
   is **never re-read from guest memory at execution time**. Re-reading would be
   a time-of-check/time-of-use hole: the guest still owns that memory and could
   hand `fallocate` a range that never passed validation.
1. Discard execution is gated on the feature actually having been negotiated
   (`acked_features`), returning `VIRTIO_BLK_S_UNSUPP` otherwise.

**Local addition on top of #6142.** `discard_count` and `discard_bytes` metrics
in `block/virtio/metrics.rs`, incremented on the discard completion path. #6142
adds no counters, and this fork's consumer deletes its host-side
`reclaimedBytes` log line when it stops zero-filling — so without these the
change would be a net loss of observability on the very behaviour it introduces.
A reclaim that freed nothing must be distinguishable from one that never ran.
These are ours; drop them only if upstream adds equivalents.

**Deliberately not changed.** `DISCARD_SECTOR_ALIGNMENT` stays at upstream's `1`
(512 B). A partial-block punch on a reflinked extent zeroes rather than frees,
so `8` (4 KiB) would be the more honest advertisement — but the guest's ext4
uses 4 KiB blocks and `mkfs.ext4` wrote the filesystem at offset 0 with no
partition table, so guest and host blocks already align and sub-4K ranges are
never emitted. Keeping upstream's constant keeps the delta droppable. Revisit
only if measurement shows partial reclaim.

**Ordering constraint for anyone deploying this.** `BlockDeviceConfig` is
`#[serde(deny_unknown_fields)]`, so a client sending `"discard": true` to an
unpatched binary gets a hard 400. **Ship the binary before the client.** In the
other direction, roll the client back first, then the binary.

**Drop condition.** Delete this branch when #6142 merges upstream and this fork
adopts a release that contains it. Watch #6142; the feature has been open since
2021 and three prior attempts were closed for contributor inactivity, never on
objection, so do not assume it will land on any particular schedule.
