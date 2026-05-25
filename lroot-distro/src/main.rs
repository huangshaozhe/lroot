mod distro;

use distro::{data_dir, distro_dir, find, DISTROS};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process;

fn print_help() {
    eprintln!(
        "\
Usage: lroot-distro <command> [options...]

Commands:
  install <distro>     Download and extract a distribution rootfs
  list                 List installed distributions
  run <distro> [cmd]   Run a command inside the distribution
  login <distro>       Start an interactive shell inside the distribution
  remove <distro>      Delete an installed distribution

Known distros: {}",
        DISTROS.iter().map(|d| d.name).collect::<Vec<_>>().join(", ")
    );
}

fn cmd_install(name: &str) {
    let dir = distro_dir(name);
    if dir.exists() {
        if dir.read_dir().ok().map_or(false, |mut it| it.next().is_some()) {
            eprintln!("already installed: {name} ({})", dir.display());
            process::exit(1);
        }
    }

    let distro = find(name).unwrap_or_else(|| {
        eprintln!("unknown distro: {name}");
        eprintln!("known: alpine, alpine-amd64, ubuntu, debian, arch");
        process::exit(1);
    });

    std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
        eprintln!("failed to create {dir:?}: {e}");
        process::exit(1);
    });

    eprintln!("downloading {}...", distro.tarball_url);
    let data = download(distro.tarball_url);

    eprintln!("extracting...");
    extract_tarball(&data, &dir);
    eprintln!("installed {name} at {}", dir.display());
}

fn cmd_list() {
    let dir = data_dir();
    if !dir.exists() {
        return;
    }
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in &entries {
        let name = entry.file_name();
        let size = dir_size(&entry.path());
        if size > 1024 * 1024 * 1024 {
            println!("{}  {:.1}G", name.to_string_lossy(), size as f64 / (1024.0 * 1024.0 * 1024.0));
        } else if size > 1024 * 1024 {
            println!("{}  {:.1}M", name.to_string_lossy(), size as f64 / (1024.0 * 1024.0));
        } else {
            println!("{}  {}K", name.to_string_lossy(), size / 1024);
        }
    }
}

fn cmd_run(name: &str, cmd: &[String]) {
    let dir = distro_dir(name);
    if !dir.exists() {
        eprintln!("distribution not installed: {name}");
        eprintln!("install with: lroot-distro install {name}");
        process::exit(1);
    }

    let lroot = find_lroot();
    let rootfs = dir.to_str().unwrap();

    let mut child = process::Command::new(&lroot);
    child.arg("-r").arg(rootfs);

    if Path::new("/sdcard").exists() {
        child.arg("-b").arg("/sdcard:/sdcard");
    }

    if cmd.is_empty() {
        child.arg("/bin/sh");
    } else {
        child.args(cmd);
    }

    let status = child.status().unwrap_or_else(|e| {
        eprintln!("failed to run lroot: {e}");
        process::exit(1);
    });
    process::exit(status.code().unwrap_or(1));
}

fn cmd_remove(name: &str) {
    let dir = distro_dir(name);
    if !dir.exists() {
        eprintln!("not installed: {name}");
        process::exit(1);
    }
    eprintln!("removing {}...", dir.display());
    std::fs::remove_dir_all(&dir).unwrap_or_else(|e| {
        eprintln!("failed to remove: {e}");
        process::exit(1);
    });
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_help();
        process::exit(1);
    }

    match args[1].as_str() {
        "install" if args.len() >= 3 => cmd_install(&args[2]),
        "list" => cmd_list(),
        "run" if args.len() >= 3 => cmd_run(&args[2], &args[3..]),
        "login" if args.len() >= 3 => cmd_run(&args[2], &[]),
        "remove" if args.len() >= 3 => cmd_remove(&args[2]),
        "-h" | "--help" | "help" => print_help(),
        _ => {
            eprintln!("lroot-distro: unknown command or missing argument");
            eprintln!("Try 'lroot-distro --help' for more info.");
            process::exit(1);
        }
    }
}

fn download(url: &str) -> Vec<u8> {
    let resp = reqwest::blocking::get(url).unwrap_or_else(|e| {
        eprintln!("download failed: {e}");
        process::exit(1);
    });
    let status = resp.status();
    if !status.is_success() {
        eprintln!("download returned {status}");
        process::exit(1);
    }
    resp.bytes()
        .unwrap_or_else(|e| {
            eprintln!("read response failed: {e}");
            process::exit(1);
        })
        .to_vec()
}

fn extract_tarball(data: &[u8], dest: &Path) {
    let reader: Box<dyn Read> = if data.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(data))
    } else if data.starts_with(&[0xfd, b'7', b'z', b'X', b'Z']) {
        eprintln!("xz-compressed tarballs not supported (need liblzma)");
        process::exit(1);
    } else {
        Box::new(data)
    };

    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.unpack(dest).unwrap_or_else(|e| {
        eprintln!("extract failed: {e}");
        process::exit(1);
    });
}

fn find_lroot() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.parent().unwrap().join("lroot");
        if sibling.exists() {
            return sibling;
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = PathBuf::from(dir).join("lroot");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    eprintln!("lroot not found (install lroot first, or place it in PATH)");
    process::exit(1);
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                total += dir_size(&entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}
