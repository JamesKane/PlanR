# Open questions

Numbered so other documents can refer to them. When one is resolved, record
it as an ADR and link it here.

| # | Question | Proposal | Needed by |
|---|---|---|---|
| Q1 | ~~May host-side tools use host `std`, crates.io crates, or system C libraries?~~ | **Resolved, [ADR 0006](adr/0006-host-zone-tools.md):** host tools and `std` yes; no crates.io or other unauditable dependency chains in the source. | — |
| Q2 | How is upstream code (LLVM, rustc, Rust `std` platform module, libc++) needed for self-hosting handled, given the core+alloc rule? | Quarantine it in `foreign/`, built against APE2, outside the dependency rule, and optional to install. | P8 |
| Q3 | ~~Project license?~~ | **Resolved, [ADR 0008](adr/0008-license-mit.md):** MIT. | — |
| Q4 | Any 9P extensions (`9P2000.R`)? | None until measured. Candidates: larger qid version, server-side walk-open, read-ahead hints. | P5 |
| Q5 | Native disk file system design: venti+fossil-like, cwfs-like, or gefs-like CoW B-tree? | A CoW B-epsilon tree with checksums and snapshots (gefs-inspired), plus an optional venti-style archival tier later. | P4 |
| Q6 | Authentication protocol to replace p9sk1? | Augmented PAKE over X25519 with AEAD tickets (dp9ik-inspired). Write it up and get it reviewed before implementing. | P5 |
| Q7 | Randomness API: `#c/random` only, or also a `getrandom` syscall? | Both: the syscall for early runtime and APE2, the file for scripts. | P2 |
| Q8 | ~~Which Ryzen board and CPU?~~ | **Resolved, [ADR 0009](adr/0009-reference-machines.md):** Ryzen 9 7900X on X870E AORUS ELITE WIFI7, shared with AbyssBSD. | — |
| Q9 | Can we get CIX P1 NPU (Zhouyi) and GPU (Immortalis-G720) documentation and firmware licensing? | Contact the vendor early. Panthor/Arm docs cover the GPU; the Zhouyi NPU may be blocked. | P9 |
| Q10 | ~~AMD64 baseline ISA level?~~ | **Resolved, [ADR 0007](adr/0007-amd64-baseline-x86-64-v2.md):** x86-64-v2, with runtime dispatch for newer extensions. | — |
| Q11 | Orange Pi 6 Plus ACPI gaps? | Mostly answered by AbyssBSD's bring-up: ACPI is usable. Known gaps: no SPCR (use DBG2), dangling `I2C0.UXC*` references, clocks via AML→PCC→SCMI. Handle them in code; no device tree needed. | P7 |
| Q12 | Kernel wait primitive for multiplexing many fds (needed by APE2 `poll` and useful natively)? | Prototype in P3 using helper procs (Plan 9 style); add a kernel primitive only if measurements demand it. | P8 |
| Q13 | Kernel-resident vs user-space drivers for storage/net on the hot path? | Kernel for NVMe/virtio/NICs (performance, boot); user-space via `#H` for GPU/NPU/USB class drivers. | P4 |
| Q14 | Keep `troff`/`postscript` documentation tooling? | Keep `man` working (a smaller man-macro renderer); defer full troff. | Later |
