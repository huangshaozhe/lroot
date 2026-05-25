## Description

Briefly describe what this PR does.

## Related issue

Fixes #(issue)

## Testing

- [ ] `cargo test -p lroot` passes
- [ ] `cargo test -p lroot-distro` passes
- [ ] Tested on real device (if applicable)

## Checklist

- [ ] Code follows existing style (no added comments, 2-space indent)
- [ ] All hooks use `libc::syscall(SYS_*)` (no PLT calls)
- [ ] Android-specific code guarded with `#[cfg(target_os = "android")]`
