# Open questions

Numbered so other documents can refer to them. When one is resolved, record
it as an ADR and link it here.

| # | Question | Proposal | Needed by |
|---|---|---|---|
| Q1 | May host-side tools (`xtask`, oracles, fuzzers) use host `std`, crates.io crates, or system C libraries? | Host `std`: yes. crates.io: no. System libraries (OpenSSL, libsodium): only in `oracle/`. | P0 |
| Q2 | How is upstream code (LLVM, rustc, Rust `std` platform module, libc++) needed for self-hosting handled, given the core+alloc rule? | Quarantine it in `foreign/`, built against APE2, outside the dependency rule, and optional to install. | P8 |
| Q3 | Project license? | MIT, matching the Plan 9 Foundation release (or MIT OR Apache-2.0, the Rust convention). | P0 |
| Q4 | Any 9P extensions (`9P2000.R`)? | None until measured. Candidates: larger qid version, server-side walk-open, read-ahead hints. | P5 |
| Q5 | Native disk file system design: venti+fossil-like, cwfs-like, or gefs-like CoW B-tree? | A CoW B-epsilon tree with checksums and snapshots (gefs-inspired), plus an optional venti-style archival tier later. | P4 |
| Q6 | Authentication protocol to replace p9sk1? | Augmented PAKE over X25519 with AEAD tickets (dp9ik-inspired). Write it up and get it reviewed before implementing. | P5 |
| Q7 | Randomness API: `#c/random` only, or also a `getrandom` syscall? | Both: the syscall for early runtime and APE2, the file for scripts. | P2 |
| Q8 | Which Ryzen board and CPU? | A board with a serial header or documented debug UART, a well-documented NIC (Intel i226 preferred), and preferably a Ryzen AI part (XDNA NPU). | P7 |
| Q9 | Can we get CIX P1 NPU/GPU documentation and firmware licensing? | Contact the vendor early. Mali uses public Panthor/Arm docs; the NPU may be blocked. | P9 |
| Q10 | AMD64 baseline ISA level? | x86-64-v2 (keeps Zen 1+ and older Intel), with runtime dispatch for AVX2/AES-NI/SHA-NI in `lib/sec`. | P0 |
| Q11 | What if Orange Pi 6 Plus ACPI is incomplete? | First try vendor firmware updates or community EDK2. Second: our own ACPI table overlay loaded by `9boot`. Last resort: an ADR for limited device-tree support. | P7 |
| Q12 | Kernel wait primitive for multiplexing many fds (needed by APE2 `poll` and useful natively)? | Prototype in P3 using helper procs (Plan 9 style); add a kernel primitive only if measurements demand it. | P8 |
| Q13 | Kernel-resident vs user-space drivers for storage/net on the hot path? | Kernel for NVMe/virtio/NICs (performance, boot); user-space via `#H` for GPU/NPU/USB class drivers. | P4 |
| Q14 | Keep `troff`/`postscript` documentation tooling? | Keep `man` working (a smaller man-macro renderer); defer full troff. | Later |
