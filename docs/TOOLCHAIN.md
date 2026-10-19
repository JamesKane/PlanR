# Toolchain

## Phase A: cross-building from a Linux host (P0–P7)

- Upstream stable Rust, pinned in `rust-toolchain.toml`. Nightly only if
  a needed feature forces it, and recorded in an ADR.
- Kernel and loader use the built-in targets: `x86_64-unknown-none`,
  `aarch64-unknown-none`, `x86_64-unknown-uefi`, `aarch64-unknown-uefi`.
- User space uses custom target specs `x86_64-unknown-planr.json` /
  `aarch64-unknown-planr.json` (ELF, static PIE, our linker script, panic
  = abort at first). `core` and `alloc` are built for them with
  `-Zbuild-std=core,alloc` or a stable equivalent. This is the only
  place `build-std` is needed.
- Linking uses `rust-lld`. No host C toolchain is needed to build Plan R.
- `cargo xtask` drives everything: builds, image creation (GPT + FAT ESP +
  root fs, written by our own code), QEMU, tests, and the `deps` check for
  the [dependency rule](adr/0001-core-alloc-only.md).
- Release builds are reproducible today (verified: identical image hashes
  across clean builds; PE images are linked with `/Brepro`). Debug builds
  are not yet: rust-lld's PDB output varies, which needs looking into.
- Builds must be reproducible: fixed paths (`--remap-path-prefix`), no
  timestamps, deterministic archive and filesystem image writers.

## No custom dialects

[ADR 0005](adr/0005-no-custom-c-dialect.md). Plan 9's C (`?c`) was a
private dialect (unnamed struct members, `#pragma varargck`, its own ABI),
so Plan 9 drifted away from the rest of the C world and porting software
was expensive. Plan R:

- Native code is Rust: standard Rust, the standard edition, nothing private
  except our own crates.
- C and C++ exist only to run *foreign* software (LLVM, existing tools)
  and are compiled by **standard clang** as C17/C++20 against APE2.
- Any extension we need goes upstream (e.g. a Plan R target in
  rustc/LLVM), not into a fork.

## APE2: the POSIX environment, rebuilt

Old APE was a C library written in Plan 9 C, frozen around C89/POSIX.1-1990
and missing threads, `mmap`, sockets as fds, modern `stdio`, locales and
C11 atomics. Neither a modern C toolchain nor anything the Rust toolchain
needs would run on it.

APE2 goals:

1. **Written in Rust (`core` + `alloc`), exporting a C ABI.** This
   follows the dependency rule and gives memory safety in the
   compatibility layer.
2. **Covers what LLVM, clang, rustc and cargo need:** POSIX.1-2017 subset —
   `pthreads` (on `rfork(RFMEM)` + TLS + `semacquire`), `mmap`/`munmap`/
   `mprotect` (on segments), `dlopen` (Later; static linking first),
   sockets (on `/net` with `dial`/`announce`), `poll`/`select` (on
   proc-per-fd helpers or a kernel wait primitive, Q12), `fork`/`exec`/
   `posix_spawn`, signals (emulated on notes), `termios` (on `/dev/cons`
   ctl), `stat` with the uid/gid strings mapped, `getaddrinfo` (on `cs`),
   `clock_gettime` (on `nsec`), `C11/C++ atomics` and `thread_local`.
3. **Headers:** a fresh set of C17 headers. Musl's headers are a good
   reference for structure.
4. **C++ runtime:** LLVM libc++/libc++abi/libunwind built as foreign code
   on top of APE2.
5. **Kernel help where it pays:** some POSIX behaviours are cheap in the
   kernel and costly to emulate (e.g. a multi-fd wait, `mprotect`
   granularity, robust futex-style waits). Every such addition is a native
   Plan R feature first, has an ADR, and must be useful to native code too.

## Phase B: Rust on Plan R (P8)

1. **`std` for Plan R.** Upstream `library/std` needs a platform module
   (`std/src/sys/pal/planr`). It can be built on APE2 (like a Unix target)
   or directly on `lib/abi` (cleaner, more work). The proposal: directly
   on the native ABI for fs/process/thread/time and APE2 only where `std`
   expects libc symbols (e.g. `libunwind`). This code is upstream-shaped
   and so lives in `foreign/` (Q2).
2. **LLVM + clang** cross-built from Linux against APE2 + libc++, then
   self-hosted.
3. **rustc + cargo** cross-built, then self-hosted. Lighter alternative to
   evaluate: `rustc_codegen_cranelift` for fast debug builds.
4. **Upstreaming:** propose `*-unknown-planr` as tier-3 targets once the
   ABI is stable.
5. **Crates on Plan R:** `cargo` needs a registry or vendored sources. For
   building Plan R itself none are needed (dependency rule). For foreign
   software, `cargo vendor` tarballs are fetched over `webfs`.

## Long-term question

Self-hosting rustc pulls in a lot of foreign code (LLVM is millions of
lines). An alternative is to keep Plan R permanently cross-built and offer
self-hosting as an optional, separately maintained "foreign" package. To be
decided in P8 (Q2).
