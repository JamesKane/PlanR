# Architecture

This is the starting design. Decisions with lasting consequences are
recorded as ADRs in [adr/](adr/); everything else here can change as we
learn.

## System shape

Like Plan 9, Plan R has a **monolithic kernel with a file-server
personality**. Core devices live in the kernel and are reached through
`#x` device names. Everything else is a user-space file server reached
through 9P and assembled into a per-process namespace. The three Plan 9
roles stay: **terminal**, **CPU server** and **file/auth server**. They are
configurations of one kernel, not different builds.

```
 user processes ──── syscalls (planr-abi) ────┐
   rc, acme, rio,                             │
   factotum, fs…                              ▼
        ▲                 ┌──────────── kernel ────────────┐
        │ 9P over pipes,  │ namespace (Pgrp/mount table)   │
        │ /srv, /net      │ Chan ─► Dev trait ─► #c #p #e │
        └─────────────────┤          #| #s #M #/ #I #S #u  │
                          │ proc/sched · VM/segments · qio │
                          │ arch: amd64 | arm64            │
                          └──────────── UEFI + ACPI ───────┘
```

## Source tree

A Cargo workspace. Every crate shipped in Plan R is `#![no_std]` and
depends only on `core`, `alloc` and other in-tree crates
([ADR 0001](adr/0001-core-alloc-only.md)).

```
PlanR/
├─ boot/9boot/          UEFI loader (x86_64/aarch64-unknown-uefi)
├─ kernel/
│  ├─ port/             portable kernel (proc, chan, namespace, qio, …)
│  ├─ dev/              in-kernel devices (cons, proc, env, mnt, ip, sd, …)
│  ├─ arch/amd64/       entry, MMU, APIC, traps, context switch
│  ├─ arch/arm64/       entry, MMU, GICv3, exceptions, PSCI
│  └─ drivers/          virtio, nvme, ahci, xhci, nics, uarts, gop/fb
├─ lib/                 no_std libraries, host-testable
│  ├─ abi/              planr-abi: syscall numbers, typed args, error codes
│  ├─ 9p/               9P2000 codec + client/server framework
│  ├─ acpi/             table parser + AML interpreter
│  ├─ bootinfo/         9boot → kernel handoff types (ADR 0010)
│  ├─ uefi/             UEFI type definitions (loader only)
│  ├─ elf/              ELF64 parser/loader
│  ├─ frames/           physical frame allocator (buddy)
│  ├─ sec/              crypto primitives, TLS 1.3, PAKE (libsec successor)
│  ├─ net/              IP/TCP/UDP stack core
│  ├─ draw/             memdraw/memlayer, draw protocol
│  ├─ fs/               native disk fs format, FAT
│  ├─ flate/ regexp/ utf/ fmt/ ndb/ auth/ …
│  └─ r/                libr: the user runtime (replaces libc+lib9+bio+thread)
├─ cmd/                 user commands (rc, mk, acme, rio, cpu, …)
├─ ape2/                POSIX environment exporting a C ABI (see TOOLCHAIN.md)
├─ foreign/             quarantined upstream code for self-hosting (Q2)
├─ root/                files for the installed tree (/lib/namespace, /rc/bin, …)
├─ xtask/               host build tool (host zone)
├─ oracle/              host test oracles (host zone)
└─ docs/
```

## Kernel core

### Errors

Plan 9 uses `error()`/`waserror()`/`nexterror()`, a `setjmp`-based
exception scheme, plus error strings. Plan R uses `Result` everywhere:

```rust
pub enum Error {
    NotFound, Exists, Perm, IsDir, NotDir, BadArg, NoMem, Interrupted,
    Shutdown, Hungup, InUse, TooBig, BadFd, Io, Timeout, Unsupported,
    Protocol(&'static str),
    /// Error text from a 9P server (Rerror), passed through unchanged.
    Remote(ErrString),
}
```

