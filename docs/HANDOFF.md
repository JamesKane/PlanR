# Handoff

Where work stands, for picking it up in a new session. Last updated
2026-10-18, after slice 10.

## State

- **Phase:** P1 (boot and kernel bring-up). Slices 1–10 are done and
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
  10. Stack-overflow reports on both arches (arm64 vectors test bit 16 of
      the frame address and switch to an overflow stack); the boot CPU
      moves to a guarded kernel stack.
- **Tree:** clean. `cargo xtask ci` is green: fmt, pedantic clippy with
  `-D warnings`, the dependency check, host tests, and every QEMU boot
  test on every profile.
- **The plan of record** is `docs/ROADMAP.md`. Its P1 section lists what
  each slice did and, under **Remaining**, what is left.

## Next steps (P1 remaining)

In the suggested order:

1. **ACPI dump corpus** in `oracle/acpi-corpus`: QEMU on both arches now,
   then the boards.
2. **Reference boards:** check SMP, the CPU protections, the framebuffer
   console and log-ring survival on the Orange Pi 6 Plus and the Ryzen.

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
