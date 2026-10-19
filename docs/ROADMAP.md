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

## P0: Foundations ✅ (2026-10-17)

Set up the repository and the habits the whole project depends on.

**Status:** exit criterion met. `cargo xtask ci` runs fmt, pedantic clippy,
the dependency-rule check, host unit tests (UEFI layouts, GPT, FAT32, JSON,
CRC-32) and headless QEMU boot tests on amd64 and arm64. 9boot reports
firmware, memory map, ACPI RSDP and GOP on both architectures, from a
GPT/FAT32 image our own code writes (checked with `fsck.vfat`). Release
images are reproducible byte for byte. Still open: a hosted CI service
(there is no remote yet) and a contribution guide.

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
- Process: ADRs (`docs/adr/`), MIT license (ADR 0008), a contribution guide.

**Exit:** `cargo xtask qemu --arch {amd64,arm64}` builds and boots a stub
loader that prints to the UEFI console, and CI runs it headless on every
commit.

## P1: Boot & bring-up (in progress)

**Status (2026-10-18), first slice done:** 9boot loads the kernel ELF, builds
the boot page tables (identity, direct map, W^X kernel), exits boot services
and enters the kernel per [ADR 0010](adr/0010-boot-protocol.md), including
the arm64 EL2→EL1 drop with and without VHE. The kernel validates
`BootInfo`, parses ACPI (RSDP/XSDT, MADT, FADT, SPCR, DBG2; `lib/acpi`,
host-tested), picks its console from SPCR → DBG2 → COM1 (16550 or PL011,
MMIO-mapped through its own page-table code), and in test mode reports and
powers off (isa-debug-exit / PSCI). CI boots amd64 plus three arm64
profiles (EL2+VHE, EL2 without VHE, EL1).

**Second slice done:** traps. amd64 has its own GDT, TSS (IST stacks for
double fault, NMI, machine check) and a 256-vector IDT; arm64 has a
`VBAR_EL1` table with `SError` unmasked. Both decode the trap into a typed
`Exception`, resume breakpoints, recover faults listed in a linker-built
fixup table (the basis for `copyin`/`copyout`), and otherwise print a
register dump and frame-pointer backtrace. An in-kernel test runner
(`test=1`) and a negative trap test (`test=trap`) run in CI.