At the user boundary, `Error` turns into the familiar `errstr` text
(`"file does not exist"`, `"permission denied"`, …). Scripts that match
errors keep working. Inside the system, code matches on the variant.
Interrupted syscalls (notes) are `Error::Interrupted`, carried up by `?`
instead of a `longjmp`.

### Chan, Qid, Dev

```rust
pub struct Qid { pub path: u64, pub vers: u32, pub kind: QidKind }
pub struct QidKind: u8 { DIR, APPEND, EXCL, MOUNT, AUTH, TMP }   // bitflags (ours)

pub enum OpenMode { Read, Write, ReadWrite, Exec }
pub struct OpenFlags: u8 { TRUNC, CEXEC, RCLOSE }

pub trait Dev: Sync {
    fn dc(&self) -> char;                                   // 'c', 'p', 'M', …
    fn attach(&self, spec: &str) -> Result<Chan>;
    fn walk(&self, c: &Chan, names: &[&str]) -> Result<Walk>;
    fn stat(&self, c: &Chan) -> Result<Dir>;
    fn open(&self, c: Chan, mode: OpenMode, f: OpenFlags) -> Result<Chan>;
    fn create(&self, c: Chan, name: &str, mode: OpenMode, perm: Perm) -> Result<Chan>;
    fn read(&self, c: &Chan, buf: &mut [u8], off: u64) -> Result<usize>;
    fn write(&self, c: &Chan, buf: &[u8], off: u64) -> Result<usize>;
    fn remove(&self, c: Chan) -> Result<()>;
    fn wstat(&self, c: &Chan, d: &Dir) -> Result<()>;
    fn close(&self, c: Chan);
    // optional: bread/bwrite for Block-based zero-copy paths
}
```

A `Chan` is reference counted (`Arc`). Device-private state is an
associated typed value instead of Plan 9's `void *aux` / `c->qid.path`
bit-packing. Each device defines `enum Node { Root, Ctl, Data(u32), … }`
and stores its encoding in the qid path.

### Devices and drivers

How devices are found, matched to drivers, granted hardware and detached,
and which drivers run in user space through `#H`, is in
[DRIVER-MODEL.md](DRIVER-MODEL.md) (proposed).

### Namespaces

`Pgrp` holds the mount table: a map from `(Qid, devtype, devno)` to
an ordered union list of `Mount { to: Chan, flags: MountFlags }`.
`bind`/`mount`/`unmount` follow Plan 9 semantics exactly, since they are
the core of the user experience. `rfork` flags are typed:

```rust
pub enum NsShare { Share, Copy, Clean }          // RFNAMEG / RFCNAMEG
pub struct RforkFlags { proc: bool, mem: MemShare, ns: NsShare,
                        fd: FdShare, env: EnvShare, note: bool, wait: bool }
```

### Processes and scheduling

- Plan 9 procs stay the unit of execution. `rfork(RFPROC|RFMEM)` gives
  thread-like sharing. The Rust `std` port maps threads onto it.
- Per-CPU run queues with work stealing, 20 priority levels as in Plan 9,
  and a CPU-topology model (SMT siblings, clusters, NUMA, core class
  big/mid/little from ACPI PPTT/CPPC). Topology is used for placement from
  P9 on.
- Notes stay as Plan 9 notes (strings, `notify`/`noted`); there are no
  POSIX signals in the native API. APE2 emulates signals on top of notes.
- Sleep/wakeup become `Rendez` with a condition closure; locks are RAII
  (`SpinLock<T>`, `QLock<T>`, `RwLock<T>`). Lock order is documented per
  subsystem and checked in debug builds.

### Memory

- Kernel: higher-half direct map of physical memory, a slab allocator for
  fixed-size objects, a general-purpose heap behind `#[global_allocator]`.
  All ours.
- User: Plan 9 segments (`Text`, `Data`, `Bss`, `Stack`, `Shared`, `Phys`,
  plus new `Dma` for IOMMU-mapped buffers), demand paging, CoW, `segattach`
  for shared and physical memory. ASLR and W^X are enforced.
