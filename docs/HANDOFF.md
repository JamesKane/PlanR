# Handoff

Where work stands, for picking it up in a new session. Last updated
2026-10-18, after P2 slice 6.

## State

- **Phase:** P2 (kernel core), started 2026-10-18. The slice plan is in
  `docs/ROADMAP.md` under P2. Done so far:
  1. Frame allocator: `lib/frames` (binary buddy, orders up to 1 GiB,
     free lists threaded through free pages, one state byte per page in
     each arena's first pages). `mm::alloc_frame`, `alloc_frames(order)`
     (zeroed) and `free_frames`; a bad free panics.
  2. Kernel heap: `lib/heap` size classes (16 B to 2 KiB, slabs of at
     least 8 objects, each slab a buddy block found by masking an object's
     address) behind `port/heap.rs`'s `#[global_allocator]`, one lock per
     class. Larger requests are whole frame blocks. `alloc` is usable in
     the kernel after `mm::init`.
  3. Page tables (`port/mmu.rs`, `trait Mmu` in each `arch/*/mmu.rs`).
     `mm::init` builds the kernel's own tables and switches to them: the
     direct map in 2 MiB blocks, the framebuffer if it is not RAM, and
     the image W^X from `__text_end`/`__rodata_end` in `link.ld`. The
     lower half is empty. A `Space` is a lower half. On amd64 every root
     shares the kernel half's 256 PDPTs, which are allocated up front; on
     arm64 the halves have their own TTBRs. `KERNEL_SPACE` is where a CPU
     with no process runs. APs start through a `StartupMap`, which maps
     the entry code at its physical address and is freed afterwards.
     `shootdown(mask, va, pages)` flushes the TLBs.
  4. Threads (`port/proc.rs`, `arch/*/context.rs`). `proc::spawn(name,
     f)` makes a kernel thread on an `mm::Stack` (freed on drop through
     `free_stack`, which unmaps, shoots down and reuses the slot).
     `yield_now` is cooperative round-robin over one global `READY`
     queue. A CPU's own context is not a `Proc`: it runs only when nothing
     is ready. A thread that was switched away from is requeued or freed
     only in `finish_switch`, run by the context switched to, so no other
     CPU can resume it half-saved. arm64 saves d8-d15 because its kernel
     target uses FP; amd64's is soft-float.
  5. Scheduler (`port/proc.rs`, `port/qlock.rs`):
     - Per-CPU queues by priority (20 levels, round robin within one).
       `ready` puts a thread on the least loaded CPU and sends a wake IPI
       if that CPU should switch now. Idle CPUs steal unwired threads.
     - Preemption: `proc::tick` marks a CPU once the quantum (2 ticks) is
       used up, and `proc::preempt` switches at IRQ exit (amd64 and arm64
       trap handlers).
     - `on_cpu` keeps a thread that is still switching out from being
       resumed elsewhere.
     - `Rendez::sleep(&Sleepable, cond)`, `wakeup` and `wakeup_all`;
       `QLock`/`RwLock` hand the lock straight to the next waiter (FIFO);
       `rendezvous(tag, value)` uses one global table until process
       groups exist.
     - The boot CPU now ends in `cpu::idle_loop`, like the APs, instead of
       halting.
     - Separately, `5487d7b`: arm64 trap frames now save q0-q31,
       FPCR and FPSR.
  6. User mode:
     - `lib/abi` (crate `abi`): call numbers for the whole Plan 9 set
       plus `settls`/`getrandom`, the register conventions, the result
       pair (`encode`/`decode`: value, then an error word that is 0 on
       success), and typed flags (`OpenMode`, `RforkFlags`, `BindFlags`,
       `Whence`).
     - amd64 entry: `syscall` lands in `planr_syscall_entry` (`arch/amd64/
       trap.rs`), which swaps GS, switches to `CpuLocal.kstack` (at
       `gs:16`, in `Cpu::arch`) and builds an ordinary `TrapFrame` with
       vector 256. It returns by `sysret` if rip is below `USER_TOP`,
       else by the trap exit's `iretq`. Traps from ring 3 `swapgs` on the
       way in and out. TSS `rsp0` and `kstack` are set at every switch.
     - arm64 entry: `svc #0` from EL0 arrives in `trap` as
       `(Origin::User, Svc(0))`; the kernel stack is just `SP_EL1`, which
       is the stack top whenever the thread is in user mode.
     - `port/syscall.rs`: `dispatch` runs with interrupts on. Done so
       far: `errstr` (swaps text both ways), `exits`, `settls`, `sleep`
       (yields; `sleep(n>0)` yields in a loop until slice 10 brings timed
       sleep). The rest fail with "system call not yet implemented", and
       unknown numbers with "bad system call". A user trap other than a
       syscall or an interrupt ends the thread with a `sys: trap: …`
       status (`user_trap`).
     - `port/uaccess.rs`: `copy_in`, `copy_out`, `copy_in_str`, all
       checked against `USER_TOP` (0x7fff_ffff_f000) before the arch copy
       (`rep movsb` in a `stac` window; `ldtrb`/`sttrb` on arm64), which
       stops at a fault through a fixup.
     - `proc::spawn_user(how, space, pc, sp, arg)`: a `Proc` with a
       `Space` and `UserRegs` (amd64: the `fxsave` area and FS base;
       arm64: `TPIDR_EL0` and, since slice 7, `SP_EL0`; the trap frame
       has the rest). Every
       switch activates the next thread's space (the kernel space for a
       kernel thread or a CPU's own context) and saves/loads user
       registers. `Space::activate` skips a space that is already current.
     - Tests: two user threads on one CPU (hand-written programs in
       `arch/*/context.rs` `ktests`) yield to each other 50 times and
       check their registers, SSE/NEON value and thread pointer, then
       `errstr` and `exits`; a third reads address 0 and is ended.
       Disabling the user-register save makes the first fail.
  7. Segments, demand paging, `exec`, `exits`/`await`, the boot archive:
     - `port/segment.rs`: a `Memory` is a `Space` plus sorted, disjoint
       `Segment`s (text, data, bss, stack), each owning the frames mapped
       in it and filled on first touch from an optional `&'static` image
       (zeros elsewhere). The trap handlers call `segment::fault` for a
       not-present (amd64) or translation (arm64) fault from user mode,
       and for one in a kernel user copy before taking its fixup, then
       retry. Frames go back when the `Memory` drops, which is safe without
       a shootdown because no CPU runs in the space by then and switching
       flushes the user half. `brk` moves the bss end (clamped as Plan 9
       does when the linked bss starts inside data's last page) and
       unmaps, flushes and frees what it gives up.
     - `port/exec.rs`: `load(path, argv)` finds `path` (minus its `/`) in
       the boot archive, checks the ELF (no W+X, nothing below 64 KiB or
       into the stack, at most one writable segment, entry in text) and
       builds text, data, bss and a 16 MiB stack below `USER_TOP`, with
       the arguments laid out as `lib/abi` documents (sp at argc, argv,
       null, strings above; 64 KiB at most). `exec::spawn` starts a
       program as a child of the caller.
     - `port/proc.rs`: a `Proc` has `mem: SpinLock<Option<Arc<Memory>>>`
       (taken by `finish_switch` once dead, so it is freed whoever holds
       the `Proc`), a name that `exec` changes, a parent (`Weak`), a count
       of live children and their `Wait` messages (at most 128 kept).
       `proc::exec` swaps in new memory and re-enters user mode at the
       top of the kernel stack; `await_child` sleeps on `child_exited`.
     - Calls: `exec`, `await` (Plan 9's text, `abi::format_waitmsg`; only
       real time is counted), `brk`. `dispatch` no longer holds the thread
       across a call, since `exits` and `exec` never return; it used to
       leak every exited thread.
     - `lib/bootarc`: the archive format (sorted table, page-aligned
       contents, clean relative names), host-tested. 9boot loads
       `\planr\boot.arc` into its own pages (`BootInfo` version 3,
       `archive`); `port/archive.rs` keeps it where it is.
     - User space: `lib/r` (libr: `_start`, `main!`, the calls, `Error`
       holding errstr text, exit on panic) and `cmd/utest` (`tmem`,
       `targs`, `texec`, `tfault`), linked by `lib/r/user.ld` at
       0x400000 for the `*-unknown-none` targets. `xtask build` packs them
       as `boot/<name>` (symbols kept, debug info stripped).
     - Tests: `exec: programs run from the boot archive` starts all seven
       runs at once under a kernel thread that awaits them and checks each
       wait message; `exec: programs' memory is freed` checks the free
       frame count comes back. The slice-6 programs now run in a `Memory`
       too, with a different stack pointer each, and check it survives.
- **P1** is done apart from BTI, which waits on the toolchain. Its slices
  and the board results are in `docs/ROADMAP.md` and in git history
  (`5905e4d`..`96d70d8`).
- **Tree:** `cargo xtask ci` is green: fmt, pedantic clippy with
  `-D warnings`, the dependency check, host tests, and every QEMU boot
  test on every profile.

## Next steps

Slice 8: the Chan/Dev core, the name space, and the first devices.
- `Dev` (Plan 9's `Dev` table as a trait: attach, walk, stat, open,
  create, read, write, remove, wstat), `Chan`, `Qid`, and `Dir` with
  Plan 9's stat encoding (likely in `lib/abi` or a small new crate,
  since `lib/9p` needs it in slice 11).
- `Pgrp` (the mount table, `bind` with `BindFlags`), `Fgrp` (file
  descriptors), `Egrp`; `walk` through mounts and unions.
- Calls: `open`, `create`, `close`, `read`, `write`, `pread`, `pwrite`,
  `seek`, `stat`, `fstat`, `wstat`, `fwstat`, `remove`, `fd2path`,
  `dup`, `chdir`, `bind`; `mount` waits for `#M` (slice 11).
- Devices: `#/` (the boot archive as a tree, replacing `exec`'s direct
  lookup in `port/archive.rs`), `#c` (cons), `#e` (env), `#d` (dup), `#r`
  (rtc), `#P` (arch).
- `exec` then opens its file through the name space. Segment images are
  `&'static [u8]` today, which only the archive can give; a file from
  anywhere else needs an owned image (or a page source) instead.

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
  only through `port/uaccess.rs` (or `arch::probe_user_read`). Its range
  check is what keeps a user pointer out of kernel memory; SMAP and PAN
  only keep the kernel out of user pages. `stac`, `clac` and `msr pan`
  are undefined without the feature, so they sit behind the
  `protect::SMAP` / `protect::PAN` flags; arm64's copies use the
  unprivileged `ldtrb`/`sttrb`, which PAN does not affect.
- **SYSRET on AMD takes SS's RPL from STAR:** STAR[63:48] must be
  `KERNEL_DS | 3`. Intel ORs in 3 itself and so does TCG, so only KVM on
  the Ryzen host showed it: a user SS of RPL 0 works until the first
  `iretq` back to user mode from a hardware frame (a demand-paging
  fault), which takes #GP(0x18).
- **arm64 traps do not save `SP_EL0`:** the kernel never touches it, so
  `UserRegs` keeps it across switches with `TPIDR_EL0`. Without that, a
  preempted thread came back on another thread's stack pointer; the
  slice-6 test missed it because both its threads used one stack address.
- **User programs on amd64 are soft-float,** like the kernel: they build
  for `x86_64-unknown-none`, whose ABI has no SSE. The kernel still saves
  and restores user SSE state. A Plan R target (TOOLCHAIN.md) fixes it.
- **amd64 GS:** an NMI or machine check between `syscall` and its
  `swapgs` would run with the user's GS base. Both are fatal, so only
  the report would suffer, but a real NMI handler will need Linux's
  "paranoid" entry (check the GS base MSR).
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
- **Scheduler limits:**
  - No priority aging: a busy high-priority thread starves lower ones on
    its CPU, and a CPU's own context only runs when nothing is ready.
  - No timed sleep yet (`tsleep`): that comes with `alarm` in slice 10.
  - Tests that need their threads queued together spawn them with
    interrupts off (`ktests::together`); otherwise a tick can preempt
    the spawner after the first one.
  - Locks that sleep take a `Sleepable`, which only a thread with
    interrupts on can get.
- **TLB shootdown rule:** never unmap or call `mmu::shootdown` with a
  `SpinLock` held. Spin locks hold interrupts off, so a CPU spinning on
  one cannot take the shootdown IPI. Change the tables under the lock,
  drop it, then flush; a stuck shootdown panics after 1 s and names the
  CPU. There are no ASIDs/PCIDs yet: switching spaces flushes the user
  half (CR3 reload; arm64 `tlbi aside1` on ASID 0 with nG user leaves).
  Kernel leaves are not global on amd64.
- **The AP trampoline's GDT has its Accessed bits preset.** Loading a
  selector otherwise writes them, which faults once the read-only start-up
  map is live. KVM does this write and TCG does not, so only KVM runs
  caught it (a silent triple fault right after "clock: ticking").
- **9boot's `Boot` regions** (its page tables, the boot stack,
  BootInfo and the boot archive, which `#/` will serve in place) are
  still not given to the frame allocator. That needs
  BootInfo copied out first and the boot stack left behind.
- **Heap:** every block it uses is zeroed by `mm::alloc_frames`, so
  large allocations pay for a memset they may not need. There are no
  per-CPU caches yet; add them if the class locks show up in profiles.
- **Frames:** usable RAM below 1 MiB (`mm::FLOOR`) is never handed out;
  `mm::low_pages` finds the amd64 AP trampoline there. Frames may now come
  from anywhere, including above 4 GiB. The `Boot` and `AcpiReclaimable`
  regions are not yet given to the allocator (see below). The
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
