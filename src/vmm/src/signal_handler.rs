// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use libc::{
    SIGBUS, SIGHUP, SIGILL, SIGPIPE, SIGSEGV, SIGSYS, SIGXCPU, SIGXFSZ, c_int, c_void, siginfo_t,
};

use crate::FcExitCode;
use crate::logger::{IncMetric, METRICS, StoreMetric};
use crate::utils::signal::register_signal_handler;

// The offset of `si_syscall` (offending syscall identifier) within the siginfo structure
// expressed as an `(u)int*`.
// Offset `6` for an `i32` field means that the needed information is located at `6 * sizeof(i32)`.
// See /usr/include/linux/signal.h for the C struct definition.
// See https://github.com/rust-lang/libc/issues/716 for why the offset is different in Rust.
const SI_OFF_SYSCALL: isize = 6;

const SYS_SECCOMP_CODE: i32 = 1;

/// Size of the stack buffer used by [`write_stderr`]; messages longer than
/// this are truncated.
const STDERR_MSG_CAP: usize = 128;

/// Writes a formatted message to stderr with a single `write(2)`.
///
/// This runs in signal handler context, so it must only perform
/// async-signal-safe operations: the logger (mutex) and anything that
/// allocates are off-limits, because the interrupted thread may already hold
/// the logger or allocator lock, which would deadlock the process. Formatting
/// into a fixed stack buffer and calling `write(2)` avoids both.
fn write_stderr(args: std::fmt::Arguments) {
    struct Buf {
        data: [u8; STDERR_MSG_CAP],
        len: usize,
    }
    impl std::fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            // Truncate instead of failing; this is a best-effort message.
            let n = s.len().min(self.data.len() - self.len);
            self.data[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }
    let mut buf = Buf {
        data: [0; STDERR_MSG_CAP],
        len: 0,
    };
    let _ = std::fmt::Write::write_fmt(&mut buf, args);
    // SAFETY: Safe because the buffer lives on the stack for the duration of
    // the call; `write(2)` is async-signal-safe per signal-safety(7).
    unsafe { libc::write(libc::STDERR_FILENO, buf.data.as_ptr().cast(), buf.len) };
}

#[inline]
fn exit_with_code(exit_code: FcExitCode) {
    // Note: no metrics flush here. This is only called from signal handlers,
    // and `METRICS.write()` locks and allocates, neither of which is
    // async-signal-safe. In-memory metrics are lost on this exit path.
    // SAFETY: Safe because we're terminating the process anyway.
    unsafe { libc::_exit(exit_code as i32) };
}

macro_rules! generate_handler {
    ($fn_name:ident ,$signal_name:ident, $exit_code:ident, $signal_metric:expr, $body:ident) => {
        #[inline(always)]
        extern "C" fn $fn_name(num: c_int, info: *mut siginfo_t, ucontext: *mut c_void) {
            // SAFETY: Safe because we're just reading some fields from a supposedly valid argument.
            let si_signo = unsafe { (*info).si_signo };
            // SAFETY: Safe because we're just reading some fields from a supposedly valid argument.
            let si_code = unsafe { (*info).si_code };

            if num != si_signo || num != $signal_name {
                exit_with_code(FcExitCode::UnexpectedError);
            }
            $signal_metric.store(1);

            write_stderr(format_args!(
                "Shutting down VM after intercepting signal {}, code {}.\n",
                si_signo, si_code
            ));

            $body(si_code, info, ucontext);

            match si_signo {
                $signal_name => exit_with_code(crate::FcExitCode::$exit_code),
                _ => exit_with_code(FcExitCode::UnexpectedError),
            };
        }
    };
}

