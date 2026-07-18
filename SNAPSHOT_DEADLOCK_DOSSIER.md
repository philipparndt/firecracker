# Intermittent `/snapshot/create` deadlock in `save_vcpu_states` — evidence dossier

**Status:** ROOT CAUSE IDENTIFIED (symbolized). An intermittent all-threads
`futex` deadlock during `/snapshot/create`, caused by **Firecracker's
fault/seccomp signal handlers logging (mutex + malloc) — which is not
async-signal-safe** — deadlocking against the logger/malloc locks held by other
threads during snapshot save. Reproducible on demand. Self-contained so another
engineer/AI can go straight to a fix. See §9 for the root cause and §7 for the
symbolized proof. (One honest caveat about instrumentation-influenced trigger in
§9 — the handler-logging bug itself is real regardless.)

## 1. Symptom
`PUT /snapshot/create` (`snapshot_type: Full`) intermittently **never returns**.
The client (host agent) times out after 8 min and SIGKILLs the VMM. ~50% of
attempts on a busy guest. A retry (fresh reboot of the same rootfs + snapshot
again) succeeds in ~20 s, so the deadlock is **transient per attempt**, not a
permanently corrupted state.

Reproduces identically on **v1.16.1** and on **main (1.17.0-dev)** including
`#6031` (vsock RX-starvation fix) — so it is not vsock- or version-specific.

## 2. Environment / how to reproduce
- Host: x86_64, KVM, bare metal, many microVMs; kernel 6.4.
- Guest: **8 vCPUs, ~18 GiB RAM**, a busy k3s cluster (~80 pods). Devices:
  virtio-block (rootfs), virtio-net, virtio-vsock. Balloon disabled.
- Snapshot sequence driven by the host agent:
  1. guest agent syncs disks (vCPUs left online — offlining them makes no
     difference, see §6).
  2. host closes its vsock control connection.
  3. `PATCH /vm {state:"Paused"}` — always succeeds (~100–500 µs).
  4. `PUT /snapshot/create {snapshot_type:"Full"}` — ~50% never returns.
- Drive repeated Full snapshots of this guest. ~1 in 2 hangs. Serial (one build
  at a time) is enough — it is **not** load/parallelism dependent (§6).

## 3. Localization — phase logging inside `save_state` / `save_vcpu_states`
Added `info!` phase logs (this branch). A **successful** save completes all 8
vCPUs in ~2 ms:
```
save_vcpu: acquiring vcpus_handles lock
save_vcpu: lock acquired (8 vcpus); sending SaveState
save_vcpu: send/sent SaveState -> vcpu 0..7
vcpu 0..7: SaveState received, calling kvm_vcpu.save_state()
vcpu 0..7: save_state done, sending SavedState
save_vcpu: got response <- vcpu 0..7
save_vcpu: all 8 responses collected
```
A **hung** save (captured from `last-failed-fc-debug.log`) freezes here:
```
save_vcpu: acquiring vcpus_handles lock
save_vcpu: lock acquired (8 vcpus); sending SaveState
save_vcpu: send SaveState -> vcpu 0   / sent -> vcpu 0
save_vcpu: send SaveState -> vcpu 1   / sent -> vcpu 1
save_vcpu: send SaveState -> vcpu 2   / sent -> vcpu 2
save_vcpu: send SaveState -> vcpu 3          <-- LAST main-thread line (no "sent -> vcpu 3")
[fc_vcpu 0] vcpu 0: SaveState received, calling kvm_vcpu.save_state()
[fc_vcpu 1] vcpu 1: SaveState received, calling kvm_vcpu.save_state()
[fc_vcpu 2] vcpu 2: SaveState received, calling kvm_vcpu.save_state()
<< nothing further, ever; 8-min timeout -> SIGKILL >>
received: 2-3   done: 0
```
So at the freeze:
- **main thread** is inside `Vm::save_vcpu_states` → `VcpuHandle::send_event(SaveState)`
  for vcpu 3 (logged "send", never "sent"). `save_vcpu_states` is at
  `src/vmm/src/vstate/vm.rs:321`; `send_event` at `src/vmm/src/vstate/vcpu.rs:605`.
- **vCPUs 0/1/2** have taken `VcpuEvent::SaveState` in the `paused()` loop
  (`src/vmm/src/vstate/vcpu.rs:324`) and are inside `KvmVcpu::save_state()`
  (`src/vmm/src/arch/x86_64/vcpu.rs:555`), none returned.
- `vcpus_handles` mutex is **acquired fine** (not the blocker).
- The 30 s `RECV_TIMEOUT_SEC` on the response `recv` is never reached — the
  hang is *before* the recv loop, in the send/kick + vcpu-side save.

## 4. Kernel stacks at the hang (all 10 threads)
Every thread — `firecracker` (main), `fc_api`, `fc_vcpu 0..7` — is parked in a
futex; identical across samples; no thread in a KVM ioctl (`D`/`kvm_*`):
```
futex_wait_queue+0x60 -> futex_wait -> do_futex -> __x64_sys_futex -> do_syscall_64
```
=> a **userspace lock / lost-wakeup deadlock**, not a KVM-kernel block. The
vCPUs "in save_state()" are actually parked on a *futex*, i.e. blocked on a
userspace lock reached from within `save_state()`'s helpers, not in the
`KVM_GET_*` ioctls themselves.

