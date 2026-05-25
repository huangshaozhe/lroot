use std::cell::UnsafeCell;
use std::ffi::{c_char, CStr};
use std::io::Write;

use std::sync::OnceLock;

// musl's libc crate doesn't expose statx, define it ourselves
#[cfg(target_env = "musl")]
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct statx_timespec {
    tv_sec: i64,
    tv_nsec: u32,
    __reserved: i32,
}
#[cfg(target_env = "musl")]
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct statx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    __spare0: [u16; 1],
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: statx_timespec,
    stx_btime: statx_timespec,
    stx_ctime: statx_timespec,
    stx_mtime: statx_timespec,
    __spare4: [u64; 4],
    __spare5: [u32; 14],
    __spare6: u32,
}

macro_rules! dprintln {
    ($($arg:tt)*) => {
        if crate::is_verbose() {
            let _ = write!(&mut std::io::stderr(), "[lroot] ");
            let _ = writeln!(&mut std::io::stderr(), $($arg)*);
        }
    };
}

static ROOTFS_BYTES: OnceLock<Vec<u8>> = OnceLock::new();
static QEMU_PATH: OnceLock<Vec<u8>> = OnceLock::new();
static ROOT_ID: OnceLock<bool> = OnceLock::new();
static BINDS: OnceLock<Vec<(Vec<u8>, Vec<u8>)>> = OnceLock::new();

fn is_verbose() -> bool {
    unsafe {
        let p = libc::getenv(b"LITTLE_VERBOSE\0".as_ptr() as *const c_char);
        !p.is_null()
    }
}

fn binds() -> &'static [(Vec<u8>, Vec<u8>)] {
    BINDS.get_or_init(|| {
        unsafe {
            let p = libc::getenv(b"LITTLE_BINDS\0".as_ptr() as *const c_char);
            if !p.is_null() {
                let s = std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                    p as *const u8,
                    libc::strlen(p),
                ));
                return s
                    .split(';')
                    .filter_map(|pair| {
                        let mut it = pair.splitn(2, ':');
                        let src = it.next()?;
                        let dst = it.next()?;
                        Some((src.as_bytes().to_vec(), dst.as_bytes().to_vec()))
                    })
                    .collect();
            }
        }
        vec![]
    })
}

fn is_root_id() -> bool {
    *ROOT_ID.get_or_init(|| unsafe {
        let v = libc::getenv(c"LITTLE_ROOT_ID".as_ptr());
        !v.is_null()
    })
}

// newfstatat on x86_64, fstatat64 on arm/x86 (i686), fstatat=79 on aarch64
#[cfg(all(target_arch = "aarch64", not(target_os = "android")))]
const SYS_FSTATAT: libc::c_long = libc::SYS_newfstatat as libc::c_long;
// Android aarch64 libc doesn't export SYS_newfstatat
#[cfg(all(target_arch = "aarch64", target_os = "android"))]
const SYS_FSTATAT: libc::c_long = 79;
#[cfg(any(target_arch = "arm", target_arch = "x86"))]
const SYS_FSTATAT: libc::c_long = libc::SYS_fstatat64 as libc::c_long;
#[cfg(not(any(target_arch = "aarch64", target_arch = "arm", target_arch = "x86")))]
const SYS_FSTATAT: libc::c_long = libc::SYS_newfstatat as libc::c_long;

#[inline(always)]
fn rootfs_bytes() -> &'static [u8] {
    ROOTFS_BYTES
        .get_or_init(|| {
            unsafe {
                let p = libc::getenv(b"LITTLE_ROOTFS\0".as_ptr() as *const c_char);
                if !p.is_null() {
                    let len = libc::strlen(p);
                    let slice = std::slice::from_raw_parts(p as *const u8, len);
                    let s = std::str::from_utf8_unchecked(slice).trim_end_matches('/');
                    return s.as_bytes().to_vec();
                }
            }
            vec![]
        })
        .as_slice()
}

struct PathBuf {
    data: UnsafeCell<[u8; 4096]>,
}

unsafe impl Send for PathBuf {}
unsafe impl Sync for PathBuf {}

std::thread_local! {
    static TL_BUF: PathBuf = PathBuf { data: UnsafeCell::new([0u8; 4096]) };
}

#[inline(always)]
fn cstr_bytes<'a>(p: *const c_char) -> &'a [u8] {
    if p.is_null() {
        return b"";
    }
    unsafe {
        let len = libc::strlen(p);
        std::slice::from_raw_parts(p as *const u8, len)
    }
}

#[inline(always)]
fn translate(path: *const c_char) -> *const c_char {
    if path.is_null() {
        return path;
    }
    let src = cstr_bytes(path);
    if src.is_empty() || src[0] != b'/' {
        return path;
    }

    let rfs = rootfs_bytes();
    let rlen = rfs.len();
    if src.len() >= rlen && src[..rlen] == *rfs {
        return path;
    }
    if src.starts_with(b"/proc") || src.starts_with(b"/sys") || src.starts_with(b"/dev") {
        return path;
    }

    dprintln!("translate: {:?} (rootfs={:?})", std::str::from_utf8(src).unwrap_or("?"), std::str::from_utf8(rfs).unwrap_or("?"));

    // Bind mount check: if path starts with a bind destination, replace with source
    let binds = binds();
    if !binds.is_empty() {
        for (host_src, guest_dst) in binds.iter() {
            let dl = guest_dst.len();
            if src.len() < dl || src[..dl] != guest_dst[..] {
                continue;
            }
            let suffix = &src[dl..];
            if !suffix.is_empty() && suffix[0] != b'/' {
                continue;
            }
            let total = host_src.len() + suffix.len();
            return TL_BUF.with(|buf| {
                let dst = unsafe { &mut *buf.data.get() };
                if total >= dst.len() {
                    return path;
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        host_src.as_ptr(),
                        dst.as_mut_ptr(),
                        host_src.len(),
                    );
                    if !suffix.is_empty() {
                        std::ptr::copy_nonoverlapping(
                            suffix.as_ptr(),
                            dst.as_mut_ptr().add(host_src.len()),
                            suffix.len(),
                        );
                    }
                    dst[total] = 0;
                }
                dst.as_ptr() as *const c_char
            });
        }
    }

    // Prepend rootfs to absolute path
    if rlen == 0 {
        return path;
    }
    let total = rlen + src.len();
    return TL_BUF.with(|buf| {
        let dst = unsafe { &mut *buf.data.get() };
        if total >= dst.len() {
            return path;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(rfs.as_ptr(), dst.as_mut_ptr(), rlen);
            std::ptr::copy_nonoverlapping(b"/\0".as_ptr(), dst.as_mut_ptr().add(rlen), 2);
            let rest = &src[1..];
            let rest_len = rest.len().min(dst.len() - rlen - 1);
            std::ptr::copy_nonoverlapping(rest.as_ptr(), dst.as_mut_ptr().add(rlen + 1), rest_len);
            dst[rlen + 1 + rest_len] = 0;
        }
        dst.as_ptr() as *const c_char
    })
}

#[inline(always)]
fn match_non_abs(path: *const c_char) -> *const c_char {
    if path.is_null() {
        return path;
    }
    let bytes = cstr_bytes(path);
    if bytes.first() == Some(&b'/') {
        translate(path)
    } else {
        path
    }
}

/// Translate `path`, then save a copy into `buf` so that a later `translate()`
/// call cannot clobber the result (via round-robin buffer).  Returns the
/// stack-buffer pointer if the path fits, otherwise the original pointer.
unsafe fn save_path(path: *const c_char, buf: &mut [u8]) -> *const c_char {
    if path.is_null() {
        return std::ptr::null();
    }
    let t = translate(path);
    if t.is_null() {
        return std::ptr::null();
    }
    let len = libc::strlen(t);
    if len < buf.len() {
        std::ptr::copy_nonoverlapping(t as *const u8, buf.as_mut_ptr(), len + 1);
        buf.as_ptr() as *const c_char
    } else {
        t // fallback – rare for paths >4K
    }
}

// ── ELF architecture detection ──────────────────────────────────────────────

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

const EM_AARCH64: u16 = 183;
const EM_X86_64: u16 = 62;
const EM_ARM: u16 = 40;
const EM_386: u16 = 3;

fn host_arch() -> u16 {
    if cfg!(target_arch = "aarch64") {
        EM_AARCH64
    } else if cfg!(target_arch = "x86_64") {
        EM_X86_64
    } else if cfg!(target_arch = "arm") {
        EM_ARM
    } else if cfg!(target_arch = "x86") {
        EM_386
    } else {
        0
    }
}

fn qemu_name(machine: u16) -> Option<&'static [u8]> {
    Some(match machine {
        EM_AARCH64 => b"qemu-aarch64",
        EM_X86_64 => b"qemu-x86_64",
        EM_ARM => b"qemu-arm",
        EM_386 => b"qemu-i386",
        _ => return None,
    })
}

fn find_qemu(name: &[u8]) -> Option<*const c_char> {
    // 1. Check LITTLE_QEMU env var (explicit path from user)
    let saved = QEMU_PATH.get()?;
    if !saved.is_empty() {
        return Some(saved.as_ptr() as *const c_char);
    }

    // 2. Search PATH
    let path_var = unsafe {
        let p = libc::getenv(b"PATH\0".as_ptr() as *const c_char);
        if p.is_null() {
            return None;
        }
        std::slice::from_raw_parts(p as *const u8, libc::strlen(p))
    };

    TL_BUF.with(|buf| {
        let dst = unsafe { &mut *buf.data.get() };
        for dir in path_var.split(|&b| b == b':') {
            if dir.is_empty() {
                continue;
            }
            let total = dir.len() + 1 + name.len();
            if total + 1 > dst.len() {
                continue;
            }

            unsafe {
                std::ptr::copy_nonoverlapping(dir.as_ptr(), dst.as_mut_ptr(), dir.len());
                dst[dir.len()] = b'/';
                std::ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    dst.as_mut_ptr().add(dir.len() + 1),
                    name.len(),
                );
                dst[total] = 0;

                let fd = libc::syscall(
                    libc::SYS_openat,
                    libc::AT_FDCWD,
                    dst.as_ptr() as *const c_char,
                    libc::O_RDONLY | libc::O_CLOEXEC,
                    0,
                );
                if fd >= 0 {
                    libc::syscall(libc::SYS_close, fd);
                    return Some(dst.as_ptr() as *const c_char);
                }
            }
        }
        None
    })
}

