use std::process::Command;

fn lroot_distro() -> Command {
    // cargo test sets CARGO_BIN_EXE_lroot-distro
    let exe = std::env!("CARGO_BIN_EXE_lroot-distro");
    Command::new(exe)
}

#[test]
fn help() {
    let out = lroot_distro().arg("--help").output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Usage: lroot-distro <command>"));
    assert!(stderr.contains("install"));
    assert!(stderr.contains("list"));
    assert!(stderr.contains("run"));
    assert!(stderr.contains("login"));
    assert!(stderr.contains("remove"));
    assert!(stderr.contains("alpine"));
    assert!(out.status.success());
}

#[test]
fn help_flag() {
    let out = lroot_distro().arg("-h").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("Usage:"));
    assert!(out.status.success());
}

#[test]
fn no_args_shows_help() {
    let out = lroot_distro().output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("Usage:"));
    assert!(!out.status.success());
}

#[test]
fn install_unknown_distro() {
    let out = lroot_distro()
        .args(["install", "nonexistent-distro-xyz"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown distro"));
    assert!(!out.status.success());
}

#[test]
fn install_missing_arg() {
    let out = lroot_distro().arg("install").output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn remove_unknown_distro() {
    let out = lroot_distro()
        .args(["remove", "nonexistent-distro-xyz"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not installed"));
    assert!(!out.status.success());
}

#[test]
fn run_unknown_distro() {
    let out = lroot_distro()
        .args(["run", "nonexistent-distro-xyz"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not installed"));
    assert!(!out.status.success());
}

#[test]
fn login_unknown_distro() {
    let out = lroot_distro()
        .args(["login", "nonexistent-distro-xyz"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not installed"));
    assert!(!out.status.success());
}

#[test]
fn list_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(std::env!("CARGO_BIN_EXE_lroot-distro"))
        .arg("list")
        .env("LROOT_DISTRO_DIR", tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), "");
    assert!(out.status.success());
}

#[test]
fn list_with_env_dir() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("mydistro")).unwrap();
    let out = Command::new(std::env!("CARGO_BIN_EXE_lroot-distro"))
        .arg("list")
        .env("LROOT_DISTRO_DIR", tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("mydistro"));
    assert!(out.status.success());
}

#[test]
fn install_requires_name() {
    let out = lroot_distro().arg("install").output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn run_with_unrelated_distro_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("testroot");
    std::fs::create_dir_all(d.join("bin")).unwrap();
    let out = Command::new(std::env!("CARGO_BIN_EXE_lroot-distro"))
        .args(["run", "testroot"])
        .env("LROOT_DISTRO_DIR", tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("lroot not found"), "should have found lroot");
    assert!(!out.status.success());
}

#[test]
fn login_requires_name() {
    let out = lroot_distro().arg("login").output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn remove_requires_name() {
    let out = lroot_distro().arg("remove").output().unwrap();
    assert!(!out.status.success());
}
