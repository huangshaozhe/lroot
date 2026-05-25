use std::io::Read;
use std::path::PathBuf;
use std::process;

const INTERCEPT_SO: &str = "libintercept.so";

fn elf_interp(path: &str) -> Option<(u8, String)> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut hdr = [0u8; 64];
    f.read_exact(&mut hdr).ok()?;
    if hdr[0] != 0x7f || hdr[1] != b'E' || hdr[2] != b'L' || hdr[3] != b'F' {
        return None;
    }
    let class = hdr[4]; // 1=32-bit, 2=64-bit
    let e_phoff = if class == 2 {
        u64::from_le_bytes(hdr[32..40].try_into().ok()?)
    } else {
        u32::from_le_bytes(hdr[28..32].try_into().ok()?) as u64
    };
    let e_phentsize = if class == 2 {
        u16::from_le_bytes(hdr[54..56].try_into().ok()?)
    } else {
        u16::from_le_bytes(hdr[42..44].try_into().ok()?)
    } as u64;
    let e_phnum = if class == 2 {
        u16::from_le_bytes(hdr[56..58].try_into().ok()?)
    } else {
        u16::from_le_bytes(hdr[44..46].try_into().ok()?)
    };

    // Read program headers
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(e_phoff)).ok()?;
    for _ in 0..e_phnum {
        let mut phdr = vec![0u8; e_phentsize as usize];
        f.read_exact(&mut phdr).ok()?;
        let p_type = u32::from_le_bytes(phdr[0..4].try_into().ok()?);
        if p_type == 3 {
            // PT_INTERP
            let p_offset = if class == 2 {
                u64::from_le_bytes(phdr[8..16].try_into().ok()?)
            } else {
                u32::from_le_bytes(phdr[4..8].try_into().ok()?) as u64
            };
            let p_filesz = if class == 2 {
                u64::from_le_bytes(phdr[32..40].try_into().ok()?)
            } else {
                u32::from_le_bytes(phdr[16..20].try_into().ok()?) as u64
            };
            let mut interp = vec![0u8; p_filesz as usize];
            f.seek(std::io::SeekFrom::Start(p_offset)).ok()?;
            f.read_exact(&mut interp).ok()?;
            let s = String::from_utf8_lossy(&interp).trim_matches('\0').to_string();
            return Some((class, s));
        }
    }
    Some((class, String::new()))
}

fn variant_name(class: u8, interp: &str) -> &'static str {
    let bits = if class == 1 { "32" } else { "64" };
    let libc_kind = if interp.contains("musl") { "musl" } else { "glibc" };
    match (bits, libc_kind) {
        ("64", "glibc") => "64-glibc",
        ("32", "glibc") => "32-glibc",
        ("64", "musl") => "64-musl",
        ("32", "musl") => "32-musl",
        _ => "64-glibc",
    }
}

