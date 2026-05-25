pub mod error;
pub mod intercept;
pub mod pty;

use error::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct App {
    running: Arc<AtomicBool>,
    intercept_manager: Option<intercept::Manager>,
    pty_manager: Option<pty::Manager>,
    rootfs_path: String,
}

impl App {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            intercept_manager: None,
            pty_manager: None,
            rootfs_path: String::new(),
        }
    }

    pub fn start(&mut self) -> Result<()> {
        log::info!("Starting lroot");

        if !self.rootfs_path.is_empty() {
            log::info!("RootFS path set: {}", self.rootfs_path);
        }

        self.intercept_manager = Some(intercept::Manager::new(&self.rootfs_path)?);
        log::info!("Intercept layer initialized");

        self.pty_manager = Some(pty::Manager::new()?);
        log::info!("PTY manager initialized");

        Ok(())
    }

    pub fn run(&self) -> ! {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if !self.running.load(Ordering::Relaxed) {
                std::process::exit(0);
            }
        }
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    pub fn set_rootfs_path(&mut self, path: &str) {
        self.rootfs_path = path.to_string();
        log::info!("RootFS path set to: {path}");
    }

    pub fn rootfs_path(&self) -> &str {
        &self.rootfs_path
    }

    pub fn create_pty_session(&mut self, shell: &str) -> Result<u32> {
        match self.pty_manager {
            Some(ref mut mgr) => {
                let rootfs = &self.rootfs_path;
                let id = mgr.create_session(shell, rootfs)?;
                log::info!("PTY session {id} created for shell: {shell}, rootfs: {rootfs}");
                Ok(id)
            }
            None => Err(crate::error::Error::InvalidState(
                "PTY manager not initialized".into(),
            )),
        }
    }

    pub fn get_session_fd(&self, session_id: u32) -> Option<std::os::fd::RawFd> {
        self.pty_manager
            .as_ref()
            .and_then(|mgr| mgr.get_session_fd(session_id))
    }

    pub fn write_pty(&self, session_id: u32, data: &[u8]) -> Result<()> {
        match self.pty_manager {
            Some(ref mgr) => mgr.write_input(session_id, data),
            None => Err(crate::error::Error::InvalidState(
                "PTY manager not initialized".into(),
            )),
        }
    }

    pub fn resize_pty(&self, session_id: u32, cols: u16, rows: u16) -> Result<()> {
        match self.pty_manager {
            Some(ref mgr) => mgr.resize(session_id, cols, rows),
            None => Err(crate::error::Error::InvalidState(
                "PTY manager not initialized".into(),
            )),
        }
    }
}
