# Roadmap

Phases are ordered by dependency, not by calendar. Each one ends with an
**exit criterion** that can be shown in QEMU (later on hardware) and
checked by an automated test. Some tracks run in parallel; see
[Parallel tracks](#parallel-tracks).

```
P0 Foundations
 └─ P1 Boot & bring-up (amd64 + arm64, QEMU)
     └─ P2 Kernel core (procs, namespaces, Chan/Dev, syscalls)
         ├─ P3 Userland bootstrap (libr, rc, core cmds, lib9p)
         │   ├─ P5 Network & distribution (/net, auth, cpu, TLS)
         │   │   └─ P6 Graphics & interactive (draw, rio, acme, sam)
         │   └─ P8 Toolchain (APE2, Rust std port, self-host)
         └─ P4 Devices & storage (PCIe, virtio, NVMe, xHCI, fs)
             └─ P7 Real hardware (Ryzen, Orange Pi 6 Plus)
                 └─ P9 Accelerators & SoC (IOMMU, GPU, NPU, DPU, power)
                     └─ P10 Hardening → 1.0
```

---

## P0: Foundations

Set up the repository and the habits the whole project depends on.

- Cargo workspace layout (see [ARCHITECTURE.md § Source tree](ARCHITECTURE.md#source-tree)).
- Enforce the dependency rule: a CI check (`xtask deps`) fails if any shipped
  crate depends on anything except `core`, `alloc` or other in-tree crates.
- Pin the Rust toolchain (`rust-toolchain.toml`), with targets
  `x86_64-unknown-none`, `aarch64-unknown-none` and `x86_64-unknown-uefi` /
  `aarch64-unknown-uefi`.
- `cargo xtask` for `build`, `image`, `qemu`, `test` and `deps`. It writes
  FAT ESP disk images itself, with no `mtools` dependency.
- QEMU runners: `q35` + OVMF, `virt` + edk2 (ACPI mode, `-machine virt,acpi=on`).
- Lints: `#![deny(unsafe_op_in_unsafe_fn, missing_docs)]` on public items,
  clippy pedantic in CI, and `rustfmt`.
- Process: ADRs (`docs/adr/`), a project license (Q3), a contribution guide.

**Exit:** `cargo xtask qemu --arch {amd64,arm64}` builds and boots a stub
loader that prints to the UEFI console, and CI runs it headless on every
commit.

## P1: Boot & bring-up

- **`9boot` (UEFI loader, ours):** our own UEFI bindings (protocol
  GUIDs, Boot/Runtime Services tables, GOP, Simple File System, Loaded Image).
  It loads the kernel ELF and a boot archive from the ESP, reads `plan9.ini`
  style config, gets the memory map, RSDP and GOP framebuffer, calls
  `ExitBootServices` and hands over a typed `BootInfo`.
- Kernel entry on both architectures: higher-half mapping, early serial
  (16550 on amd64; PL011 or the SPCR-described UART on arm64), early frame
  allocator and kernel heap (`alloc` global allocator, ours).
- **ACPI tables (ours):** RSDP/XSDT, MADT, FADT, MCFG, HPET, SPCR, GTDT,
  IORT, DMAR/IVRS, SRAT/SLIT, PPTT. Table parsing only. AML comes in P4.
- Interrupts: x2APIC/IOAPIC; GICv3 (redistributors, ITS later). Exceptions,
  a panic handler, and symbolized backtraces from frame pointers.
- Timers: TSC (calibrated via HPET/ACPI PM timer), local APIC timer; ARM
  generic timer.
- SMP bring-up: INIT-SIPI-SIPI on amd64, PSCI `CPU_ON` on arm64. Per-CPU
  data (`GS` / `TPIDR_EL1`).
- CPU security features turned on from day one: NX, SMEP/SMAP, UMIP;
  PAN, PXN, and BTI where present.

**Exit:** both architectures boot under QEMU with 4+ CPUs, every CPU checks
in, the timer interrupt ticks on all of them, and a kernel self-test suite
reports pass/fail through `isa-debug-exit` / PSCI `SYSTEM_OFF` status.

## P2: Kernel core

The Plan 9 `port/` layer, redesigned in Rust.

- Physical memory: buddy or bitmap frame allocator, NUMA-aware later.
  Virtual memory: page tables behind a `trait Mmu` per architecture.
- Processes: `Proc`, kernel stacks, context switch, a preemptive SMP
  scheduler with priorities and per-CPU run queues. Heterogeneous-core
  awareness is planned for P9. Plan 9 EDF real-time scheduling is deferred.
- Segments: text, data, bss, stack, shared and physical (`segattach`);
  demand paging, copy-on-write `rfork`.
- **Chan/Dev framework:** a `Dev` trait, `Chan`, `Qid`, `Dir`, typed
  `OpenMode`/`Perm`, mount table and `Pgrp`/`Fgrp`/`Egrp`, `bind`/`mount`
  with `MREPL|MBEFORE|MAFTER|MCREATE` as a typed enum and flags.
- First devices: `#c` cons, `#p` proc, `#e` env, `#|` pipe, `#s` srv,
  `#d` dup, `#/` root (boot archive), `#M` mnt (9P client), `#r` rtc,
  `#P` arch.
- The new syscall ABI ([ADR 0002](adr/0002-new-syscall-abi.md)), defined
  once in `planr-abi` and shared by kernel and `libr`.
- ELF loader (static, PIE), `exec`, `exits`/`await`, notes
  (`notify`/`noted`), `rendezvous`, `semacquire`/`tsemacquire`, `nsec`,
  `alarm`, and per-process TLS (`FS` base / `TPIDR_EL0`).
- `qio` queues, `Block`s, sleep/wakeup, `QLock`/`RWLock`/spinlocks as
  RAII types.

**Exit:** `init` from the boot archive runs, builds a namespace, and starts
a set of user-mode test programs (fork, exec, pipe, bind/mount, notes, srv
plus a toy 9P server). All of them pass on both architectures.

## P3: Userland bootstrap

- **`libr`:** the Plan R runtime. Syscall wrappers, `print`/`fmt`
  (Plan 9 `%` verbs as a Rust formatting layer), `bio`-style buffered I/O,
  args (`ARGBEGIN` replacement), UTF-8/Rune, errors with `errstr`,
  threads/procs, channels (`libthread` model), regexp (`libregexp`
  semantics), `ndb`, `auth` stubs. Built on `core` + `alloc` only.
- **`lib9p` (ours):** a typed 9P2000 codec (`enum Tmsg`/`Rmsg`), server
  and client framework. The same codec is used by `#M` in the kernel.
- Commands, in order: `rc` (full rewrite, same grammar), `echo cat ls cp mv
  rm mkdir pwd bind mount unmount ns ps kill date sleep wc grep sed awk
  sort uniq tee test cmp diff tr du`, `mk`, `ramfs`, `srv`, `9fs`.
- Boot sequence: `boot`/`init`/`termrc`/`cpurc` equivalents and `newns`
  with `/lib/namespace`.

**Exit:** interactive `rc` on the serial console in QEMU. Oracle tests:
the same `rc` scripts and text-tool pipelines give byte-identical output
on Plan R and on host plan9port (see [TESTING.md](TESTING.md)).

## P4: Devices & storage

- PCIe enumeration via ACPI MCFG (ECAM), BARs, MSI/MSI-X.
- **AML interpreter (ours):** enough for `_PRT`, `_CRS`, `_STA`, `_INI`,
  `_OSC`, `_PS0/_PS3`, power buttons and shutdown/reboot (`_S5`).
  Fuzzed and host-tested against tables taken from QEMU and the target
  boards.
- virtio 1.x over PCI: `blk`, `net`, `rng`, `console`, `input`, `gpu`
  (2D first), `9p` (host share for development).
- Native drivers: NVMe, AHCI, xHCI (keyboard, mouse, mass storage, hubs).
- `#S` sd, `#u` usb as file servers, following the Plan 9 model.
- File systems: FAT (ESP, read/write, ours), a boot archive format, and
  **a new native disk file system** (Q5). Copy-on-write with checksums and
  snapshots. Inspired by venti/fossil and 9front's gefs, not compatible
  with them.

**Exit:** QEMU boots from a virtio/NVMe disk image with a persistent root;
`ls /dev/sd*` and USB HID work; reboot and power-off go through ACPI.

## P5: Network & distribution

- **IP stack (ours)** behind the familiar `/net` interface:
  `/net/{tcp,udp,icmp,ipifc,arp,ndp,…}/clone|ctl|data|status|local|remote`.
  IPv4 and IPv6 from the start, with modern TCP (SACK, window scaling,
  CUBIC or similar).
- `cs`, `dns`, `dial`/`announce`/`listen`, `ndb`, `ipconfig` (DHCPv4,
  SLAAC/DHCPv6).
- **Crypto library (ours):** SHA-2/3, BLAKE2/3, HMAC, HKDF, AES-GCM,
  ChaCha20-Poly1305, X25519, Ed25519, P-256, RSA (verify and legacy only),
  and a CSPRNG fed from `RDRAND`/`RNDR`/virtio-rng. Every primitive is
  differentially tested against host OpenSSL/libsodium plus Wycheproof
  vectors.
- **TLS 1.3 (ours)** with a kernel record layer (`#a`-style `devtls`) and
  a user-space handshake, as in Plan 9.
- **Auth:** `factotum`, `secstore`, `keyfs`, and an `authsrv` running a
  new PAKE-based protocol in place of p9sk1/DES (Q6).
- `exportfs`, `import`, `cpu`, `aan`; a `drawterm`-equivalent host client
  (Linux/macOS, in Rust, host-zone rules) so you can reach Plan R from
  existing machines.

**Exit:** two QEMU instances on a virtual network: `cpu -h other` gets a
shell with the caller's namespace imported; `import` of `/net`
works; TLS 1.3 interop with host OpenSSL `s_server`/`s_client` passes.

## P6: Graphics & interactive

- Framebuffer from GOP, then virtio-gpu.
- `#i` draw device with `memdraw`/`memlayer` rewritten in Rust; the draw
  protocol modernized (32-bit color, alpha, high-DPI scale factor).
- `libdraw`, `libframe`, `libplumb` equivalents in `libr`.
- Fonts: free vector fonts (the Lucida fonts are not redistributable).
  A rasterizer we write that produces Plan 9-style subfonts on demand.
- `rio`, `acme`, `sam`/`samterm`, `plumber`, `page`, `stats`,
  `winwatch`, `faces` later.

**Exit:** a full `rio` session in QEMU with `acme` editing files and the
plumber routing; screenshots in CI compared against reference images.

## P7: Real hardware

Hardware order: **AMD Ryzen board first** (more mature UEFI/ACPI, closer to
QEMU), then **Orange Pi 6 Plus**. Details in [HARDWARE.md](HARDWARE.md).

- Ryzen: real ACPI quirks, x2APIC, HPET/TSC, NVMe, xHCI, an onboard NIC
  (Realtek/Intel, Q8), AMD GOP framebuffer, AMD-Vi IOMMU.
- Orange Pi 6 Plus: CIX P1 UEFI/ACPI firmware, GICv3, generic timer,
  SBSA/vendor UART, PCIe, NVMe, USB, RTL8126 5GbE, SMMUv3; big/mid/little
  CPU topology from PPTT.
- Netboot from a development machine (UEFI HTTP/PXE, or `9boot` fetching
  over TFTP/HTTP) to keep the edit-test loop fast.

**Exit:** both boards boot Plan R from local NVMe and from the network,
reach a `rio` desktop (framebuffer), and serve and use 9P over Ethernet.

## P8: Toolchain & self-hosting

Can start once P3 is done; runs in parallel with P4–P7. See
[TOOLCHAIN.md](TOOLCHAIN.md).

1. Tier-3 Rust targets `x86_64-unknown-planr` / `aarch64-unknown-planr`
   (custom target JSON first, upstream later).
2. **APE2:** a POSIX environment over Plan R syscalls, written in Rust
   (`core` + `alloc`) and exporting a C ABI. It replaces the old APE so
   standard C/C++ can be built.
3. The upstream Rust `std` "sys" backend for Plan R, implemented on APE2 or
   directly on `planr-abi`. It lives in foreign code, per Q2.
4. Cross-built LLVM/clang, `rustc` and `cargo` from Linux, then
   native builds on Plan R.

**Exit:** Plan R rebuilds its own kernel and userland, on Plan R, with
results identical to the cross-build (reproducible builds).

## P9: Accelerators & SoC

- IOMMU subsystem (AMD-Vi, SMMUv3) with a per-file-server DMA domain.
- **User-space driver framework:** a new `#H` (hardware) device giving a privileged
  file server MMIO segments, interrupts as readable files and IOMMU-bound
  DMA buffers.
- **Accelerator file interface** (`/dev/accel/N/…`, see
  [HARDWARE.md § Accelerator model](HARDWARE.md#accelerator-model)): one
  uniform shape for GPU compute, NPU and DPU, with buffers shared as
  segments.
- GPU: Mali (CIX P1) and AMD RDNA. Display/KMS first, then compute, then
  3D. Open Linux drivers (Panthor, amdgpu) are documentation to read, not
  code to port.
- NPU: CIX NPU and AMD XDNA, depending on documentation (Q9).
- DPU and offload: NIC checksum/TSO offload, then SmartNIC/DPU as a 9P
  endpoint.
- Power: ACPI CPPC, P/C-states, DVFS, thermal zones, `/dev/power`,
  suspend-to-RAM later. A heterogeneous-core scheduler (energy-aware
  placement).

**Exit:** a user program submits a compute job to the GPU or NPU on the
Orange Pi through `/dev/accel` and checks the result; `cpu` into a Ryzen
box and use its accelerator remotely through the imported namespace.

## P10: Hardening → 1.0

Fuzzing campaigns (9P, syscalls, AML, net, fs), an external crypto review,
`unsafe` audit, KASLR, Armv9 MTE where supported, documentation (man
pages in the Plan 9 style), installer, and update tooling (a `replica`
successor).

---

## Parallel tracks

| Track | Starts after | Runs alongside |
|---|---|---|
| Kernel (P1→P2→P4) | P0 | everything |
| Userland & `libr` (P3) | P2 syscalls stable | P4 |
| Crypto & TLS library | P0 (pure `no_std`, host-tested) | P1–P4 |
| Net stack core (host-tested on TAP) | P0 | P1–P4 |
| AML interpreter (host-tested) | P0 | P1–P3 |
| Graphics (`memdraw` host-tested to PNG) | P0 | P2–P5 |
| Toolchain (P8) | P3 | P4–P7 |
| Hardware bring-up (P7) | P4 | P5, P6 |

Many subsystems are pure `no_std` libraries (crypto, 9P codec, AML, net,
memdraw, fs format), so they can be built and tested on the host before the
kernel can run them. That is the main way the schedule gets shorter.

## Near-term work: P0 and P1 backlog

1. Choose a license (Q3) and commit an initial workspace skeleton.
2. `xtask`: build loader and kernel, write a FAT32 ESP image, launch QEMU
   with OVMF/AAVMF, map the exit status to a test result.
3. `uefi-raw` (ours): minimal UEFI types (System Table, Boot Services,
   GOP, SFS, File, memory map).
4. `9boot`: load `/planr/kernel` ELF and `/planr/boot.arc`, build
   `BootInfo`, exit boot services, jump.
5. `kernel` stub for each architecture: serial `print!`, panic, page tables,
   heap.
6. `acpi` (ours): RSDP→XSDT walk, MADT and SPCR parsing, with host tests on
   table dumps from QEMU (`-acpitable` / `acpidump` captures).
7. Interrupts and timer on both architectures; SMP bring-up.
8. A kernel test runner (`#[test_case]`-style, ours) wired to CI.