fn log_sigsys_err(si_code: c_int, info: *mut siginfo_t, ucontext: *mut c_void) {
    if si_code != SYS_SECCOMP_CODE {
        // We received a SIGSYS for a reason other than `bad syscall`.
        exit_with_code(FcExitCode::UnexpectedError);
    }

    // SAFETY: Other signals which might do async unsafe things incompatible with the rest of this
    // function are blocked due to the sa_mask used when registering the signal handler.
    let syscall = unsafe { *(info as *const i32).offset(SI_OFF_SYSCALL) };

    // DIAG: for a seccomp-trapped syscall the args are in the saved registers in
    // the ucontext. On x86_64 the syscall ABI is rdi,rsi,rdx,r10,r8,r9; for mmap
    // (syscall 9) that is addr,len,prot,flags,fd,off. Dump flags/prot/fd/len so
    // the exact blocked mmap can be added to the seccomp filter.
    #[cfg(target_arch = "x86_64")]
    let (flags, prot, fd, len): (i64, i64, i64, i64) = if ucontext.is_null() {
        (-1, -1, -1, -1)
    } else {
        // SAFETY: ucontext is a valid ucontext_t for the duration of the handler.
        let g = unsafe { (*(ucontext as *const libc::ucontext_t)).uc_mcontext.gregs };
        (
            g[libc::REG_R10 as usize],
            g[libc::REG_RDX as usize],
            g[libc::REG_R8 as usize],
            g[libc::REG_RSI as usize],
        )
    };
    #[cfg(not(target_arch = "x86_64"))]
    let (flags, prot, fd, len): (i64, i64, i64, i64) = {
        let _ = ucontext;
        (-1, -1, -1, -1)
    };

    write_stderr(format_args!(
        "Shutting down VM after intercepting a bad syscall ({}). mmap-args \
         flags={} prot={} fd={} len={}\n",
        syscall, flags, prot, fd, len
    ));
}

fn empty_fn(_si_code: c_int, _info: *mut siginfo_t, _ucontext: *mut c_void) {}

generate_handler!(
    sigxfsz_handler,
    SIGXFSZ,
    SIGXFSZ,
    METRICS.signals.sigxfsz,
    empty_fn
);

generate_handler!(
    sigxcpu_handler,
    SIGXCPU,
    SIGXCPU,
    METRICS.signals.sigxcpu,
    empty_fn
);

generate_handler!(
    sigbus_handler,
    SIGBUS,
    SIGBUS,
    METRICS.signals.sigbus,
    empty_fn
);

generate_handler!(
    sigsegv_handler,
    SIGSEGV,
    SIGSEGV,
    METRICS.signals.sigsegv,
    empty_fn
);

generate_handler!(
    sigsys_handler,
    SIGSYS,
    BadSyscall,
    METRICS.seccomp.num_faults,
    log_sigsys_err
);

generate_handler!(
    sighup_handler,
    SIGHUP,
    SIGHUP,
    METRICS.signals.sighup,
    empty_fn
);
generate_handler!(
    sigill_handler,
    SIGILL,
    SIGILL,
    METRICS.signals.sigill,
    empty_fn
);

#[inline(always)]
extern "C" fn sigpipe_handler(num: c_int, info: *mut siginfo_t, _unused: *mut c_void) {
    // Just record the metric and allow the process to continue, the EPIPE error needs
    // to be handled at caller level.
    //
    // Unlike the other handlers this one returns and lets the process keep
    // running, so it is the one where async-signal-safety matters most: it
    // must not touch the logger or allocate. SIGPIPE can interrupt a thread
    // that holds the allocator or logger lock (e.g. a vCPU mid-allocation in
    // save_state()); blocking on either lock here deadlocks every thread that
    // subsequently logs or allocates.

    // SAFETY: Safe because we're just reading some fields from a supposedly valid argument.
    let si_signo = unsafe { (*info).si_signo };
    // SAFETY: Safe because we're just reading some fields from a supposedly valid argument.
    let si_code = unsafe { (*info).si_code };

    if num != si_signo || num != SIGPIPE {
        write_stderr(format_args!(
            "Received invalid signal {}, code {}.\n",
            si_signo, si_code
        ));
        return;
    }

    METRICS.signals.sigpipe.inc();

    write_stderr(format_args!(
        "Received signal {}, code {}.\n",
        si_signo, si_code
    ));
}