- Page size 4 KiB on both architectures, with huge pages for the direct map
  and large segments.

## System call ABI

[ADR 0002](adr/0002-new-syscall-abi.md). A new ABI that keeps the Plan 9
semantics. It is defined once in `lib/abi` and used by kernel, `libr` and
APE2:

| Group | Calls |
|---|---|
| Files | `open create close read write pread pwrite seek stat fstat wstat fwstat remove fd2path dup pipe chdir` |
| Namespace | `bind mount unmount fversion fauth` |
| Processes | `rfork exec exits await errstr sleep alarm nsec` |
| Notes | `notify noted` |
| Sync | `rendezvous semacquire tsemacquire semrelease` |
| Memory | `brk segattach segdetach segfree segflush` |
| New | `settls` (thread pointer), `getrandom` (or `#c/random` only, Q7), `futex`-style `semacquire` addressing 64-bit words |

Entry is `syscall` on amd64 and `svc #0` on arm64. Arguments go in
registers and results come back as `Result`-encoded register pairs. There
is no `errno`. The error text is fetched lazily with `errstr`, which keeps
Plan 9 behaviour without a string copy on every failure.

## 9P

- **9P2000 is the wire protocol.** The message set is a pair of Rust enums
  (`Tmsg`, `Rmsg`) with a zero-copy decoder, fuzzed from the start.
- Modernizations that don't change the framing: a large default `msize`
  (≥ 1 MiB), many outstanding tags, and pipelined walks.
- Optional extensions go in a negotiated `fversion` string such as
  `9P2000.R`, and only once the need is proven (Q4).
- Transport-agnostic: pipes, TCP, TLS, virtio-9p/vsock (development host
  shares) and, later, RDMA for DPUs.

## Security model

- Plan 9's model stays: no superuser, `hostowner` owns the machine, all
  authentication goes through `factotum`, and file servers authorize on 9P
  `attach` with an `afid`.
- New auth protocol replacing p9sk1 (DES). The proposal is an
  augmented PAKE (X25519-based, in the spirit of 9front's dp9ik) for the
  password-to-ticket exchange, with ChaCha20-Poly1305/AES-GCM tickets and
  authenticators (Q6).
- TLS 1.3 for all network 9P across untrusted links.
- Hardware hardening: SMEP/SMAP/UMIP, PAN/PXN/BTI/PAC, IOMMU for all DMA,
  and MTE on Armv9 parts that have it.

## Graphics

`devdraw` keeps the Plan 9 model: the kernel (or a privileged file server)
owns images, `rio` multiplexes `/dev/draw` per window. Changes:

- 32-bit premultiplied RGBA everywhere, a scale factor for high-DPI, and
  vector fonts rasterized to subfonts.
- `memdraw` is a pure `no_std` library, so it can be tested on the host
  against reference renders and accelerated later by the GPU file server.
- Displays: GOP framebuffer → virtio-gpu → native display engines (P9).

## Runtime library: `libr`

`libr` is the one user-space runtime for native Plan R programs, built on
`core` + `alloc`. It takes the place of Plan 9's `libc`, `lib9`, `libbio`,
`libthread`, `libregexp`, `libString`, `libndb`, `libauth` and `libplumb`,
as Rust modules:

`sys` (syscalls) · `io` (bio-style buffered I/O, `Read`/`Write` traits of
our own) · `fmt` (Plan 9 `print` verbs) · `arg` (flags) · `rune` · `str` ·
`thread` (procs + channels, `alt`) · `re` (regexp) · `ndb` · `auth` ·
`plumb` · `dial` · `env` · `time` · `rand` · `collections` beyond `alloc`.

A program uses `#![no_std] #![no_main]` and a `libr::main!` entry macro that
sets up args, environment, the allocator and the panic → `exits` path.
