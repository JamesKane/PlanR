# Handoff

Where work stands, for picking it up in a new session. Last updated
2026-10-19: P2 is done, in CI and on both reference boards; the P3
slice plan is agreed.

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
- **P1** is done apart from BTI, which waits on the toolchain. Its slices
  and the board results are in `docs/ROADMAP.md` and in git history
  (`5905e4d`..`96d70d8`).
- **Tree:** `cargo xtask ci` is green: fmt, pedantic clippy with
  `-D warnings`, the dependency check, host tests, and every QEMU boot
  test on every profile.

## Next steps

P3, the userland bootstrap. Its 13 slices were agreed on 2026-10-19 and
are in `docs/ROADMAP.md` under P3; `srv` and `9fs` moved to P5. Next is
slice 1, console input: UART receive interrupts on both arches, Plan 9's
line discipline in `#c/cons`, `consctl` raw mode, DEL posting
`interrupt`, and a QEMU test that types on the serial line.

The oracle harness (slice 7) needs host plan9port, built by
`scripts/install-plan9port.sh` (as the user, into `$PLAN9` or
`~/plan9port`, at a fixed revision). On 2026-10-19 this host lacked
only `libXt-devel` for it.

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
