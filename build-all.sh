#!/bin/sh
# Build libintercept.so for all supported targets.
# Each build produces libintercept-{arch}-{libc}.so in target/release/.
# lroot auto-detects the target binary's arch/libc and picks the matching .so.
#
# Requirements by target:
#   x86_64-*-glibc   : nothing extra (default Rust target)
#   i686-*-glibc     : rustup target add i686-unknown-linux-gnu
#                      apt install gcc-multilib
#   x86_64-*-musl    : rustup target add x86_64-unknown-linux-musl
#                      + musl-gcc (apt install musl-tools)
#   aarch64-*-glibc  : rustup target add aarch64-unknown-linux-gnu
#                      apt install gcc-aarch64-linux-gnu
#   aarch64-*-musl   : cross-compiler + musl for aarch64

set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

BUILD_DIR="target/release"

build_target() {
    local target="$1"
    local variant="$2"
    local info="$3"
    echo "==> $target ($variant) - $info"

    if ! rustup target add "$target" 2>/dev/null; then
        echo "  SKIP (rustup target add $target failed)"
        return
    fi

    # Build with custom linker if CC_{target} is set
    local cc_var="CC_$(echo "$target" | tr '[:upper:]-' '[:lower:]_')"
    local cc_val
    eval "cc_val=\${$cc_var:-}"

    if [ -n "$cc_val" ]; then
        export "CC_$target"="$cc_val"
    fi

    if cargo build --release -p intercept --target "$target" 2>/dev/null; then
        cp "target/$target/release/libintercept.so" \
           "$BUILD_DIR/libintercept-$variant.so"
        echo "  OK -> libintercept-$variant.so"
    else
        echo "  SKIP (build failed)"
    fi
}

# Ensure output dir exists
mkdir -p "$BUILD_DIR"

# ── Host: x86_64 glibc (always works) ─────────────────────────────────
build_target x86_64-unknown-linux-gnu "64-glibc" "native x86_64"

# ── 32-bit: i686 glibc ────────────────────────────────────────────────
# Needs: apt install gcc-multilib libc6-dev-i386 lib32gcc-s1
# cargo check passes (all type errors fixed), but linking requires
# i686 system libraries (crti.o, libgcc_s.so) not available without
# multilib.  On Ubuntu/Debian:
#   sudo apt install gcc-multilib libc6-dev-i386 lib32gcc-s1
# Or set linker="rust-lld" in .cargo/config.toml for this target and
# provide the i686 library paths via -L flags.
build_target i686-unknown-linux-gnu "32-glibc" "32-bit x86"

# ── lroot-distro (works on any host) ──────────────────────────────────
echo "==> lroot-distro"
cargo build --release -p lroot-distro 2>/dev/null && \
  echo "  OK -> target/release/lroot-distro" || \
  echo "  SKIP (build failed)"

# ── musl: x86_64 musl ─────────────────────────────────────────────────
# Requires musl libc libraries in /tmp/musl-sysroot/lib/:
#   make_musl_sysroot()  # run once after installing alpine-amd64
#
# The sysroot is populated from the Alpine rootfs and a libgcc_s stub.
# See .cargo/config.toml for linker/rustflags used.
make_musl_sysroot() {
    local rootfs="$HOME/.lroot/distros/alpine-amd64"
    if [ ! -d "$rootfs/lib" ]; then
        echo "  SKIP (install alpine-amd64 first: lroot-distro install alpine-amd64)"
        return 1
    fi
    mkdir -p /tmp/musl-sysroot/lib
    cp "$rootfs/lib/libc.musl-x86_64.so.1" /tmp/musl-sysroot/lib/
    ln -sf libc.musl-x86_64.so.1 /tmp/musl-sysroot/lib/libc.so
    cp "$rootfs/lib/ld-musl-x86_64.so.1" /tmp/musl-sysroot/lib/
    # Build a libgcc_s stub providing _Unwind_* symbols needed by Rust cdylib
    cat > /tmp/libgcc_s_stub.c << 'STUBEOF'
/* Stub for lroot musl - provides unwind symbols needed by Rust cdylib */
void *_Unwind_GetLanguageSpecificData(void *c) { return (void*)0; }
unsigned long _Unwind_GetIPInfo(void *c, int *i) { *i = 0; return 0; }
void _Unwind_SetGR(void *c, int i, unsigned long v) {}
void _Unwind_SetIP(void *c, unsigned long v) {}
unsigned long _Unwind_GetIP(void *c) { return 0; }
unsigned long _Unwind_GetRegionStart(void *c) { return 0; }
unsigned long _Unwind_GetDataRelBase(void *c) { return 0; }
unsigned long _Unwind_GetTextRelBase(void *c) { return 0; }
int _Unwind_Backtrace(int (*fn)(void*,void*), void *p) { return 0; }
int _Unwind_Resume(void *e) { return 0; }
STUBEOF
    gcc -shared -fPIC -o /tmp/musl-sysroot/lib/libgcc_s.so /tmp/libgcc_s_stub.c
    return 0
}
if make_musl_sysroot; then
    build_target x86_64-unknown-linux-musl "64-musl" "x86_64 musl"
else
    echo "  SKIP (musl sysroot not available)"
fi

# ── aarch64 glibc ─────────────────────────────────────────────────────
# Needs: apt install gcc-aarch64-linux-gnu
#   export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
build_target aarch64-unknown-linux-gnu "aarch64-glibc" "ARM 64-bit (cross)"

echo ""
echo "======================================"
echo "Done. Variants in $BUILD_DIR/:"
ls -1 "$BUILD_DIR"/libintercept-*.so 2>/dev/null || echo "  (none)"
echo ""
echo "Install:"
echo "  cp $BUILD_DIR/libintercept-*.so /usr/local/lib/"
