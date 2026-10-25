//! Small Unix pty helpers: start a shell in a pseudo-terminal, resize it, close it.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

/// Turn a libc return code into an error when it failed.
fn check(ret: libc::c_int) -> io::Result<()> {
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Start `shell` in a new pty. Returns our end of the pty (read output,
/// write input) and the shell process.
pub fn spawn(shell: &str, rows: u16, cols: u16) -> io::Result<(File, Child)> {
    let (mut master, mut slave) = (0, 0);
    let ws = winsize(rows, cols);
    // SAFETY: the pointers are valid; null name and settings are allowed.
    check(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws) })?;
    // SAFETY: openpty just made these fds and nothing else owns them.
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    // Don't let later shells inherit these. If they did, a closed pane
    // would never see its terminal close.
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        // SAFETY: fd is open and owned above.
        check(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) })?;
    }

    let mut cmd = Command::new(shell);
    cmd.env("TERM", "xterm-256color").stdin(slave.try_clone()?).stdout(slave.try_clone()?).stderr(slave);
    // SAFETY: only calls that are safe between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            // Own session, with the pty as its terminal, so Ctrl+C and job control work.
            check(libc::setsid())?;
            check(libc::ioctl(0, libc::TIOCSCTTY as _, 0))
        });
    }
    let child = cmd.spawn()?;
    Ok((master, child))
}

/// Tell the pty its new size. The kernel lets the shell know (SIGWINCH).
pub fn resize(master: &File, rows: u16, cols: u16) -> io::Result<()> {
    let ws = winsize(rows, cols);
    // SAFETY: fd is an open pty, ws lives through the call.
    check(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) })
}

/// Ask the shell to quit, like closing a terminal window does (SIGHUP).
/// Unlike a hard kill, the shell still saves its history.
pub fn hangup(child: &Child) {
    // SAFETY: plain signal send to our own child's pid.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGHUP) };
}