fn find_intercept(variant: &str) -> Option<PathBuf> {
    let name = format!("libintercept-{variant}.so");
    let search = |n: &str| -> Option<PathBuf> {
        let prefix = std::env::var("PREFIX").ok();
        let mut paths = vec![
            format!("/usr/lib/{n}"),
            format!("/usr/local/lib/{n}"),
        ];
        if let Some(ref p) = prefix {
            paths.push(format!("{p}/lib/{n}"));
        }
        for p in &paths {
            let pb = PathBuf::from(p);
            if pb.exists() {
                return Some(pb);
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            let sibling = exe.parent()?.join(n);
            if sibling.exists() {
                return Some(sibling);
            }
            if let Some(parent) = exe.parent()?.parent() {
                let alt = parent.join(n);
                if alt.exists() {
                    return Some(alt);
                }
            }
        }
        None
    };
    // Try exact variant first
    if let Some(pb) = search(&name) {
        return Some(pb);
    }
    // Fall back to generic name
    search(INTERCEPT_SO)
}

fn print_help() {
    eprintln!(
        "\
Usage: lroot [option...] -r rootfs [command...]
       lroot [option...] rootfs [command...]

Run a command inside a rootfs with path translation via LD_PRELOAD.
Faster than proot (2-4x) since it hooks libc functions instead of
ptrace syscall interception.

Options:
  -r, --rootfs <path>     Path to the root filesystem
  -0, --root-id           Make uid/gid appear as 0 (fake root)
  -w, --pwd <dir>         Set initial working directory inside rootfs
  -b, --bind <src:dest>   Mount --bind, map host path into rootfs
  -q, --qemu <binary>     QEMU user-mode binary for cross-architecture
  -k, --kernel-release <ver>  Fake kernel version (uname -r)
  -v, --verbose           Enable debug logging from the intercept library
  -i, --intercept <so>    Path to libintercept.so (auto-detected otherwise)
  -h, --help              Show this help

Path translation:
  All file operations (open, stat, readlink, execve, etc.) have their
  paths prefixed with the rootfs path. /proc, /sys, /dev paths pass
  through to the host unchanged. Symlink targets are also translated.

Fake root (-0):
  Hook getuid/geteuid/getgid/getegid to return 0.
  Also fakes Uid:/Gid: lines in /proc/self/status.
  Requires a /etc/passwd with root entry (supplied via -r rootfs).

Bind mount (-b):
  Format: host_path:guest_path
  The guest path is inside the rootfs. Reads/writes to the guest path
  will be redirected to the host path.

QEMU (-q):
  When enabled, any ELF binary with a different architecture than the
  host is automatically run through the specified QEMU user-mode binary.
  The QEMU binary itself must exist on the host (not inside rootfs).

Multi-architecture:
  lroot auto-detects the target binary's ELF class (32/64-bit) and
  libc (glibc/musl) from its program interpreter. It then selects the
  matching libintercept-{{arch}}-{{libc}}.so, falling back to libintercept.so.
  Use -i to override.

Examples:
  lroot -r ~/rootfs /bin/sh
  lroot ~/rootfs ls -la
  lroot -r ~/rootfs -0 /bin/sh
  lroot -r ~/rootfs -b /home:/mnt/host /bin/sh
  lroot -r ~/rootfs -q qemu-aarch64 /bin/sh
  lroot -r ~/rootfs -k 5.10.0 /bin/sh"
    );
}

struct Args {
    rootfs: String,
    command: Vec<String>,
    qemu: Option<String>,
    fake_root: bool,
    pwd: Option<String>,
    binds: Vec<(String, String)>,
    kernel_release: Option<String>,
    verbose: bool,
    intercept: Option<String>,
}

fn parse_args() -> Args {
    let mut argv = std::env::args().skip(1);
    let mut a = Args {
        rootfs: String::new(),
        command: Vec::new(),
        qemu: None,
        fake_root: false,
        pwd: None,
        binds: Vec::new(),
        kernel_release: None,
        verbose: false,
        intercept: None,
    };

    let mut positional = false;
    while let Some(arg) = argv.next() {
        if positional || !arg.starts_with('-') {
            if a.rootfs.is_empty() {
                a.rootfs = arg;
            } else {
                a.command.push(arg);
                a.command.extend(argv);
                break;
            }
            continue;
        }
        match arg.as_str() {
            "--" => positional = true,
            "-r" | "--rootfs" => {
                a.rootfs = argv.next().unwrap_or_else(|| {
                    eprintln!("-r needs a path");
                    process::exit(1);
                })
            }
            "-q" | "--qemu" => {
                a.qemu = Some(argv.next().unwrap_or_else(|| {
                    eprintln!("-q needs a path");
                    process::exit(1);
                }))
            }
            "-k" | "--kernel-release" => {
                a.kernel_release = Some(argv.next().unwrap_or_else(|| {
                    eprintln!("-k needs a version");
                    process::exit(1);
                }))
            }
            "-i" | "--intercept" => {
                a.intercept = Some(argv.next().unwrap_or_else(|| {
                    eprintln!("-i needs a path");
                    process::exit(1);
                }))
            }
            "-0" | "--root-id" => a.fake_root = true,
            "-w" | "--pwd" => {
                a.pwd = Some(argv.next().unwrap_or_else(|| {
                    eprintln!("-w needs a path");
                    process::exit(1);
                }))
            }
            "-b" | "--bind" => {
                let b = argv.next().unwrap_or_else(|| {
                    eprintln!("-b needs src:dest");
                    process::exit(1);
                });
                if let Some(i) = b.find(':') {
                    a.binds.push((b[..i].into(), b[i + 1..].into()));
                } else {
                    a.binds.push((b.clone(), b));
                }
            }
            "-v" | "--verbose" => a.verbose = true,
            "-h" | "--help" => {
                print_help();
                process::exit(0);
            }
            _ => {
                eprintln!("unknown option: {arg}");
                process::exit(1);
            }
        }
    }

    if a.rootfs.is_empty() {
        eprintln!("lroot: no rootfs specified");
        eprintln!("Try 'lroot --help' for more info.");
        process::exit(1);
    }

    if a.command.is_empty() {
        a.command.push("/bin/sh".to_string());
    }

    a
}

static CHILD_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn forward_signal(sig: libc::c_int) {
    let pid = CHILD_PID.load(std::sync::atomic::Ordering::Relaxed);
    if pid > 0 {
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

fn install_signal_handlers() {
    unsafe {
        for &sig in &[
            libc::SIGINT,
            libc::SIGTERM,
            libc::SIGHUP,
            libc::SIGQUIT,
            libc::SIGPIPE,
        ] {
            libc::signal(sig, forward_signal as *const () as libc::sighandler_t);
        }
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
    }
}

fn main() {
    install_signal_handlers();

    let args = parse_args();
    let rootfs = &args.rootfs;
    let cmd = &args.command;

    let prog = if cmd[0].starts_with('/') {
        format!("{}{}", rootfs, cmd[0])
    } else {
        cmd[0].clone()
    };

    // Follow rootfs-relative symlinks (e.g. Alpine's busybox symlinks).
    // Absolute symlinks inside the rootfs can't be resolved on the host;
    // prepend rootfs to find the real file.
    fn resolve_rootfs(prog: &str, rootfs: &str) -> String {
        let mut path = prog.to_string();
        for _ in 0..16 {
            match std::fs::read_link(&path) {
                Ok(target) if target.is_absolute() => {
                    path = format!("{rootfs}{}", target.display());
                }
                Ok(target) => {
                    // Relative symlink: resolve relative to parent
                    let parent = std::path::Path::new(&path).parent().unwrap();
                    path = parent.join(target).to_string_lossy().to_string();
                }
                _ => break,
            }
        }
        path
    }
    let real_prog = resolve_rootfs(&prog, rootfs);

    let preload = if let Some(path) = &args.intercept {
        let pb = PathBuf::from(path);
        if !pb.exists() {
            eprintln!("intercept library not found: {path}");
            process::exit(1);
        }
        pb
    } else {
        let variant = elf_interp(&real_prog)
            .map(|(class, interp)| variant_name(class, &interp))
            .unwrap_or("64-glibc");

        if args.verbose {
            eprintln!("[lroot] target binary: {prog}");
            eprintln!("[lroot] selected variant: {variant}");
        }

        find_intercept(variant).unwrap_or_else(|| {
            eprintln!("{INTERCEPT_SO} not found for variant {variant}");
            eprintln!("Build: cargo build -p intercept --release");
            eprintln!("Then: cp target/release/{INTERCEPT_SO} /usr/local/lib/libintercept-{variant}.so");
            process::exit(1);
        })
    };

    // If the binary's ELF interpreter doesn't exist on the host but does
    // inside the rootfs, run through the rootfs interpreter. This lets us
    // run musl binaries (Alpine, etc.) on a glibc host and vice versa.
    let rootfs_interp = elf_interp(&real_prog)
        .and_then(|(_class, interp)| {
            if interp.is_empty() || PathBuf::from(&interp).exists() {
                return None;
            }
            let ri = format!("{rootfs}{interp}");
            if PathBuf::from(&ri).exists() { Some(ri) } else { None }
        });
    if let Some(ref ri) = rootfs_interp {
        if args.verbose {
            eprintln!("[lroot] using rootfs interpreter: {ri}");
        }
    }
    let mut child = if let Some(ref interp_path) = rootfs_interp {
        let mut c = process::Command::new(interp_path);
        c.arg(&real_prog);
        // If the resolved binary differs from the requested one (e.g. busybox
        // symlink → busybox), inject the original basename as the applet name
        // so busybox can dispatch correctly.
        let orig_base = std::path::Path::new(&prog).file_name().map(|s| s.to_string_lossy().to_string());
        let need_applet = real_prog != prog;
        if need_applet {
            if let Some(ref name) = orig_base {
                c.arg(name.as_str());
            }
        }
        c.args(&cmd[1..]);
        c
    } else {
        let mut c = process::Command::new(&prog);
        c.args(&cmd[1..]);
        c
    };
    let rootfs_libs = format!(
        "{rootfs}/lib:{rootfs}/lib64:{rootfs}/usr/lib:{rootfs}/usr/lib64:\
         {rootfs}/lib/x86_64-linux-gnu:{rootfs}/lib/aarch64-linux-gnu:\
         {rootfs}/lib/arm-linux-gnueabihf:\
         {rootfs}/usr/lib/x86_64-linux-gnu:{rootfs}/usr/lib/aarch64-linux-gnu:\
         {rootfs}/usr/lib/arm-linux-gnueabihf"
    );
    let mut lib_path = rootfs_libs;
    if let Ok(host_ld) = std::env::var("LD_LIBRARY_PATH") {
        lib_path.push(':');
        lib_path.push_str(&host_ld);
    }
    child
        .env(
            "PATH",
            format!("{rootfs}/usr/bin:{rootfs}/bin:{rootfs}/sbin:/system/bin"),
        )
        .env("LD_LIBRARY_PATH", &lib_path)
        .env("TERM", "xterm-256color")
        .env("LITTLE_ROOTFS", rootfs)
        .env("LD_PRELOAD", preload.to_str().unwrap());

    if args.fake_root {
        child.env("HOME", format!("{rootfs}/root"));
        child.env("LITTLE_ROOT_ID", "1");
    }

    if !args.binds.is_empty() {
        let s: Vec<String> = args
            .binds
            .iter()
            .map(|(src, dst)| format!("{src}:{dst}"))
            .collect();
        child.env("LITTLE_BINDS", s.join(";"));
    }

    if let Some(k) = &args.kernel_release {
        child.env("LITTLE_KERNEL_RELEASE", k);
    }

    if let Some(qemu) = &args.qemu {
        child.env("LITTLE_QEMU", qemu);
    }

    if args.verbose {
        child.env("LITTLE_VERBOSE", "1");
    }

    if let Some(pwd) = &args.pwd {
        let p = pwd.strip_prefix('/').unwrap_or(pwd);
        child.current_dir(format!("{rootfs}/{p}"));
    }

    let mut child_process = match child.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("spawn error: {e}");
            process::exit(1);
        }
    };

    CHILD_PID.store(
        child_process.id() as i32,
        std::sync::atomic::Ordering::Relaxed,
    );

    let status = child_process.wait().unwrap_or_else(|e| {
        eprintln!("wait error: {e}");
        process::exit(1);
    });

    process::exit(status.code().unwrap_or(1));
}
