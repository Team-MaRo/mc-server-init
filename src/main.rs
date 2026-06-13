//! mc-server-init — a tiny PID-1 init for the Spigot container.
//!
//! It replaces itzg's mc-server-runner. The key difference: it runs the server
//! behind a **pseudo-terminal (PTY)** instead of a plain pipe, so Spigot's JLine
//! console detects a real terminal and keeps the `>` prompt + line-editing. On
//! top of that it does what mc-server-runner did:
//!
//!   * forwards the container's own stdin (interactive `docker run -it` /
//!     `docker attach`) into the server console,
//!   * forwards a named pipe (FIFO, default /tmp/console-in) into the console so
//!     `console <cmd>` can inject commands without RCON,
//!   * turns SIGTERM / SIGINT — and a typed Ctrl+C (raw 0x03 byte) — into a
//!     clean `stop`, so the world is saved with the "Saving…" logs, falling back
//!     to SIGKILL only after a timeout,
//!   * reaps the child as PID 1 and propagates its exit code.
//!
//! Usage:
//!   mc-server-init [--console-pipe PATH] [--stop-timeout SECS] [--stop-command CMD] -- <program> [args…]
//!
//! Dependencies: `nix` (syscalls; re-exports `libc`) and `clap` (arg parsing).

use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::time::{Duration, Instant};

use clap::Parser;
use nix::libc;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::pty::{openpty, OpenptyResult, Winsize};
use nix::sys::signal::{sigprocmask, SigSet, SigmaskHow, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execvp, fork, ForkResult, Pid};

const BUF: usize = 8192;

/// Tiny PID-1 init for Minecraft server containers: PTY-backed console,
/// named-pipe injection, and SIGTERM/SIGINT/Ctrl+C -> graceful stop.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Named pipe the `console` helper writes to (console command injection).
    #[arg(long, default_value = "/tmp/console-in")]
    console_pipe: String,

    /// Seconds to wait for a graceful stop before sending SIGKILL.
    #[arg(long, default_value_t = 60)]
    stop_timeout: u64,

    /// Console command sent to the server on shutdown.
    #[arg(long, default_value = "stop")]
    stop_command: String,

    /// The server program and its arguments, after `--`
    /// (e.g. `-- java -jar /opt/spigot.jar --nogui`).
    #[arg(last = true, required = true)]
    argv: Vec<String>,
}

fn main() {
    // clap handles --help / --version / bad input (printing + exiting) for us.
    let cli = Cli::parse();
    std::process::exit(run(cli));
}

fn run(cli: Cli) -> i32 {
    // The program + args to exec, as C strings (NUL bytes are rejected here).
    let argv: Vec<CString> = match cli.argv.iter().map(|s| CString::new(s.as_bytes())).collect() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("[mc-server-init] a program argument contains a NUL byte");
            return 2;
        }
    };

    // Window size for the PTY: copy the container terminal's if it has one.
    let ws = current_winsize(libc::STDIN_FILENO).unwrap_or(Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    });

    let OpenptyResult { master, slave } = match openpty(Some(&ws), None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[mc-server-init] openpty failed: {e}");
            return 1;
        }
    };
    let master_fd = master.as_raw_fd();
    let slave_fd = slave.as_raw_fd();

    // Block the signals we manage and read them via a signalfd, so they are
    // handled synchronously in the poll loop (never interrupting a write). Must
    // happen before fork so the child inherits the block; the child unblocks.
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGCHLD);
    mask.add(Signal::SIGWINCH);
    if let Err(e) = sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None) {
        eprintln!("[mc-server-init] sigprocmask failed: {e}");
        return 1;
    }

    let child = match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // ---- child: become a session leader on the slave, exec the server.
            unsafe {
                libc::setsid();
                libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
                libc::dup2(slave_fd, libc::STDIN_FILENO);
                libc::dup2(slave_fd, libc::STDOUT_FILENO);
                libc::dup2(slave_fd, libc::STDERR_FILENO);
                if slave_fd > libc::STDERR_FILENO {
                    libc::close(slave_fd);
                }
                libc::close(master_fd);
            }
            // Restore default signal disposition for the server.
            let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None);
            let _ = execvp(&argv[0], &argv);
            // Only reached if exec failed.
            eprintln!("[mc-server-init] exec failed: {}", std::io::Error::last_os_error());
            unsafe { libc::_exit(127) }
        }
        Ok(ForkResult::Parent { child }) => child,
        Err(e) => {
            eprintln!("[mc-server-init] fork failed: {e}");
            return 1;
        }
    };

    // ---- parent (PID 1): the supervisor.
    drop(slave); // close our copy of the slave so master sees EOF when the child dies

    let sfd = match SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[mc-server-init] signalfd failed: {e}");
            let _ = waitpid(child, None);
            return 1;
        }
    };
    let sig_fd = sfd.as_raw_fd();

    // The console FIFO. Open O_RDWR so it never hits EOF when no writer is
    // attached (we hold a write end ourselves).
    let _ = std::fs::remove_file(&cli.console_pipe);
    let cpath = CString::new(cli.console_pipe.as_bytes()).unwrap();
    let fifo_fd = unsafe {
        if libc::mkfifo(cpath.as_ptr(), 0o600) != 0 {
            eprintln!(
                "[mc-server-init] mkfifo {} failed: {}",
                cli.console_pipe,
                std::io::Error::last_os_error()
            );
        }
        libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK)
    };

    // Put the container stdin in raw mode (if it's a TTY) so keystrokes — and a
    // typed Ctrl+C (0x03) — reach us byte-for-byte. Restored before we exit.
    let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
    let saved_termios = if stdin_is_tty {
        set_raw(libc::STDIN_FILENO)
    } else {
        None
    };

    eprintln!(
        "[mc-server-init] supervising pid {} (console pipe: {}, stop after {}s)",
        child, cli.console_pipe, cli.stop_timeout
    );

    let exit_code = event_loop(EventLoop {
        child,
        master_fd,
        fifo_fd,
        sig_fd,
        stop_command: &cli.stop_command,
        stop_timeout: Duration::from_secs(cli.stop_timeout),
    });

    if let Some(t) = saved_termios {
        restore_termios(libc::STDIN_FILENO, &t);
    }
    let _ = std::fs::remove_file(&cli.console_pipe);
    exit_code
}

