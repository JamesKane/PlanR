# Handoff

Where work stands, for picking it up in a new session. Last updated
2026-10-18, after slice 11 and both reference boards.

## State

- **Phase:** P2 (kernel core), started 2026-10-18. The slice plan is in
  `docs/ROADMAP.md` under P2. Done so far:
  1. Frame allocator: `lib/frames` (binary buddy, orders up to 1 GiB,
     free lists threaded through free pages, one state byte per page in
     each arena's first pages). `mm::alloc_frame`, `alloc_frames(order)`
     (zeroed) and `free_frames`; a bad free panics.
- **P1** is done apart from BTI, which waits on the toolchain. Its slices
  and the board results are in `docs/ROADMAP.md` and in git history
  (`5905e4d`..`96d70d8`).
- **Tree:** `cargo xtask ci` is green: fmt, pedantic clippy with
  `-D warnings`, the dependency check, host tests, and every QEMU boot
  test on every profile.

## Next steps

Slice 2, the kernel heap: a `#[global_allocator]` of ours on top of
`mm::alloc_frames` (size classes for small objects, whole blocks for
large ones), then `extern crate alloc` in the kernel. After that, slice 3
(`trait Mmu`), and so on down the ROADMAP list. Each slice gets its own
commit after `cargo xtask ci`, and this file is updated with it.

**Reference boards** (`docs/HARDWARE.md`, "Booting Plan R on a board"):
no runs until P2's exit test. P1 results, for reference:
   - **Orange Pi 6 Plus: passed on 2026-10-18** with `scripts/hwcheck.ini`:
     EL2 entry, PAN, 12 of 12 CPUs, DBG2 PL011 console, 1920×1080
     framebuffer, 17 of 17 in-kernel tests. It clears RAM on every reset
     (even warm PSCI `SYSTEM_RESET2`), so it has no log ring; its serial
     header is the post-mortem console (no USB-TTL adapter yet).
   - **Ryzen: passed on 2026-10-18** with `hwcheck.ini`: SMEP/SMAP/UMIP,
     24 of 24 CPUs, xAPIC as the firmware leaves it, two I/O APICs, TSC
     4691 MHz via HPET, RTX 4070 framebuffer at 0xf800000000, 21 of 21
     tests. The log ring survives the FADT reset (`hwreset.ini`), which is
     its post-mortem path.

## Things to know

- **User memory:** with SMAP or PAN on, the kernel may touch a user page
  only through `arch::probe_user_read` (a `stac`/`clac` or PAN window
  with a fixup). P2's copy-in/copy-out should grow from it. `stac`,
  `clac` and `msr pan` are undefined without the feature, so they sit
  behind the `protect::SMAP` / `protect::PAN` flags.
- **BTI is deferred:** it needs `bti` landing pads, and the stable toolchain
  cannot emit them (`-Z branch-protection` is nightly only; the prebuilt
  `core` has none).

- **Kernel images come from `build/<arch>/kernel`,** not cargo's output:
  `xtask` copies the linked kernel there with the symbol table written in.
  The block is 512 KiB (debug kernels use about 200 KiB); `xtask` prints
  the usage and fails with a clear error if it overflows.
- **What the board ACPI sets show** (`oracle/acpi-corpus`):
  - The Orange Pi has no SPCR. Its console comes from DBG2: a PL011 at
    0x040D_0000, `\_SB.COM2`.
  - Its IORT has named components (the DPUs) with several single
    mappings each, one per stream ID. `map_id` returns only the first,
    so P4's SMMU binding needs an iterator over all of a node's IDs.
  - Its IORT also has a type-6 RMR (reserved memory range) node, which
    is parsed only as `Other(6)`.
  - Neither board's PPTT lists caches.
  - The Orange Pi's MCFG has three windows on segment 0 that share one
    ECAM base, with different bus ranges.
  - The Ryzen's MADT matches Linux's APIC IDs exactly.
- **9boot allocates only `EfiLoaderData`.** The Ryzen's AMI firmware
  (F2) hangs in ExitBootServices once memory of an OS-defined type
  (`0x8000_0000` and up) is in use, so Plan R's own types are gone. The
  final map reports loader memory as `Boot`, and 9boot carves the kernel
  and the log ring out by address (`bootinfo::carve`). It keeps the ring
  out of the direct map by address too. Diagnosing it took
  `bootdebug=exittest` (reset right after the exit) and the framebuffer
  marks: 9boot's squares 0–2 and the kernel's from 3 on.
- **EL2 must set VPIDR_EL2/VMPIDR_EL2** before dropping to EL1. EL1
  reads MIDR/MPIDR from them, and they reset to unknown values. The Orange
  Pi's A520s read garbage affinities until both 9boot and the AP entry set
  them (`b17a270`). QEMU resets them to the real values, so no CI test
  catches a regression.
- **Panic backtraces stop after one frame:** the prebuilt `core` has no
  frame pointers, so the chain breaks in the panic machinery. Trap
  backtraces are unaffected.
- **Frames:** usable RAM below 1 MiB (`mm::FLOOR`) is never handed out;
  `mm::low_pages` finds the amd64 AP trampoline there. Frames may now come
  from anywhere, including above 4 GiB. The `Boot` and `AcpiReclaimable`
  regions are not yet given to the allocator: that waits for the
  kernel's own page tables (slice 3) and copies of what it needs. The
  per-page state byte has spare bits, but CoW refcounts (slice 9) will
  need a separate array.
- **Kernel stacks** come only from `mm::alloc_stack`, in 128 KiB slots.
  arm64's overflow check relies on every stack an exception can arrive on
  following that layout and being `mm::STACK_PAGES` long. A future thread
  stack must too. The overflow path clobbers `TPIDR_EL0` and
  `TPIDRRO_EL0`. That is harmless while it is fatal, but P2 must keep it
  fatal or save them first.
- **Workflow:** run `cargo xtask ci` before every commit. Boot tests are
  listed in `BOOT_TESTS` in `xtask/src/main.rs`, and the in-kernel tests
  are registered in `kernel/src/port/ktest.rs`. `cargo xtask qemu --arch
  amd64 [--gui]` boots interactively.
- **Dependency rule (ADR 0001):** the shipped code may use only `core` and
  `alloc`. Host tools (QEMU, mtools, OpenSSL as an oracle) are fine, but
  `xtask` has no crates.io dependencies either (ADR 0006).
- **Boot protocol (ADR 0010):** the direct map (HHDM) is at
  `0xffff_8000_0000_0000`, the MMIO window at `0xffff_c000_0000_0000`,
  and the stack window at `0xffff_c100_0000_0000`. `BootInfo` is at
  version 2.
- **The log ring:**
  - 1 MiB, placed at the highest 2 MiB-aligned site in free RAM, in
    UEFI memory type `0x8000_0003`;
  - 9boot saves the previous boot's ring to `\planr\last.log`, but only
    by overwriting a file that already exists, never by creating one.
    xtask images carry an empty one; keep that rule.
- **amd64 PAT:** entry 1 (PWT alone) is reprogrammed to write-combining
  for the framebuffer and the ring. Don't use PWT-only mappings for
  anything else.
- **Shell quirk:** `ls` is aliased to eza; use `command ls` in scripts.

## Open caveats

- TSC calibration falls back to the PIT, but some recent Intel platforms
  clock-gate the 8254 by default. If a board has neither an HPET nor a
  working PIT, add the ACPI PM timer (FADT `X_PM_TMR_BLK`) as a further
  source; the kernel reports the failure rather than guessing.

- Whether the log ring survives a warm reset on real hardware is untested.
  The Ryzen may retrain its memory and clear it, and either firmware may
  overwrite it.
- 9boot now writes `last.log` to its own boot volume. Early USB runs on
  the boards are therefore no longer strictly read-only.
- The console refuses a framebuffer that overlaps usable RAM rather than
  reserving it.

## Hardware safety (from `docs/HARDWARE.md`, always in force)

- **Ryzen disks:** never write to the Fedora NVMe or the data disks
  (2× WD 8 TB RAID0 and an HGST 6 TB).
- **Media writing:** tools that write media must refuse every target
  except a designated Plan R medium, identified by GPT GUID or serial,
  never by device path.
- **Foreign disks** attach read-only.
- **Root on the boards:** root steps go through scripts that the user
  runs. The boards are the Orange Pi at 192.168.0.27 and Ryzen Fedora at
  192.168.0.30, user `jkane`.
- **Boot entries:** add our own and use BootNext; never edit Fedora's.
- **Before experimenting** on hardware, read the ground truth.