fn build_qemu_argv(
    qemu: *const c_char,
    translated: *const c_char,
    orig_argv: *const *const c_char,
) -> Vec<*const c_char> {
    let mut argv = Vec::new();
    argv.push(qemu);
    argv.push(translated);
    if !orig_argv.is_null() {
        let mut i = 1;
        unsafe {
            while !(*orig_argv.add(i)).is_null() {
                argv.push(*orig_argv.add(i));
                i += 1;
            }
        }
    }
    argv.push(std::ptr::null());
    argv
}

#[cfg(not(target_os = "android"))]
macro_rules! bionic_compat_fn {
    (fn $name:ident($($arg:ident: $argty:ty),*) -> $ret:ty $body:block) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name($($arg: $argty),*) -> $ret $body
    };
}

#[cfg(target_os = "android")]
macro_rules! bionic_compat_fn {
    (fn $name:ident($($arg:ident: $argty:ty),*) -> $ret:ty $body:block) => {};
}

// ── ELF detection (single-pass read_elf_info) ─────────────────────────

struct ElfInfo {
    machine: u16,
    is_static: bool,
    interp: Vec<u8>,
}

fn read_elf_info(path: *const c_char) -> Option<ElfInfo> {
    unsafe {
        let fd = libc::syscall(libc::SYS_openat, libc::AT_FDCWD, path, libc::O_RDONLY, 0);
        if fd < 0 {
            return None;
        }

        let mut hdr = [0u8; 64];
        let n = libc::syscall(
            libc::SYS_pread64,
            fd,
            hdr.as_mut_ptr() as *mut libc::c_void,
            64i64,
            0i64,
        );
        if n < 52 || hdr[..4] != ELF_MAGIC {
            libc::syscall(libc::SYS_close, fd);
            return None;
        }

        let machine = u16::from_ne_bytes([hdr[18], hdr[19]]);
        let is_64 = hdr[4] == 2;
        if hdr[4] != 1 && hdr[4] != 2 {
            libc::syscall(libc::SYS_close, fd);
            return None;
        }

        let (e_phoff, e_phentsize, e_phnum) = if is_64 {
            let off = u64::from_le_bytes(hdr[32..40].try_into().unwrap());
            let size = u16::from_le_bytes(hdr[54..56].try_into().unwrap());
            let num = u16::from_le_bytes(hdr[56..58].try_into().unwrap());
            (off, size, num)
        } else {
            let off = u32::from_le_bytes(hdr[28..32].try_into().unwrap()) as u64;
            let size = u16::from_le_bytes(hdr[42..44].try_into().unwrap());
            let num = u16::from_le_bytes(hdr[44..46].try_into().unwrap());
            (off, size, num)
        };

        let min_entsize = if is_64 { 56 } else { 32 };
        if e_phnum == 0 || e_phentsize < min_entsize {
            libc::syscall(libc::SYS_close, fd);
            return Some(ElfInfo {
                machine,
                is_static: true,
                interp: vec![],
            });
        }

        let phdr_size = e_phentsize as usize * e_phnum as usize;
        let mut phdrs = vec![0u8; phdr_size];
        libc::syscall(
            libc::SYS_pread64,
            fd,
            phdrs.as_mut_ptr() as *mut libc::c_void,
            phdrs.len() as i64,
            e_phoff as i64,
        );

        let mut is_static = true;
        let mut interp = vec![];

        for i in 0..e_phnum as usize {
            let off = i * e_phentsize as usize;
            let p_type = u32::from_le_bytes(phdrs[off..off + 4].try_into().unwrap());
            if p_type == 3 {
                is_static = false;
                let (p_offset, p_filesz) = if is_64 {
                    let po = u64::from_ne_bytes(phdrs[off + 8..off + 16].try_into().unwrap());
                    let ps = u64::from_ne_bytes(phdrs[off + 32..off + 40].try_into().unwrap());
                    (po, ps)
                } else {
                    let po = u32::from_le_bytes(phdrs[off + 4..off + 8].try_into().unwrap()) as u64;
                    let ps =
                        u32::from_le_bytes(phdrs[off + 16..off + 20].try_into().unwrap()) as u64;
                    (po, ps)
                };
                if p_filesz > 0 && p_filesz < 4096 {
                    let mut ibuf = vec![0u8; p_filesz as usize];
                    libc::syscall(
                        libc::SYS_pread64,
                        fd,
                        ibuf.as_mut_ptr() as *mut libc::c_void,
                        p_filesz as i64,
                        p_offset as i64,
                    );
                    while ibuf.last() == Some(&0) {
                        ibuf.pop();
                    }
                    interp = ibuf;
                }
                break;
            }
        }

        libc::syscall(libc::SYS_close, fd);
        Some(ElfInfo {
            machine,
            is_static,
            interp,
        })
    }
}

fn try_rootfs_interp(
    tpath: *const c_char,
    interp: &[u8],
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> Option<libc::c_int> {
    let rfs = rootfs_bytes();
    if rfs.is_empty() || interp.is_empty() {
        return None;
    }

    let mut full = Vec::with_capacity(rfs.len() + interp.len() + 1);
    full.extend_from_slice(rfs);
    full.extend_from_slice(interp);
    full.push(0);

    let interp_fd = unsafe {
        libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            full.as_ptr() as *const c_char,
            libc::O_RDONLY,
            0,
        )
    };
    if interp_fd < 0 {
        return None;
    }
    unsafe {
        libc::syscall(libc::SYS_close, interp_fd);
    }

    let mut new_argv = Vec::new();
    new_argv.push(full.as_ptr() as *const c_char);   // [0] = ld-musl
    new_argv.push(tpath);                              // [1] = target binary path
    if !argv.is_null() {
        let mut i = 1;  // skip argv[0] (applet identifier, already consumed by busybox dispatch)
        unsafe {
            while !(*argv.add(i)).is_null() {
                new_argv.push(*argv.add(i));           // [2..] = original argv[1..]
                i += 1;
            }
        }
    }
    new_argv.push(std::ptr::null());

    let ret = raw_execve(full.as_ptr() as *const c_char, new_argv.as_ptr(), envp);
    if ret < 0 {
        return None;
    }
    Some(ret)
}

// ── Proc/sys faking helpers ──────────────────────────────────────────────────

fn make_fake_fd(content: &[u8]) -> i32 {
    unsafe {
        let fd = libc::syscall(
            libc::SYS_memfd_create,
            "__lroot_fake\0".as_ptr() as *const libc::c_void,
            0i64,
        ) as i32;
        if fd >= 0 {
            libc::syscall(
                libc::SYS_write,
                fd as i64,
                content.as_ptr() as *const libc::c_void,
                content.len() as i64,
            );
            libc::syscall(libc::SYS_lseek, fd as i64, 0i64, libc::SEEK_SET as i64);
            return fd;
        }
        // Fallback: O_TMPFILE (available since Linux 3.11)
        let fd = libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            b"/tmp\0".as_ptr() as *const c_char,
            (libc::O_RDWR | libc::O_TMPFILE | libc::O_CLOEXEC) as i64,
            0o600,
        ) as i32;
        if fd >= 0 {
            libc::syscall(
                libc::SYS_write,
                fd as i64,
                content.as_ptr() as *const libc::c_void,
                content.len() as i64,
            );
            libc::syscall(libc::SYS_lseek, fd as i64, 0i64, libc::SEEK_SET as i64);
            return fd;
        }
        fd
    }
}

unsafe fn read_cstring(buf: &[u8]) -> &[u8] {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    &buf[..len]
}

fn fake_proc_version() -> i32 {
    unsafe {
        let mut utsname: libc::utsname = std::mem::zeroed();
        libc::syscall(libc::SYS_uname, &mut utsname as *mut _ as i64);

        let release = {
            let k = libc::getenv(b"LITTLE_KERNEL_RELEASE\0".as_ptr() as *const c_char);
            if !k.is_null() {
                let len = libc::strlen(k);
                std::slice::from_raw_parts(k as *const u8, len)
            } else {
                read_cstring(unsafe {
                    &*(&utsname.release as *const [c_char; 65] as *const [u8; 65])
                })
            }
        };

        let rel_str = std::str::from_utf8(release).unwrap_or("0.0.0");
        let rel_clean = rel_str
            .split(|c: char| c == ' ' || c == '\t')
            .next()
            .unwrap_or(rel_str);
        let content = format!(
            "Linux version {} (lroot) #1 SMP Tue Jan 1 00:00:00 UTC 2019\n",
            rel_clean
        );
        make_fake_fd(content.as_bytes())
    }
}

fn fake_proc_maps() -> i32 {
    unsafe {
        let fd = libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            b"/proc/self/maps\0".as_ptr() as *const c_char,
            libc::O_RDONLY,
            0,
        );
        if fd < 0 {
            return fd as i32;
        }

        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = libc::syscall(
                libc::SYS_read,
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as i64,
            );
            if n <= 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n as usize]);
        }
        libc::syscall(libc::SYS_close, fd);

        let rfs = rootfs_bytes();
        if rfs.is_empty() {
            return make_fake_fd(&raw);
        }

        let mut out = Vec::with_capacity(raw.len());
        let mut line_start = 0;
        while line_start < raw.len() {
            let line_end = raw[line_start..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| line_start + p + 1)
                .unwrap_or(raw.len());
            let line = &raw[line_start..line_end];
            line_start = line_end;

            // Find pathname: skip 5 whitespace-delimited fields, then look for /
            let mut fields_seen = 0;
            let mut in_ws = true;
            let mut path_idx = None;
            for (i, &b) in line.iter().enumerate() {
                if b == b' ' || b == b'\t' {
                    if !in_ws {
                        fields_seen += 1;
                    }
                    in_ws = true;
                } else if in_ws && fields_seen >= 5 && b == b'/' {
                    path_idx = Some(i);
                    break;
                } else {
                    in_ws = false;
                }
            }

            if let Some(pi) = path_idx {
                let before = &line[..pi];
                let path_part = &line[pi..];
                let path_end = path_part
                    .iter()
                    .position(|&b| b == b' ' || b == b'\t' || b == b'\n')
                    .unwrap_or(path_part.len());
                let trunk_path = &path_part[..path_end];
                let suffix = &path_part[path_end..];

                let translated = if trunk_path.starts_with(rfs)
                    && (trunk_path.len() == rfs.len() || trunk_path[rfs.len()] == b'/')
                {
                    let rest = &trunk_path[rfs.len()..];
                    if rest.is_empty() {
                        &b"/"[..]
                    } else {
                        rest
                    }
                } else {
                    trunk_path
                };

                out.extend_from_slice(before);
                out.extend_from_slice(translated);
                out.extend_from_slice(suffix);
            } else {
                out.extend_from_slice(line);
            }
        }

        make_fake_fd(&out)
    }
}