struct EventLoop<'a> {
    child: Pid,
    master_fd: RawFd,
    fifo_fd: RawFd,
    sig_fd: RawFd,
    stop_command: &'a str,
    stop_timeout: Duration,
}

fn event_loop(e: EventLoop) -> i32 {
    let mut buf = [0u8; BUF];
    let mut watch_stdin = true;
    let mut stop_deadline: Option<Instant> = None;
    let mut final_status: Option<i32> = None;

    let begin_stop = |master_fd: RawFd, deadline: &mut Option<Instant>| {
        if deadline.is_none() {
            // Leading newline so this doesn't land on the server's `>` prompt line.
            eprintln!("\n[mc-server-init] stopping: sending `{}`", e.stop_command);
            let line = format!("{}\n", e.stop_command);
            write_all(master_fd, line.as_bytes());
            *deadline = Some(Instant::now() + e.stop_timeout);
        }
    };

    loop {
        // Reap opportunistically; if the child is gone we're done.
        if let Some(code) = try_reap(e.child) {
            final_status = Some(code);
            break;
        }

        // Build the poll set fresh each iteration (borrows are short-lived).
        let master_b = unsafe { BorrowedFd::borrow_raw(e.master_fd) };
        let fifo_b = unsafe { BorrowedFd::borrow_raw(e.fifo_fd) };
        let sig_b = unsafe { BorrowedFd::borrow_raw(e.sig_fd) };
        let stdin_b = unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) };

        let mut fds = vec![
            PollFd::new(master_b, PollFlags::POLLIN),
            PollFd::new(fifo_b, PollFlags::POLLIN),
            PollFd::new(sig_b, PollFlags::POLLIN),
        ];
        if watch_stdin {
            fds.push(PollFd::new(stdin_b, PollFlags::POLLIN));
        }

        let timeout = match stop_deadline {
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    eprintln!("[mc-server-init] stop timed out; sending SIGKILL");
                    unsafe { libc::kill(e.child.as_raw(), libc::SIGKILL) };
                    final_status = Some(waitpid_blocking(e.child));
                    break;
                }
                let ms = (d - now).as_millis().min(1000) as u16;
                PollTimeout::from(ms)
            }
            None => PollTimeout::NONE,
        };

        match poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(err) => {
                eprintln!("[mc-server-init] poll error: {err}");
                break;
            }
        }

        let master_re = fds[0].revents().unwrap_or(PollFlags::empty());
        let fifo_re = fds[1].revents().unwrap_or(PollFlags::empty());
        let sig_re = fds[2].revents().unwrap_or(PollFlags::empty());
        let stdin_re = if watch_stdin {
            fds[3].revents().unwrap_or(PollFlags::empty())
        } else {
            PollFlags::empty()
        };

        // Server output -> container stdout.
        if master_re.intersects(PollFlags::POLLIN) {
            match read_fd(e.master_fd, &mut buf) {
                n if n > 0 => write_all(libc::STDOUT_FILENO, &buf[..n as usize]),
                _ => break, // EOF/EIO: child closed the PTY -> it's exiting
            }
        } else if master_re.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }

        // Console injection (FIFO) -> server stdin.
        if fifo_re.intersects(PollFlags::POLLIN) {
            let n = read_fd(e.fifo_fd, &mut buf);
            if n > 0 {
                write_all(e.master_fd, &buf[..n as usize]);
            }
        }

        // Container stdin (interactive) -> server stdin, intercepting Ctrl+C.
        if stdin_re.intersects(PollFlags::POLLIN) {
            let n = read_fd(libc::STDIN_FILENO, &mut buf);
            if n > 0 {
                let bytes = &buf[..n as usize];
                if bytes.contains(&0x03) { // Ctrl+C
                    begin_stop(e.master_fd, &mut stop_deadline);
                }
                // Forward everything except the bare ETX so it isn't echoed weirdly.
                let forward: Vec<u8> = bytes.iter().copied().filter(|b| *b != 0x03).collect();
                if !forward.is_empty() {
                    write_all(e.master_fd, &forward);
                }
            } else {
                watch_stdin = false; // EOF on stdin: stop watching it
            }
        } else if stdin_re.intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL) {
            watch_stdin = false;
        }

        // Signals.
        if sig_re.intersects(PollFlags::POLLIN) {
            drain_signals(e.sig_fd, |signo| match signo {
                libc::SIGTERM | libc::SIGINT => begin_stop(e.master_fd, &mut stop_deadline), // docker stop / Ctrl-C-as-signal
                libc::SIGWINCH => copy_winsize(libc::STDIN_FILENO, e.master_fd), // terminal resized → resize PTY
                libc::SIGCHLD => {} // handled by try_reap at the top of the loop
                _ => {}
            });
        }
    }

    // Make sure the child is reaped and we have a code.
    final_status.unwrap_or_else(|| waitpid_blocking(e.child))
}

