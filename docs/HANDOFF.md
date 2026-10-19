# Handoff

Where work stands, for picking it up in a new session. Last updated
2026-10-18, after slice 11 and the first Orange Pi run.

## State

- **Phase:** P1 (boot and kernel bring-up). Slices 1–11 are done and
  committed, one commit per slice:
  1. `5905e4d` 9boot loads the kernel; it boots on amd64 and arm64.
  2. `a2cb1f7` trap vectors, fault fixups, backtraces, in-kernel tests.
  3. `eb0f819` interrupt controllers, timers, the clock.
  4. `7e920c9` SMP bring-up on both architectures.
  5. `142373e` framebuffer console and persistent log ring.
  6. `a4ee281` CPU protections: SMEP/SMAP/UMIP on amd64, PAN on arm64.
     BTI is deferred; see below.
  7. `ef81e22` MCFG, PPTT and IORT parsing in `lib/acpi` (`tables.rs`, `pptt.rs`,
     `iort.rs`), with host tests and a boot-report summary.
  8. `b80bb23` PIT fallback for TSC calibration (`drivers/pit.rs`);
     `tsc=pit` forces it.
  9. `faacfe1` Symbolized backtraces: `xtask` writes a demangled symbol table
     (`lib/ksyms`, `xtask/src/demangle.rs`) into a block `link.ld`
     reserves.
  10. `7e61597` Stack-overflow reports on both arches (arm64 vectors test bit 16 of
      the frame address and switch to an overflow stack); the boot CPU
      moves to a guarded kernel stack.
  11. `8aba1b4` ACPI corpus: `oracle/acpi-corpus` holds the QEMU q35 and
      virt tables (`cargo xtask acpidump`), checked by `lib/acpi`'s corpus
      test. The Ryzen (`ryzen-x870e`) and Orange Pi (`opi6p`) sets
      followed in the next commit.
- **Tree:** clean. `cargo xtask ci` is green: fmt, pedantic clippy with
  `-D warnings`, the dependency check, host tests, and every QEMU boot
  test on every profile.
- **The plan of record** is `docs/ROADMAP.md`. Its P1 section lists what
  each slice did and, under **Remaining**, what is left.

## Next steps (P1 remaining)

In the suggested order:

1. **Reference boards** (procedure in `docs/HARDWARE.md`, "Booting Plan R
   on a board"):
   - **Orange Pi 6 Plus: passed on 2026-10-18** with `scripts/hwcheck.ini`.
     It entered at EL2 and ran at EL1, with PAN on, 12 of 12 CPUs online,
     the DBG2 PL011 console chosen, and the 1920×1080 framebuffer at
     scale 1. The log ring attached and all 17 in-kernel tests passed.
     Serial output itself is unobserved (no USB-TTL adapter yet).
   - **Log ring on the Orange Pi: not possible.** RAM is cleared on every
     reset, even PSCI `SYSTEM_RESET2` (warm). The `hwreset.ini` run found
     the ring and all 12 canary pages zeroed, so this is a board
     limitation (`docs/HARDWARE.md`). The serial header is that board's
     post-mortem console. The test path built for it stays:
     - 9boot's BootNext second boot, with a marker in `last.log` against
       loops;
     - the lost-ring and canary report;
     - the warm PSCI reset;
     - the ring cache flush.
   - **Ryzen: passed on 2026-10-18** with `hwcheck.ini`, after the AMI
     ExitBootServices fix (see below). It showed:
     - SMEP, SMAP and UMIP on, and 24 of 24 CPUs online;
     - the local APIC in xAPIC mode, as the firmware leaves it, and two
       I/O APICs;
     - the TSC at 4691 MHz, calibrated against the HPET, with the APIC
       timer one-shot (no TSC-deadline);
     - the RTX 4070's 1920×1080 framebuffer at 0xf800000000;
     - all 21 in-kernel tests passing.
     Still to check: log-ring survival (`hwreset.ini`). Its FADT reset
     register is an SMI command (port 0xB2, value 0xBE), and the fallback
     writes 6 (hard reset) to port 0xCF9; if RAM is lost, try 4 (soft
     reset).

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