fn fake_proc_status() -> i32 {
    unsafe {
        let fd = libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            b"/proc/self/status\0".as_ptr() as *const c_char,
            libc::O_RDONLY,
            0,
        );
        if fd < 0 {
            return fd as i32;
        }

        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = libc::syscall(
                libc::SYS_read,
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as i64,
            );
            if n <= 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n as usize]);
        }
        libc::syscall(libc::SYS_close, fd);

        if !is_root_id() {
            return make_fake_fd(&raw);
        }

        let mut out = Vec::with_capacity(raw.len());
        let mut line_start = 0;
        while line_start < raw.len() {
            let line_end = raw[line_start..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| line_start + p + 1)
                .unwrap_or(raw.len());
            let line = &raw[line_start..line_end];
            line_start = line_end;

            if line.starts_with(b"Uid:") {
                out.extend_from_slice(b"Uid:\t0\t0\t0\t0\n");
            } else if line.starts_with(b"Gid:") {
                out.extend_from_slice(b"Gid:\t0\t0\t0\t0\n");
            } else {
                out.extend_from_slice(line);
            }
        }
        make_fake_fd(&out)
    }
}

fn fake_proc_cmdline() -> i32 {
    unsafe {
        let fd = libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            b"/proc/self/cmdline\0".as_ptr() as *const c_char,
            libc::O_RDONLY,
            0,
        );
        if fd < 0 {
            return fd as i32;
        }

        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = libc::syscall(
                libc::SYS_read,
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as i64,
            );
            if n <= 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n as usize]);
        }
        libc::syscall(libc::SYS_close, fd);

        let rfs = rootfs_bytes();
        if rfs.is_empty() {
            return make_fake_fd(&raw);
        }

        let mut out = Vec::with_capacity(raw.len());
        let mut pos = 0;
        while pos < raw.len() {
            let end = raw[pos..]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(raw.len() - pos);
            let arg = &raw[pos..pos + end];
            if !arg.is_empty()
                && arg[0] == b'/'
                && arg.starts_with(rfs)
                && (arg.len() == rfs.len() || arg[rfs.len()] == b'/')
            {
                let rest = &arg[rfs.len()..];
                if rest.is_empty() {
                    out.extend_from_slice(b"/");
                } else {
                    out.extend_from_slice(rest);
                }
            } else {
                out.extend_from_slice(arg);
            }
            out.push(0);
            pos += end + 1;
        }
        make_fake_fd(&out)
    }
}

fn fake_proc_mountinfo() -> i32 {
    unsafe {
        let fd = libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            b"/proc/self/mountinfo\0".as_ptr() as *const c_char,
            libc::O_RDONLY,
            0,
        );
        if fd < 0 {
            return fd as i32;
        }

        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = libc::syscall(
                libc::SYS_read,
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as i64,
            );
            if n <= 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n as usize]);
        }
        libc::syscall(libc::SYS_close, fd);

        let rfs = rootfs_bytes();
        if rfs.is_empty() {
            return make_fake_fd(&raw);
        }

        let mut out = Vec::with_capacity(raw.len());
        let mut i = 0;
        while i < raw.len() {
            let at_field_start =
                i == 0 || raw[i - 1] == b'\n' || raw[i - 1] == b' ' || raw[i - 1] == b'\t';
            if at_field_start && raw[i..].starts_with(rfs) {
                let after = i + rfs.len();
                if after < raw.len()
                    && raw[after] != b'/'
                    && raw[after] != b' '
                    && raw[after] != b'\t'
                    && raw[after] != b'\n'
                {
                    out.push(raw[i]);
                    i += 1;
                    continue;
                }
                // Strip rootfs prefix from mount point
                if after >= raw.len()
                    || raw[after] == b' '
                    || raw[after] == b'\t'
                    || raw[after] == b'\n'
                {
                    out.push(b'/');
                }
                i = after;
            } else {
                out.push(raw[i]);
                i += 1;
            }
        }
        make_fake_fd(&out)
    }
}

fn fake_proc_id_map() -> Option<i32> {
    if !is_root_id() {
        return None;
    }
    Some(make_fake_fd(b"         0          0 4294967295\n"))
}

fn try_proc_fake_open(bytes: &[u8]) -> Option<i32> {
    if bytes == b"/proc/version" {
        Some(fake_proc_version())
    } else if bytes == b"/proc/sys/kernel/version" {
        Some(fake_proc_version())
    } else if bytes == b"/proc/sys/kernel/osrelease" {
        Some(fake_proc_version())
    } else if bytes == b"/proc/self/maps" {
        Some(fake_proc_maps())
    } else if bytes == b"/proc/self/status" {
        Some(fake_proc_status())
    } else if bytes == b"/proc/self/mountinfo" {
        Some(fake_proc_mountinfo())
    } else if bytes == b"/proc/self/mounts" {
        Some(fake_proc_mountinfo())
    } else if bytes == b"/proc/self/cmdline" {
        Some(fake_proc_cmdline())
    } else if bytes == b"/proc/self/uid_map" {
        fake_proc_id_map()
    } else if bytes == b"/proc/self/gid_map" {
        fake_proc_id_map()
    } else if bytes == b"/proc/self/setgroups" {
        Some(make_fake_fd(b"allow\n"))
    } else if bytes == b"/proc/filesystems" {
        Some(make_fake_fd(
            b"nodev\tsysfs\nnodev\tproc\nnodev\tdevtmpfs\nnodev\tdevpts\n\
              nodev\ttmpfs\nnodev\tsecurityfs\nnodev\tcgroup2\n\text4\n\tbtrfs\n\txfs\n\tvfat\n"
        ))
    } else if bytes == b"/proc/self/attr/current" {
        // Minimal SELinux fake so tools don't error
        Some(make_fake_fd(b"kernel=unconfined\n"))
    } else {
        None
    }
}

// ── Hooked functions ─────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn open(
    path: *const c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    let bytes = cstr_bytes(path);
    if let Some(fd) = try_proc_fake_open(bytes) {
        return fd;
    }
    libc::syscall(
        libc::SYS_openat,
        libc::AT_FDCWD,
        translate(path),
        flags as i64,
        mode as i64,
    ) as libc::c_int
}

bionic_compat_fn! {
    fn open64(path: *const c_char, flags: libc::c_int, mode: libc::mode_t) -> libc::c_int {
        libc::syscall(libc::SYS_openat, libc::AT_FDCWD, translate(path), flags as i64, mode as i64) as libc::c_int
    }
}

unsafe fn fopen_flags(mode: *const c_char) -> (i32, i32) {
    let bytes = std::ffi::CStr::from_ptr(mode).to_bytes();
    if bytes.is_empty() {
        return (libc::O_RDONLY, 0);
    }
    match bytes[0] {
        b'r' => {
            if bytes.get(1) == Some(&b'+') {
                (libc::O_RDWR, 0)
            } else {
                (libc::O_RDONLY, 0)
            }
        }
        b'w' => {
            if bytes.get(1) == Some(&b'+') {
                (libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC, 0o644)
            } else {
                (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644)
            }
        }
        b'a' => {
            if bytes.get(1) == Some(&b'+') {
                (libc::O_RDWR | libc::O_CREAT | libc::O_APPEND, 0o644)
            } else {
                (libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND, 0o644)
            }
        }
        _ => (libc::O_RDONLY, 0),
    }
}

#[no_mangle]
pub extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> *mut libc::FILE {
    unsafe {
        let bytes = cstr_bytes(path);
        if let Some(fd) = try_proc_fake_open(bytes) {
            return libc::fdopen(fd, mode);
        }
        let (flags, mode_bits) = fopen_flags(mode);
        let fd = libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            translate(path),
            flags as i64,
            mode_bits as i64,
        ) as libc::c_int;
        if fd < 0 {
            return std::ptr::null_mut();
        }
        libc::fdopen(fd, mode)
    }
}

bionic_compat_fn! {
    fn fopen64(path: *const c_char, mode: *const c_char) -> *mut libc::FILE {
        fopen(path, mode)
    }
}

#[no_mangle]
pub extern "C" fn freopen(
    path: *const c_char,
    mode: *const c_char,
    stream: *mut libc::FILE,
) -> *mut libc::FILE {
    unsafe {
        let old_fd = libc::fileno(stream);
        if old_fd < 0 {
            return std::ptr::null_mut();
        }
        let (flags, mode_bits) = fopen_flags(mode);
        let fd = libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            translate(path),
            flags as i64,
            mode_bits as i64,
        ) as libc::c_int;
        if fd < 0 {
            return std::ptr::null_mut();
        }
        libc::syscall(libc::SYS_dup3, fd as i64, old_fd as i64, 0);
        libc::syscall(libc::SYS_close, fd as i64);
        // Clear stream error/EOF flags by seeking to current position
        libc::syscall(libc::SYS_lseek, old_fd as i64, 0i64, libc::SEEK_CUR as i64);
        // fdopen to create a new FILE* wrapping the same fd
        // Actually dup the fd first so fdopen owns it
        let new_fd = libc::syscall(
            libc::SYS_fcntl,
            old_fd as i64,
            libc::F_DUPFD_CLOEXEC as i64,
            0i64,
        ) as libc::c_int;
        if new_fd < 0 {
            return std::ptr::null_mut();
        }
        libc::fdopen(new_fd, mode)
    }
}

#[no_mangle]
pub extern "C" fn stat(path: *const c_char, buf: *mut libc::stat) -> libc::c_int {
    unsafe { libc::syscall(SYS_FSTATAT, libc::AT_FDCWD as i64, translate(path), buf as *mut libc::c_void, 0i64) as libc::c_int }
}