**Third slice done:** interrupts and the clock. amd64 masks the 8259s and
runs the local APIC in x2APIC mode where available (xAPIC otherwise), with
I/O APIC routing of GSIs and ISA overrides. The TSC is calibrated against
the HPET (or CPUID leaf 0x15), and the APIC timer runs in TSC-deadline mode
where available (one-shot count otherwise). arm64 brings up a GICv3
distributor, redistributor and system-register CPU interface (9boot opens
`ICC_SRE_EL2`), with the EL1 virtual timer on the PPI from the GTDT. A
portable interrupt table dispatches by vector. `SpinLock` holds interrupts
off (Plan 9's `ilock`). The clock ticks at `HZ`=100 from a one-shot timer
re-armed at absolute deadlines. In-kernel tests cover a self-IPI, the tick
rate and (amd64) an HPET interrupt through the I/O APIC. The
`legacy-apic` boot test forces xAPIC and the count timer, because KVM
would otherwise hide them.

**Fourth slice done:** SMP. Each CPU has a portable `Cpu` (index, hardware
ID, its own tick, a cross-CPU call slot), found through `GS` (amd64) or
`TPIDR_EL1` (arm64). amd64 starts each MADT processor with INIT and
start-up IPIs to a trampoline copied below 1 MiB, with a low copy of the
top-level page table because 32-bit CR3 cannot reach above 4 GiB; each CPU
gets its own GDT, TSS and IST stacks. arm64 starts each GICC with PSCI
`CPU_ON` at a position-independent entry that handles firmware starting it
at EL2 (VHE or not) or EL1, then attaches its own redistributor. Every CPU
runs its own timer; CPU 0's ticks are the system's. Kernel stacks for new
CPUs live in their own window above unmapped guard pages. In-kernel tests
check that every listed CPU is online and ticking, that a cross-CPU call
runs on its target, and that a spin lock hammered by all CPUs loses no
updates; a `uniprocessor` boot test covers the one-CPU path. This meets
P1's exit criteria under QEMU.

**Fifth slice done:** console output for machines without a serial port.
The kernel draws its console as text on the GOP framebuffer (`lib/fbtext`:
an 8×16 font drawn for Plan R, scaled up on wide screens, mapped
write-combining via the PAT on amd64 and Normal-NC on arm64), and copies
it into a 1 MiB persistent log ring (`lib/logring`). 9boot places the ring
at the top of RAM, finds the previous boot's ring by checking each 2 MiB
boundary of free memory for a valid header, saves it to `\planr\last.log`
and echoes its last line. In-kernel tests read the text back from the
screen and the ring; the `log-ring` boot test resets warm (FADT reset
register on amd64, PSCI `SYSTEM_RESET` on arm64) and checks that the next
boot recovered the log.

**Sixth slice done:** CPU protections, each turned on per CPU where the
CPU reports it: SMEP, SMAP and UMIP on amd64 (NX and `CR0.WP` were already
on from 9boot), PAN on arm64 with `SCTLR_EL1.SPAN` cleared so every
exception entry sets it (PXN/UXN were already on every kernel mapping).
`stac`/`clac` and the PAN writes fault on CPUs without the feature, so
they sit behind a runtime flag; the amd64 trap entry runs `clac` and a
fixup resume closes the window. `probe_user_read` is the one path through
which the kernel touches user memory. In-kernel tests read each feature
back on every CPU (reporting `skipped` where the CPU lacks it) and check
that a plain kernel read of a lower-half user page faults while the
user-access read works. **BTI is deferred:** it needs `bti` landing pads,
which the stable toolchain cannot emit (`-Z branch-protection` is nightly
only), and the prebuilt `core` has none.

**Seventh slice done:** MCFG, PPTT and IORT parsing in `lib/acpi`. MCFG
gives the ECAM windows and each function's configuration-space address.
PPTT gives each processor's path through cores, clusters and packages
and the caches it can use, with levels counted outwards and shared caches
listed once. IORT decodes ITS groups, SMMUs, root complexes and named
components, and follows ID mappings to give a device's SMMU stream ID and
its ITS device ID. Walks over both graph tables are bounded, so a cyclic
table ends them. Host tests cover each table, including cycles and bad
references; the kernel's boot report summarizes all three.

**Eighth slice done:** a PIT fallback for TSC calibration. The TSC
frequency comes from CPUID, else the HPET, else a 50 ms one-shot count on
PIT channel 2 (whose gate and output sit on port 0x61); a PIT that does
not count is reported, not trusted. `tsc=hpet` or `tsc=pit` forces a
source, and the `pit-calibration` boot test uses the latter. An in-kernel
test checks that the frequency in use is within 1% of a fresh HPET
measurement. The HPET now starts counting when it is found, not only when
it calibrates the TSC.

**Ninth slice done:** symbolized backtraces. Each linker script reserves a
512 KiB zero-filled block in rodata; after the link, `xtask` reads the ELF
symbol table (`lib/elf`), demangles the names with its own v0 demangler,
and writes a sorted address-to-name table (`lib/ksyms`) into a copy of the
image, so no address moves. Frames print as `name+offset`, looked up at
`ret - 1` so a call ending a function names the caller. An image built
without `xtask` prints bare addresses. The demangler's host tests compare
every symbol of the built kernels against `nm -C` when binutils is
installed; the trap boot test now expects `kernel::kmain+0x`.

**Tenth slice done:** stack-overflow reports. Kernel stacks now sit in
128 KiB slots of their own, ending at the midpoint, so at least 64 KiB of
unmapped guard lies below each, and on arm64 a stack pointer within a stack
has bit 16 clear. arm64 has no IST, so each vector tests that bit before
saving anything (stashing x0 in sp to do it without memory) and, if it is
set, moves to the CPU's overflow stack: a static one for the boot CPU,
allocated for the others before they start. amd64's double fault, already
on an IST stack, now names an overflow when the saved stack pointer is
unmapped. The boot CPU leaves 9boot's unguarded stack for a kernel stack
right after `mm::init`, continuing in `boot_main`. `test=overflow`
recurses until the stack runs out; the `overflow` and `overflow-boot-cpu`
boot tests run it on an application processor and on the boot CPU.

**Eleventh slice done:** the ACPI corpus. `test=acpidump` prints the
RSDP, the XSDT, every listed table and the DSDT and FACS in hex.
`cargo xtask acpidump` boots QEMU with it and writes the tables to
`oracle/acpi-corpus/qemu-q35` and `qemu-virt`, named as Linux's sysfs
names them, with each table's physical address. `capture.sh` copies a
board's tables from sysfs into the same layout. A host test in `lib/acpi`
replays every set (placing tables itself when there are no addresses) and
compares the parse with a checked-in `summary.txt`. Its first run found
DBG2 device fields read two bytes early, a bug the synthetic unit test
shared. The `acpidump` boot test keeps the dump path working.

**Reference boards done (2026-10-18).** Both pass `scripts/hwcheck.ini`
from a USB stick:
- **Orange Pi 6 Plus:** 12 of 12 CPUs and 17 tests. It needed EL2 to set
  VPIDR/VMPIDR and DBG2 parsing fixed. It clears RAM on every reset, so it
  has no log ring.
- **Ryzen:** 24 of 24 CPUs and 21 tests, and the log ring survives a
  reset. It needed 9boot to drop OS-defined memory types, which hang this
  AMI firmware's ExitBootServices, and to take the console display's
  framebuffer.

**Remaining:** BTI once the toolchain can emit landing pads.

- **`9boot` (UEFI loader, ours):** our own UEFI bindings (protocol
  GUIDs, Boot/Runtime Services tables, GOP, Simple File System, Loaded Image).
  It loads the kernel ELF and a boot archive from the ESP, reads `plan9.ini`
  style config, gets the memory map, RSDP and GOP framebuffer, calls
  `ExitBootServices` and hands over a typed `BootInfo`.
- Kernel entry on both architectures: higher-half mapping, early console,
  early frame allocator and kernel heap (`alloc` global allocator, ours).
  arm64 entry handles **EL2** (as on the Orange Pi): minimal EL2 stub, then
  drop to EL1.
- Early console from the SPCR UART, else **DBG2** (neither reference
  board has SPCR), else COM1 on amd64; plus a **GOP framebuffer console**
  (the Ryzen has no serial port) and a **persistent log ring** that 9boot
  saves to the ESP after a warm reset.
- **ACPI tables (ours):** RSDP/XSDT, MADT, FADT, MCFG, HPET, SPCR, GTDT,
  IORT, DMAR/IVRS, SRAT/SLIT, PPTT. Table parsing only. AML comes in P4.
- Interrupts: xAPIC **and** x2APIC (the X870E firmware leaves xAPIC on), IOAPIC; GICv3 (redistributors, ITS later). Exceptions,
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

## P2: Kernel core (done 2026-10-19)

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
  once in `lib/abi` and shared by kernel and `libr`.
- ELF loader (static, PIE), `exec`, `exits`/`await`, notes
  (`notify`/`noted`), `rendezvous`, `semacquire`/`tsemacquire`, `nsec`,
  `alarm`, and per-process TLS (`FS` base / `TPIDR_EL0`).
- `qio` queues, `Block`s, sleep/wakeup, `QLock`/`RWLock`/spinlocks as
  RAII types.

**Exit:** `init` from the boot archive runs, builds a namespace, and starts
a set of user-mode test programs (fork, exec, pipe, bind/mount, notes, srv
plus a toy 9P server). All of them pass on both architectures.

**Slices** (agreed 2026-10-18; one commit each):
1. ✅ Frame allocator: `lib/frames`, a binary buddy allocator over the boot
   map's free RAM, host-tested; `mm::alloc_frames`/`free_frames`.
2. ✅ Kernel heap: `lib/heap` (power-of-two size classes, 16 B to
   2 KiB, in slabs; larger requests take whole blocks) behind our
   `#[global_allocator]`.
3. ✅ `trait Mmu` (`port/mmu.rs`): the kernel's own page tables, user
   address spaces (`Space`), start-up identity maps for the APs, and TLB
   shootdown (IPIs on amd64, broadcast TLBI on arm64).
4. ✅ Threads (`port/proc.rs`): kernel-thread `Proc`s on freeable
   `mm::Stack`s, the context switch on both arches, and cooperative
   round-robin `yield_now`.
5. ✅ Scheduler: preemptive SMP, per-CPU run queues with Plan 9's 20
   priorities, work stealing and wired threads; `Rendez` sleep/wakeup,
   `QLock`/`RwLock` taking `Sleepable`, and `rendezvous`.
6. ✅ User mode: `lib/abi` (call numbers, typed flags, the result
   pair), the syscall entry (`syscall`/`sysret` with `swapgs`, `svc #0`),
   checked copy-in/copy-out (`port/uaccess.rs`), `settls`, user FP/SIMD
   state on switch, and user threads entered by hand.
7. ✅ Segments, demand paging, ELF `exec`, `exits`/`await`, and the boot
   archive (`lib/bootarc`), with the first user programs on `lib/r`.
8. ✅ Chan/Dev core: `Dev`, `Chan`, `Qid`, `Dir`, `Pgrp`/`Fgrp`/`Egrp`,
   the mount table, `bind`/`unmount` (`mount` waits for `#M`); `#/ #c #e
   #d #r #P`; `exec` through the name space.
9. ✅ `rfork` with copy-on-write; `qio`/`Block`; `#|` and `#s`.
10. ✅ Notes, `#p`, `alarm`, `nsec`, `semacquire`/`tsemacquire`, `segattach`.
11. ✅ `#M` with a first 9P codec (`lib/9p`), `mount`, `fversion`, `fauth`.
12. ✅ The exit test: `init` and the user test programs, in CI on both arches,
    and on both reference boards (2026-10-19).

User test programs are small `no_std` Rust binaries built by `xtask`
on a first `lib/r` (the full libr is P3).

## P3: Userland bootstrap (in progress)

- **`libr`:** the Plan R runtime. Syscall wrappers, `print`/`fmt`
  (Plan 9 `%` verbs as a Rust formatting layer), `bio`-style buffered I/O,
  args (`ARGBEGIN` replacement), UTF-8/Rune, errors with `errstr`,
  threads/procs, channels (`libthread` model), regexp (`libregexp`
  semantics), `ndb`, `auth` stubs. Built on `core` + `alloc` only.
- **`lib9p` (ours):** a typed 9P2000 codec (`enum Tmsg`/`Rmsg`), server
  and client framework. The same codec is used by `#M` in the kernel.
- Commands, in order: `rc` (full rewrite, same grammar), `echo cat ls cp mv
  rm mkdir pwd bind mount unmount ns ps kill date sleep wc grep sed awk
  sort uniq tee test cmp diff tr du`, `mk`, `ramfs`. (`srv` and `9fs`
  dial the network, so they moved to P5 on 2026-10-19.)
- Boot sequence: `boot`/`init`/`termrc`/`cpurc` equivalents and `newns`
  with `/lib/namespace`.

**Exit:** interactive `rc` on the serial console in QEMU. Oracle tests:
the same `rc` scripts and text-tool pipelines give byte-identical output
on Plan R and on host plan9port (see [TESTING.md](TESTING.md)).

**Slices** (agreed 2026-10-19; one commit each):
1. ✅ Console input: UART receive interrupts (16550, PL011) into an input
   queue; Plan 9's line discipline in `#c/cons` (echo, backspace, ^U,
   ^D), `consctl` raw mode, DEL posting `interrupt` to the console's note
   group; a QEMU test that types on the serial line.
