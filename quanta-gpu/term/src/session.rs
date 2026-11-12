//! PTY session for the QuantaGPU terminal.
//!
//! Mirrors the orbterm/FerriticUI setup: `/dev/ptmx`, grantpt/unlockpt, a
//! shell with the slave as stdio, `setsid` in the child, and `TIOCSWINSZ`.
//! Redox's pty scheme prefixes each read packet with a one-byte flag, so the
//! first byte is stripped before the payload reaches the terminal emulator.

use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

extern "C" {
    fn ptsname(fd: libc::c_int) -> *const libc::c_char;
    fn grantpt(fd: libc::c_int) -> libc::c_int;
    fn unlockpt(fd: libc::c_int) -> libc::c_int;
}

pub struct Session {
    master: File,
    child: Child,
}

impl Session {
    pub fn spawn(shell: &str, cols: u16, rows: u16) -> io::Result<Self> {
        let (master_fd, tty_path) = getpty(cols, rows)?;
        let master = unsafe { File::from_raw_fd(master_fd) };
        let (slave_in, slave_out, slave_err) = slave_stdio(&tty_path)?;

        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
        let mut command = Command::new(shell);
        command
            .stdin(Stdio::from(slave_in))
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(slave_err))
            .env_remove("COLUMNS")
            .env_remove("LINES")
            .env("TTY", &tty_path)
            .env("TERM", "xterm-256color")
            // init starts scheme services with a minimal `env -i` environment;
            // ion needs the usual login variables to expand its prompt.
            .env("HOME", &home)
            .env("USER", "root")
            .env("LOGNAME", "root")
            .env("PWD", &home)
            .env("SHELL", shell)
            .env("XDG_RUNTIME_DIR", "/tmp")
            .env("XDG_CONFIG_HOME", format!("{home}/.config"))
            .env("XDG_DATA_HOME", format!("{home}/.local/share"))
            .env("XDG_CACHE_HOME", format!("{home}/.cache"));
        unsafe {
            command.pre_exec(before_exec);
        }
        let child = command.spawn()?;

        Ok(Self { master, child })
    }

    /// Reads all currently available output, returning the bytes appended.
    pub fn read(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        let start = out.len();
        let mut buffer = [0u8; 8192];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    // Redox's pty scheme prefixes each packet with a flag byte.
                    if count > 1 {
                        out.extend_from_slice(&buffer[1..count]);
                    }
                }
                Err(ref error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(ref error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(out.len() - start)
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.master.write_all(bytes)?;
        self.master.flush()
    }

    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn getpty(cols: u16, rows: u16) -> io::Result<(RawFd, String)> {
    let master_fd = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open("/dev/ptmx")?
        .into_raw_fd();

    unsafe {
        if grantpt(master_fd) < 0 {
            return Err(io::Error::last_os_error());
        }
        if unlockpt(master_fd) < 0 {
            return Err(io::Error::last_os_error());
        }
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if libc::ioctl(master_fd, libc::TIOCSWINSZ, &size) < 0 {
            return Err(io::Error::last_os_error());
        }
        let name = ptsname(master_fd);
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok((
            master_fd,
            CStr::from_ptr(name).to_string_lossy().into_owned(),
        ))
    }
}

fn slave_stdio(tty_path: &str) -> io::Result<(File, File, File)> {
    let path = CString::new(tty_path)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "pty path contains a nul byte"))?;
    let open = |flags: libc::c_int| -> io::Result<File> {
        let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    };
    Ok((
        open(libc::O_RDONLY)?,
        open(libc::O_WRONLY)?,
        open(libc::O_WRONLY)?,
    ))
}

fn before_exec() -> io::Result<()> {
    unsafe {
        if libc::setsid() < 0 {
            return Err(io::Error::last_os_error());
        }
        // Redox's relibc does not implement TIOCSCTTY; the slave still becomes
        // the shell's controlling terminal.
        #[cfg(not(target_os = "redox"))]
        {
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}
