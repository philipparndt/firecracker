# Fork patches

This fork (`philipparndt/firecracker`) carries a small number of local patches
on top of upstream `firecracker-microvm/firecracker`. Every patch is listed here
with its rationale, its upstream reference, and — most importantly — the
**condition under which it should be deleted**. A patch with no drop condition
is a patch nobody will ever remove.

## Base: upstream `v1.17.0`

`fork/v1.17.0` is the upstream `v1.17.0` tag plus the patches below. Releases
are tagged `v1.17.0-fork.N` and published as GitHub releases (static musl
`firecracker` and `jailer` for x86_64 and aarch64, plus `SHA256SUMS-<arch>`) by
`.github/workflows/build-firecracker-binary.yml`.

**Snapshot compatibility.** `v1.17.0` has `SNAPSHOT_VERSION` `12.0.0`. The
loader rejects a major version mismatch, so this build cannot restore snapshots
made by the `v1.16.1`-based fork builds (`10.0.0`), and vice versa. Clusters
moving to this build must rebuild their base snapshots and must not carry
suspended instances across. Even upstream `v1.16.2` (`11.0.0`) breaks that
compatibility, so no upstream release could be adopted without a rebuild.

## Branches

| Branch                            | Contents                                                     |
| --------------------------------- | ------------------------------------------------------------ |
| `fork/v1.17.0`                    | `v1.17.0` + patches 1–4 below. Current base for new clusters. |
| `fix/v1.16.1-snapshot-deadlock`   | Legacy: `v1.16.1` + the deadlock/seccomp patch.              |
| `feat/v1.16.1-virtio-blk-discard` | Legacy: the above + discard backport. Existing clusters.     |

The `v1.16.1` branches stay as they are for clusters whose snapshots are still
`10.0.0`. They receive no upstream fixes; retire them once those clusters are
rebuilt.

## What became obsolete with v1.17.0

- **virtio-blk discard backport** — upstream merged
  [#6142](https://github.com/firecracker-microvm/firecracker/pull/6142) into
  `v1.17.0`, with the same `discard` drive configuration field, so clients
  written against the backport work unchanged. Only our metrics remain (patch
  3).
- **SIGPIPE handler logging** — upstream fixed this in
  [#6086](https://github.com/firecracker-microvm/firecracker/pull/6086) (logger
  uses an `RwLock`) and
  [#6143](https://github.com/firecracker-microvm/firecracker/pull/6143)
  (`sigpipe_handler` no longer logs). The fatal handlers are still covered by
  patch 2.

## Patch 1 — vcpu seccomp rules for musl mallocng

- **File**: `resources/seccomp/x86_64-unknown-linux-musl.json`

Allows musl mallocng's `mmap(MAP_ANONYMOUS|MAP_PRIVATE, PROT_NONE)` reservation
and its `mprotect(PROT_READ|PROT_WRITE)` commit on the `vcpu` thread. Snapshot
state serialization allocates on that thread; without these rules it takes a
`SIGSYS`. Upstream `v1.17.0` (and `main` as of 2026-10-27) only allows
`mmap(..., PROT_READ|PROT_WRITE)` on `vcpu`.

**Not applied to aarch64.** The aarch64 filter has the same gap, but the SIGSYS
was only ever observed and validated on x86_64. Port the rule if an aarch64
deployment hits it.

**Upstream reference.** None yet. This should be reported upstream.

**Drop condition.** Upstream allows these calls on the `vcpu` thread.

## Patch 2 — async-signal-safe fatal signal handlers

- **File**: `src/vmm/src/signal_handler.rs`

The fatal handlers (SIGSYS, SIGSEGV, SIGBUS, SIGXFSZ, SIGXCPU, SIGHUP, SIGILL)
log through a fixed stack buffer and a single `write(2)` to stderr instead of
`error_unrestricted!`, which formats into a heap `String`. `exit_with_code` no
longer calls `METRICS.write()`, which locks and allocates. Either can block
forever when the signal lands on a thread that holds the allocator lock, which
is what produced the `/snapshot/create` all-threads futex deadlock. Upstream's
`RwLock` fix removes the logger self-deadlock but not the allocator one.

Trade-off: in-memory metrics are not flushed on a fatal-signal exit, and the
fatal message goes to stderr rather than the configured log file.

Includes SIGPIPE regression tests (`signal_handler::tests`).

**Drop condition.** Upstream makes its fatal handlers allocation-free.

## Patch 3 — discard metrics

- **Files**: `src/vmm/src/devices/virtio/block/virtio/{metrics,request}.rs`,
  `tests/host_tools/fcmetrics.py`

`discard_count` and `discard_bytes` block device metrics, advanced only for a
completed discard, by the byte count the engine reports, and aggregated into the
global `block` metrics. Upstream adds no discard counters. This fork's consumer
dropped its host-side reclaim log line when it switched from zero-filling to
`fstrim`, so without these a reclaim that freed nothing would be
indistinguishable from one that never ran.

The `v1.16.1` backport forgot the aggregation and the integration-test metrics
schema; both are fixed here.

**Drop condition.** Upstream adds equivalent counters.

## Patch 4 — CI

- **Files**: `.github/workflows/build-firecracker-binary.yml`,
  `.github/workflows/send_release_notification.yml`

The fork's build and release workflow, plus a guard that skips upstream's
release Slack notification outside the upstream repository (its secret does not
exist here).

**Drop condition.** Never, while this fork ships its own binaries.

## Moving to a newer upstream release

1. Branch `fork/vX.Y.Z` from the upstream tag and cherry-pick the patches above
   that do not meet their drop condition.
1. Update the `DEVCTR_IMAGE` tag in the workflow to match `tools/devtool`.
1. Check `SNAPSHOT_VERSION`; if the major changed, plan a snapshot rebuild.
1. Update this file, tag `vX.Y.Z-fork.1`.
