//! PTY master plus child process.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

pub struct Pty {
    pub master: OwnedFd,
    /// Path of the slave device, used to find processes holding it open.
    pub slave_path: String,
}

/// Opens a PTY pair and starts `argv` on the slave as a session leader
/// with the PTY as its controlling terminal.
pub fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &[(String, String)],
    cols: u16,
    rows: u16,
) -> io::Result<(Pty, Child)> {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let slave_path = ptsname(&master, Vec::new())?
        .into_string()
        .map_err(|_| io::Error::other("ptsname"))?;
    let slave = rustix::fs::open(
        &slave_path,
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    set_size(&master, cols, rows)?;

    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::other("empty argv"))?;
    let mut command = Command::new(OsStr::new(program));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    for (key, value) in env {
        command.env(key, value);
    }
    // SAFETY: setsid and ioctl are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    rustix::io::ioctl_fionbio(&master, true)?;
    Ok((Pty { master, slave_path }, child))
}

pub fn set_size(master: &impl AsFd, cols: u16, rows: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ takes a pointer to a winsize.
    if unsafe { libc::ioctl(master.as_fd().as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends `signal` to the child's process group, else to every process that
/// holds the slave open. Returns false when no process was signalled.
pub fn signal(pgid: u32, slave_path: &str, signal: i32) -> bool {
    // SAFETY: kill with a negative pid targets the process group.
    if unsafe { libc::kill(-(pgid as i32), signal) } == 0 {
        return true;
    }
    let mut any = false;
    for pid in holders::holders(slave_path) {
        // SAFETY: kill is safe to call with any pid.
        if unsafe { libc::kill(pid as i32, signal) } == 0 {
            any = true;
        }
    }
    any
}

pub fn signal_number(name: &str) -> Option<i32> {
    Some(match name.trim_start_matches("SIG") {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "KILL" => libc::SIGKILL,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "TERM" => libc::SIGTERM,
        "CONT" => libc::SIGCONT,
        "STOP" => libc::SIGSTOP,
        "TSTP" => libc::SIGTSTP,
        "WINCH" => libc::SIGWINCH,
        _ => return None,
    })
}

pub fn signal_name(number: i32) -> String {
    for name in [
        "HUP", "INT", "QUIT", "KILL", "USR1", "USR2", "TERM", "CONT", "STOP", "TSTP", "WINCH",
    ] {
        if signal_number(name) == Some(number) {
            return name.to_string();
        }
    }
    number.to_string()
}

/// Processes holding a path open, by platform.
mod holders {
    #[cfg(target_os = "linux")]
    pub fn holders(path: &str) -> Vec<u32> {
        let mut out = Vec::new();
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return out;
        };
        for entry in procs.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            if fds
                .flatten()
                .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|t| t.as_os_str() == path))
            {
                out.push(pid);
            }
        }
        out
    }

    #[cfg(target_os = "macos")]
    pub fn holders(path: &str) -> Vec<u32> {
        use std::ffi::CStr;

        // From <sys/proc_info.h>; the libc crate lacks these three.
        const PROC_ALL_PIDS: u32 = 1;
        const PROC_PIDFDVNODEPATHINFO: i32 = 2;
        #[repr(C)]
        struct ProcFileInfo {
            fi_openflags: u32,
            fi_status: u32,
            fi_offset: i64,
            fi_type: i32,
            fi_guardflags: u32,
        }
        #[repr(C)]
        struct VnodeFdInfoWithPath {
            pfi: ProcFileInfo,
            pvip: libc::vnode_info_path,
        }

        let mut out = Vec::new();
        // SAFETY: proc_listpids with a null buffer returns the size needed.
        let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
        if bytes <= 0 {
            return out;
        }
        let mut pids = vec![0i32; bytes as usize / size_of::<i32>()];
        // SAFETY: the buffer holds `bytes` bytes of pids.
        let bytes =
            unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), bytes) };
        pids.truncate(bytes.max(0) as usize / size_of::<i32>());
        for pid in pids.into_iter().filter(|p| *p > 0) {
            // SAFETY: PROC_PIDLISTFDS with a null buffer returns the size needed.
            let size = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0)
            };
            if size <= 0 {
                continue;
            }
            let mut fds = vec![
                libc::proc_fdinfo {
                    proc_fd: 0,
                    proc_fdtype: 0
                };
                size as usize / size_of::<libc::proc_fdinfo>()
            ];
            // SAFETY: the buffer holds `size` bytes of proc_fdinfo.
            let size = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size)
            };
            fds.truncate(size.max(0) as usize / size_of::<libc::proc_fdinfo>());
            for fd in fds
                .iter()
                .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
            {
                // SAFETY: the struct is plain data.
                let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
                let len = size_of::<VnodeFdInfoWithPath>() as i32;
                // SAFETY: the out pointer is a VnodeFdInfoWithPath of `len` bytes.
                let got = unsafe {
                    libc::proc_pidfdinfo(
                        pid,
                        fd.proc_fd,
                        PROC_PIDFDVNODEPATHINFO,
                        (&mut info as *mut VnodeFdInfoWithPath).cast(),
                        len,
                    )
                };
                if got != len {
                    continue;
                }
                // SAFETY: vip_path is a NUL-terminated path.
                let open_path = unsafe { CStr::from_ptr(info.pvip.vip_path.as_ptr().cast()) };
                if open_path.to_bytes() == path.as_bytes() {
                    out.push(pid as u32);
                    break;
                }
            }
        }
        out
    }
}
