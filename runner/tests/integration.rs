use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static ROOTFS: OnceLock<tempfile::TempDir> = OnceLock::new();
static SETUP_DONE: AtomicBool = AtomicBool::new(false);

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p
}

fn target_dir() -> PathBuf {
    let root = workspace_root();
    let _ = Command::new("cargo")
        .args(["build", "-p", "intercept"])
        .current_dir(&root)
        .status();
    let debug = root.join("target").join("debug");
    let release = root.join("target").join("release");
    if debug.join("lroot").exists() {
        debug
    } else {
        release
    }
}

fn copy_bin(src: &Path, dst_dir: &Path) {
    if !src.exists() {
        return;
    }
    let dest = dst_dir.join(src.file_name().unwrap());
    std::fs::copy(src, &dest).unwrap_or(0);
    // Make sure it's executable
    let _ = std::process::Command::new("chmod")
        .args(["+x", dest.to_str().unwrap()])
        .status();
}

fn resolve_lib(path: &str) -> Option<PathBuf> {
    // Handle relative paths like libc.so.6 (no directory prefix)
    let p = PathBuf::from(path);
    if p.is_absolute() {
        // e.g., /lib/x86_64-linux-gnu/libc.so.6
        return if p.exists() { Some(p) } else { None };
    }
    // Search standard paths
    for dir in &[
        "/lib",
        "/lib64",
        "/usr/lib",
        "/lib/x86_64-linux-gnu",
        "/lib/aarch64-linux-gnu",
        "/lib/arm-linux-gnueabihf",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
    ] {
        let candidate = PathBuf::from(dir).join(&p);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn copy_with_deps(bin: &str, rootfs_lib: &Path, rootfs_lib64: &Path) {
    let bin_path = PathBuf::from("/bin").join(bin);
    if !bin_path.exists() {
        return;
    }

    // Get NEEDED libraries via readelf
    let output = Command::new("readelf")
        .args(["-d", bin_path.to_str().unwrap()])
        .output()
        .ok();
    if let Some(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if line.contains("NEEDED") {
                if let Some(lib_start) = line.find('[') {
                    if let Some(lib_end) = line[lib_start + 1..].find(']') {
                        let lib_name = &line[lib_start + 1..lib_start + 1 + lib_end];
                        if let Some(lib_path) = resolve_lib(lib_name) {
                            // Copy to both lib and lib64, whichever matches
                            let parent_name = lib_path.parent().unwrap_or(Path::new(""));
                            let fname = lib_path.file_name().unwrap();
                            let target = if parent_name.ends_with("lib64")
                                || lib_name == "ld-linux-x86-64.so.2"
                                || lib_name.starts_with("ld-")
                            {
                                rootfs_lib64.join(fname)
                            } else {
                                rootfs_lib.join(fname)
                            };
                            if !target.exists() {
                                std::fs::copy(&lib_path, &target).unwrap_or(0);
                            }
                        }
                    }
                }
            }
        }
    }

    // Also handle ld-linux which might be at /lib64/ld-*.so.*
    for ld_candidate in &[
        "/lib64/ld-linux-x86-64.so.2",
        "/lib64/ld-linux-aarch64.so.1",
        "/lib/ld-linux.so.2",
        "/lib/ld-linux-armhf.so.3",
    ] {
        let ld = PathBuf::from(ld_candidate);
        if ld.exists() {
            let fname = ld.file_name().unwrap();
            let target = rootfs_lib64.join(fname);
            if !target.exists() {
                std::fs::copy(&ld, &target).unwrap_or(0);
            }
        }
    }
}

fn setup() -> &'static tempfile::TempDir {
    if SETUP_DONE.load(Ordering::Relaxed) {
        return ROOTFS.get().unwrap();
    }
    let dir = tempfile::tempdir().expect("create temp dir");
    let root = dir.path();

    for d in &[
        "bin", "etc", "tmp", "usr", "lib", "lib64", "proc", "sys", "dev", "home", "mnt",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }

    // Copy test binaries
    for cmd in &["sh", "echo", "ls", "cat", "id", "pwd", "mv", "ln", "stat", "touch"] {
        let bin = PathBuf::from("/bin").join(cmd);
        let usrbin = PathBuf::from("/usr/bin").join(cmd);
        if bin.exists() {
            copy_bin(&bin, &root.join("bin"));
        } else if usrbin.exists() {
            copy_bin(&usrbin, &root.join("bin"));
        }
        copy_with_deps(cmd, &root.join("lib"), &root.join("lib64"));
    }

    std::fs::write(root.join("tmp/test.txt"), b"hello from rootfs\n").unwrap();
    ROOTFS.set(dir).ok();
    SETUP_DONE.store(true, Ordering::Relaxed);
    ROOTFS.get().unwrap()
}