2. ✅ libr base: a user heap (`#[global_allocator]` on `brk`), `print!` and
   `fprint!` on `core::fmt` with helpers for `%q` and `%r`, an
   `ARGBEGIN` replacement, UTF-8/Rune, `bio`.
3. ✅ libr system layer: directory reads, stat helpers, `cleanname`, `/env`,
   exec with a `$path` search, `wait`, `postnote`, time and date,
   `tokenize`, `getfields`, `quote`.
4. ✅ Our lib9p server framework (fid table, dispatch, file trees) on
   `lib/9p`'s codec; `ramfs`, mounted by `init` on `/tmp`.
5. `rc`, part 1: lexer, recursive-descent parser for rc's grammar, the
   interpreter: commands, pipelines, redirections, `& ; && || !`, lists,
   `^`, `$#`, `$"`, quoting, globbing, `if`/`if not`/`for`/`while`/
   `switch`/`~`, functions, core builtins.
6. `rc`, part 2: here documents, `` `{} ``, `<{}`/`>{}`, `$status`,
   `$apid`, notes (`sigint`, `sigexit`), `rcmain`, `-c -e -i -l -x`,
   the prompt; interactive `rc` on the serial console in QEMU.
7. Oracle harness: `cargo xtask oracle` runs `oracle/rc/` scripts on host
   plan9port (`scripts/install-plan9port.sh`) and in QEMU and compares
   stdout, stderr and status, with an allow-file; in CI from then on.
8. File commands: `echo cat ls cp mv rm mkdir pwd touch chmod basename
   cleanname seq date sleep wc tee test cmp xd strings tail du`.
9. Name-space commands (`bind mount unmount ns ps kill`), `newns` with
   `/lib/namespace`, `root/` in the archive; the real `init` (name
   space, `termrc`, `rc -i`); the P2 test programs under `/sys/test`,
   run by rc scripts with `test=1`.
10. Regexp (`libregexp` semantics), `grep sed tr sort uniq`.
11. `awk` and `diff`.
12. `mk`.
13. The exit test: a scripted interactive `rc` session over serial and
    the full oracle corpus, in CI on both arches; `test=1` on both
    reference boards (interactive use there waits for USB HID in P4).

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
- `srv` and `9fs` (moved from P3: both dial a network address).
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

- Ryzen 9 7900X / X870E: 15 SSDTs of AMI AML, xAPIC, HPET/TSC (no
  TSC-deadline), NVMe, xHCI, **RTL8125**, GOP on the RTX 4070, FCH
  watchdog (WDRT), AMD-Vi. No serial, so framebuffer and log ring only.
  Writes go only to the designated Plan R NVMe (HARDWARE.md §5).
- Orange Pi 6 Plus: EL2 entry, DBG2 console, GICv3 + ITS (erratum
  2941627), generic timer, ECAM PCIe, NVMe, **ACPI-attached xHCI**,
  **RTL8126**, SBSA watchdog via `WOR`, three-class CPU topology from PPTT.
- One RTL8125/8126 driver serves both boards.
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
   directly on `lib/abi`. It lives in foreign code, per Q2.
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

1. ~~License and workspace skeleton~~ (done in P0).
2. ~~`xtask`: build, GPT/FAT32 image, QEMU with OVMF/AAVMF, test verdicts~~
   (done in P0; extend it to the kernel).
3. ~~`lib/uefi`: Simple File System, File, Loaded Image~~ (done).
4. ~~`9boot`: load the kernel, build `BootInfo`, exit boot services, jump~~
   (done; the boot archive comes with P2's `init`).
5. ~~`kernel` stub: console `print!`, panic, MMIO mapping~~ (done; heap
   with P2's allocator).
6. ~~`acpi`: RSDP→XSDT, MADT, FADT, SPCR, DBG2~~ (done, synthetic-table
   tests; add captured QEMU/board dumps to `oracle/acpi-corpus`).
7. Interrupts and timer on both architectures; SMP bring-up.
8. A kernel test runner (`#[test_case]`-style, ours) wired to CI.
