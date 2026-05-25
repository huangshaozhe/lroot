# lroot – Fast Linux Path Translation Runner

`lroot` runs commands inside an alternate root filesystem without needing
root privileges. It hooks ~50 libc functions via `LD_PRELOAD` to translate
file paths, making programs think they're running in a different root.

**2–4× faster than proot** — uses libc hooking instead of ptrace syscall
interception.

## Install

```sh
cargo build --release -p runner -p intercept
cp target/release/lroot target/release/libintercept.so /usr/local/bin/
```

The runner auto-detects the target binary's architecture (ELF class) and
libc type (glibc/musl), selects the matching `libintercept-{arch}-{libc}.so`.

## Usage

```
lroot [option...] -r rootfs [command...]
lroot [option...] rootfs [command...]
```

| Option | Description |
|--------|-------------|
| `-r` / `--rootfs` | Path to root filesystem |
| `-0` / `--root-id` | Fake uid/gid as 0 |
| `-w` / `--pwd` | Working directory inside rootfs |
| `-b` / `--bind` | Mount–bind (`host:guest`) |
| `-q` / `--qemu` | QEMU user-mode binary |
| `-k` / `--kernel-release` | Fake kernel version (`uname -r`) |
| `-v` / `--verbose` | Debug logging |
| `-i` / `--intercept` | Override libintercept.so path |
| `-h` / `--help` | Show help |

## Examples

```sh
lroot -r ~/rootfs /bin/sh
lroot -r ~/rootfs -0 /bin/sh                # fake root
lroot -r ~/rootfs -b /home:/mnt/host /bin/sh # bind mount
lroot -r ~/rootfs -q qemu-aarch64 /bin/sh    # cross-arch
```

## Features

- **Path translation**: open, stat, readlink, execve, etc. — all paths
  translated to the rootfs prefix. `/proc`, `/sys`, `/dev` pass through.
- **Fake root** (`-0`): hooks getuid/geteuid/getgid/getegid, fakes
  `Uid:`/`Gid:` in `/proc/self/status`, fakes `uid_map`/`gid_map`.
- **Bind mount** (`-b`): redirect host paths into the rootfs.
- **QEMU**: auto-runs non-native ELF binaries through a specified QEMU
  user-mode binary when enabled.
- **Multi-arch**: auto-selects the intercept variant
  (`libintercept-64-glibc.so`, etc.) based on the target binary.
- **Fake /proc**: version, self/maps, self/status, self/mountinfo,
  self/cmdline, uid_map/gid_map, self/mounts.
- **Security hooks**: ptrace, chroot, pivot_root, init/finit_module,
  reboot, swapon, kexec_load, iopl blocked with EPERM.
- **Static / setuid fallback**: auto-detects and uses ptrace for binaries
  where LD_PRELOAD doesn't work.

## How it works

`lroot` (runner) sets up environment variables (`LITTLE_ROOTFS`,
`LITTLE_BINDS`, `LITTLE_ROOT_ID`) and spawns the target command with
`LD_PRELOAD=libintercept.so`. The intercept library hooks libc functions
(open, stat, execve, readlink, etc.) to translate paths by prepending
the rootfs prefix. All hooks call the kernel directly via `libc::syscall`
to avoid PLT self-resolution loops.

## Comparison with proot

| Aspect | lroot | proot |
|--------|-------|-------|
| Mechanism | LD_PRELOAD (libc hooks) | ptrace (syscall interception) |
| 1000× stat | ~0.03s | ~0.35s |
| 100× exec | ~0.19s | ~0.46s |
| Binary size | 342K + 325K | 1.9M (statically linked) |
| Dependencies | glibc (or musl variant) | None (statically linked) |
| QEMU integration | Built-in | Built-in (proot -q) |
| seccomp | Not needed | Optional |
| Static/setuid binaries | Ptrace fallback | Ptrace always |
| Security isolation | Minimal (safety hooks only) | None by default |

## Building

```sh
cargo build --release -p runner -p intercept
```

Cross-compilation targets require additional toolchains (see `build-all.sh`):
- **x86_64-glibc**: default (works out of the box)
- **i686-glibc**: `rustup target add i686-unknown-linux-gnu` + gcc-multilib
- **x86_64-musl**: `rustup target add x86_64-unknown-linux-musl` + musl-tools
- **aarch64-glibc**: `rustup target add aarch64-unknown-linux-gnu` + cross-gcc

## License

Unlicense — public domain. See `LICENSE` or
<https://unlicense.org/>.
