use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::{Error, Result};

pub struct Manager {
    rootfs: PathBuf,
    hook_lib_path: PathBuf,
    preload_enabled: bool,
    path_translator: PathTranslator,
}

struct PathTranslator {
    rootfs: PathBuf,
    mappings: HashMap<String, String>,
}

impl PathTranslator {
    fn new(rootfs: &PathBuf) -> Self {
        let mut mappings = HashMap::new();
        mappings.insert("/usr".into(), format!("{}/usr", rootfs.display()));
        mappings.insert("/bin".into(), format!("{}/bin", rootfs.display()));
        mappings.insert("/lib".into(), format!("{}/lib", rootfs.display()));
        mappings.insert("/lib64".into(), format!("{}/lib64", rootfs.display()));
        mappings.insert("/etc".into(), format!("{}/etc", rootfs.display()));
        mappings.insert("/var".into(), format!("{}/var", rootfs.display()));
        mappings.insert("/opt".into(), format!("{}/opt", rootfs.display()));
        mappings.insert("/tmp".into(), format!("{}/tmp", rootfs.display()));
        mappings.insert("/home".into(), format!("{}/home", rootfs.display()));
        mappings.insert("/sbin".into(), format!("{}/sbin", rootfs.display()));
        mappings.insert("/root".into(), format!("{}/root", rootfs.display()));
        Self {
            rootfs: rootfs.clone(),
            mappings,
        }
    }

    fn translate(&self, path: &str) -> String {
        if path.starts_with("/proc/") || path.starts_with("/sys/") || path.starts_with("/dev/") {
            return path.to_string();
        }
        if path.starts_with(self.rootfs.to_str().unwrap_or("")) {
            return path.to_string();
        }
        for (prefix, mapped) in &self.mappings {
            if path == prefix || path.starts_with(&format!("{prefix}/")) {
                let rest = path.strip_prefix(prefix).unwrap_or("");
                return format!("{mapped}{rest}");
            }
        }
        path.to_string()
    }
}

impl Manager {
    pub fn new(rootfs_path: &str) -> Result<Self> {
        let rootfs = if rootfs_path.is_empty() {
            PathBuf::from("/data/data/com.littlelinuxrunner/files/rootfs")
        } else {
            PathBuf::from(rootfs_path)
        };

        let hook_lib = rootfs.join("lib").join("libintercept.so");
        let path_translator = PathTranslator::new(&rootfs);
        Ok(Self {
            rootfs,
            hook_lib_path: hook_lib,
            preload_enabled: false,
            path_translator,
        })
    }

    pub fn prepare_rootfs(&self) -> Result<()> {
        let dirs = [
            "usr", "bin", "lib", "lib64", "etc", "var", "opt", "tmp", "home", "sbin", "dev",
            "proc", "sys", "root",
        ];
        for dir in &dirs {
            let path = self.rootfs.join(dir);
            std::fs::create_dir_all(&path)
                .map_err(|e| Error::Intercept(format!("mkdir {dir}: {e}")))?;
        }
        log::info!("RootFS directories prepared at {}", self.rootfs.display());
        Ok(())
    }

    pub fn enable_preload_hooks(&mut self) -> Result<()> {
        let hook_path = if self.hook_lib_path.exists() {
            self.hook_lib_path.clone()
        } else {
            let alt = PathBuf::from("/data/data/com.littlelinuxrunner/lib/libintercept.so");
            if alt.exists() {
                alt
            } else {
                log::warn!("Hook library not found, LD_PRELOAD disabled");
                return Ok(());
            }
        };

        std::env::set_var("LD_PRELOAD", hook_path.to_str().unwrap_or(""));
        std::env::set_var("LITTLE_ROOTFS", self.rootfs.to_str().unwrap_or(""));
        self.preload_enabled = true;
        log::info!("LD_PRELOAD hooks enabled: {}", hook_path.display());
        Ok(())
    }

    pub fn translate_path(&self, path: &str) -> String {
        self.path_translator.translate(path)
    }

    pub fn rootfs_path(&self) -> &PathBuf {
        &self.rootfs
    }

    pub fn spawn_in_rootfs(&self, program: &str, args: &[&str]) -> Result<std::process::Child> {
        let translated = self.translate_path(program);
        let envs: Vec<(String, String)> = vec![
            (
                "PATH".into(),
                format!(
                    "{}/usr/bin:{}/bin",
                    self.rootfs.display(),
                    self.rootfs.display()
                ),
            ),
            (
                "LD_LIBRARY_PATH".into(),
                format!(
                    "{}/lib:{}/lib64",
                    self.rootfs.display(),
                    self.rootfs.display()
                ),
            ),
            ("HOME".into(), format!("{}/root", self.rootfs.display())),
            ("TERM".into(), "xterm-256color".into()),
            (
                "WAYLAND_DISPLAY".into(),
                std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "little-wayland".into()),
            ),
        ];

        let child = std::process::Command::new(&translated)
            .args(args)
            .envs(envs)
            .spawn()
            .map_err(|e| Error::Intercept(format!("spawn {program}: {e}")))?;

        Ok(child)
    }
}