## 5. Futex map (`/proc/<tid>/syscall`, op/uaddr)
```
main firecracker  uaddr=A (0x…efa58) op=0x89 (FUTEX_WAIT_BITSET_PRIVATE) val=2 (locked+waiters)
fc_vcpu 1         uaddr=A
fc_vcpu 3         uaddr=A
fc_vcpu 0         uaddr=B (0x…f3428) op=0x80
fc_vcpu 2         uaddr=B
fc_api            uaddr=unique
fc_vcpu 4/5/6/7   uaddr=unique each (contiguous — per-vCPU event-channel parks)
```
So it is **not** a single contended mutex: multiple distinct wait objects, i.e.
a lost-wakeup / cross-wait, with 3 threads (incl. main) blocked on mutex A and 2
on mutex B.

**Caveat (important):** these phase logs are emitted from the main thread *and*
all 8 vCPU threads in the tightest µs of the save, so mutex **A is very likely
Firecracker's logger mutex**, contended *by the diagnostics themselves*. The
underlying stock deadlock exists with **zero** added logging (stock v1.16.1
shows the same all-threads-`futex_wait`), so the diagnostics did not create it —
but the exact A/B identities may be perturbed by logging. Symbolized backtraces
(§7) are needed to name the true stock locks.

## 6. Ruled out (each tested on this hardware, not just by code reading)
- **vCPU offlining in quiesce** (parking cpu1..n in `hlt_play_dead`): changed
  guest agent to sync-only, vCPUs left online — **still stalls** on a serial
  build. Not the cause.
