# Vision

## What Plan R is

Plan R carries the Plan 9 idea forward onto modern 64-bit hardware: a
distributed system built from per-process namespaces and file servers
speaking one protocol. It is written in Rust, from scratch, using the Bell
Labs 4th edition (`../plan9`) as the reference for behaviour and design. It
is not a source port.

## Goals

1. **Keep the day-to-day experience.** Someone who knows Plan 9 should feel
   at home: `bind`, `mount`, `/proc`, `/net`, `/dev/draw`, `rc`, `mk`, `acme`,
   `sam`, `rio`, `plumb`, `cpu`, `import`, `exportfs`, `factotum`,
   `auth/…`, `ndb`. Familiar commands, flags, file layouts and protocols
   are the compatibility promise.
2. **Rust with algebraic types throughout.** Protocol messages, syscall
   arguments, errors, device state and kernel objects are `enum`s and
   `struct`s whose types make invalid states unrepresentable. No stringly
   typed control paths inside the system. Strings appear only at real text
   interfaces such as `ctl` files and `errstr`.
3. **Modern hardware, modern firmware.** AMD64 and ARM64 only, booted by
   UEFI, described by ACPI. No BIOS, no MP tables, no device trees in the
   first cut (see [ADR 0003](adr/0003-uefi-acpi-only.md)).
4. **Modern security.** Current cryptography (X25519, Ed25519,
   ChaCha20-Poly1305, AES-GCM, SHA-2/3, TLS 1.3, a PAKE-based auth
   protocol), W^X, SMEP/SMAP, PAN/PXN, BTI/PAC and IOMMU-isolated DMA. We
   check our crypto against the host's libraries
   ([ADR 0004](adr/0004-host-crypto-oracles.md)).
5. **First-class modern SoCs.** GPUs, NPUs, DPUs, video codecs, crypto
   engines, power domains and heterogeneous CPU clusters (big/mid/little)
   are handled as Plan 9 handles everything else: as file servers in a
   namespace.
6. **Self-hosting with standard languages.** In the end Plan R builds itself
   with an upstream Rust toolchain, and C/C++ code is compiled by a standard
   compiler through APE2. No new language dialects
   ([ADR 0005](adr/0005-no-custom-c-dialect.md)).

## Non-goals

- Binary compatibility with Plan 9, 9front, Inferno or POSIX.
- Wire compatibility with p9sk1/DES authentication. 9P2000 framing stays.
  Insecure auth goes.
- 32-bit targets, MIPS, SPARC, POWER, BIOS boot.
- Porting the Plan 9 C compilers (`?c`, `?a`, `?l`) or the C dialects.
- Being Linux. We borrow ideas, not interfaces.

## The dependency rule

> **Plan R's Rust code may depend only on `core` and `alloc`. Everything
> else is ours.**

See [ADR 0001](adr/0001-core-alloc-only.md). In practice:

- No crates.io dependencies in anything that ships in Plan R: kernel,
  loader, libraries, commands.
- No `std` inside Plan R system code. We write our own runtime library
  (`libr`, see [ARCHITECTURE.md](ARCHITECTURE.md)) on top of `core` +
  `alloc`. It does the jobs `std`, libc and lib9 used to do.
- We write our own UEFI loader, ACPI tables and AML interpreter, virtio, IP
  stack, crypto, TLS, compression, regexp, graphics, fonts and so on.
- **Host-side tools are a separate zone.** Build tools (`xtask`) and test
  oracles run on the development host and never ship. Whether they may use
  host `std` and the host's system libraries is open question Q1. The
  current proposal: host `std` yes, crates.io no, and system libraries such
  as OpenSSL only inside the oracle harness.
- **Foreign code is quarantined.** Running upstream software (LLVM, rustc,
  its `std`) on Plan R for self-hosting is "foreign" code. It is built from
  source under `/sys/src/foreign` against APE2 and is not part of Plan R
  proper (open question Q2).

## Design principles

1. **Resources are files, served over 9P.** If a new feature can't be
   expressed as a file server, the design is probably wrong.
2. **Mechanism in the kernel, policy in file servers.** Complex device
   stacks (GPU, NPU, Wi-Fi) run as user-space file servers, isolated by the
   IOMMU, wherever performance allows.
3. **Types at boundaries.** Every boundary (syscall, 9P, device register
   block, ACPI table, network packet) is parsed once into a typed value.
   Callers never re-parse bytes.
4. **`unsafe` is local and documented.** Every `unsafe` block has a
   `// SAFETY:` comment. Raw hardware access lives in small, reviewed
   modules. `#![deny(unsafe_op_in_unsafe_fn)]` everywhere.
5. **Testable off-target.** Protocol codecs, namespace logic, schedulers,
   allocators, the AML interpreter, crypto and file system formats are
   plain `no_std` crates that can be tested on the host.
6. **Small, readable, Plan 9-sized.** Clarity beats cleverness. A
   subsystem should be readable in an afternoon, as Plan 9's were.