/// Registers all the required signal handlers.
///
/// Custom handlers are installed for: `SIGBUS`, `SIGSEGV`, `SIGSYS`
/// `SIGXFSZ` `SIGXCPU` `SIGPIPE` `SIGHUP` and `SIGILL`.
pub fn register_signal_handlers() -> vmm_sys_util::errno::Result<()> {
    // Call to unsafe register_signal_handler which is considered unsafe because it will
    // register a signal handler which will be called in the current thread and will interrupt
    // whatever work is done on the current thread, so we have to keep in mind that the registered
    // signal handler must only do async-signal-safe operations.
    register_signal_handler(SIGSYS, sigsys_handler)?;
    register_signal_handler(SIGBUS, sigbus_handler)?;
    register_signal_handler(SIGSEGV, sigsegv_handler)?;
    register_signal_handler(SIGXFSZ, sigxfsz_handler)?;
    register_signal_handler(SIGXCPU, sigxcpu_handler)?;
    register_signal_handler(SIGPIPE, sigpipe_handler)?;
    register_signal_handler(SIGHUP, sighup_handler)?;
    register_signal_handler(SIGILL, sigill_handler)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::logger::LOGGER;

    /// How long to wait for the signal handler to return before declaring it
    /// deadlocked.
    const HANDLER_TIMEOUT: Duration = Duration::from_secs(5);

    /// Regression test for an intermittent all-threads futex deadlock during
    /// `PUT /snapshot/create`.
    ///
    /// A signal handler that logs takes the logger mutex from signal context.
    /// If the signal lands on a thread whose interrupted frame holds a lock
    /// the logging path needs (the allocator lock, or the logger mutex
    /// itself), the handler blocks forever inside signal context, the
    /// interrupted frame never releases its lock, and every other thread that
    /// logs or allocates piles up behind the two locks.
    ///
    /// The handler must therefore complete without touching the logger: this
    /// test raises SIGPIPE while another thread holds the logger mutex, and a
    /// handler that is not async-signal-safe never returns from the raise.
    #[test]
    fn test_sigpipe_handler_completes_while_logger_mutex_is_held() {
        // Logging macros dispatch through the `log` facade, which is a no-op
        // until LOGGER is registered (in production: at startup, in main()).
        // Register it here, or a handler that logs would never reach
        // Logger::log and its mutex, and this test would pass vacuously.
        // Ignore the error if another test already registered it.
        let _ = LOGGER.init();
        register_signal_handler(SIGPIPE, sigpipe_handler).unwrap();

        // Hold the logger mutex for the duration of the raise, standing in
        // for the lock-holder the signal handler must not wait on.
        let logger_guard = LOGGER.0.lock().unwrap();

        let (tx, rx) = mpsc::channel();
        let raiser = std::thread::spawn(move || {
            // SAFETY: Safe because a handler is installed for SIGPIPE.
            assert_eq!(unsafe { libc::raise(SIGPIPE) }, 0);
            tx.send(()).unwrap();
        });

        let result = rx.recv_timeout(HANDLER_TIMEOUT);
        // Release the mutex before asserting so that on failure the blocked
        // handler can still finish and the test exits with a message instead
        // of poisoning the logger for the rest of the test run.
        drop(logger_guard);
        raiser.join().unwrap();
        result.expect(
            "sigpipe_handler did not return while the logger mutex was held by another thread: \
             it is not async-signal-safe",
        );
    }

    #[test]
    fn test_sigpipe_handler_increments_metric() {
        register_signal_handler(SIGPIPE, sigpipe_handler).unwrap();

        let before = METRICS.signals.sigpipe.count();
        // SAFETY: Safe because a handler is installed for SIGPIPE.
        assert_eq!(unsafe { libc::raise(SIGPIPE) }, 0);
        // `>` rather than `==`: other tests in this process may raise SIGPIPE
        // concurrently.
        assert!(METRICS.signals.sigpipe.count() > before);
    }
}