// ---------------------------------------------------------------------------
// Small syscall helpers (libc), kept separate to keep the loop readable.
// ---------------------------------------------------------------------------

fn read_fd(fd: RawFd, buf: &mut [u8]) -> isize {
    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) }
}

fn write_all(fd: RawFd, mut buf: &[u8]) {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n <= 0 {
            break; // child gone / pipe broken: drop the rest
        }
        buf = &buf[n as usize..];
    }
}

/// Read all pending siginfo structs from the signalfd and dispatch each.
fn drain_signals(sig_fd: RawFd, mut f: impl FnMut(libc::c_int)) {
    let size = std::mem::size_of::<libc::signalfd_siginfo>();
    loop {
        let mut si: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::read(
                sig_fd,
                &mut si as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if n != size as isize {
            break; // EAGAIN (drained) or short read
        }
        f(si.ssi_signo as libc::c_int);
    }
}

/// Non-blocking reap. Returns the exit code if the child has terminated.
fn try_reap(child: Pid) -> Option<i32> {
    match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::Exited(_, code)) => Some(code),
        Ok(WaitStatus::Signaled(_, sig, _)) => Some(128 + sig as i32),
        _ => None,
    }
}

fn waitpid_blocking(child: Pid) -> i32 {
    match waitpid(child, None) {
        Ok(WaitStatus::Exited(_, code)) => code,
        Ok(WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
        _ => 0,
    }
}

fn current_winsize(fd: RawFd) -> Option<Winsize> {
    if unsafe { libc::isatty(fd) } != 1 {
        return None;
    }
    let mut ws: Winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 {
        Some(ws)
    } else {
        None
    }
}

fn copy_winsize(from: RawFd, to: RawFd) {
    if let Some(ws) = current_winsize(from) {
        unsafe { libc::ioctl(to, libc::TIOCSWINSZ, &ws) };
    }
}

fn set_raw(fd: RawFd) -> Option<libc::termios> {
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
        return None;
    }
    let mut raw = orig;
    unsafe { libc::cfmakeraw(&mut raw) };
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    Some(orig)
}

fn restore_termios(fd: RawFd, t: &libc::termios) {
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) };
}