- **`RLIMIT_SIGPENDING` exhaustion (#5943)**: limit is already 8 204 633 on the
  host (agent + pid 1). Not the cause. (A scope `LimitSIGPENDING=infinity` is
  also invalid — `systemd-run --scope` rejects it.)
- **Parallelism / disk-I/O contention**: a single serial build with nothing else
  running still hangs 8 min. Not load-dependent. (3 concurrent 18 GiB builds
  *separately* destabilise the node — unrelated.)
- **`vcpus_handles` mutex**: acquired fine before any send (logged).
- **The 30 s response recv timeout**: hang is upstream of it.
- **Memory dump / slow disk**: hang is before mem dump; no `mem` file written.

## 7. Symbolized backtraces (eu-stack, debug build) — THE SMOKING GUN
Captured live from a frozen VMM (pid 94908), debug build. Key threads:

**main (TID 94908)** — blocked acquiring the logger mutex, from a log call in save:
```
#0 syscall
#1 std::sys::sync::mutex::futex::Mutex::lock_contended
#2 vmm::logger::logging::Logger::log
#3 vmm::Vmm::save_state
#4 vmm::rpc_interface::RuntimeApiController::handle_request
```
**vCPU TID 95053 — the lock holder / deadlock pivot:**
```
#0 __lock                      (glibc malloc arena lock)
#1 __libc_free
#2 vmm::logger::logging::Logger::log          <-- HOLDS the logger mutex, now in free()
#3 log::__private_api::log_impl (GlobalLogger)
#4 vmm::vstate::vcpu::…        (a vCPU thread logging)
```
**vCPU TID 95052 — a fault signal handler that logs:**
```
#0 syscall
#1 std::sys::sync::mutex::futex::Mutex::lock_contended
#2 vmm::logger::logging::Logger::log
#3 vmm::signal_handler::sigsys_handler        <-- SIGSYS (seccomp) handler, logging
```
**vCPUs TID 95054 / main / others** — all in `Logger::log → Mutex::lock_contended`
(piled up on the logger mutex). vCPUs 95055-95059 are idle in `VcpuEvent recv`
(paused, uninvolved); fc_api is in the normal response-channel recv.

### The deadlock chain
```
TID 95053: holds LOGGER mutex (inside Logger::log) → calls free() → waits on MALLOC arena lock
TID 95052: in SIGSYS handler → error_unrestricted!() → Logger::log → waits on LOGGER mutex (held by 95053)
main/95054: → Logger::log → wait on LOGGER mutex (held by 95053)
```
Circular: the SIGSYS handler needs the logger mutex; the logger-mutex holder
needs the malloc lock; the malloc lock is held by a thread interrupted mid-alloc
(the SIGSYS was delivered on a thread that was inside malloc, or another thread
is mid-malloc). Every thread ends up parked → whole VMM hangs → 8-min SIGKILL.

## 8. Relevant code
- `Vm::save_vcpu_states` — `src/vmm/src/vstate/vm.rs:321`: locks `vcpus_handles`,
  `send_event(SaveState)` to each handle, then `recv_timeout(30s)` per handle.
- `VcpuHandle::send_event` — `src/vmm/src/vstate/vcpu.rs:605`:
  `event_sender.send(event)` (unbounded mpsc) → `set_kvm_immediate_exit(1)` →
  `fence(Release)` → `vcpu_thread.kill(SIGRTMIN+VCPU_RTSIG_OFFSET)`.
- `Vcpu::paused()` `SaveState` arm — `src/vmm/src/vstate/vcpu.rs:324`:
  `event_receiver.recv()` → `kvm_vcpu.save_state()` → `response_sender.send(...)`.
- `handle_signal` — `src/vmm/src/vstate/vcpu.rs:115`: async-signal-safe, just
  `fence(Acquire)`. The kick’s only purpose is to make a *running* vCPU leave
  `KVM_RUN`; for an already-*paused* vCPU (blocked in `recv()`) the kick is
  redundant (candidate simplification / fix direction).
- `KvmVcpu::save_state` — `src/vmm/src/arch/x86_64/vcpu.rs:555`: sequence of
  `KVM_GET_*` ioctls + helpers (`get_cpuid`, `get_msr_chunks`, `get_xsave`) that
  allocate — no explicit `.lock()`, so the futex the vCPUs park on is either an
  allocator arena lock or something these helpers call.

## 9. ROOT CAUSE
**Firecracker's fault/seccomp signal handlers are not async-signal-safe: they
log.** `generate_handler!` (`src/vmm/src/signal_handler.rs:34`) and
`log_sigsys_err` (`:61`) call `error_unrestricted!(...)` → `Logger::log`, which
takes a `Mutex` and allocates (malloc/free). When such a signal (here **SIGSYS**,
seccomp) is delivered while any thread holds the logger mutex or the malloc arena
lock, the handler deadlocks against it (logger ↔ malloc circular wait), parking
every thread in `futex_wait`. The VMM never returns from `/snapshot/create` →
8-min client timeout → SIGKILL. `libc::_exit` at the end of the handler is never
reached because the log call before it hangs.

This is a genuine, latent Firecracker bug: **signal handlers must only call
async-signal-safe functions; `Logger::log` (mutex + allocation) is not.** The
`sa_mask` (handler comment at `:67`) blocks *other signals* but does nothing
about the interrupted thread already holding the logger/malloc locks.

### Honest caveat — instrumentation influence on the TRIGGER
The diagnostic build adds `info!` logging **inside the vCPU threads** during
save (`vcpu.rs` SaveState arm). Stock FC does not log from vCPU threads there.
Two consequences:
1. It greatly widens the window in which a vCPU holds the logger mutex / is in
   malloc during save (amplifying the deadlock).
2. The **SIGSYS itself is most likely triggered by this added logging**: vCPU
   threads run under a *restrictive vCPU seccomp filter*; the syscalls my logging
   makes (write / malloc→mmap/madvise) may not be in that filter → SIGSYS.
So the *trigger* (SIGSYS) in this build may be an instrumentation artifact. What
is NOT an artifact: (a) the handler-logs-→-deadlock mechanism is real and latent
in stock FC; (b) stock v1.16.1 shows the same all-threads-`futex_wait` in the
same save phase (but stock is stripped — no symbols — so its exact lock chain is
unconfirmed).

### Decisive experiment to separate real-bug from artifact
Rebuild with **main-thread-only** phase logging (remove ALL vCPU-thread `info!`
in the SaveState arm), re-catch:
- If it still deadlocks → the stock trigger is real (not my logging); capture its
  symbolized stacks.
- If it stops deadlocking → the SIGSYS trigger was the vCPU-thread logging hitting
  the vCPU seccomp filter; the stock ~50% stall is then a *related but distinct*
  contention (needs its own capture, e.g. logger-mutex vs malloc under the
  snapshot's own allocations).

## 9b. Fix directions
1. **Make the signal handlers async-signal-safe** (the core fix, valuable
   regardless of trigger): do not call `Logger::log`/allocate in the handler.
   Use only async-signal-safe ops — e.g. `write(2)` a pre-formatted static
   byte buffer to stderr/log fd, `store` the metric, then `_exit`. This turns a
   hang into a clean shutdown for every fault signal.
2. **Identify the seccomp-blocked syscall** if SIGSYS fires in stock save:
   check `METRICS.seccomp.num_faults` and the logged syscall number, then add it
   to the relevant (vCPU/vmm) filter — so no SIGSYS in the first place.
3. Reproduce cheaply in-tree: a unit/integration test that raises a handled
   signal on one thread while another holds the logger mutex + is in malloc will
   deterministically deadlock on unpatched FC and pass once the handler is
   async-signal-safe.

## 10. Reproduce/capture tooling used
- Phase logging: this branch (`vm.rs` `save_vcpu_states`, `vcpu.rs` `paused()`).
- Live capture: detect a builder `fc-debug.log` that has `save_state … start`
  but no `all responses collected` and stopped growing >70 s → dump
  `/proc/<pid>/task/*/{comm,wchan,stack,syscall}` and `eu-stack -p <pid>` before
  the 8-min SIGKILL. (`hostPID=true`, so pod pid == host pid; eu-stack lives on
  the host.)