bionic_compat_fn! {
    fn stat64(path: *const c_char, buf: *mut libc::stat64) -> libc::c_int {
        unsafe { libc::syscall(SYS_FSTATAT, libc::AT_FDCWD as i64, translate(path), buf as *mut libc::c_void, 0i64) as libc::c_int }
    }
}

bionic_compat_fn! {
    fn lstat64(path: *const c_char, buf: *mut libc::stat64) -> libc::c_int {
        unsafe { libc::syscall(SYS_FSTATAT, libc::AT_FDCWD as i64, translate(path), buf as *mut libc::c_void, libc::AT_SYMLINK_NOFOLLOW as i64) as libc::c_int }
    }
}

#[no_mangle]
pub extern "C" fn lstat(path: *const c_char, buf: *mut libc::stat) -> libc::c_int {
    unsafe { libc::syscall(SYS_FSTATAT, libc::AT_FDCWD as i64, translate(path), buf as *mut libc::c_void, libc::AT_SYMLINK_NOFOLLOW as i64) as libc::c_int }
}

#[cfg(not(target_env = "musl"))]
type Statx = libc::statx;
#[cfg(target_env = "musl")]
type Statx = statx;

#[no_mangle]
pub unsafe extern "C" fn statx(
    dirfd: libc::c_int,
    path: *const c_char,
    flags: libc::c_int,
    mask: libc::c_int,
    buf: *mut Statx,
) -> libc::c_int {
    let tpath = match_non_abs(path);
    libc::syscall(
        libc::SYS_statx,
        dirfd as i64,
        tpath,
        flags as i64,
        mask as i64,
        buf as *mut libc::c_void,
    ) as libc::c_int
}

bionic_compat_fn! {
    fn __xstat(_ver: libc::c_int, path: *const c_char, buf: *mut libc::stat) -> libc::c_int {
        unsafe { libc::syscall(SYS_FSTATAT, libc::AT_FDCWD as i64, translate(path), buf as *mut libc::c_void, 0i64) as libc::c_int }
    }
}

bionic_compat_fn! {
    fn __lxstat(_ver: libc::c_int, path: *const c_char, buf: *mut libc::stat) -> libc::c_int {
        unsafe { libc::syscall(SYS_FSTATAT, libc::AT_FDCWD as i64, translate(path), buf as *mut libc::c_void, libc::AT_SYMLINK_NOFOLLOW as i64) as libc::c_int }
    }
}

