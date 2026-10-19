# Handoff

Where work stands, for picking it up in a new session. Last updated
2026-10-19: P3 slices 1 to 9 (console input, libr base, libr system
layer, the 9P server framework and `ramfs`, `rc` parts 1 and 2, the
oracle harness, the file commands, the name-space commands and the real
boot and init) are done.

## State

- **Phase:** P2 (kernel core), 2026-10-18 to 2026-10-19, is done; P3
  (userland bootstrap) is next. The P2 slices, as in `docs/ROADMAP.md`:
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
       `rendezvous(tag, value)` used one global table until slice 9
       gave each process a rendezvous group.
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
     - `port/exec.rs`: `load(path, argv)` finds `path` (in the archive
       then; through the name space since slice 8), checks the ELF (no W+X, nothing below 64 KiB or
       into the stack, at most one writable segment, entry in text) and
       builds text, data, bss and a 16 MiB stack below `USER_TOP`, with
       the arguments laid out as `lib/abi` documents (sp at argc, argv,
       null, strings above; 64 KiB at most). `exec::spawn` starts a
       program as a child of the caller; since slice 12 only
       `exec::start_init` uses it.
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
       `archive`); the kernel keeps it where it is (`port/devroot.rs` since
       slice 8).
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
  8. The Chan/Dev core, the name space, the first devices:
     - `lib/abi` gains `Qid`, `Dir` with Plan 9's stat encoding
       (`encode`, `decode`, `dirs` for directory reads, `DONT_TOUCH` for
       wstat), and the `QT*`/`DM*` bits. Host-tested.
     - `port/dev.rs`: `trait Dev` (attach, walk one name, stat, open,
       create, read, write, close, remove, wstat, and `mapped` for files
       that stay in memory), the device table (`dev::all`), and helpers
       for table-driven devices (`Entry`, `walk_gen`, `dirread_gen`,
       `open_checked`). Every file is owned by `eve`, the only user, so
       the owner bits decide.
     - `port/chan.rs`: `Chan` (device, instance, qid, cleaned name, open
       mode, offset, union members). Names are cleaned lexically first
       (`clean`; `..` stops at `/` or `#c`). A relative name with no `..`
       walks from dot, any other from its root. `walk` follows the mount
       table at the start and after each element, and after the last
       only if asked (not for `bind`'s old or `chdir`). Union walks try
       each member; union creates go to the first `CREATE` member, else
       fail with "mounted directory forbids creation"; union reads go
       member by member, from 0 or onward only. Dropping an open chan
       calls `Dev::close`, and removes an `RCLOSE` file.
     - `port/pgrp.rs`: `Pgrp` (mount heads keyed by `same_file`; a new
       union starts with the mount point itself, as in Plan 9), `Fgrp`
       (lowest free fd, at most 1024, close-on-exec), and `Files`, which
       a user `Proc` holds: the groups, `slash` and `dot`. `Files::boot`
       is a fresh one: `#/` at `/`, nothing bound. `exit` closes the
       descriptors first, while the environment is still there.
     - Devices: `#/` (`devroot`: Plan 9's mount-point directories plus
       the archive, read-only, pages used in place), `#c` (`cons` writes
       to the console and has no input yet; `null`, `zero`, `pid`,
       `ppid`, `user`, `sysname`, `osversion`, `drivers`, `time`), `#e`
       (a per-process `Egrp`), `#d`, `#r` (`rtc`, from
       `BootInfo::boot_time`, version 4, which 9boot reads with UEFI
       `GetTime`; the counter carries it on), `#P` (`cputype`).
     - `port/sysfile.rs`: `open`, `create`, `close`, `read`, `write`,
       `pread`, `pwrite`, `seek`, `stat`, `fstat`, `wstat`, `fwstat`,
       `remove`, `fd2path`, `dup`, `chdir`, `bind`, `unmount`. Data goes
       through a kernel buffer, at most 128 KiB a read. A stat buffer
       too small gets just the 2-byte size. Errors are Plan 9's texts
       (`port/error.rs`; `Error` holds a `Cow`, so a device can give its
       own).
     - `exec` opens its file for exec through the name space. A segment
       `Image` is either the archive's bytes in place or an `Arc<[u8]>`
       read in.
     - libr wraps the new calls. `cmd/utest/tfile` exercises them on
       every device, then runs `targs` from a copy in `#e`.
  9. `rfork` with copy-on-write, `qio`, `#|` and `#s`:
     - `port/segment.rs`: a segment is now an `Arc`, shared by every
       memory it belongs to (text always, data and bss with `RFMEM`), and
       keeps the spaces mapping it (`Weak<Space>`) so a page it replaces
       or gives up is cleared from all of them. A page is an
       `Arc<Frame>` plus a copy-on-write bit; the frame is freed when
       the last page holding it goes. `Memory::fork` shares the text (and
       data and bss for `RFMEM`) and copies the rest: the copy holds the
       same frames and every writable page of both is made read-only and
       copy-on-write, then the parent's spaces are flushed. A write fault
       copies the frame if anything else holds it, else takes it back.
       Faults now reach `segment::fault` for writes to present pages too
       (amd64 protection faults, arm64 permission faults), from user mode
       and from kernel user copies; CR0.WP is now set, so the kernel's
       own copies fault on read-only pages too. A frame's replacement
       clears every space first (break-before-make on arm64, whose TLB
       maintenance is broadcast, so it can happen under the lock) and is
       flushed once the locks are dropped (`Pending`).
     - `port/mmu.rs`: `MapKind::UserReadOnly`, and `Space::clear`
       (unmap without the shootdown), `protect`, `translate`, `flush`
       and `flush_here`.
     - `rfork` (`port/syscall.rs`): Plan 9's flags and checks.
       `Files::fork` shares each group, copies it (`RFNAMEG`, `RFFDG`,
       `RFENVG`) or starts it fresh (`RFCNAMEG`, `RFCFDG`, `RFCENVG`,
       `RFREND`); without `RFPROC` it replaces the caller's. `RFNOMNT`
       sets the `Pgrp`'s `noattach`: `#` names other than `|decp` then
       fail with "mount/attach disallowed" (mount, in slice 11, must
       check it too). `RFNOWAIT` children have no parent and ppid 0;
       `RFNOTEG` gives a new note id (`Proc::noteid`, for slice 10).
     - `proc::fork`: the child's kernel stack starts with a copy of the
       parent's system call frame, set to return 0
       (`arch::context::fork_frame`), and its first switch leaves through
       the trap exit (`resume_user`; `planr_trap_exit` and
       `planr_trap_return` are now global). Its user registers are a
       `UserRegs::snapshot` of the parent's live ones. `make` takes a
       `Birth`.
     - `Rgrp` replaces the global rendezvous table; the call is
       `rendezvous`.
     - `port/qio.rs`: Plan 9's queues. A write is cut into blocks of at
       most 64 KiB, a read takes at most one block (the rest stays, or
       is dropped on a message queue), writers wait at the limit and are
       woken below half of it, `hangup` lets readers drain then get end
       of file (and an error after three), `close` drops what is queued.
       Readers and writers each queue on a `QLock`.
     - `Chan` has `aux`, the device's state for a file, which walked and
       renamed chans share (`with_aux`, `aux::<T>()`).
     - `#|` (`devpipe`): each attach is a new pipe, 256 KiB each way;
       closing one end's last open chan hangs up the other's reads and
       its own writes, and once both are closed the pipe reopens. The
       `pipe` call opens both ends `ORDWR`. `#s` (`devsrv`): one global
       registry; create, post a descriptor by writing its number, open to
       get the posted chan, remove. `dev::check_open` is `open_checked`
       without marking the chan open.
     - libr: `rfork`, `pipe`, `rendezvous`. `cmd/utest/tproc` checks
       copy-on-write both ways, `RFMEM` (shared data and bss, own stack,
       and a grandchild's copy of a shared segment), pipe boundaries, end
       of file, hangup errors, 1 MiB through a pipe, `#s`, rendezvous,
       each group flag, `RFNOMNT`, `RFNOWAIT` and exec in a forked child.
  10. Notes, `#p`, timers, semaphores and attached segments:
     - `port/note.rs`: a process's `Notes` (at most 5 queued, its
       handler, `notified`, the handler's `Ureg` on the user stack, and
       `killed`). `note::post` queues a note and calls
       `Proc::interrupt`. A kernel note (`From::Kernel`, the `sys: …`
       ones) to a process with no handler, or one already running,
       empties the queue first. `notify` runs on every way out to user
       mode while `Proc::note_waiting` is set. It delivers as Plan 9
       does: the `Ureg`, a link to the previous handler frame and the
       text go below the user stack pointer less 256 bytes, and the
       handler is called with them. With no handler, or a kernel note
       during one, the process exits with the note's text as its status.
       It prints a line when a kernel note kills it. User notes wait
       while a handler runs. `noted` implements NCONT, NDFLT, NSAVE and
       NRSTR. A handler is inherited by `rfork` and cleared by `exec`.
     - `abi::Ureg` *is* the trap frame on both arches, checked by
       `const` asserts. `dispatch` takes `&mut Ureg` and returns `None`
       after `noted`; amd64 then returns by `iretq`, so `rcx` and `r11`
       survive. On arm64, `TrapFrame::with_ureg` puts `SP_EL0` in the
       frame's `sp` while port code runs, and `arch::context::sanitize`
       fixes up what `noted` restores. User faults are notes now
       (`syscall::user_trap` posts one and returns).
     - Sleeps that a note or a timer ends (`Rendez::sleep_intr`,
       `tsleep`, both via `sleep_for`, returning `Woke`). A poster or
       timer readies a sleeper only while its `wake` lock says it is
       armed for that sleep (`seq`), so a `QLock` waiter never gets a
       spurious wakeup. A sleeper that a note or timer readied removes
       itself from the queue. If a waker had already popped it, it passes
       the wakeup on. `interrupted` is Plan 9's `notepending`; delivery
       and an interrupted sleep both clear it. qio reads, the flow-control
       wait in qio writes, `rendezvous`, `await` and semaphores are
       interruptible. The pipe write that hits a hangup posts
       `sys: write on closed pipe`.
     - `port/timer.rs`: a list of timers (`Wake(seq)` or `Alarm`). CPU
       0's tick compares `NEXT` with the time and wakes the `clock`
       kproc, which runs the timers that are due (so posting a note never
       happens in an interrupt). `sleep(ms)` is a `tsleep`, `alarm` sets
       `Proc::alarm` (not inherited, cancelled at exit), and `nsec` is
       `devrtc::nanoseconds`.
     - `port/sema.rs`: semaphores on 64-bit signed words. They change
       through the direct map with atomics (`Memory::with_word`, which
       resolves the page for writing; `peek_word`, safe under a spin
       lock, for the sleep's condition). Waiters sleep on a `Rendez`
       keyed by (segment, address), and a release wakes them all.
     - Segments: `Kind::Shared` (shared with every child) and
       `Kind::Memory` (copied unless `RFMEM`), with `RONLY` and `CEXEC`.
       `segattach` places them below the stack, top-down, when `va` is 0.
       `segdetach` unmaps the segment and drops this space from it;
       `segfree` drops pages; `segflush` only checks the range. `exec`
       keeps the segments not marked `CEXEC` (`Memory::inherit`).
     - `#p` (`port/devproc.rs`): `args ctl fd mem note noteid notepg
       ns segment status wait` per pid, found again by pid on every
       operation (`proc::find`/`all`, the `PROCS` table). `ctl` takes
       `kill`.
     - libr: `notify`, `noted`, `alarm`, `nsec`, semaphores, the `seg*`
       calls; `sleep` now returns its error. `cmd/utest/tnote` covers
       notes from itself, the kernel and the clock, a caught fault,
       interrupted sleep, pipe read, rendezvous, await and semaphore,
       `alarm`'s remainder, no handler and NDFLT, `notepg`, `kill` of a
       sleeping and a spinning process, semaphores, attached segments
       and every `#p` file. `tproc` now expects the closed-pipe note to
       end the writer. A ktest checks `tsleep`.
  11. 9P and `#M`:
     - `lib/9p` (crate `ninep`): 9P2000's messages as `Tmsg` and `Rmsg`.
       The decoder borrows strings and data from the buffer and refuses
       anything malformed (size mismatch, short field, trailing bytes,
       bad type, non-UTF-8, more than 16 walk elements), so the encoding
       is canonical. The encoder writes into a slice (`size()` measures),
       so the crate needs no allocator and user programs use it too.
       Host tests include known bytes and a deterministic fuzz loop
       (200,000 mutated and random messages: no panic, and whatever
       decodes re-encodes to the same bytes).
     - `port/devmnt.rs`: the session (`Mnt`) hangs off the transport
       chan (`Chan::mux`, Plan 9's `c->mux`) and points back weakly;
       every `Fid` holds the transport, so the session ends, and the
       server sees end of file, when the last fid and descriptor go.
       `Tversion` asks for `MSIZE` (32 KiB of data, so a message is one
       pipe block). Chans that are only walked share a fid; open,
       create and remove first clone one (`Twalk` with no names) unless
       the chan's fid is its alone. `Fid`'s drop clunks, or, where it
       may not sleep, queues the clunk for the session's next request.
       Replies: no reader thread; the first waiter with no reader reads
       the next message and delivers it by tag (Plan 9's `mountio`). A
       note sends `Tflush`, waits for `Rflush` without being cut short,
       and returns the reply if it came first, else "interrupted".
       `ORCLOSE` goes to the server, and the chan keeps the mode without
       it. Stats and directory entries get type `M` and the session's
       devno (`mntdirfix`).
     - Calls: `mount(fd, afd, old, flags, aname)` (fails after
       `RFNOMNT`), `fversion` (returns the version's length, as Plan 9
       does), `fauth`. `#M` cannot be attached by name. A pipe carrying
       a session posts no "write on closed pipe" note (`CMSG`).
     - `cmd/utest/t9p`: a toy server (no allocation) in a child, on a
       pipe. The parent checks version and auth, stat and directory
       entries, multi-element walks, the server's own error text, a
       12 KB read across messages, create/write/`fwstat` rename/remove,
       `ORCLOSE`, `OTRUNC`, a read the server holds while another process's
       requests complete, a flush cut short by `alarm`, `RFNOMNT`, a
       second mount through `#s` on the same session, that no fid is
       left once everything is closed, and a server that exits (requests
       fail with "i/o on hungup channel", and no note comes).
     - `exec: programs' memory is freed` and `heap: empty slabs return`
       now get three tries each. With `t9p`'s extra processes, each failed
       once on arm64 `el2-nvhe`, by a single frame: a table grown for good,
       or another CPU still freeing a dead process's memory. A real leak
       fails every try.
  12. `init` and the exit test:
     - `cmd/init` (`/boot/init`): binds `#c` on `/dev`, `#r` and `#P`
       after it, `#e` on `/env` and `#s` on `/srv` (both `-c`), `#p` on
       `/proc` and `#d` on `/fd`; opens `/dev/cons` as 0 and 1 and dups
       2; checks the binds took (`/dev/pid`, its own `/proc/n/status`,
       `/dev/rtc`, a create and remove in `/env`, `/fd/0ctl`). Then it
       starts every test program at once, each by `rfork(RFPROC|RFNAMEG|
       RFENVG|RFFDG|RFNOTEG)` and `exec`, awaits them, checks each wait
       message as the old `programs_run` ktest did, prints `init: <argv>:
       ok` (or `FAILED: <msg>`; `-q` prints failures only) and `init: N
       passed, M failed`, and exits normally only if all passed.
     - `exec::start_init(argv)` starts it as the child of a kernel thread
       `boot`, which awaits it and prints `kernel: init exited normally`
       or `kernel: init exited: <msg>`. A normal boot starts it and idles
       on (no shell yet). `test=1` runs the ktests, then `init`, waits up
       to 60 s, and powers off with `kernel: ok` only if it exited
       normally; xtask's `boot` and `uniprocessor` tests require that
       line. The `programs_run` ktest is gone; `memory_freed` now runs
       `init -q`.
     - `tfile` starts with `rfork(RFCNAMEG)`, since its checks begin from
       no binds.
     - 9boot fix: it read the kernel and archive before reserving the
       previous log ring, against its own comment. With the archive past
       2 MiB (`init` added), the freed read buffer landed on the ring at
       the top of RAM, and the amd64 `log-ring` test failed every time.
       The reads now come after `logring::setup`.
- **P3** (userland bootstrap), from 2026-10-19; slices in
  `docs/ROADMAP.md`:
  1. Console input:
     - `lib/acpi`: `Spcr::irq` (`SpcrIrq::Pic` or `Gsi`) from the
       table's interrupt fields; QEMU virt's is GSI 33. The amd64
       default COM1 uses ISA IRQ 4 (`arch::DEFAULT_CONSOLE_IRQ`); arm64
       has no default. The Orange Pi has only DBG2, which gives no
       interrupt, so it has no input yet; the Ryzen falls back to COM1.
     - Drivers: `enable_rx` and `getc` on the 16550 (IER, plus MCR OUT2,
       which gates the IRQ on PCs) and PL011 (RX and RX-timeout in IMSC).
       `arch::enable_irq(irq, name, handler)` registers and routes one
       device interrupt: the I/O APIC to this CPU (below GSI 16 high and
       edge, else low and level), or a level SPI on the GIC
       (`gic::enable_spi`).
     - `port/console.rs`: `write_bytes` for output that need not be whole
       UTF-8 (`cons` writes use it now), `getc`, `enable_input`.
     - `port/devcons.rs`: the interrupt only copies bytes into a 1 KiB
       ring and wakes the `kbd` kproc, which edits them (Plan 9's
       discipline: `\r` → `\n`, ^H erases a UTF-8 character, ^U the
       line, ^D sends the line without a newline, lines of 1 KiB are
       sent as they are), echoes (`\b \b` for an erase), and queues each
       line as one block on a qio queue that `cons` reads. Input past
       64 KiB unread is dropped. DEL clears the line and posts
       `interrupt` to the note group of the last process to read `cons`.
       `consctl` (new): `rawon`/`rawoff`; raw bytes go through
       unedited and unechoed, a half-typed line is sent at `rawon`, and
       closing the last `consctl` turns raw off. With no interrupt,
       reading `cons` fails with "console has no input". The kernel
       prints `cons: input on …` at boot.
     - `test=cons`: the kernel runs `init -i`, which runs `tcons` alone;
       xtask's `cons` boot test types each step's input when `tcons`
       prompts (`qemu::Typed`). `cargo xtask test --only NAME` runs one
       boot test.
     - Caveat: Linux terminals send DEL for Backspace, so under
       `cargo xtask qemu` Backspace interrupts; use ^H to erase, as on
       Plan 9.
  2. libr base (`lib/r`, now host-tested too: `#![cfg_attr(not(test),
     no_std)]`, with `_start`, `main!`'s hook, the panic handler and the
     global allocator left out of tests):
     - `heap`: the kernel's `lib/heap` size classes over `Blocks`, page
       blocks (powers of two, aligned) carved from the bss end with
       `brk`. Freed blocks go on per-order free lists, never merged or
       given back; a larger free block is split before the break moves;
       the aligned gap below a new block is cut into free blocks.
       `Arena` is the allocator; the `#[global_allocator]` wraps it in a
       spin lock (yielding after 100 spins) because `RFMEM` children
       share the heap. `realloc` stays in place within a size class.
       `r::heap::top()` is the break as the heap has moved it.
     - `io`: `Read`/`Write` traits (`write_all`), `File(fd)`, `STDIN`,
       `STDOUT`, `STDERR`, and impls for `&[u8]` and `Vec<u8>`.
     - `print`: `print!`, `println!`, `eprint!`, `eprintln!` (errors
       ignored) and `fprint!(fd, …)` (a `Result`): formatted into 1 KiB
       and written in one piece per KiB, as Plan 9's `print`. Verbs as
       types: `Quoted` (`%q`), `RcQuoted` (adds `needsrcquote`'s
       characters), `Text` (bytes as UTF-8), `Rerr` (`%r`, via the new
       `rerrstr`). `Error` is `Display`; `Error::from_text` is public.
     - `arg`: `Opts`, Plan 9's `ARGBEGIN` (runes as flags, `--` and `-`
       end, `arg()` is `ARGF`, `rest()`, `argv0()`).
     - `utf`: `decode` (Rust's validity rules; a bad byte is one
       `RUNE_ERROR`), `full_rune`, `runes`, `rune_offsets`, `utf_len`,
       `utf_rune`, `utf_rrune`.
     - `bio`: `Bin` (`getc`, `ungetc`, `getrune`, `line(delim)` with no
       length limit, `read`, `offset`) and `Bout` (`write`, `putc`,
       `putrune`, `print`, `flush`, flush on drop).
     - `cmd/utest/tlibr` (in `init`'s list, run with options): flags,
       collections, a 3 MiB block, the break unmoved over 1000 rounds of
       freeing, two `RFMEM` processes churning the shared heap (it fails
       with the lock removed), and `print!`/`Bout`/`Bin` through a pipe.
  3. libr system layer:
     - `dir`: `DirBuf` (an owned `Dir`; `as_dir`, `null` for wstat,
       `is_dir`), `dirstat`/`dirfstat` (the buffer grows to the size the
       kernel reports), `dirwstat`/`dirfwstat`, `dirread` (one 16 KiB
       read) and `dirreadall`.
     - `env`: `getenv` (NULs between words as spaces, as Plan 9's),
       `getenv_raw`, `getenv_list` (rc's NUL-ended words), `putenv`,
       `putenv_list`; `exec_path` looks in `$path` (default `.` and
       `/bin`) unless the name starts `/`, `#`, `./` or `../`, and returns
       the first error that is not "file does not exist".
     - `proc`: `Waitmsg` (`parse`, `status` after `name pid: `), `wait`,
       `waitpid`, `postnote(Whom::Proc | Group, pid, note)` through
       `#p` (so `/proc` need not be bound), `getpid` from `#c/pid`.
     - `time`: `Tm`, `gmtime` and `tm2sec` (civil-date arithmetic, any
       `i64`), `Zone` (Plan 9's `/env/timezone` format; GMT without one),
       `localtime`, `asctime`/`ctime` (`Thu Jan  1 00:00:00 GMT 1970\n`),
       `time`, `month_days`.
     - `text`: `cleanname` (Plan 9's, including `#X` device roots with
       their spec), `gettokens`, `tokenize`, `unquote`, `getfields`
       (a port of Plan 9's loop, `max` and `multi` included), `quote`,
       `rc_quote`. Also `r::cstr` and `r::read_all`.
     - `cmd/utest/tsys` (in `init`'s list): dirstat and dirreadall of
       `/dev`, a wstat error passed through, the environment, `exec`
       through `$path`, notes to a process and a group, `getpid`, and
       `ctime`/`localtime` with a zone.
  4. The 9P server framework and `ramfs`:
     - `lib/9p` gains a `server` feature (needs `alloc`; the kernel's
       codec still does not), with three modules, host-tested end to end
       through encoded messages:
       - `srv`: `trait Fs` (attach, walk one name, open, create, read,
         `dirent` by index, write, stat, wstat, remove, a `clunk` hook;
         `File` is the fid's handle, cloned with the fid) and `Srv`,
         which owns the fid table and the protocol: version and msize
         (a new `Tversion` clunks everything), unknown and duplicate
         fids, walks that clone or move a fid and answer partially
         (an error only if the first name fails), no walk or reopen of
         an open fid, directories opened only to read, read/write
         against the open mode, directory reads packed whole-entry and
         offset-checked, `ORCLOSE` at clunk, `Tremove` clunking even when
         it fails. Requests are answered in order, one at a time, so
         `Tflush` just answers. `serve(&mut impl Transport)` reads whole
         messages from any byte stream until end of file; `set_chatty`
         traces them. `Stat` is an owned `Dir`.
       - `tree`: `Tree<T>`, Plan 9's `lib9p` file trees: `Rc` nodes, so a
         removed file stays readable and writable through fids that hold
         it. It enforces `hasperm` (other, group, owner bits OR-ed),
         search permission to walk, write on the directory to create,
         remove and rename, `OTRUNC` and `ORCLOSE` permissions, the
         directory's mask on a new file's permissions, a new file's group
         from its directory, and `wstat`'s all-or-nothing rules (rename
         to no sibling's name, length only of files with write
         permission, mode/mtime/gid only by the owner, no file↔directory
         change, never the owner).
       - `ram`: `RamFs`, the tree with `Vec<u8>` contents (`DMAPPEND`,
         holes of zeros, a 256 MiB cap per file).
     - `cmd/ramfs` (`/boot/ramfs`): `ramfs [-Di] [-m mtpt] [-s | -S
       srvname]`, as Plan 9's. It forks the server (`RFNAMEG|RFNOTEG|
       RFNOWAIT`, so it sees no mount of itself and nobody waits for it),
       posts and mounts the client end (`MREPL|MCREATE`, `/tmp` unless
       `-s`/`-S` without `-m`), and exits; the server exits when the
       session's last reference goes. `-i` serves descriptors 0 and 1.
       Files belong to `#c/user`. User programs outside `cmd/utest` need
       its `build.rs` (libr's link script); without it the kernel
       refuses them with "exec header invalid".
     - `init` runs `ramfs` in its own name space before the tests, and
       checks a file in `/tmp`.
     - `cmd/utest/tramfs` (in `init`'s list): write/pread/pwrite, create
       truncating, `OEXCL`, `chmod` then "permission denied",
       `dirwstat` rename and truncate (the first real wstat), 600
       entries read with `dirreadall`, a 1 MiB file in odd-sized
       writes, a removed file still open, `ORCLOSE`, two writers at
       once, and a second `ramfs -S tramfs -m /n` mounted again from
       `/srv` on `/mnt` (one tree, apart from `/tmp`; no creation there
       without `MCREATE`), then unmounted.
     - A server that never exits fails `memory_freed` (about 1000
       frames lost per run, checked by breaking it on purpose).
  5. `rc`, part 1 (`cmd/rc`; `/boot/rc`):
     - The library (host-tested): `lex` (Plan 9's `lex.c`: free carets,
       `SUB` after a word, `>[2=1]`/`|[2]` tokens, comments,
       `\`-newline, keywords), `parse` (recursive descent over
       `syn.y`'s precedences, a line at a time, `if not` checked against
       the last compiled `if` from line to line), `tree` (printed as
       `pcmd.c` prints it: `{a;b}`, `if(c)body`, `while (c)body`, a
       simple command's redirections first) and `pattern` (`* ? [a-z]
       [~…]`, active only where written unquoted).
     - The shell (`src/shell`): `exec` runs the tree. Plan 9's rules as
       far as known: a runtime error (`Xerror`) prints `rc: …`, sets
       `$status` to `error` and unwinds to the nearest interactive loop
       (none yet, so the shell exits); a syntax error ends only that
       input (script, `eval`, `.`). `$status` is the wait message
       (`name pid: text`), a pipeline's `left|right`; true is empty or
       only `0` and `|`. The last element of a pipeline runs in the
       shell itself; a forked child execs its last simple command
       instead of forking again (`exitnext`). The shell's own forks
       (pipeline sides, `&`, `@`) take `RFENVG`, unlike Plan 9's
       `fork()`, so their assignments stay theirs; programs share the
       shell's environment, which `Vars::export` brings up to date
       (changed globals, then locals, then `fn#name`) before each.
     - `fd`: the shell redirects its own descriptors (so builtins and
       functions see redirections) after saving each as a copy at 10 or
       above; its private descriptors move out of a redirection's way
       and are closed in every program run.
     - `word`: `$x`, `$1` (`$*(1)`), `$x(n n-m n-)`, `$#`, `$"`, `^`
       with its list rules, and globbing (directories only where more
       follows, matches sorted, no match leaves the word).
     - Builtins: `. builtin cd eval exec exit rfork shift wait whatis`.
       Not yet (part 2): here documents, `` `{} ``, `<{}`/`>{}`, notes,
       `flag`, `-eilx`, `rcmain`, the prompt and interactive use.
     - `cmd/cmds` (new, the one-file commands): `echo`, pulled forward
       from slice 8 because rc's tests need it (it is not a builtin).
     - `r::exec` now passes any number of arguments (allocating past
       64); the kernel's limit is 64 KiB of them.
     - `cmd/utest/trc` (in `init`'s list; `-v` names each case): 41
       scripts through `rc -c`, stdout, stderr and exit status each
       checked, with `/boot` bound on `/bin` and files made in
       `/tmp/trc` for the patterns.
     - `PLANR_GDB=port cargo xtask test …` starts QEMU with a gdb stub,
       for a test that hangs.
  6. `rc`, part 2:
     - The library: here documents (`<<tag`, `<<'tag'`; a tag must be a
       plain word, else "Bad here tag") are read after the line they are
       on, whole lines up to the tag (a last line with no newline is
       dropped), and substituted then, through a callback
       (`Parser::line_with`), as Plan 9's `readhere`: so `x=1; cat <<EOF`
       shows the old `$x`. `Redir::Here` shares its text with the parser.
       `` `split{cmd} `` (plan9port has it) as well as `` `{cmd} ``.
     - Here documents reach the command through a pipe, not a `/tmp`
       file: written at once up to 64 KiB, else by an `RFNOWAIT` child.
       `psubst`'s rules: `$name` is its words joined by blanks, `$n` is
       `$*(n)`, a `^` after the name goes, `$$` is `$`.
     - `` `{} `` runs in a child with its output on a pipe, split at
       `$ifs`'s characters (or the split word's), and leaves `$status`
       as the child's. `<{}` and `>{}` name `/fd/N` (init binds `#d`
       there), kept open, out of the way of saved copies, until the
       simple command they are in (or the line) is done; nobody waits
       for those children.
     - Notes (`trap.rs`): the handler only counts the eight Plan 9 knows
       (`exit hangup interrupt quit alarm kill sys: fp: term`, as
       prefixes); any other is reported (`rc: note: …`) and ends the
       shell. `Shell::dotrap`, at the start of every command and after
       each line, calls `sigint` and so on with `$*`; without one,
       `interrupt` and `quit` unwind to the interactive loop (or end a
       script), the rest exit; a forked child just exits. Waits for
       programs carry on through a note; `wait` and `` `{} `` stop.
       `sigexit` runs on `exit` and at the end, in the shell itself
       only, and leaves `$status` as it was. An all-true status exits
       normally.
     - Flags (`flag.rs`): Plan 9's `getflags` rules and messages
       (`srdiIlxepvVc:1m:1`; "Illegal flag -q", "set twice", the usage
       line, status `bad flags`), `-i` when standard input is the console
       (`fd2path` ends in `/dev/cons`) and no file is given, `-I`, and a
       name starting `-` meaning `-l`. `-e` exits after a failed simple
       command, `@` or `~` outside conditions (`if`, `while`, the left of
       `&&`/`||`), once rcmain has run `eval` or a second `.`. `-x`
       prints each simple command's words (quoted as `pwrd` quotes), `-s`
       a failed `$status` before each command, `-v`/`-V` echo the input
       into a buffer flushed where Plan 9's buffered stderr is (before
       reading a command, prompting, an error, a program, `-x`), so the
       last line's echo is lost at `exit`, as on Plan 9.
     - rcmain (`cmd/rc/rcmain`, in the archive as `/rc/lib/rcmain`, the
       first file there that is not a program): the shell imports
       `/env`'s variables, sets `pid`, `cflag`, `rcname` and `$*`, and
       runs `. rcmain $*` then exits, as Plan 9's bootstrap. rcmain sets
       `home`, `ifs`, `prompt` and `path` defaults, takes functions with
       the new `finit` (not under `-p`), and runs `-c`'s command, the
       file, or `#d/0` (`. -i` when interactive). `$0` is rcmain's name
       under `-c`, as on Plan 9. `.` now makes `$0` local too. New
       builtin `flag c [+-]`.
     - Interactive input (`fd::Source`): `$prompt(1)` before a command,
       `$prompt(2)` before each further line (here documents too); an
       interrupted read is not the end: a newline, the traps, and on.
     - `cat` (in `cmd/cmds`), pulled forward from slice 8 for the tests.
     - `init` now binds `/boot` on `/bin`, and `init -i` runs its
       programs in turn: `tcons`, then `rc -i`, which the `cons` boot
       test types a session to (a prompt of its own, the continuation
       prompt, errors that do not end it, a here document, DEL stopping a
       loop, `sigint`, ^D running `sigexit`).
     - `trc`: 59 scripts now (and the many-arguments one), with flags
       before `-c`: here documents,
       `` `{} ``, `<{}`/`>{}`, `sigexit`, notes posted through
       `/proc/$pid/note`, `-e -x -v -m`, `flag`, a bad flag, `cat`.
     - Seen in plan9port, not yet compared: its `whatis` quotes with
       `pwrd` (not `needsrcquote`), and its error texts carry stray
       newlines (`< requires singleton\n`). Slice 7 decided (below).
  7. The oracle harness:
     - `cargo xtask oracle` (`xtask/src/oracle.rs`; also the last step of
       `ci`) runs each `oracle/rc/NAME.rc` as `rc NAME.rc`, alone in an
       empty directory with stdin from `/dev/null`, on host plan9port
       (environment only `PLAN9` and `PATH=.:$PLAN9/bin`, `argv[0]`
       `rc`, 10 s each) and on both arches in QEMU, and compares stdout,
       stderr and whether the status is empty. `oracle/rc/allow` lists
       known differences (script, stream, why); one that stops showing
       fails the run. Results are kept in `build/oracle/`.
     - In QEMU: `test=oracle` runs `init -o`, which runs only
       `cmd/utest/toracle`. The scripts are in the boot archive at
       `/sys/lib/oracle/rc` (every image has them); `toracle` copies
       each to `/tmp/oracle/NAME/`, runs it with output in files, and
       prints `oracle: begin/out/err/status/end/done` lines with the
       bytes in hex, which xtask reads back from the serial log (and
       does not echo).
     - The corpus: 24 scripts (words, carets, control, functions,
       whatis, deparse, redirect, pipes, here, backquote, pipefd,
       status, waitstatus, glob, errors, oserrors, syntax, eval, exit,
       nested, env, subshell, flags, usage); see `oracle/rc/README.md`
       for how to write one.
     - Allowed: syntax errors (plan9port's 2020 hand-written parser
       words them and counts lines its own way: "missing newline at end
       of line", "token EOF"; ours are Plan 9's yacc ones), `$status`
       after a program (Plan 9's `name pid: text` against a Unix exit
       code), the system's error strings, and plan9port's usage line
       (`-DSY`).
     - Fixed in rc, to match: a simple command's redirections print
       before it, as Plan 9's parser hoists them (`>f echo hi`,
       `>[2=1]cat`); `fn name` with no body prints `fn name ` (Plan 9's
       `"fn %t %t"`); redirection errors are Plan 9's texts, `< > <>
       requires singleton\n` or `requires file\n` for an empty list
       (`>>`'s without the newline). `whatis` already quoted as
       plan9port does.
     - plan9port's rc has no `return` (nor has Plan 9's; ours has none).
     - `boot_main`'s `test=trap` arm is now `trap_test()` (clippy's line
       limit), so the `trap` boot test looks for `kernel::trap_test+0x`.
  8. The file commands:
     - `cmd/cmds` (a binary each, in the archive's `/boot`, so `/bin`):
       `ls cp mv rm mkdir pwd touch chmod basename cleanname seq date
       sleep wc tee test cmp xd strings tail du`, each a port of Plan 9's
       own `sys/src/cmd/NAME.c` in `../plan9` (the reference, not
       plan9port: the user's rule, 2026-10-19). Flags, messages, exit
       statuses and quirks are Plan 9's, deliberately: `strings` reports
       a run at offset 0 as at 1, names files only when there are more
       than two, and prints the names ahead of the buffered strings;
       `xd -r` drops the bytes read past a repeated line; `cmp -l` exits
       normally when the lengths match, and its usage goes to standard
       output; `chmod` exits normally after errors; `tail -rc3` ignores
       the `c3`; `test`'s `-o`/`-a` do not read their right side once
       decided; `wc` counts runes with `Bgetrune` and words with
       `isspacerune`; `sleep` takes up to three decimal places; `du`
       rounds to blocks and then up to its unit.
     - libr, each a port of Plan 9's libc and checked against it: `num`
       (`strtol`, `strtoul`, `atol`, `strtod` with `nan`/`inf`, and
       `fltfmt.c`'s `%e %f %g` as `format_float`/`printf_float`: `%g`
       keeps 7 digits, prints `.5`, rounds half up in decimal, ignores
       the `0` flag), `access`, `perror`, `Mode` (`%M`),
       `utf::is_space_rune`; `Bin::getrune` is now `Bgetrune` (a broken
       sequence takes the bytes its first says, a cut-off one at the end
       is dropped); `time`'s zone file is read as `readtimezone` reads it
       (three-letter names, blanks and newlines only, any error means
       GMT, a 0 ends the times) and `asctime` is Plan 9's (a short zone
       ends the text; the year is `19`/`20` and two digits). Integers are
       64 bits where Plan 9's `long` is 32.
     - How they were checked: Plan 9's `fltfmt.c`, `pow10.c`,
       `cleanname.c`, `strtol.c`, `strtoul.c` and `atol.c` compile on the
       host behind a small shim header (the `u.h`/`libc.h` types, `Fmt`
       with `flags`/`prec`/`r`), and libr's ports were run against them:
       11 449 random `%e %f %g` cases and 18 000 integer and `cleanname`
       cases agree, but for 32-bit overflow. libr's `cleanname` already
       matched Plan 9's, `#c` roots and all.
     - Kernel: `seek` on a pipe fails with "seek on a stream", as Plan
       9's (`tail` tells a pipe from a file by it). `test=oracle` allows
       init 600 s (the corpus takes about two minutes on arm64 under
       emulation; other modes keep 60), and xtask's oracle boot at least
       660.
     - The oracle has three corpora, scripts named `CORPUS/NAME`
       (`oracle/README.md`, moved up from `oracle/rc/`): `rc`, `cmd` (16
       scripts, the cases where plan9port's commands and Plan 9's agree)
       and `expect` (15: `pwd cp mv chmod`, `ls -l`, Plan 9's error
       strings, and every case where Plan 9 differs from plan9port:
       `%g`, `cmp`'s messages, `du`'s rounding and flags, `ls`'s
       cleaning and `-T`, `strings`, `sleep`, `wc`'s white space),
       whose expected output is written in `NAME.out`, `NAME.err` and
       `NAME.fail`, each line checked against Plan 9's source. `allow` is
       optional. `toracle` runs every corpus under `/sys/lib/oracle`.
     - plan9port is not Plan 9: besides the above, its rc's 2020 parser
       takes a bare `=` as an argument (Plan 9's grammar makes it a
       syntax error, as ours does, so scripts quote it), and its `stat`
       names `.` as `.` (Plan 9's kernel names it for its directory, so
       `ls -d .` prints that).
  The audit after slice 8 (2026-10-19, the user's rule: `../plan9` is
  the reference, not plan9port), of rc, libr and ramfs against Plan 9's
  source:
     - **The oracle's host side is now Plan 9's own rc**, built from
       `../plan9/sys/src/cmd/rc` on Linux by `scripts/build-plan9-rc.sh`
       (plan9port's 9c, lib9, bio and libString; `scripts/plan9rc/`
       holds the header included first and `shim.c`, which keeps Plan
       9's `/env` and `/rc/lib/rcmain` under `$RC9ROOT` and maps `#d/N`
       and `/fd/N` to `/dev/fd/N`; two lines of `plan9.c` change: lib9's
       SIGCHLD note is turned off, `Fdprefix` is `/dev/fd/`). xtask builds
       it into `build/plan9rc` when missing or older than the script, and
       runs each script with a fresh `/env` whose `path` finds it as `rc`
       and then plan9port's commands, so `-x` and `-s` traces, `rcmain`
       and the environment compare exactly. Only `oserrors` and
       `waitstatus` are allowed now (Unix error strings and exit codes).
       The rc corpus has 39 scripts, the new ones covering lexing,
       builtins, variables, scope, subscripts, matching, globbing, here
       documents, tracing, statuses and redirection errors.
     - **rc, fixed to be Plan 9's:**
       - Syntax errors count lines as Plan 9's `lineno` does: one more
         each time a byte is read after a newline, `\`-newline or the end
         (`doprompt`), the first byte included, so a message names the
         line after the one at fault; here documents count theirs as
         `readhere` does.
       - `` `word{cmd} `` is gone (Plan 9's grammar has only `` `{cmd} ``;
         it was plan9port's/9atom's); any redirection before a brace is a
         pipe file (`REDIR brace`), printed with its arrow and fd, the
         read end for `<` only.
       - `-S` is a flag (accepted, unused, as Plan 9's).
       - Forks share the environment group, as Plan 9's `fork()`
         (`RFPROC|RFFDG|RFREND`; `&` is `RFPROC|RFFDG|RFNOTEG`): what a
         subshell exports for a program it runs is in `/env` for later
         programs too.
       - `finit` runs each non-empty `/env/fn#name` as commands
         (`Xrdfn`/`execcmds`), so `-s`, `-v` and `-x` see them.
       - rc's own messages (errors, usages, prompts, `-x` and `-v`
         lines) go to the descriptor 2 it started with
         (`fd::err`, a private close-on-exec copy), whatever is
         redirected: Plan 9's rc makes redirections only for the programs
         it runs (`doredir` in `execexec`), and `whatis` looks its output
         up through them (`mapfd`), which ours does by redirecting the
         shell's own fd 1 as before.
       - A redirection's token text is Plan 9's (the digit after `[`
         twice, `>[22=`), and an error the lexer gives up at is reported
         twice, the second as the parser's `syntax error`.
       - Backquote output is split a rune at a time as `rutf` reads it (a
         sequence cut off by the end is U+FFFD).
     - **libr:** `tokenize`, `gettokens`, `getfields`, `quotestrdup`
       (`quote`, `rc_quote`) and `unquotestrdup` (`unquote`) were built
       from Plan 9's C on the host and compared on 18 000 random inputs:
       all agree now, after `unquote` was made to stop at the first
       unquoted blank (keeping the rest) and `gettokens` to pass over a
       many-byte separator whole. `utf::full_rune` is Plan 9's `fullrune`
       (by the first byte alone). Known and left: `Text` (`%s`) shows a
       byte that is not UTF-8 as U+FFFD, where Plan 9 writes it as it is
       (Rust's formatting takes only UTF-8).
     - **ramfs is a port of Plan 9's `ramfs.c`** (`cmd/ramfs`, on
       `lib/9p`'s codec; `lib/9p`'s `Srv` and `RamFs` stay, as a library
       like lib9p): its own 9P loop, slots, fids and messages (`file in
       use`, `file exists`, `not owner`, `illegal name`, `fid not in
       use`, …), and its rules: a new file takes its directory's group
       and muid and its permissions masked by the directory's (a file's
       execute bits apart), a write never changes muid, `wstat` changes
       only mode (never the directory bit), name, group and length and
       ignores the times (so `touch` of an existing file in `/tmp` keeps
       its time), a removed file is gone even if open, the search
       permission checked in a walk is the parent's, options `-Dipsu -m
       -S` (`-m` after `-S` mounts instead of posting; the `#s` file is
       `ORCLOSE`). `tramfs` and the oracle's scripts follow it.
  9. The name-space commands, `newns`, and the real boot and init (each
     a port of `../plan9`'s source):
     - **Boot** (`cmd/boot`, `/boot/boot`, from `9/boot/boot.c`): the
       kernel now starts it, not init, with the environment Plan 9's
       `init0` sets (`terminal`, `cputype`, `service=terminal`, and each
       boot configuration setting not starting `*`; `exec::boot_env`,
       `Egrp::set`). It opens `#c/cons` as 0, 1 and 2, binds `#ec`
       (absent, ignored), `#e` and `#s`, and execs `$init`, split by
       `tokenize`, else `/$cputype/init -t`. Plan 9's root-server,
       factotum, time and swap steps are not needed: the root is `#/`,
       the archive.
     - **Init** (`cmd/init`, `/$cputype/init`, from `cmd/init.c`):
       `pri 10` through `#p/n/ctl`, `objtype`, `service`, `timezone`
       (from `/adm/timezone/local`), `newns`, then forever: `rc -c '.
       /rc/bin/termrc; home=/usr/$user; cd; . lib/profile'` for a
       terminal (`cpurc` first for a CPU server), then `init: starting
       /bin/rc` and plain `rc` each time it exits, with Plan 9's messages
       and its note handler. The old test harness that was init is gone.
     - **libr `ns`** (from `libauth/newns.c`): `newns`/`addns` read
       `/lib/namespace` (or a file): `bind mount unmount import cd clear
       .`, bind's flags, `#` comments, `$var` expansion from `#e` (first
       word, 27 bytes, names ended by `/.!'$`), newns's own unquoting, at
       most 14 words, a last line with no newline ignored (`Brdline`),
       errors ignored (`set_debug` shows them). There is no factotum, so
       `fauth`'s file goes to `mount` unanswered, and no network, so
       `import` does nothing. `Error::last` now leaves the error text in
       place, as Plan 9 does, so `%r` (`Rerr`) after a failed call shows
       it (it was cleared, printing nothing).
     - **Commands** (`cmd/cmds`): `bind`, `mount` (its `auth_proxy` stops
       where factotum's `rpc` cannot be opened, with Plan 9's message),
       `unmount`, `ns` (one entry a read; names with blanks, quotes, `\`,
       `#` or `$` quoted its own way; `xlatemnt`), `ps` (`-apr`, the
       layout of `ps.c`), and `auth/newns` (`$cputype/bin/auth/newns`).
       `kill` is Plan 9's rc script, in `/rc/bin`; it needs `sed` (slice
       10), so it is not tested yet.
     - **`root/`** is the archive's tree beside the programs (a file
       starting `#!` gets mode 755): `/lib/namespace` (Plan 9's, for the
       devices Plan R has: `#c #d #e #p #s #P`, `/$cputype/bin` and
       `/rc/bin` on `/bin`, `cd /usr/$user`), `/rc/bin/termrc` (binds
       `#r`, sets `sysname`), `/rc/bin/kill`, `/rc/lib/rcmain` (moved from
       `cmd/rc`), `/usr/eve/lib/profile` (a ramfs on `/tmp` if it is not
       writable, the `term%` prompt, then `exec rc -i`), `/adm/timezone/`
       (`GMT`, `local`), `/sys/test/run` and `/sys/test/oracle`. Programs
       are in `/$cputype/bin`, `boot` in `/boot`, `init` in `/$cputype`
       (`xtask`'s `archive_path`).
     - **Kernel:**
       - The mount table is Plan 9's (`cmount`): each `Mount` has an id
         (one more for each mount anywhere), its flags and its spec; a
         union bound whole is copied (its first member with the bind's
         flags, the rest `-a`, or `-b`), for `MREPL` too; `-c` on a union
         bound whole is "inconsistent mount"; `-a` with `-b` is "bad arg
         in system call". `bind` and `mount` return the id, and `mount`
         closes its descriptor, as Plan 9's do (`t9p` and `tramfs` check
         it). `unmount`'s first name is opened, so `/srv/x` matches the
         mount made from it (its transport, Plan 9's `mchan`).
       - `#p/n/ns` gives one entry a read, in id order, as Plan 9's
         (`bind FLAGS to from` or `mount FLAGS srv from spec`, `#s/name`
         for a posted transport, `devsrv::name_of`), then `cd dir`; the
         mount point itself in a union is `bind  dir dir`. `#p/n/args`
         quotes each argument as `%q` does. `#p/n/ctl` takes `pri n`
         (`Proc::set_pri`).
       - `#c/reboot`: `halt` prints `kernel: halt` and powers off (exits
         QEMU), `reboot` resets, `panic` panics.
       - `exec` runs `#!` scripts as Plan 9's `sysexec` (`shargs`, the
         line within 32 bytes, the script's last element as `argv[0]`
         and the process name).
     - **Tests:** `test=1` and `test=oracle` set `init` to `auth/newns
       /sys/test/run` or `/sys/test/oracle` (`exec::test_init`), which
       exit, so the kernel still waits for them; `memory_freed` boots to
       `/sys/test/run -q`. `/sys/test/run` checks the name space, mounts a
       ramfs, and starts every program at once (`rfork nE`, only
       `$cputype` in the environment, a note group of its own), each
       checked by its own subshell, since rc keeps no status for a process
       another wait has had (as Plan 9's `Waitfor`). `test=cons` is now a
       real boot: the harness types `tcons` at the `term%` prompt, the
       session, then `echo halt >/dev/reboot` once init starts rc again
       (`Expect::Halt`). `toracle` gives each script an empty environment
       (`RFCENVG`). `tfile` checks unmounting one member of a mount point
       that is not the first (a shadowed index got that wrong while this
       slice was written), and `tnote` checks `ns` and `fd` against the
       current directory.
     - Oracle: `oracle/expect` gains `bind mount unmount ns ps newns`
       (77 scripts in all, each output line worked out from Plan 9's
       source).
     - Known, from Plan 9's source, not yet done: Plan 9's kernel names
       the file in a walk error (`'nonesuch' file does not exist`,
       `namelenerror`'s `"%#q %s"`); Plan R's says only `file does not
       exist`, so `oracle/expect/errors.err` holds Plan R's text, not
       Plan 9's. The kernel's errors are for the kernel audit.

- **P1** is done apart from BTI, which waits on the toolchain. Its slices
  and the board results are in `docs/ROADMAP.md` and in git history
  (`5905e4d`..`96d70d8`).
- **Tree:** `cargo xtask ci` is green: fmt, pedantic clippy with
  `-D warnings`, the dependency check, host tests, and every QEMU boot
  test on every profile.

## Next steps

P3 slice 10: regexp (`libregexp` semantics), `grep sed tr sort uniq`,
ported from `../plan9/sys/src`; with `sed`, `/rc/bin/kill` can be
tested (its script is Plan 9's). Before or with it, the kernel's audit
against `../plan9/sys/src/9/port`: walk errors that name the file
(`namelenerror`), then the rest of the P2 calls and devices, as the
slice-8 audit did for rc, libr and ramfs. The P3 slices were agreed on
2026-10-19 (`docs/ROADMAP.md`; `srv` and `9fs` moved to P5).

plan9port is built on this host (2026-10-19, by
`scripts/install-plan9port.sh`, in `~/plan9port`; `$PLAN9` is not set,
and xtask defaults to `~/plan9port`). Its compiler and libraries build
Plan 9's own rc for the oracle (`scripts/build-plan9-rc.sh`, needs
bison), and its commands stand in for Plan 9's in `oracle/cmd`. Their
error strings are Unix's, so scripts that show them send stderr to
`/dev/null` or go in `oracle/expect`; `cp mv pwd chmod` are missing
from `$PLAN9/bin`, so those are in `expect` too.

The reference for behaviour is Plan 9's own tree, `../plan9`
(`/home/jkane/Projects/OS/plan9`, Fourth Edition 2025-10-28): port from
its `sys/src`, and check a case against plan9port only where the two
agree; where they differ, Plan 9 wins and the case goes in
`oracle/expect`.

The slice 12 plan, for reference: an `init` in the boot archive that
builds a name space, runs the test programs and reports on the console,
in CI on both arches and every profile.

The slice 11 plan, for reference: `lib/9p` (enums, zero-copy decoder,
encoder, fuzzed), `#M` with tags, interruptible replies and `Tflush`,
the `mount` call checking `noattach`, `fversion`/`fauth`, and a toy 9P
server test.

The slice 10 plan, for reference: notes (`notify`/`noted`, a queue per
process, delivery on the way out, `postnote` to a pid or note group,
interruptible sleeps and "sys: write on closed pipe"); `#p`; `alarm`
and `tsleep`; `nsec`; `semacquire`/`tsemacquire`/`semrelease`;
`segattach`/`segdetach`/`segfree`.

**Reference boards** (`docs/HARDWARE.md`, "Booting Plan R on a board"):
P2 results:
   - **Orange Pi 6 Plus: passed on 2026-10-19** with `hwcheck.ini`
     (`2a5cf34`): 41 of 41 ktests (the four `init -q` runs of
     `memory_freed` all clean), then `init: 11 passed, 0 failed`,
     `kernel: init exited normally`, `kernel: ok`.
   - **Ryzen: passed on 2026-10-19** with `hwcheck.ini` (`2a5cf34`): 45
     of 45 ktests (SMEP/SMAP/UMIP on every CPU, TSC 4691343 kHz against
     the HPET's 4691434), then `init: 11 passed, 0 failed`, `kernel: init
     exited normally`, `kernel: ok`.

P1 results, for reference:
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
- **Never drop an open `Chan` under a spin lock:** a `#M` chan's last
  drop clunks its fid, a 9P round trip. `devmnt` checks
  `proc::sleepable()` and, where it may not sleep, leaves the clunk for
  the session's next request. Still, the groups take chans out under
  their locks and drop them after, so clunks are prompt. A fault no longer exits from the trap handler
  with interrupts off: it posts a note, and the process exits in
  `note::notify`, with interrupts on and nothing held.
- **Notes, and what they do not cover yet:**
  - A `QLock` wait cannot be cut short. A second reader of a pipe,
    queued behind a first one that is blocked, is not interrupted or
    killed until it gets the lock.
  - On amd64 a note handler shares the interrupted code's x87/SSE
    state. Plan 9's `fpunotify` saves it, but user code is soft-float
    for now, so the kernel does not.
  - Notes to kernel threads are refused.
  - `#p/n/ctl` takes only `kill`.
  - Timers run at tick resolution (10 ms).
- **A scheduler deadlock, fixed 2026-10-19:** two CPUs, each leaving a
  thread (to sleep, with interrupts off), could each pick the other's
  thread, woken meanwhile and queued on the other CPU, and wait for ever
  for its `on_cpu` flag, which the other held. It showed as the arm64 hang
  seen once in slice 5, and in `legacy-apic` (about 1 run in 3 once rc's
  tests grew) as a timeout or "TLB shootdown: CPU 1 did not answer". Now
  only a CPU's own context waits for `on_cpu` (`proc::reschedule`); a CPU
  leaving a thread puts a busy one back and goes on as if nothing were
  ready. To look at a hang: `PLANR_MONITOR=/path/sock cargo xtask test …`
  puts QEMU's monitor on a socket (`info registers -a` shows every CPU
  without stopping the guest; the gdb stub made this bug vanish), and a
  shootdown nobody answers now stops the late CPUs with an NMI (amd64)
  and prints where each was, its interrupt flag and its APIC's in-service
  and requested vectors. `PLANR_GDB=port` still gives a gdb stub.
- **`ramfs` limits:** one process answers every request in order, so a
  slow request holds up the rest (none are slow yet); `DMEXCL` is kept
  in the mode but not enforced; every attach is as `eve` (see `#M`), so
  the group and other permission bits are exercised only by the host
  tests.
- **`#M` limits:**
  - Walks go one element at a time: `chan::walk` calls `Dev::walk` per
    element, so each is a `Twalk` and a fid. A multi-element `Dev`
    walk would cut that down.
  - A flush waits for `Rflush` without being cut short: a server that
    never answers it holds the process, even against `kill`, until it
    hangs up.
  - An interrupted `Tversion` leaves the transport unusable.
  - `fauth` gives the auth file, but nothing speaks an auth protocol
    yet (`factotum` is P3+).
  - Every attach is as `eve`.
- **`rfork`'s group flags copy:** `RFNAMEG`, `RFFDG` and `RFENVG` give
  the child a copy; leaving them out shares the group (Plan 9's rule;
  `lib/abi` said otherwise until slice 9). `RFNOMNT` without `RFNAMEG`
  sets `noattach` on the shared name space, the parent's too.
- **Copy-on-write and the TLB:** a page's frame is replaced only after
  every space mapping it is cleared, and the old frame is freed only
  after the flush (`segment::Pending`). A fault on a page the segment
  already maps (another CPU changed it, or it was read-only until now)
  just fixes the faulting space's entry and flushes this CPU.
- **`..` is lexical,** as in Plan 9: `chdir("..")` from a bound
  directory goes to the parent of its name, not of what is bound there.
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
  The block is 1 MiB (debug kernels use about 560 KiB since slice 8); `xtask` prints
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
  per-page state byte has spare bits; copy-on-write counts holders with
  an `Arc` per page instead (`segment::Frame`).
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