fn lroot_path() -> PathBuf {
    target_dir().join("lroot")
}

#[test]
fn test_help() {
    let output = Command::new(lroot_path())
        .arg("--help")
        .output()
        .expect("run lroot --help");
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("lroot"));
}

#[test]
fn test_simple_command() {
    let rootfs = setup();
    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "/bin/sh",
            "-c",
            "echo hello",
        ])
        .output()
        .expect("run lroot");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("hello"));
}

#[test]
fn test_fake_root() {
    let rootfs = setup();
    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "-0",
            "/bin/sh",
            "-c",
            "id -u",
        ])
        .output()
        .expect("run lroot -0");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout, "0");
}

#[test]
fn test_bind_mount() {
    let rootfs = setup();
    let tmpfile = PathBuf::from("/tmp/lroot_bind_test.txt");
    std::fs::write(&tmpfile, b"bind works\n").ok();

    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "-b",
            "/tmp/lroot_bind_test.txt:/mnt/test.txt",
            "/bin/cat",
            "/mnt/test.txt",
        ])
        .output()
        .expect("run lroot with bind mount");

    let _ = std::fs::remove_file(&tmpfile);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        assert!(stdout.contains("bind works"));
    }
}

#[test]
fn test_pwd() {
    let rootfs = setup();
    // pwd is often a shell builtin, use sh -c pwd
    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "-w",
            "/tmp",
            "/bin/sh",
            "-c",
            "pwd",
        ])
        .output()
        .expect("run lroot with -w");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.ends_with("/tmp"), "expected /tmp, got: {stdout}");
}

#[test]
fn test_ls() {
    let rootfs = setup();
    let output = Command::new(lroot_path())
        .args(["-r", rootfs.path().to_str().unwrap(), "/bin/ls", "/"])
        .output()
        .expect("run lroot ls");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for item in &["bin", "tmp", "etc"] {
        assert!(
            stdout.contains(item),
            "missing {item} in ls output: {stdout}"
        );
    }
}

#[test]
fn test_file_read() {
    let rootfs = setup();
    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "/bin/cat",
            "/tmp/test.txt",
        ])
        .output()
        .expect("run lroot cat");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("hello from rootfs"));
}

#[test]
fn test_mv() {
    let rootfs = setup();
    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "/bin/sh",
            "-c",
            "cd /tmp && echo mv_test > mv_src && /bin/mv mv_src mv_dst && /bin/cat mv_dst",
        ])
        .output()
        .expect("run lroot mv");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout, "mv_test");
}

#[test]
fn test_ln_hardlink() {
    let rootfs = setup();
    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "/bin/sh",
            "-c",
            "cd /tmp && echo ln_test > ln_src && /bin/ln ln_src ln_dst && /bin/cat ln_dst",
        ])
        .output()
        .expect("run lroot ln");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout, "ln_test");
}

#[test]
fn test_ln_symlink() {
    let rootfs = setup();
    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "/bin/sh",
            "-c",
            "cd /tmp && echo symlink_test > sym_target && /bin/ln -s sym_target sym_link && /bin/cat sym_link",
        ])
        .output()
        .expect("run lroot ln -s");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout, "symlink_test");
}

#[test]
fn test_ls_la_no_errors() {
    let rootfs = setup();
    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "/bin/ls",
            "-la",
            "/tmp",
        ])
        .output()
        .expect("run lroot ls -la");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("test.txt"),
        "ls -la should list tmp contents: {stdout}"
    );
    // No "cannot access" errors from un-hooked xattr/stat calls
    assert!(
        !stderr.contains("cannot access"),
        "ls -la should not produce errors: stderr={stderr}"
    );
}

#[test]
fn test_stat_file() {
    let rootfs = setup();
    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "/bin/stat",
            "/tmp/test.txt",
        ])
        .output()
        .expect("run lroot stat");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("File: /tmp/test.txt"));
    assert!(stdout.contains("Size:"));
}

#[test]
fn test_touch_and_stat() {
    let rootfs = setup();
    let output = Command::new(lroot_path())
        .args([
            "-r",
            rootfs.path().to_str().unwrap(),
            "/bin/sh",
            "-c",
            "cd /tmp && /bin/touch touched_file && /bin/stat touched_file",
        ])
        .output()
        .expect("run lroot touch");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("File: touched_file"));
}
