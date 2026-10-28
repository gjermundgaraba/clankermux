use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;

use rustix::process::{Pid, Signal};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use tokio::process::{Child, Command};

pub struct Pty {
    pub master: OwnedFd,
    pub slave_path: String,
}

pub fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    cols: u16,
    rows: u16,
) -> io::Result<(Pty, Child)> {
    // Where `posix_openpt` cannot take `O_CLOEXEC`, a fork between the
    // open and the flag would inherit the master; the forks are here, so
    // one spawn at a time keeps every master out of every other child.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _spawning = ONE_AT_A_TIME.lock().unwrap();
    let master = open_master()?;
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

    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| io::Error::other("empty argv"))?;
    let mut command = Command::new(OsStr::new(program));
    command
        .args(arguments)
        .current_dir(cwd)
        .envs(env)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: setsid and the ioctl are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()?;
            rustix::process::ioctl_tiocsctty(std::io::stdin())?;
            Ok(())
        });
    }
    let child = command.spawn()?;
    rustix::io::ioctl_fionbio(&master, true)?;
    Ok((Pty { master, slave_path }, child))
}

#[cfg(target_os = "linux")]
fn open_master() -> io::Result<OwnedFd> {
    Ok(openpt(
        OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
    )?)
}

/// `posix_openpt` has no `O_CLOEXEC` here; the flag is set right after.
#[cfg(not(target_os = "linux"))]
fn open_master() -> io::Result<OwnedFd> {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
    Ok(master)
}

pub fn set_size(master: &impl AsFd, cols: u16, rows: u16) -> io::Result<()> {
    let size = rustix::termios::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    Ok(rustix::termios::tcsetwinsize(master, size)?)
}

pub fn signal(pgid: u32, slave_path: &str, signal: Signal) -> bool {
    let kill = |pid: u32, group: bool| {
        i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .is_some_and(|pid| {
                if group {
                    rustix::process::kill_process_group(pid, signal).is_ok()
                } else {
                    rustix::process::kill_process(pid, signal).is_ok()
                }
            })
    };
    if kill(pgid, true) {
        return true;
    }
    let mut any = false;
    for pid in holders::holders(slave_path) {
        any |= kill(pid, false);
    }
    any
}

const SIGNALS: &[(&str, i32)] = &[
    ("HUP", libc::SIGHUP),
    ("INT", libc::SIGINT),
    ("QUIT", libc::SIGQUIT),
    ("ILL", libc::SIGILL),
    ("TRAP", libc::SIGTRAP),
    ("ABRT", libc::SIGABRT),
    ("BUS", libc::SIGBUS),
    ("FPE", libc::SIGFPE),
    ("KILL", libc::SIGKILL),
    ("USR1", libc::SIGUSR1),
    ("SEGV", libc::SIGSEGV),
    ("USR2", libc::SIGUSR2),
    ("PIPE", libc::SIGPIPE),
    ("ALRM", libc::SIGALRM),
    ("TERM", libc::SIGTERM),
    ("CHLD", libc::SIGCHLD),
    ("CONT", libc::SIGCONT),
    ("STOP", libc::SIGSTOP),
    ("TSTP", libc::SIGTSTP),
    ("TTIN", libc::SIGTTIN),
    ("TTOU", libc::SIGTTOU),
    ("URG", libc::SIGURG),
    ("XCPU", libc::SIGXCPU),
    ("XFSZ", libc::SIGXFSZ),
    ("VTALRM", libc::SIGVTALRM),
    ("PROF", libc::SIGPROF),
    ("WINCH", libc::SIGWINCH),
    ("IO", libc::SIGIO),
    ("SYS", libc::SIGSYS),
];

pub fn signal_by_name(name: &str) -> Option<Signal> {
    let name = name.strip_prefix("SIG").unwrap_or(name);
    let (_, number) = SIGNALS.iter().find(|(n, _)| *n == name)?;
    Signal::from_named_raw(*number)
}

pub fn signal_name(number: i32) -> String {
    SIGNALS
        .iter()
        .find(|(_, n)| *n == number)
        .map_or_else(|| number.to_string(), |(name, _)| name.to_string())
}

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
        #[expect(
            clippy::struct_field_names,
            reason = "mirrors struct proc_fileinfo in <sys/proc_info.h>"
        )]
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
        let Ok(capacity @ 1..) = usize::try_from(bytes) else {
            return out;
        };
        let mut pids = vec![0i32; capacity / size_of::<i32>()];
        // SAFETY: the buffer holds `bytes` bytes of pids.
        let bytes =
            unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), bytes) };
        pids.truncate(usize::try_from(bytes).unwrap_or(0) / size_of::<i32>());
        for pid in pids.into_iter().filter(|p| *p > 0) {
            // SAFETY: PROC_PIDLISTFDS with a null buffer returns the size needed.
            let size = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0)
            };
            let Ok(capacity @ 1..) = usize::try_from(size) else {
                continue;
            };
            let mut fds = vec![
                libc::proc_fdinfo {
                    proc_fd: 0,
                    proc_fdtype: 0
                };
                capacity / size_of::<libc::proc_fdinfo>()
            ];
            // SAFETY: the buffer holds `size` bytes of proc_fdinfo.
            let size = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size)
            };
            fds.truncate(usize::try_from(size).unwrap_or(0) / size_of::<libc::proc_fdinfo>());
            for fd in fds
                .iter()
                .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
            {
                // SAFETY: the struct is plain data.
                let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
                let len = i32::try_from(size_of::<VnodeFdInfoWithPath>())
                    .expect("vnode_fdinfowithpath fits an int");
                // SAFETY: the out pointer is a VnodeFdInfoWithPath of `len` bytes.
                let got = unsafe {
                    libc::proc_pidfdinfo(
                        pid,
                        fd.proc_fd,
                        PROC_PIDFDVNODEPATHINFO,
                        (&raw mut info).cast(),
                        len,
                    )
                };
                if got != len {
                    continue;
                }
                // SAFETY: vip_path is a NUL-terminated path.
                let open_path = unsafe { CStr::from_ptr(info.pvip.vip_path.as_ptr().cast()) };
                if open_path.to_bytes() == path.as_bytes() {
                    out.push(pid.cast_unsigned());
                    break;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names_round_trip() {
        assert_eq!(
            signal_by_name("SIGHUP"),
            Signal::from_named_raw(libc::SIGHUP)
        );
        assert_eq!(
            signal_by_name("SEGV"),
            Signal::from_named_raw(libc::SIGSEGV)
        );
        assert_eq!(signal_by_name("SIGSIGHUP"), None);
        assert_eq!(signal_name(libc::SIGSEGV), "SEGV");
        assert_eq!(signal_name(200), "200");
    }
}
