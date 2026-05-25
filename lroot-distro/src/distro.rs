use std::path::PathBuf;

pub struct Distro {
    pub name: &'static str,
    pub tarball_url: &'static str,
}

/// Known distro definitions.
/// Tarballs are rootfs archives that extract to a single root directory.
pub const DISTROS: &[Distro] = &[
    Distro {
        name: "alpine",
        tarball_url: "https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/aarch64/alpine-minirootfs-3.21.3-aarch64.tar.gz",
    },
    Distro {
        name: "alpine-amd64",
        tarball_url: "https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86_64/alpine-minirootfs-3.21.3-x86_64.tar.gz",
    },
    Distro {
        name: "ubuntu",
        tarball_url: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-minimal-cloudimg-amd64-root.tar.xz",
    },
    Distro {
        name: "debian",
        tarball_url: "https://github.com/nickg/rootfs/releases/download/v2025-01-01/debian-bookworm-arm64.tar.gz",
    },
    Distro {
        name: "arch",
        tarball_url: "https://github.com/nickg/rootfs/releases/download/v2025-01-01/archlinux-arm64.tar.gz",
    },
];

pub fn data_dir() -> PathBuf {
    let base = if let Ok(v) = std::env::var("LROOT_DISTRO_DIR") {
        PathBuf::from(v)
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".lroot").join("distros")
    };
    base
}

pub fn distro_dir(name: &str) -> PathBuf {
    data_dir().join(name)
}

pub fn find(name: &str) -> Option<&'static Distro> {
    DISTROS.iter().find(|d| d.name == name)
}