#[no_mangle]
pub extern "C" fn access(path: *const c_char, mode: libc::c_int) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_faccessat,
            libc::AT_FDCWD,
            translate(path),
            mode as i64,
            0i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn faccessat(
    dirfd: libc::c_int,
    path: *const c_char,
    mode: libc::c_int,
    flags: libc::c_int,
) -> libc::c_int {
    let p = match_non_abs(path);
    unsafe {
        libc::syscall(
            libc::SYS_faccessat,
            dirfd as i64,
            p,
            mode as i64,
            flags as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn listxattr(path: *const c_char, list: *mut c_char, size: libc::size_t) -> libc::ssize_t {
    unsafe { libc::syscall(libc::SYS_listxattr, translate(path), list, size) as libc::ssize_t }
}

#[no_mangle]
pub extern "C" fn llistxattr(path: *const c_char, list: *mut c_char, size: libc::size_t) -> libc::ssize_t {
    unsafe { libc::syscall(libc::SYS_llistxattr, translate(path), list, size) as libc::ssize_t }
}

#[no_mangle]
pub extern "C" fn getxattr(path: *const c_char, name: *const c_char, value: *mut c_char, size: libc::size_t) -> libc::ssize_t {
    unsafe { libc::syscall(libc::SYS_getxattr, translate(path), name, value, size) as libc::ssize_t }
}

#[no_mangle]
pub extern "C" fn lgetxattr(path: *const c_char, name: *const c_char, value: *mut c_char, size: libc::size_t) -> libc::ssize_t {
    unsafe { libc::syscall(libc::SYS_lgetxattr, translate(path), name, value, size) as libc::ssize_t }
}

#[no_mangle]
pub extern "C" fn mkdir(path: *const c_char, mode: libc::mode_t) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_mkdirat,
            libc::AT_FDCWD,
            translate(path),
            mode as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn rmdir(path: *const c_char) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_unlinkat,
            libc::AT_FDCWD,
            translate(path),
            libc::AT_REMOVEDIR as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn unlink(path: *const c_char) -> libc::c_int {
    unsafe {
        libc::syscall(libc::SYS_unlinkat, libc::AT_FDCWD, translate(path), 0i64) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn rename(old: *const c_char, new: *const c_char) -> libc::c_int {
    let mut old_buf = [0u8; 4096];
    // save old before translate(new) clobbers TL_BUF
    unsafe {
        let old_t = save_path(old, &mut old_buf);
        let new_t = translate(new);
        libc::syscall(
            libc::SYS_renameat,
            libc::AT_FDCWD,
            old_t,
            libc::AT_FDCWD,
            new_t,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn symlink(tgt: *const c_char, link: *const c_char) -> libc::c_int {
    unsafe {
        let link_t = translate(link);
        libc::syscall(
            libc::SYS_symlinkat,
            tgt, // symlink target is stored as raw bytes – do NOT translate
            libc::AT_FDCWD,
            link_t,
        ) as libc::c_int
    }
}

fn do_readlink(path: *const c_char, buf: *mut c_char, size: libc::size_t) -> libc::ssize_t {
    unsafe {
        let mut tmp = [0i8; 4096];
        let n = libc::syscall(
            libc::SYS_readlinkat,
            libc::AT_FDCWD,
            translate(path),
            tmp.as_mut_ptr() as *mut libc::c_void,
            tmp.len() as i64,
        ) as libc::ssize_t;
        if n <= 0 {
            return n;
        }
        let rfs = rootfs_bytes();
        let rfs_len = rfs.len();
        let raw = std::slice::from_raw_parts(tmp.as_ptr() as *const u8, n as usize);
        let stripped = if rfs_len > 0 && raw.starts_with(rfs) {
            if raw.len() == rfs_len {
                &b"/"[..]
            } else {
                &raw[rfs_len..]
            }
        } else {
            raw
        };
        let copy = stripped.len().min(size as usize);
        std::ptr::copy_nonoverlapping(stripped.as_ptr(), buf as *mut u8, copy);
        if copy < size as usize {
            buf.add(copy).write(0);
        }
        copy as libc::ssize_t
    }
}

#[no_mangle]
pub extern "C" fn readlink(
    path: *const c_char,
    buf: *mut c_char,
    size: libc::size_t,
) -> libc::ssize_t {
    let bytes = cstr_bytes(path);
    if bytes == b"/proc/self/root" {
        if size >= 1 {
            unsafe {
                *buf = b'/' as libc::c_char;
            }
        }
        if size >= 2 {
            unsafe {
                buf.add(1).write(0);
            }
        }
        return 1;
    }
    do_readlink(path, buf, size)
}

#[no_mangle]
pub extern "C" fn __readlink_chk(
    path: *const c_char,
    buf: *mut c_char,
    size: libc::size_t,
    _ret_size: libc::size_t,
) -> libc::ssize_t {
    do_readlink(path, buf, size)
}

#[no_mangle]
pub extern "C" fn chdir(path: *const c_char) -> libc::c_int {
    unsafe { libc::syscall(libc::SYS_chdir, translate(path)) as libc::c_int }
}

#[no_mangle]
pub extern "C" fn opendir(path: *const c_char) -> *mut libc::DIR {
    unsafe {
        let fd = libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            translate(path),
            (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) as i64,
            0,
        ) as libc::c_int;
        if fd < 0 {
            return std::ptr::null_mut();
        }
        libc::fdopendir(fd)
    }
}

#[no_mangle]
pub extern "C" fn chmod(path: *const c_char, mode: libc::mode_t) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_fchmodat,
            libc::AT_FDCWD,
            translate(path),
            mode as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn chown(
    path: *const c_char,
    owner: libc::uid_t,
    group: libc::gid_t,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_fchownat,
            libc::AT_FDCWD,
            translate(path),
            owner as i64,
            group as i64,
            0i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn utimes(path: *const c_char, times: *const libc::timeval) -> libc::c_int {
    unsafe {
        if times.is_null() {
            libc::syscall(
                libc::SYS_utimensat,
                libc::AT_FDCWD,
                translate(path),
                std::ptr::null::<libc::timespec>(),
                0i64,
            ) as libc::c_int
        } else {
            let tv = *times;
            let ts = libc::timespec {
                tv_sec: tv.tv_sec,
                tv_nsec: (tv.tv_usec * 1000) as libc::c_long,
            };
            libc::syscall(
                libc::SYS_utimensat,
                libc::AT_FDCWD,
                translate(path),
                &ts as *const libc::timespec,
                0i64,
            ) as libc::c_int
        }
    }
}

#[no_mangle]
pub extern "C" fn utimensat(
    dirfd: libc::c_int,
    path: *const c_char,
    times: *const libc::timespec,
    flags: libc::c_int,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_utimensat,
            dirfd as i64,
            translate(path),
            times as *const libc::c_void,
            flags as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub unsafe extern "C" fn openat(
    dirfd: libc::c_int,
    path: *const c_char,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    let bytes = cstr_bytes(path);
    if !bytes.is_empty() && bytes[0] == b'/' {
        if let Some(fd) = try_proc_fake_open(bytes) {
            return fd;
        }
    } else if dirfd == libc::AT_FDCWD {
        if let Some(fd) = try_proc_fake_open(bytes) {
            return fd;
        }
    }
    libc::syscall(
        libc::SYS_openat,
        dirfd as i64,
        match_non_abs(path),
        flags as i64,
        mode as i64,
    ) as libc::c_int
}

#[no_mangle]
pub unsafe extern "C" fn fstatat(
    dirfd: libc::c_int,
    path: *const c_char,
    buf: *mut libc::stat,
    flags: libc::c_int,
) -> libc::c_int {
    libc::syscall(
        SYS_FSTATAT,
        dirfd as i64,
        match_non_abs(path),
        buf as *mut libc::c_void,
        flags as i64,
    ) as libc::c_int
}

#[no_mangle]
pub extern "C" fn readlinkat(
    dirfd: libc::c_int,
    path: *const c_char,
    buf: *mut c_char,
    bufsiz: libc::size_t,
) -> libc::ssize_t {
    let p = if dirfd == libc::AT_FDCWD {
        translate(path)
    } else {
        path
    };
    if dirfd == libc::AT_FDCWD {
        let bytes = cstr_bytes(path);
        if bytes == b"/proc/self/exe" || bytes == b"/proc/self/root" {
            return do_readlink(path, buf, bufsiz);
        }
    }
    unsafe {
        let mut tmp = [0i8; 4096];
        let n = libc::syscall(
            libc::SYS_readlinkat,
            dirfd as i64,
            p,
            tmp.as_mut_ptr() as *mut libc::c_void,
            tmp.len() as i64,
        ) as libc::ssize_t;
        if n <= 0 {
            return n;
        }
        let rfs = rootfs_bytes();
        let rfs_len = rfs.len();
        let raw = std::slice::from_raw_parts(tmp.as_ptr() as *const u8, n as usize);
        let stripped = if rfs_len > 0 && raw.starts_with(rfs) {
            if raw.len() == rfs_len {
                &b"/"[..]
            } else {
                &raw[rfs_len..]
            }
        } else {
            raw
        };
        let copy = stripped.len().min(bufsiz as usize);
        std::ptr::copy_nonoverlapping(stripped.as_ptr(), buf as *mut u8, copy);
        if copy < bufsiz as usize {
            buf.add(copy).write(0);
        }
        copy as libc::ssize_t
    }
}

#[no_mangle]
pub extern "C" fn mkdirat(
    dirfd: libc::c_int,
    path: *const c_char,
    mode: libc::mode_t,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_mkdirat,
            dirfd as i64,
            match_non_abs(path),
            mode as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn unlinkat(
    dirfd: libc::c_int,
    path: *const c_char,
    flags: libc::c_int,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_unlinkat,
            dirfd as i64,
            match_non_abs(path),
            flags as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn renameat(
    oldfd: libc::c_int,
    old: *const c_char,
    newfd: libc::c_int,
    new: *const c_char,
) -> libc::c_int {
    let mut old_buf = [0u8; 4096];
    unsafe {
        let old_t = save_path(old, &mut old_buf);
        let new_t = match_non_abs(new);
        libc::syscall(
            libc::SYS_renameat,
            oldfd as i64,
            old_t,
            newfd as i64,
            new_t,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn renameat2(
    oldfd: libc::c_int,
    old: *const c_char,
    newfd: libc::c_int,
    new: *const c_char,
    flags: libc::c_uint,
) -> libc::c_int {
    let mut old_buf = [0u8; 4096];
    unsafe {
        let old_t = save_path(old, &mut old_buf);
        let new_t = match_non_abs(new);
        libc::syscall(
            libc::SYS_renameat,
            oldfd as i64,
            old_t,
            newfd as i64,
            new_t,
            flags as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn linkat(
    oldfd: libc::c_int,
    old: *const c_char,
    newfd: libc::c_int,
    new: *const c_char,
    flags: libc::c_int,
) -> libc::c_int {
    let mut old_buf = [0u8; 4096];
    unsafe {
        let old_t = save_path(old, &mut old_buf);
        let new_t = match_non_abs(new);
        libc::syscall(
            libc::SYS_linkat,
            oldfd as i64,
            old_t,
            newfd as i64,
            new_t,
            flags as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn symlinkat(
    tgt: *const c_char,
    newfd: libc::c_int,
    link: *const c_char,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_symlinkat,
            tgt, // symlink target is raw bytes – do NOT translate
            newfd as i64,
            match_non_abs(link),
        ) as libc::c_int
    }
}

// ── Additional missing hooks ─────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn truncate(path: *const c_char, length: libc::off_t) -> libc::c_int {
    unsafe {
        #[cfg(target_os = "android")]
        let sysno = 45;
        #[cfg(not(target_os = "android"))]
        let sysno = libc::SYS_truncate;
        libc::syscall(sysno, translate(path), length as i64) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn link(old: *const c_char, new: *const c_char) -> libc::c_int {
    let mut old_buf = [0u8; 4096];
    unsafe {
        let old_t = save_path(old, &mut old_buf);
        let new_t = translate(new);
        libc::syscall(
            libc::SYS_linkat,
            libc::AT_FDCWD,
            old_t,
            libc::AT_FDCWD,
            new_t,
            0i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn lchown(
    path: *const c_char,
    owner: libc::uid_t,
    group: libc::gid_t,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_fchownat,
            libc::AT_FDCWD,
            translate(path),
            owner as i64,
            group as i64,
            libc::AT_SYMLINK_NOFOLLOW as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn fchmodat(
    dirfd: libc::c_int,
    path: *const c_char,
    mode: libc::mode_t,
    flags: libc::c_int,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_fchmodat,
            dirfd as i64,
            match_non_abs(path),
            mode as i64,
            flags as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn fchownat(
    dirfd: libc::c_int,
    path: *const c_char,
    owner: libc::uid_t,
    group: libc::gid_t,
    flags: libc::c_int,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_fchownat,
            dirfd as i64,
            match_non_abs(path),
            owner as i64,
            group as i64,
            flags as i64,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn statfs(path: *const c_char, buf: *mut libc::statfs) -> libc::c_int {
    unsafe {
        #[cfg(target_os = "android")]
        let sysno = 44;
        #[cfg(not(target_os = "android"))]
        let sysno = libc::SYS_statfs;
        libc::syscall(sysno, translate(path), buf as i64) as libc::c_int
    }
}

// ── capget hook ─────────────────────────────────────────────────────────────

#[repr(C)]
pub struct CapHeader {
    pub version: u32,
    pub pid: i32,
}

#[repr(C)]
pub struct CapData {
    pub effective: u32,
    pub permitted: u32,
    pub inheritable: u32,
}

const _LINUX_CAPABILITY_VERSION_1: u32 = 0x19980330;
const _LINUX_CAPABILITY_VERSION_2: u32 = 0x20071026;
const _LINUX_CAPABILITY_VERSION_3: u32 = 0x20080522;
const CAP_FULL: u32 = 0xffffffff;

#[no_mangle]
pub extern "C" fn capget(hdr: *mut CapHeader, data: *mut CapData) -> libc::c_int {
    if is_root_id() {
        unsafe {
            if !data.is_null() && !hdr.is_null() {
                let ver = (*hdr).version;
                if ver == _LINUX_CAPABILITY_VERSION_2 || ver == _LINUX_CAPABILITY_VERSION_3 {
                    let s = std::slice::from_raw_parts_mut(data, 2);
                    for d in s {
                        d.effective = CAP_FULL;
                        d.permitted = CAP_FULL;
                        d.inheritable = CAP_FULL;
                    }
                } else {
                    (*data).effective = CAP_FULL;
                    (*data).permitted = CAP_FULL;
                    (*data).inheritable = CAP_FULL;
                }
            }
            return 0;
        }
    }
    unsafe { libc::syscall(libc::SYS_capget, hdr, data) as libc::c_int }
}

// ── Mount/umount hooks ──────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn mount(
    source: *const c_char,
    target: *const c_char,
    fstype: *const c_char,
    flags: libc::c_ulong,
    data: *const libc::c_void,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_mount,
            source,
            translate(target),
            fstype,
            flags,
            data,
        ) as libc::c_int
    }
}

#[no_mangle]
pub extern "C" fn umount(target: *const c_char) -> libc::c_int {
    unsafe { libc::syscall(libc::SYS_umount2, translate(target), 0i64) as libc::c_int }
}

#[no_mangle]
pub extern "C" fn umount2(target: *const c_char, flags: libc::c_int) -> libc::c_int {
    unsafe { libc::syscall(libc::SYS_umount2, translate(target), flags as i64) as libc::c_int }
}

fn raw_execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> libc::c_int {
    unsafe { libc::syscall(libc::SYS_execve, path as i64, argv as i64, envp as i64) as libc::c_int }
}

fn raw_execveat(
    dirfd: libc::c_int,
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
    flags: libc::c_int,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            dirfd as i64,
            path as i64,
            argv as i64,
            envp as i64,
            flags as i64,
        ) as libc::c_int
    }
}

fn envp_has_ld_preload(envp: *const *const c_char) -> bool {
    if envp.is_null() { return true; }
    unsafe {
        for i in 0..4096 {
            let p = *envp.add(i);
            if p.is_null() { return false; }
            let bytes = std::slice::from_raw_parts(p as *const u8, libc::strlen(p));
            if bytes.starts_with(b"LD_PRELOAD=") {
                return true;
            }
        }
    }
    false
}

#[no_mangle]
/// Inject the basename of `orig_path` as argv[0] when the resolved binary
/// differs from the requested one (busybox-style multi-call dispatch).
/// Returns the original argv if no injection is needed.
fn inject_applet(argv: *const *const c_char, orig_path: *const c_char, tpath: *const c_char) -> *const *const c_char {
    if orig_path.is_null() || tpath.is_null() {
        return argv;
    }
    let orig_s = unsafe { CStr::from_ptr(orig_path) }.to_bytes();
    let tpath_s = unsafe { CStr::from_ptr(tpath) }.to_bytes();
    // If the resolved path ends with the same basename as original, no injection needed
    let orig_base = orig_s.rsplit(|&b| b == b'/').next().unwrap_or(orig_s);
    let tpath_base = tpath_s.rsplit(|&b| b == b'/').next().unwrap_or(tpath_s);
    if orig_base == tpath_base {
        return argv;
    }
    // Build new argv: [basename_of_orig, original_argv[0], original_argv[1..]]
    unsafe {
        let _orig_argv0 = if !argv.is_null() && !(*argv).is_null() {
            *argv
        } else {
            orig_path
        };
        // Count original args
        let mut argc = 0;
        if !argv.is_null() {
            while !(*argv.add(argc)).is_null() {
                argc += 1;
            }
        }
        // Allocate new argv on the heap (leaked — process will exec or ptrace will clean up)
        let new_argv = libc::malloc((argc + 2) as usize * std::mem::size_of::<*const c_char>()) as *mut *const c_char;
        if new_argv.is_null() {
            return argv;
        }
        // argv[0] = basename of original path (applet name)
        let applet = libc::malloc(orig_base.len() + 1);
        if applet.is_null() { return argv; }
        std::ptr::copy_nonoverlapping(orig_base.as_ptr(), applet as *mut u8, orig_base.len());
        *(applet.add(orig_base.len()) as *mut u8) = 0;
        *new_argv = applet as *const c_char;
        // argv[1..] = original argv
        for i in 0..=argc {
            *new_argv.add(i + 1) = *argv.add(i);
        }
        new_argv as *const *const c_char
    }
}

#[no_mangle]
pub extern "C" fn execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> libc::c_int {
    // Save path to stack immediately: path may point into TL_BUF (when called
    // from execvp's translate()), and later translate() calls will overwrite it.
    let mut saved_path = [0u8; 4096];
    let path_len = unsafe { libc::strlen(path) }.min(4095);
    unsafe {
        std::ptr::copy_nonoverlapping(path as *const u8, saved_path.as_mut_ptr(), path_len);
        saved_path[path_len] = 0;
    }
    let orig_path = saved_path.as_ptr() as *const c_char;

    let mut tpath = translate(path);

    // Follow busybox-style symlinks inside the rootfs.
    let mut symlink_count = 0;
    for _ in 0..16 {
        let mut buf = [0u8; 4096];
        let ret = unsafe {
            libc::syscall(
                libc::SYS_readlinkat,
                libc::AT_FDCWD as i64,
                tpath,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if ret <= 0 {
            break;
        }
        let target = &buf[..ret as usize];
        if !target.starts_with(b"/") {
            break;
        }
        tpath = translate(target.as_ptr() as *const c_char);
        symlink_count += 1;
    }

    // Always inject applet name; inject_applet returns original argv unchanged
    // when basenames match (no busybox dispatch needed).
    let argv = inject_applet(argv, orig_path, tpath);

    dprintln!("execve: {:?} -> {:?}", unsafe { CStr::from_ptr(path) }.to_str().unwrap_or("?"), unsafe { CStr::from_ptr(tpath) }.to_str().unwrap_or("?"));

    // Single ELF probe for machine, static/linked, and interpreter
    let info = read_elf_info(tpath);

    if let Some(ref info) = info {
        // QEMU cross-arch wrapping
        if info.machine != host_arch() {
            if let Some(name) = qemu_name(info.machine) {
                if let Some(qemu) = find_qemu(name) {
                    let new_argv = build_qemu_argv(qemu, tpath, argv);
                    let _ = raw_execve(qemu, new_argv.as_ptr(), envp);
                }
            }
        } else {
            // Same arch: static/setuid/clean-envp → ptrace fallback
            if has_setuid_bit(tpath) || info.is_static || !envp_has_ld_preload(envp) {
                let ret = ptrace_fallback_execve(tpath, argv, envp);
                if ret >= 0 {
                    return ret;
                }
            }

            // Dynamically linked: try rootfs ld-linux
            if !info.interp.is_empty() {
                if let Some(ret) = try_rootfs_interp(tpath, &info.interp, argv, envp) {
                    if ret >= 0 {
                        return ret;
                    }
                }
            }
        }
    }

    raw_execve(tpath, argv, envp)
}

#[no_mangle]
pub extern "C" fn execvp(path: *const c_char, argv: *const *const c_char) -> libc::c_int {
    let s = unsafe { CStr::from_ptr(path) }.to_str().unwrap_or("");
    if s.contains('/') {
        execve(translate(path), argv, std::ptr::null())
    } else {
        // Search PATH for the executable
        let path_env = unsafe {
            let p = libc::getenv(b"PATH\0".as_ptr() as *const c_char);
            if p.is_null() {
                return raw_execve(path, argv, std::ptr::null());
            }
            CStr::from_ptr(p).to_str().unwrap_or("")
        };
        for dir in path_env.split(':') {
            let mut full = Vec::with_capacity(dir.len() + 1 + s.len() + 1);
            full.extend_from_slice(dir.as_bytes());
            full.push(b'/');
            full.extend_from_slice(s.as_bytes());
            full.push(0);
            let candidate = full.as_ptr() as *const c_char;
            // Check existence and exec bit using access()
            if unsafe {
                libc::syscall(
                    libc::SYS_faccessat,
                    libc::AT_FDCWD as i64,
                    candidate,
                    libc::X_OK as i64,
                    0i64,
                ) as libc::c_int
            } == 0
            {
                return execve(candidate, argv, std::ptr::null());
            }
        }
        -1
    }
}

// ── Fake root ( -0 ) ─────────────────────────────────────────────────────────

fn real_uid(sys: libc::c_long) -> libc::uid_t {
    if is_root_id() {
        0
    } else {
        unsafe { libc::syscall(sys) as libc::uid_t }
    }
}

#[no_mangle]
pub extern "C" fn getuid() -> libc::uid_t {
    real_uid(libc::SYS_getuid)
}
#[no_mangle]
pub extern "C" fn geteuid() -> libc::uid_t {
    real_uid(libc::SYS_geteuid)
}
#[no_mangle]
pub extern "C" fn getgid() -> libc::gid_t {
    real_uid(libc::SYS_getgid)
}
#[no_mangle]
pub extern "C" fn getegid() -> libc::gid_t {
    real_uid(libc::SYS_getegid)
}

// ── uname (fake machine for cross-arch) ──────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn uname(buf: *mut libc::utsname) -> libc::c_int {
    let ret = libc::syscall(libc::SYS_uname, buf as i64) as libc::c_int;
    if ret == 0 {
        let r = buf.as_mut().unwrap_unchecked();
        // Override machine (for QEMU cross-arch)
        let m = libc::getenv(b"LITTLE_UNAME\0".as_ptr() as *const c_char);
        if !m.is_null() {
            let len = libc::strlen(m);
            let copy = len.min(64);
            libc::strncpy(r.machine.as_mut_ptr(), m, copy);
            r.machine[copy] = 0;
        }
        // Override release (-k option)
        let k = libc::getenv(b"LITTLE_KERNEL_RELEASE\0".as_ptr() as *const c_char);
        if !k.is_null() {
            let len = libc::strlen(k);
            let copy = len.min(64);
            libc::strncpy(r.release.as_mut_ptr(), k, copy);
            r.release[copy] = 0;
        }
    }
    ret
}

// ── execveat (modern exec variant) ────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn execveat(
    dirfd: libc::c_int,
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
    flags: libc::c_int,
) -> libc::c_int {
    if path.is_null() {
        return raw_execveat(dirfd, path, argv, envp, flags);
    }
    let bytes = cstr_bytes(path);
    if bytes.is_empty() || bytes[0] != b'/' {
        return raw_execveat(dirfd, path, argv, envp, flags);
    }

    // Save path to stack before TL_BUF overwrites it
    let mut saved_path = [0u8; 4096];
    let path_len = unsafe { libc::strlen(path) }.min(4095);
    unsafe {
        std::ptr::copy_nonoverlapping(path as *const u8, saved_path.as_mut_ptr(), path_len);
        saved_path[path_len] = 0;
    }
    let orig_path = saved_path.as_ptr() as *const c_char;

    let mut tpath = translate(path);

    // Same symlink resolution as execve()
    for _ in 0..16 {
        let mut buf = [0u8; 4096];
        let ret = unsafe {
            libc::syscall(
                libc::SYS_readlinkat,
                libc::AT_FDCWD as i64,
                tpath,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if ret <= 0 {
            break;
        }
        let target = &buf[..ret as usize];
        if !target.starts_with(b"/") {
            break;
        }
        tpath = translate(target.as_ptr() as *const c_char);
    }

    let argv = inject_applet(argv, orig_path, tpath);

    dprintln!("execveat: {:?} -> {:?}", unsafe { CStr::from_ptr(path) }.to_str().unwrap_or("?"), unsafe { CStr::from_ptr(tpath) }.to_str().unwrap_or("?"));
    let info = read_elf_info(tpath);

    if let Some(ref info) = info {
        if info.machine != host_arch() {
            if let Some(name) = qemu_name(info.machine) {
                if let Some(qemu) = find_qemu(name) {
                    let new_argv = build_qemu_argv(qemu, tpath, argv);
                    let _ = raw_execveat(dirfd, qemu, new_argv.as_ptr(), envp, flags);
                }
            }
        } else {
            if has_setuid_bit(tpath) || info.is_static || !envp_has_ld_preload(envp) {
                let ret = ptrace_fallback_execve(tpath, argv, envp);
                if ret >= 0 {
                    return ret;
                }
            }
        }
    }

    raw_execveat(dirfd, tpath, argv, envp, flags)
}

// ── Static binary & setuid detection ────────────────────────────────────

fn has_setuid_bit(path: *const c_char) -> bool {
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::syscall(
            SYS_FSTATAT,
            libc::AT_FDCWD,
            path,
            &mut st as *mut libc::stat,
            0,
        ) == 0
        {
            (st.st_mode & libc::S_ISUID) != 0
        } else {
            false
        }
    }
}

// ── Ptrace fallback for static/setuid binaries (aarch64) ────────────────

const PTRACE_O_TRACESYSGOOD: libc::c_int = 0x00000001;
const PTRACE_O_EXITKILL: libc::c_int = 0x00100000;
const NT_PRSTATUS: libc::c_int = 1;

#[cfg(target_arch = "aarch64")]
fn ptrace_syscall_no(regs: &libc::user_regs_struct) -> i64 {
    regs.regs[8] as i64
}

#[cfg(target_arch = "aarch64")]
fn ptrace_arg(regs: &libc::user_regs_struct, n: usize) -> u64 {
    if n < 31 {
        regs.regs[n]
    } else {
        0
    }
}

#[cfg(target_arch = "aarch64")]
fn ptrace_set_arg(regs: &mut libc::user_regs_struct, n: usize, val: u64) {
    if n < 31 {
        regs.regs[n] = val;
    }
}

#[cfg(target_arch = "aarch64")]
fn ptrace_sp(regs: &libc::user_regs_struct) -> u64 {
    regs.sp
}

#[cfg(target_arch = "x86_64")]
fn ptrace_syscall_no(regs: &libc::user_regs_struct) -> i64 {
    regs.orig_rax as i64
}

#[cfg(target_arch = "x86_64")]
fn ptrace_arg(regs: &libc::user_regs_struct, n: usize) -> u64 {
    match n {
        0 => regs.rdi,
        1 => regs.rsi,
        2 => regs.rdx,
        3 => regs.r10,
        4 => regs.r8,
        5 => regs.r9,
        _ => 0,
    }
}

#[cfg(target_arch = "x86_64")]
fn ptrace_set_arg(regs: &mut libc::user_regs_struct, n: usize, val: u64) {
    match n {
        0 => regs.rdi = val,
        1 => regs.rsi = val,
        2 => regs.rdx = val,
        3 => regs.r10 = val,
        4 => regs.r8 = val,
        5 => regs.r9 = val,
        _ => {}
    }
}

#[cfg(target_arch = "x86_64")]
fn ptrace_sp(regs: &libc::user_regs_struct) -> u64 {
    regs.rsp
}

// ── i686 ptrace helpers ────────────────────────────────────────────────────

#[cfg(target_arch = "x86")]
fn ptrace_syscall_no(regs: &libc::user_regs_struct) -> i64 {
    regs.orig_eax as i64
}

#[cfg(target_arch = "x86")]
fn ptrace_arg(regs: &libc::user_regs_struct, n: usize) -> u64 {
    match n {
        0 => regs.ebx as u64,
        1 => regs.ecx as u64,
        2 => regs.edx as u64,
        3 => regs.esi as u64,
        4 => regs.edi as u64,
        5 => regs.ebp as u64,
        _ => 0,
    }
}

#[cfg(target_arch = "x86")]
fn ptrace_set_arg(regs: &mut libc::user_regs_struct, n: usize, val: u64) {
    let v = val as i32;
    match n {
        0 => regs.ebx = v,
        1 => regs.ecx = v,
        2 => regs.edx = v,
        3 => regs.esi = v,
        4 => regs.edi = v,
        5 => regs.ebp = v,
        _ => {}
    }
}

#[cfg(target_arch = "x86")]
fn ptrace_sp(regs: &libc::user_regs_struct) -> u64 {
    regs.esp as u64
}

fn tracee_read_mem(pid: libc::pid_t, addr: u64, buf: &mut [u8]) -> i64 {
    unsafe {
        let mut pbuf = [0u8; 64];
        let prefix = b"/proc/";
        let suffix = b"/mem";
        let mut pos = 0;
        pbuf[pos..pos + 6].copy_from_slice(prefix);
        pos += 6;
        let mut n = pid;
        let mut digits = [0u8; 12];
        let mut dpos = 0;
        if n == 0 {
            digits[dpos] = b'0';
            dpos += 1;
        } else {
            while n > 0 {
                digits[dpos] = b'0' + (n % 10) as u8;
                n /= 10;
                dpos += 1;
            }
            digits[..dpos].reverse();
        }
        pbuf[pos..pos + dpos].copy_from_slice(&digits[..dpos]);
        pos += dpos;
        pbuf[pos..pos + 4].copy_from_slice(suffix);
        pos += 4;
        pbuf[pos] = 0;

        let fd = libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            pbuf.as_ptr() as *const libc::c_char,
            libc::O_RDONLY | libc::O_CLOEXEC,
            0,
        );
        if fd < 0 {
            return -1;
        }
        let n = libc::syscall(
            libc::SYS_pread64,
            fd,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            addr as i64,
        );
        libc::syscall(libc::SYS_close, fd);
        n as i64
    }
}

fn tracee_write_mem(pid: libc::pid_t, addr: u64, data: &[u8]) -> bool {
    unsafe {
        let mut pbuf = [0u8; 64];
        let prefix = b"/proc/";
        let suffix = b"/mem";
        let mut pos = 0;
        pbuf[pos..pos + 6].copy_from_slice(prefix);
        pos += 6;
        let mut n = pid;
        let mut digits = [0u8; 12];
        let mut dpos = 0;
        if n == 0 {
            digits[dpos] = b'0';
            dpos += 1;
        } else {
            while n > 0 {
                digits[dpos] = b'0' + (n % 10) as u8;
                n /= 10;
                dpos += 1;
            }
            digits[..dpos].reverse();
        }
        pbuf[pos..pos + dpos].copy_from_slice(&digits[..dpos]);
        pos += dpos;
        pbuf[pos..pos + 4].copy_from_slice(suffix);
        pos += 4;
        pbuf[pos] = 0;

        let fd = libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            pbuf.as_ptr() as *const libc::c_char,
            libc::O_RDWR | libc::O_CLOEXEC,
            0,
        );
        if fd < 0 {
            return false;
        }
        let n = libc::syscall(
            libc::SYS_pwrite64,
            fd,
            data.as_ptr() as *const libc::c_void,
            data.len(),
            addr as i64,
        );
        libc::syscall(libc::SYS_close, fd);
        (n as i64) == data.len() as i64
    }
}

fn tracee_read_string(pid: libc::pid_t, addr: u64) -> Option<Vec<u8>> {
    let mut buf = [0u8; 4096];
    let n = tracee_read_mem(pid, addr, &mut buf);
    if n <= 0 {
        return None;
    }
    let len = buf[..n as usize]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(n as usize);
    Some(buf[..len].to_vec())
}

fn tracee_get_regs(pid: libc::pid_t, regs: &mut libc::user_regs_struct) -> bool {
    unsafe {
        let mut iov = libc::iovec {
            iov_base: regs as *mut _ as *mut libc::c_void,
            iov_len: std::mem::size_of::<libc::user_regs_struct>(),
        };
        libc::ptrace(
            libc::PTRACE_GETREGSET,
            pid,
            NT_PRSTATUS as *mut libc::c_void,
            &mut iov as *mut libc::iovec as *mut libc::c_void,
        ) == 0
    }
}

fn tracee_set_regs(pid: libc::pid_t, regs: &libc::user_regs_struct) -> bool {
    unsafe {
        let mut iov = libc::iovec {
            iov_base: regs as *const _ as *mut libc::c_void,
            iov_len: std::mem::size_of::<libc::user_regs_struct>(),
        };
        libc::ptrace(
            libc::PTRACE_SETREGSET,
            pid,
            NT_PRSTATUS as *mut libc::c_void,
            &mut iov as *mut libc::iovec as *mut libc::c_void,
        ) == 0
    }
}

fn translate_tracee_path(
    pid: libc::pid_t,
    regs: &mut libc::user_regs_struct,
    arg_idx: usize,
    path_addr: u64,
) {
    if path_addr == 0 {
        return;
    }
    let orig = match tracee_read_string(pid, path_addr) {
        Some(p) => p,
        None => return,
    };
    if orig.is_empty() || orig[0] != b'/' {
        return;
    }

    // Skip if path already starts with rootfs (prevents double prefix on race)
    let rfs = rootfs_bytes();
    if !rfs.is_empty() && orig.len() >= rfs.len() && orig[..rfs.len()] == rfs[..] {
        return;
    }

    let mut local = orig.clone();
    local.push(0);
    let translated = translate(local.as_ptr() as *const libc::c_char);
    let tbytes = unsafe {
        let len = libc::strlen(translated);
        std::slice::from_raw_parts(translated as *const u8, len)
    };

    if tbytes == orig.as_slice() {
        return;
    }

    let sp = ptrace_sp(regs);
    let alloc = (tbytes.len() + 1 + 15) & !15;
    if alloc > 4096 {
        return;
    }
    let waddr = sp.wrapping_sub(alloc as u64);
    if tracee_write_mem(pid, waddr, tbytes)
        && tracee_write_mem(pid, waddr + tbytes.len() as u64, b"\0")
    {
        ptrace_set_arg(regs, arg_idx, waddr);
        tracee_set_regs(pid, regs);
    }
}

fn resolve_tracee_fd_path(pid: libc::pid_t, dirfd: libc::c_int) -> Option<Vec<u8>> {
    let mut pbuf = [0u8; 128];
    let prefix = b"/proc/";
    let mid = b"/fd/";
    let mut pos = 0;
    pbuf[pos..pos + 6].copy_from_slice(prefix);
    pos += 6;
    let mut n = pid;
    let mut digits = [0u8; 12];
    let mut dpos = 0;
    if n == 0 {
        digits[dpos] = b'0';
        dpos += 1;
    } else {
        while n > 0 {
            digits[dpos] = b'0' + (n % 10) as u8;
            n /= 10;
            dpos += 1;
        }
        digits[..dpos].reverse();
    }
    pbuf[pos..pos + dpos].copy_from_slice(&digits[..dpos]);
    pos += dpos;
    pbuf[pos..pos + 4].copy_from_slice(mid);
    pos += 4;

    // write dirfd
    let mut fd = dirfd;
    let mut fdigits = [0u8; 12];
    let mut fdpos = 0;
    if fd < 0 {
        pbuf[pos] = b'-';
        pos += 1;
        fd = -fd;
    }
    if fd == 0 {
        fdigits[fdpos] = b'0';
        fdpos += 1;
    } else {
        while fd > 0 {
            fdigits[fdpos] = b'0' + (fd % 10) as u8;
            fd /= 10;
            fdpos += 1;
        }
        fdigits[..fdpos].reverse();
    }
    pbuf[pos..pos + fdpos].copy_from_slice(&fdigits[..fdpos]);
    pos += fdpos;
    pbuf[pos] = 0;

    let mut link = [0u8; 4096];
    let n = unsafe {
        libc::syscall(
            libc::SYS_readlinkat,
            libc::AT_FDCWD,
            pbuf.as_ptr() as *const libc::c_char,
            link.as_mut_ptr() as *mut libc::c_char,
            link.len(),
        )
    };
    if n <= 0 {
        None
    } else {
        Some(link[..n as usize].to_vec())
    }
}

fn handle_ptrace_syscall(pid: libc::pid_t) -> bool {
    let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    if !tracee_get_regs(pid, &mut regs) {
        return false;
    }

    let sys = ptrace_syscall_no(&regs);

    if sys == 221 {
        let path_addr = ptrace_arg(&regs, 0);
        if path_addr == 0 {
            return false;
        }
        let path = match tracee_read_string(pid, path_addr) {
            Some(p) => p,
            None => return false,
        };
        if path.is_empty() || path[0] != b'/' {
            return false;
        }

        let mut local = path.clone();
        local.push(0);
        let is_static = read_elf_info(local.as_ptr() as *const libc::c_char)
            .map(|i| i.is_static)
            .unwrap_or(false);
        translate_tracee_path(pid, &mut regs, 0, path_addr);
        return !is_static;
    }

    if sys == 281 {
        let path_addr = ptrace_arg(&regs, 1);
        if path_addr == 0 {
            return false;
        }
        let path = match tracee_read_string(pid, path_addr) {
            Some(p) => p,
            None => return false,
        };
        if path.is_empty() || path[0] != b'/' {
            return false;
        }

        let mut local = path.clone();
        local.push(0);
        let is_static = read_elf_info(local.as_ptr() as *const libc::c_char)
            .map(|i| i.is_static)
            .unwrap_or(false);

        if let Some(resolved) =
            resolve_ptrace_path(pid, ptrace_arg(&regs, 0) as libc::c_int, path_addr)
        {
            ptrace_set_arg(&mut regs, 1, resolved);
            tracee_set_regs(pid, &regs);
        }
        return !is_static;
    }

    if sys == 56
        || sys == 48
        || sys == 79
        || sys == 34
        || sys == 35
        || sys == 78
        || sys == 88
        || sys == 276
    {
        let dirfd = ptrace_arg(&regs, 0) as libc::c_int;
        let path_addr = ptrace_arg(&regs, 1);
        if let Some(resolved) = resolve_ptrace_path(pid, dirfd, path_addr) {
            if resolved != path_addr {
                ptrace_set_arg(&mut regs, 1, resolved);
                tracee_set_regs(pid, &regs);
            }
        }
        return false;
    }

    if sys == 49 {
        let arg0 = ptrace_arg(&regs, 0);
        translate_tracee_path(pid, &mut regs, 0, arg0);
        return false;
    }

    if sys == 36 {
        let arg2 = ptrace_arg(&regs, 2);
        translate_tracee_path(pid, &mut regs, 2, arg2);
        return false;
    }

    if sys == 37 || sys == 38 {
        let dirfd = ptrace_arg(&regs, 0) as libc::c_int;
        let path_addr = ptrace_arg(&regs, 1);
        if let Some(resolved) = resolve_ptrace_path(pid, dirfd, path_addr) {
            if resolved != path_addr {
                ptrace_set_arg(&mut regs, 1, resolved);
            }
        }
        let newdirfd = ptrace_arg(&regs, 2) as libc::c_int;
        let newpath_addr = ptrace_arg(&regs, 3);
        if let Some(resolved) = resolve_ptrace_path(pid, newdirfd, newpath_addr) {
            if resolved != newpath_addr {
                ptrace_set_arg(&mut regs, 3, resolved);
            }
        }
        tracee_set_regs(pid, &regs);
        return false;
    }

    false
}

fn resolve_ptrace_path(pid: libc::pid_t, dirfd: libc::c_int, path_addr: u64) -> Option<u64> {
    let path = tracee_read_string(pid, path_addr)?;
    if path.is_empty() {
        return None;
    }

    let full = if path[0] == b'/' {
        path
    } else if dirfd == -100 {
        // AT_FDCWD: CWD already inside rootfs, leave relative paths alone
        return None;
    } else {
        let dir = resolve_tracee_fd_path(pid, dirfd)?;
        let mut combined = dir;
        combined.push(b'/');
        combined.extend_from_slice(&path);
        combined
    };

    let mut local = full.clone();
    local.push(0);
    let translated = translate(local.as_ptr() as *const libc::c_char);
    let tbytes = unsafe {
        let len = libc::strlen(translated);
        std::slice::from_raw_parts(translated as *const u8, len)
    };

    if tbytes == full.as_slice() {
        return Some(path_addr);
    }

    let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    if !tracee_get_regs(pid, &mut regs) {
        return None;
    }
    let sp = ptrace_sp(&regs);
    let alloc = (tbytes.len() + 1 + 15) & !15;
    if alloc > 4096 {
        return None;
    }
    let waddr = sp.wrapping_sub(alloc as u64);
    if tracee_write_mem(pid, waddr, tbytes)
        && tracee_write_mem(pid, waddr + tbytes.len() as u64, b"\0")
    {
        Some(waddr)
    } else {
        None
    }
}

fn wait_and_exit(pid: libc::pid_t) -> ! {
    loop {
        let mut status: libc::c_int = 0;
        let ret = unsafe { libc::waitpid(pid, &mut status, 0) };
        if ret <= 0 {
            unsafe {
                libc::_exit(1);
            }
        }
        if libc::WIFEXITED(status) {
            unsafe {
                libc::_exit(libc::WEXITSTATUS(status));
            }
        }
        if libc::WIFSIGNALED(status) {
            unsafe {
                libc::_exit(128 + libc::WTERMSIG(status));
            }
        }
    }
}

fn ptrace_loop(pid: libc::pid_t) -> ! {
    let null = std::ptr::null_mut::<libc::c_void>();
    unsafe {
        let _ = libc::ptrace(
            libc::PTRACE_SETOPTIONS,
            pid,
            null,
            (PTRACE_O_TRACESYSGOOD | PTRACE_O_EXITKILL) as *mut libc::c_void,
        );
        libc::ptrace(libc::PTRACE_SYSCALL, pid, null, null);
    }

    let mut status: libc::c_int = 0;
    let mut in_syscall = false;
    let mut detach_next = false;

    loop {
        let ret = unsafe { libc::waitpid(pid, &mut status, libc::__WALL) };
        if ret <= 0 {
            unsafe {
                libc::_exit(1);
            }
        }

        if libc::WIFEXITED(status) {
            unsafe {
                libc::_exit(libc::WEXITSTATUS(status));
            }
        }
        if libc::WIFSIGNALED(status) {
            unsafe {
                libc::_exit(128 + libc::WTERMSIG(status));
            }
        }

        let stopsig = libc::WSTOPSIG(status);

        if stopsig == (libc::SIGTRAP | 0x80) {
            if in_syscall {
                in_syscall = false;
                if detach_next {
                    unsafe {
                        libc::ptrace(
                            libc::PTRACE_DETACH,
                            pid,
                            null,
                            std::ptr::null_mut::<libc::c_void>(),
                        );
                    }
                    wait_and_exit(pid);
                }
                unsafe {
                    libc::ptrace(libc::PTRACE_SYSCALL, pid, null, null);
                }
            } else {
                in_syscall = true;
                detach_next = handle_ptrace_syscall(pid);
                unsafe {
                    libc::ptrace(libc::PTRACE_SYSCALL, pid, null, null);
                }
            }
        } else {
            unsafe {
                libc::ptrace(
                    libc::PTRACE_SYSCALL,
                    pid,
                    null,
                    stopsig as *mut libc::c_void,
                );
            }
        }
    }
}

fn ptrace_fallback_execve(
    tpath: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
) -> libc::c_int {
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            libc::syscall(libc::SYS_ptrace, libc::PTRACE_TRACEME, 0, 0, 0);
            libc::syscall(libc::SYS_execve, tpath, argv, envp);
            libc::_exit(127);
        }
    }
    if pid < 0 {
        return -1;
    }

    // Wait for the initial execve stop (SIGTRAP from PTRACE_TRACEME + execve)
    let mut status: libc::c_int = 0;
    let ret = unsafe { libc::waitpid(pid, &mut status, libc::__WALL) };
    if ret <= 0 || !libc::WIFSTOPPED(status) {
        unsafe {
            libc::_exit(1);
        }
    }

    ptrace_loop(pid);
}

// ── Security hooks ────────────────────────────────────────────────────

macro_rules! eperm {
    () => {
        unsafe {
            #[cfg(target_os = "android")] { *libc::__errno() = libc::EPERM; }
            #[cfg(not(target_os = "android"))] { *libc::__errno_location() = libc::EPERM; }
            -1
        }
    };
}

#[no_mangle]
pub extern "C" fn ptrace(
    _request: libc::c_int,
    _pid: libc::pid_t,
    _addr: *mut libc::c_void,
    _data: *mut libc::c_void,
) -> libc::c_long {
    eperm!()
}

#[no_mangle]
pub extern "C" fn chroot(_path: *const c_char) -> libc::c_int {
    eperm!()
}

#[no_mangle]
pub extern "C" fn pivot_root(_new: *const c_char, _old: *const c_char) -> libc::c_int {
    eperm!()
}

#[no_mangle]
pub extern "C" fn init_module(
    _img: *mut libc::c_void,
    _len: libc::c_ulong,
    _args: *const c_char,
) -> libc::c_int {
    eperm!()
}

#[no_mangle]
pub extern "C" fn finit_module(
    _fd: libc::c_int,
    _args: *const c_char,
    _flags: libc::c_int,
) -> libc::c_int {
    eperm!()
}

#[no_mangle]
pub extern "C" fn delete_module(_name: *const c_char, _flags: libc::c_int) -> libc::c_int {
    eperm!()
}

#[no_mangle]
pub extern "C" fn reboot(
    _magic: libc::c_int,
    _magic2: libc::c_int,
    _cmd: libc::c_uint,
    _arg: *mut libc::c_void,
) -> libc::c_int {
    eperm!()
}

#[no_mangle]
pub extern "C" fn swapon(_path: *const c_char, _swapflags: libc::c_int) -> libc::c_int {
    eperm!()
}

#[no_mangle]
pub extern "C" fn swapoff(_path: *const c_char) -> libc::c_int {
    eperm!()
}

#[no_mangle]
pub extern "C" fn kexec_load(
    _entry: libc::c_ulong,
    _nr_segments: libc::c_ulong,
    _segments: *mut libc::c_void,
    _flags: libc::c_ulong,
) -> libc::c_long {
    eperm!()
}

#[no_mangle]
pub extern "C" fn iopl(_level: libc::c_int) -> libc::c_int {
    eperm!()
}

#[no_mangle]
pub extern "C" fn personality(persona: libc::c_ulong) -> libc::c_int {
    const ASLR_OFF: libc::c_ulong = 0x00040000;
    if persona as isize != -1 && (persona & ASLR_OFF) == ASLR_OFF {
        eperm!()
    } else {
        unsafe { libc::syscall(libc::SYS_personality, persona as i64) as libc::c_int }
    }
}

// ── Initialization (all lazy in accessors above) ──────────────────────────────
