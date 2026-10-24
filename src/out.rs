//! Printing that survives a closed pipe.
//!
//! `selx status | head` closes stdout under us. Rust ignores `SIGPIPE`, so
//! the write returns `EPIPE` instead — which `println!` has nowhere to
//! report but a panic.
//!
//! Taking the signal back (restoring `SIG_DFL`) would fix the printing and
//! break something else: the CLI also writes to the daemon's socket, where
//! a broken pipe is an error the code recovers from — `try_ping` reads one
//! as "this daemon is stale, restart it" — and a signal would kill the
//! process mid-request instead. A signal disposition is process-wide and
//! cannot tell the two writes apart, so `SIGPIPE` stays ignored and the
//! `EPIPE` is handled here, where the printing happens.
//!
//! `clippy.toml` bans `println!` in this workspace so a new call site
//! cannot quietly reintroduce the panic.

use std::io::Write;

/// What a program killed by `SIGPIPE` (signal 13) reports, so pipelines
/// see the status they expect from any other tool.
pub const BROKEN_PIPE_EXIT: i32 = 141;

/// Back an [`outln!`](crate::outln) with a locked stdout, ending the
/// process quietly when the reader has gone away.
pub fn line(args: std::fmt::Arguments<'_>) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // Rust's stdout is line-buffered, so the newline is what actually
    // reaches the pipe — and what discovers that nobody is reading.
    let written = out.write_fmt(args).and_then(|()| out.write_all(b"\n"));
    match written {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            std::process::exit(BROKEN_PIPE_EXIT)
        }
        Err(e) => {
            eprintln!("selx: writing to stdout: {e}");
            std::process::exit(crate::exit::ERROR as i32);
        }
    }
}

/// Back a [`warnln!`](crate::warnln) with a locked stderr.
///
/// One shape for everything the user is meant to read: `selx: warning: …`,
/// matching the `selx: …` of a fatal error. `tracing` stays for `-v`
/// diagnostics, which are a different audience. Unwritable stderr is
/// dropped rather than escalated — a warning is not worth failing over,
/// and the command's own result is still on stdout.
pub fn warn_line(args: std::fmt::Arguments<'_>) {
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    let _ = err
        .write_all(b"selx: warning: ")
        .and_then(|()| err.write_fmt(args))
        .and_then(|()| err.write_all(b"\n"));
}

/// `println!` that treats a closed pipe as the end of the program rather
/// than as a panic. See the module docs.
#[macro_export]
macro_rules! outln {
    () => { $crate::out::line(format_args!("")) };
    ($($arg:tt)*) => { $crate::out::line(format_args!($($arg)*)) };
}

/// A warning the user is meant to read, on stderr as `selx: warning: …`.
/// For anything only a `-v` run should see, use `tracing` instead.
#[macro_export]
macro_rules! warnln {
    ($($arg:tt)*) => { $crate::out::warn_line(format_args!($($arg)*)) };
}
