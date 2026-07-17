# `/snapshot/create` intermittent hang — production evidence

Working notes for reproducing and root-causing an intermittent (~50%) hang of
`PUT /snapshot/create` (Full) on a busy guest. This branch adds phase logging
(below) on top of `main` so the next occurrence is localized; `main` already
contains the vsock RX-starvation fix (`dde124627`, "stop busy-spinning the
event thread on guest RX starvation").

## Environment

- **Firecracker**: release **v1.16.1** (the hanging binary). This branch =
  `main` + phase logging.
- **Arch/host**: x86_64, KVM, single bare-metal host running many microVMs.
- **Guest**: 8 vCPUs, ~18 GiB RAM, a busy k3s cluster (~80 pods).
- **Devices**: virtio-block (rootfs), virtio-net, **virtio-vsock** (a guest
  agent listens on port 1024; the host polls it). Balloon is **disabled** — a
  balloon device present made Full snapshots hang reliably (separate but
  possibly related; see below).
- **Snapshot sequence** driven by the host:
  1. guest agent, over vsock, `sync()` + offlines vCPUs 1..7
     (`/sys/devices/system/cpu/cpuN/online = 0`; they enter `hlt_play_dead`);
     cpu0 keeps running.
  2. host closes its vsock control connection; waits ~3s.
  3. `PATCH /vm {state:"Paused"}` — always succeeds.
  4. `PUT /snapshot/create {snapshot_type:"Full"}` — **~50% never returns**;
     the client times out after 8 min and the process is SIGKILLed.
  A successful Full snapshot of the same guest completes in ~20–30 s.

## Observed at a hang (captured in production)

### Thread state — a true park, not slow I/O
All 10 threads (`firecracker` main, `fc_api`, `fc_vcpu 0..7`) in `S` /
`futex_wait_queue`; **kernel stacks identical across two samples 45 s apart**;
`/proc/<pid>/io` `wchar` **unchanged** between samples; **no snapshot `mem`
file created** for the hung generation. i.e. FC is deadlocked, having written
zero snapshot bytes — not blocked in disk I/O.

```
tid=... comm=firecracker  wchan=futex_wait_queue
tid=... comm=fc_api       wchan=futex_wait_queue
tid=... comm=fc_vcpu 0..7 wchan=futex_wait_queue
stack (every thread): futex_wait_queue -> futex_wait -> do_futex -> __x64_sys_futex -> do_syscall_64
```

### FC debug log tail (`--log ... --level Debug`)
```
... vsock muxer: RX/TX churn — connections (src_port 1073741825/826/827) opening & closing right up to pause ...
[fc_api]   The API server received a Patch request on "/vm" with body {"state":"Paused"}
[fc_vcpu 1..7] KVM_KVMCLOCK_CTRL call failed Invalid argument (os error 22)   # the offlined vCPUs
[main]     'pause vm' VMM action took ~100 us.
[fc_api]   The request was executed successfully. Status code: 204 No Content.
[fc_api]   The API server received a Put request on "/snapshot/create" with body {... "snapshot_type":"Full"}
                       << log ends here — nothing further, ever >>
```
The PUT is **received by the API thread** and then **no further log line** is
ever emitted (no `'create ... snapshot' VMM action took`, no error). Both the
`fc_api` thread and the main VMM thread are parked in `futex_wait`.

## Ruled out (by v1.16.1 source reading — please re-verify)

- **vCPU save-state lost wakeup**: a *paused* vCPU receives `SaveState` over its
  channel (no signal needed), and every vCPU wait is bounded by
  `RECV_TIMEOUT_SEC = 30s` → a stuck vCPU yields an HTTP 500 within ≤4 min, not
  an 8-min silence. Inconsistent with the observed indefinite hang.
- **Device-state save blocking**: in v1.16.1 vsock `prepare_save`/`save` and
  balloon `save` are non-blocking memory copies; no wait/flush on host
  connections.
- **Slow memory dump / disk I/O**: ruled out by zero `wchar` progress, frozen
  stacks, and no `mem` file created.

## Working hypothesis (to confirm with this branch in production)

The PUT is parsed by the API thread but **never actioned by the VMM thread** —
i.e. the request is lost across FC's internal API↔VMM handoff, with both sides
parked:
- `fc_api`: `vmm_response_receiver.recv()` (no timeout) in
  `src/firecracker/src/api_server/mod.rs`.
- main VMM: the paused-mode inner loop `from_api.recv()` (no timeout) in
  `src/firecracker/src/api_server_adapter.rs`.

Correlate: every captured hang shows **vsock muxer connection churn right up to
the pause** (a host-side agent polling the guest). A paused guest posts no RX
buffers, so it is permanently RX-starved — the exact condition `main`'s
`dde124627` addresses ("delayed servicing of the API socket"). Whether that fix
also closes this handoff race is what we are testing.

## What this branch changes (diagnostics only)

Phase logging inside snapshot creation (previously silent):
- `src/vmm/src/persist.rs` `create_snapshot`: logs before/after `save_state`,
  vmstate-file write, and guest-memory dump.
- `src/vmm/src/lib.rs` `save_state`: logs before/after device save, vcpu save,
  kvm save.

On the next hang the last log line names the stuck phase:
`grep 'snapshot phase\|save_state phase\|create full snapshot' <fc-debug.log>`.
- ends at `... Put request on "/snapshot/create"` with **no** `snapshot phase`
  line → hang is in the API→VMM handoff (hypothesis above), not in
  `create_snapshot` itself.
- ends at a specific `save_state phase` / `snapshot phase` line → localizes to
  device / vcpu / kvm save, vmstate write, or memory dump.

## Reproduce / test procedure

1. Build this branch for the target (x86_64):
   `tools/devtool build --release` → `build/cargo_target/.../release/firecracker`.
2. Run it as the snapshotting VMM in place of the v1.16.1 binary.
3. Drive repeated (ideally concurrent) Full snapshots of the busy 8-vCPU/18 GiB
   guest **while a host process polls the guest over vsock during provisioning**
   (that reproduces the churn). Record the stall rate over ≥20 attempts.
4. On any hang, capture: per-thread `wchan`+`stack`, two `/proc/<pid>/io`
   samples, the `fc-debug.log` tail, and `grep SigQ /proc/<pid>/status`.

## Related upstream

- `dde124627` / PR #6031 — vsock RX-starvation busy-spin (in `main`, unreleased).
- PR #5943 — lost SIGRTMIN vCPU kick when `RLIMIT_SIGPENDING` is exhausted →
  unbounded `join()`; check `SigQ` on a hung process.
