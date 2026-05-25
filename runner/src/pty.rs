use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};

use nix::pty::{self, OpenptyResult, Winsize};

use crate::error::{Error, Result};

pub struct Manager {
    sessions: HashMap<u32, Session>,
    next_id: u32,
}

struct Session {
    pty_master: OwnedFd,
    child_pid: libc::pid_t,
}

impl Manager {
    pub fn new() -> Result<Self> {
        Ok(Self {
            sessions: HashMap::new(),
            next_id: 1,
        })
    }

    fn build_envp(rootfs: &str) -> Vec<CString> {
        let mut env = Vec::new();

        env.push(CString::new("TERM=xterm-256color").expect("CString"));
        env.push(CString::new("PS1=$ ").expect("CString"));

        let wayland_display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
        env.push(CString::new(format!("WAYLAND_DISPLAY={wayland_display}")).expect("CString"));

        if rootfs.is_empty() {
            env.push(CString::new(
                "PATH=/sbin:/system/bin:/system/xbin:/data/data/com.littlelinuxrunner/files/rootfs/usr/bin:/data/data/com.littlelinuxrunner/files/rootfs/bin"
            ).expect("CString"));
            env.push(CString::new("HOME=/data/data/com.littlelinuxrunner").expect("CString"));
        } else {
            env.push(CString::new(format!("HOME={rootfs}/root")).expect("CString"));
            env.push(
                CString::new(format!(
                    "PATH={rootfs}/usr/bin:{rootfs}/bin:/system/bin:/system/xbin"
                ))
                .expect("CString"),
            );
            env.push(
                CString::new(format!(
                    "LD_LIBRARY_PATH={rootfs}/lib:{rootfs}/lib64:/system/lib64:/system/lib"
                ))
                .expect("CString"),
            );
            env.push(CString::new(format!("LITTLE_ROOTFS={rootfs}")).expect("CString"));

            let preload_path = format!("{rootfs}/lib/libintercept.so");
            if std::path::Path::new(&preload_path).exists() {
                env.push(CString::new(format!("LD_PRELOAD={preload_path}")).expect("CString"));
                log::info!("LD_PRELOAD: {preload_path}");
            } else {
                let alt = "/data/data/com.littlelinuxrunner/lib/libintercept.so";
                if std::path::Path::new(alt).exists() {
                    env.push(CString::new(format!("LD_PRELOAD={alt}")).expect("CString"));
                    log::info!("LD_PRELOAD: {alt}");
                }
            }
        }

        env
    }

    pub fn create_session(&mut self, shell: &str, rootfs: &str) -> Result<u32> {
        let id = self.next_id;
        self.next_id += 1;

        let shell_cstr = CString::new(shell).unwrap_or(CString::new("/system/bin/sh").unwrap());
        let argv0 = shell_cstr.as_ptr();
        let argv_raw = [argv0, c"-i".as_ptr(), std::ptr::null()];
        let env_cstrings = Self::build_envp(rootfs);
        let env_raw: Vec<*const libc::c_char> = env_cstrings
            .iter()
            .map(|c| c.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        let OpenptyResult { master, slave } =
            pty::openpty(None::<&Winsize>, None::<&nix::sys::termios::Termios>)
                .map_err(|e| Error::Pty(e.to_string()))?;

        let child = unsafe { libc::fork() };
        if child == -1 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }

        if child == 0 {
            drop(master);
            let slave_fd = slave.as_raw_fd();
            unsafe {
                libc::dup2(slave_fd, 0);
                libc::dup2(slave_fd, 1);
                libc::dup2(slave_fd, 2);
            }
            if slave_fd > 2 {
                drop(slave);
            }

            unsafe {
                libc::setsid();
                libc::execve(shell_cstr.as_ptr(), argv_raw.as_ptr(), env_raw.as_ptr());
            }
            unsafe {
                libc::_exit(127);
            }
        }

        drop(slave);

        self.sessions.insert(
            id,
            Session {
                pty_master: master,
                child_pid: child,
            },
        );
        log::info!("PTY session {id} created, pid={child}");

        Ok(id)
    }

    pub fn write_input(&self, session_id: u32, data: &[u8]) -> Result<()> {
        if let Some(session) = self.sessions.get(&session_id) {
            let fd = session.pty_master.as_raw_fd();
            unsafe {
                libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
            }
            Ok(())
        } else {
            Err(Error::Pty(format!("Session {session_id} not found")))
        }
    }

    pub fn resize(&self, session_id: u32, cols: u16, rows: u16) -> Result<()> {
        if let Some(session) = self.sessions.get(&session_id) {
            let fd = session.pty_master.as_raw_fd();
            let ws = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            unsafe {
                libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
            }
            Ok(())
        } else {
            Err(Error::Pty(format!("Session {session_id} not found")))
        }
    }

    pub fn get_session_fd(&self, session_id: u32) -> Option<libc::c_int> {
        self.sessions
            .get(&session_id)
            .map(|s| unsafe { libc::dup(s.pty_master.as_raw_fd()) })
    }

    pub fn close_session(&mut self, session_id: u32) {
        if let Some(session) = self.sessions.remove(&session_id) {
            unsafe {
                libc::kill(session.child_pid, libc::SIGTERM);
                libc::waitpid(session.child_pid, std::ptr::null_mut(), 0);
            }
            log::info!("PTY session {session_id} closed");
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.sessions.keys().copied().collect();
        for id in ids {
            self.close_session(id);
        }
    }
}
