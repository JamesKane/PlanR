# Driver Model

- Status: **Proposed** (to become an ADR when accepted)
- Date: 2026-10-18
- Scope: how the kernel finds devices, picks drivers, grants hardware, runs
  drivers and takes them away again; the `#H` hardware device; the
  accelerator file servers built on it (P4 onward)
- Evidence: AbyssBSD's survey of driver models
  (`../AbyssBSD-Rust/docs/DRIVER-MODEL-SURVEY.md`, cited here as *S§n*),
  its design (`../AbyssBSD-Rust/docs/DRIVER-MODEL.md`, cited as *A§n*),
  the Plan 9 source in `../plan9`, and the report *Panthor GPU Architecture
  Analysis* (reviewed in [Appendix A](#appendix-a-review-of-the-panthor-report))

The short version: **two tiers, one set of rules.** Drivers on the I/O hot
path (storage, network, interrupt controllers, consoles) run in the kernel,
inside a framework taken largely from AbyssBSD: the framework owns the
lifecycle, matching is data, a dependency graph replaces link order, and
every way into a driver goes through its binding, so detach can drain it.
Drivers for devices that run their own firmware schedulers (GPU, NPU) or
that sit at the end of a slow bus (USB classes) run as user-space file
servers, holding the same **grants** through the `#H` device, confined by
the IOMMU. Either way the user sees files: Plan 9 devices for the kernel
tier, 9P servers for the user tier, and `#$` for the device tree itself.

## 1. Where this comes from

### 1.1 What we take from AbyssBSD

AbyssBSD's survey covers FreeBSD, the BSDs, illumos, Linux and Rust for
Linux, IOKit/DriverKit, WDM/WDF, Fuchsia, the Rust research kernels and
the microkernels. Its findings hold for Plan R unchanged, and are not
repeated here. The ones this design rests on:

- **The same bugs recur wherever drivers own their own teardown** (S§13.2):
  async entry points outliving detach, confusing the device's lifetime with
  the binding's and with the user-visible object's, ordering by convention,
  completion races, wrong execution context, DMA ownership errors, touching
  an unpowered device, locks held across probe.
- **The mature systems converged** (S§13.3) on declarative matching, a
  framework-owned lifecycle (WDF), an explicit dependency graph, declared
  concurrency, and classes that keep leaf drivers small.
- **Isolation costs** (S§13.4): a thread hop or address-space crossing per
  I/O costs 50–150% CPU (Fuchsia) or 5.6× packet rate (RedLeaf). Type-based
  safety with no crossings is about free.
- **Memory safety is not enough** (S§13.5): lifecycle, ordering and
  DMA-content bugs need a framework that owns the protocol.

### 1.2 What is different for Plan R

| AbyssBSD | Plan R | Consequence |
|---|---|---|
| Must reproduce FreeBSD's userland ABI byte for byte (`devinfo`, `devctl`, `hw.bus.*`, `/dev/pci`) | No compatibility ABI for devices. The user interface is files | The device tree is a small file server, `#$` (§6), designed fresh. All of A§11 drops out |
| 2% performance bound against FreeBSD; user-space drivers rejected outright | Design principle 2 of [VISION](VISION.md): "complex device stacks (GPU, NPU, Wi-Fi) run as user-space file servers, isolated by the IOMMU, wherever performance allows"; Q13 | Two tiers (§3). The survey's cost numbers decide which device goes where |
| Devclass names (`nvme0`, `em0`) are a contract | Plan 9 names (`ether0`, `sdC0`, `#t/eia0`) | Names come from enumeration order, assigned before attach (§4.3) |
| Classes are GEOM, ifnet, tty | Classes are Plan 9 devices: `#S`, `#l`, `#t`, `#c`, `#u` | Plan 9 already had the class split (§1.3) |

### 1.3 What Plan 9 already got right, and what it lacked

Plan 9's kernel drivers are mostly small leaf drivers behind a class
device. `#S` (`port/devsd.c`) drives every disk through the `SDifc` table
(`pnp`, `enable`, `disable`, `verify`, `online`, `rio`, `rctl`, `wctl`),
and `#l` (`port/devether.c`) drives every NIC through an `Ether` with
`attach`, `transmit`, `interrupt`, `ctl` and `promiscuous`. The class owns
naming, queueing and the file interface. That is the IOKit-family / sDDF
lesson (S§6, S§10) a decade before either, and Plan R keeps it.

What Plan 9 lacked is everything after attach:

- **No teardown.** `SDifc.disable` exists, but nothing guarantees it
  drains interrupts or kprocs. Most drivers never call `intrdisable`:
  seven driver files under `pc/` do, none of them Ethernet. Drivers were never meant to
  be detached; there was no hotplug.
- **State spill.** Controllers live on driver-global lists
  (`rtl8169ctlrhead` in `pc/ether8169.c`) with `active` flags, and each
  driver's `pnp()` walks PCI itself with `pcimatch`. Matching is code.
- **Names depend on link order.** `pc/pcf` says it outright: "order of
  ethernet drivers should match that in ../pcboot/boot so that devices are
  detected in the same order by bootstraps and kernels and thus given the
  same controller numbers."
- **No dependency model.** Fine on a 1995 PC. Fatal on an ACPI arm64 SoC
  where a USB controller needs an SCMI power domain, which needs a PCC
  mailbox (see the Orange Pi profile in [HARDWARE.md §4](HARDWARE.md)).
- **Untyped hand-offs.** `void *ctlr`, `edev->port`, `edev->irq`: the same
  ivar problem the survey finds in FreeBSD (S§2.3.5).

## 2. Goals and non-goals

### 2.1 Goals

| # | Goal |
|---|---|
| G1 | **Detach is always safe**, in both tiers: no use of a driver, its hardware or its DMA after detach, whatever caused it (`ctl` write, surprise removal, shutdown, a user-tier server exiting or crashing) |
| G2 | **Ordering is derived, not configured**: attach, shutdown and suspend order come from one dependency graph built from ACPI |
| G3 | **Kernel-tier drivers are safe Rust**: each driver module is `#![forbid(unsafe_code)]`; MMIO, DMA and interrupt plumbing live in a small audited layer |
| G4 | **User-tier drivers cannot escape their grants**: a compromised GPU server can wreck its GPU, not the kernel or other devices' memory |
| G5 | **Wrong execution context is a compile error** in the kernel tier |
| G6 | **Device-written data is untrusted input** in both tiers |
| G7 | **Host-testable**: matching, the dependency graph, naming and every file format under `cargo test` |
| G8 | **Both ISAs from the first driver** |
| G9 | **Everything is a file**: the tree, its events, device control, and the hardware grants themselves |

### 2.2 Non-goals

- A stable driver ABI, binary or source. The kernel is one build, and
  user-tier servers talk to `#H` through files, which are versioned like
  any other file interface.
- Loadable kernel modules. A kernel driver is in the build or it isn't.
- Device tree enumeration ([ADR 0003](adr/0003-uefi-acpi-only.md)).
- Restarting a kernel-tier driver after a panic. A panic is a kernel bug.
  *Device* reset and rebind are in scope; so is restarting a user-tier
  server.
- Porting Linux drivers or a LinuxKPI. Panthor and amdgpu are
  documentation, not code ([ROADMAP](ROADMAP.md) P9).

## 3. Two tiers

```
 user        apps ─► /dev/accel/0/…  /dev/display/0/…   usb/kb  usb/disk   (9P)
             ─────────────────────────────────────────────────────────────────
             user-tier servers: gpufs (Mali CSF / RDNA), npufs, displayfs, usb class drivers
                   │ #H/N/{ctl,mmio,irq,dma}           │ #u/usb/epN.M
 ════════════════════╪════════════════════════════════════╪═══════════════════════
 kernel      #$ (device tree)   #S  #l  #t  #c  #u  #H  ◄── classes as Plan 9 devices
             ──────────────────────────────────────────────────────────────────
             device manager: node table · matcher · dependency graph · lifecycle
             ──────────────────────────────────────────────────────────────────
             kernel-tier drivers: nvme ahci virtio-* xhci rtl8126 uart gic-its …
             ──────────────────────────────────────────────────────────────────
             buses: platform root ── acpi ── pci ── virtio
             ──────────────────────────────────────────────────────────────────
             broker: resource spaces · irq domains · dma tags · iommu · power providers
             ──────────────────────────────────────────────────────────────────
             arch: mmu, interrupt entry, barriers, cache maintenance
```

### 3.1 The placement rule

A device's driver goes in the kernel if **any** of these hold:

1. Its data path runs per I/O at high rate and the CPU is in the loop for
   every request (NVMe, AHCI, virtio-blk, NICs). Crossings here cost what
   the survey says they cost (S§13.4).
2. The system cannot boot or report a failure without it (consoles,
   interrupt controllers, timers, the watchdog, the root disk).
3. Other drivers depend on it as a supplier (GPIO, I2C, PCC mailboxes,
   SCMI providers, ACPI power resources, the IOMMU itself).
4. It is a bus or host controller (PCI, xHCI).

Otherwise it goes in user space, through `#H` (§7). The rule favors user
space for:

- **Firmware-scheduled accelerators.** A Mali CSF GPU runs its own
  scheduler on a Cortex-M7 (the report's main point, which holds). Work is
  submitted by writing to rings in shared memory and ringing a doorbell.
  The CPU is in the loop per *batch of command streams*, not per draw. The
  same is true of AMD's RDNA GPUs with their MES firmware scheduler
  *(from AMD's public prose; details unverified)*. A crossing per frame
  costs nothing measurable; a GPU driver bug costs everything.
- **USB class drivers**, as in Plan 9 (`usb/kb`, `usb/disk` over `#u`).
- **Low-rate, complex, risky stacks**: Wi-Fi firmware interfaces, codecs.

This settles Q13 the way [OPEN-QUESTIONS](OPEN-QUESTIONS.md) proposes it.

### 3.2 One set of rules

The tiers share the framework below the line. A user-tier driver is, to
the device manager, a binding whose driver is `#H`: the node attaches, its
resources are granted, and the grants are then **handed out as files** to
the server that opens `#H/N`. The lifecycle (§5), the dependency graph
(§4.4), the IOMMU domain and the power guarantees (§4.7) are the same.
Only the holder of the grants differs.

## 4. The kernel framework

This follows A§4–§10 closely. Where it is the same, this section says so
and gives only what Plan R needs to know; read AbyssBSD's document for the
detail.

### 4.1 Three lifetimes

As A§4.1, and for the same reason (the devm lesson, S§5.4.1):

| Object | Type | Lives |
|---|---|---|
| Device node | `DeviceRef` (refcounted) | Discovery until deletion |
| Binding | `Binding<D>`, owned by the manager | Start of attach to end of detach |
| Handle | Class objects reached through a `Gate` | Publication until the last open `Chan` lets go |

For Plan R the handle is concrete: **a `Chan` on a class device**. A
`Chan` open on `#S/sdC0/data` holds the class's `SDunit` object, not the
driver. After detach, operations on it fail with `Error::Hungup`
(`"i/o on hungup channel"`), and `close` still works. That is Plan 9's own
error for a vanished stream, applied to vanished devices.

### 4.2 Nodes and states

```rust
pub enum NodeState {
    Unbound,                    // discovered, no driver chosen (or none matches)
    Waiting(WaitReason),        // matched, required suppliers not attached
    Attaching,
    Attached(Power),
    Detaching(DetachReason),
    Failed(AttachFailure),
    Gone,                       // surprise-removed; deleted when the last DeviceRef drops
}

pub enum WaitReason {
    Supplier(DeviceId),         // a known node is not attached yet
    Unresolved(FirmwareRef),    // a firmware reference no node provides (the Sky1 I2C0.UXC*)
    Frozen,                     // attach held by `#$/ctl` ("freeze")
}
```

### 4.3 Names

- **Names are Plan 9's**: `ether0`, `sdC0`, `eia0`, `usbxhci0`. For
  devices Plan 9 never had, the class picks a short name
  (`accel0`, `display0`).
- **Numbers come from enumeration order**, never link order (§1.3).
  Discovery and matching run in one thread in bus order (ACPI namespace
  order, PCI depth-first by segment, bus, device, function), and the
  number is assigned when the driver is chosen, before attach. Attaches
  then run in parallel, but names never depend on timing.
- `sd` keeps Plan 9's letter scheme: the controller letter is assigned
  in the same deterministic order (`sdC`, `sdD`, …). Whether NVMe gets its
  own letter range is open (§10).

### 4.4 Dependencies and ordering

As A§6, unchanged in substance. Supplier → consumer edges come from
firmware before any driver runs: the tree itself, `_DEP`, `_CRS` resource
sources (`GpioIo`, `GpioInt`, `I2cSerialBus`, …), power resources
(`_PR0`, `_PR3`), PCC channels used by AML, IORT (SMMU and ITS), IVRS,
`_PRT` link devices. Edges are required or optional.

- A node attaches when all required suppliers are attached. When a node
  attaches, only its consumers are re-examined. **No blind retry**
  (S§5.4.2).
- A driver that finds a need firmware didn't describe returns
  `AttachError::Wait(Need)`, which names it.
- At the end of boot, every node still `Waiting` is logged with its chain
  of missing suppliers and readable at `#$/waiting`.
- Shutdown, reboot and suspend run in reverse topological order; resume in
  topological order. `quiesce` stops DMA before ACPI S5 or PSCI
  `SYSTEM_OFF`, which matters for the log ring's warm-reset survival.

The graph code (build, cycle-breaking, order, the blocking report) lives
in a host-tested `lib/devtree` crate and runs against
`oracle/acpi-corpus`.

### 4.5 Matching and registration

```rust
pub enum Rank { Exact = 0, Vendor = -10, Default = -20, Class = -100, Fallback = -1000 }

pub const NVME_IDS: &[PciId] = &[
    PciId::class(0x01_08_02).rank(Rank::Class),
    PciId::device(0x144d, 0xa80a).rank(Rank::Default).quirks(Q_SAMSUNG_980),
];
```

- **Tables are `const` data**, evaluated by the manager without running
  driver code. Ties at equal rank are a build error (`cargo xtask ci`
  evaluates every table in a build's driver set).
- An optional read-only `refine` runs once, for the winner only. There is
  no double probe.
- **The driver table is Plan 9's `link` section, as code**: one reviewed
  `static DRIVERS: &[DriverDecl]` per build in `kernel/src/port/config.rs`
  or its successor. No linker-section registration (A§7.1).
- `#H` is itself a driver in the table, with `Rank::Fallback` for any
  node the build marks as user-tier (`gpu`, `npu`) and a `ctl` override
  for any other node (§6.2).

### 4.6 The driver trait and its contexts

As A§7.3 and A§6.6: `attach` (claim grants, reset, build `Self`), `start`
(install handlers, publish, add children), `stop` (no more DMA or
interrupts; infallible), optional `may_detach`, `suspend`, `resume`. No
`detach` callback: detach is `stop`, then framework teardown, then `Drop`.

Context tokens make the wrong context a type error:

| Token | Where | Allows |
|---|---|---|
| `IrqCx` | interrupt filters | spin locks, MMIO, completing to the class, waking kprocs |
| `ThreadCx` | kprocs, timers, class calls | `QLock`, `RwLock`, bounded sleep (`Rendez`) |
| `AttachCx` | `attach` | grants, dependency declarations, sleeping |
| `StartCx` | `start` | handlers, kprocs, publication, children |
| `StopCx` | `stop`, `quiesce` | MMIO, bounded sleep; no new async sources |

Plan R's locks ([ARCHITECTURE.md](ARCHITECTURE.md): RAII `SpinLock`,
`QLock`, `RwLock`) take a `&Sleepable` proof that only thread contexts can
make. Plan 9 has no ithreads: interrupt work that must sleep runs in a
**kproc the binding owns**, woken from the filter. That is the same shape
as a FreeBSD filter plus ithread, built from Plan 9 parts.

Concurrency is declared: `Serialized` (the default; the framework holds
the binding's `QLock` around thread-context entry) or `Concurrent`
(multi-queue NVMe and NICs). Nothing on the data path hops threads.

### 4.7 Grants: the broker

As A§8. Drivers never see a physical address and never construct an
`Mmio`. They receive owned, typed grants that the binding's ledger
records:

| Grant | Kernel tier | User tier (`#H`, §7) |
|---|---|---|
| MMIO window | `Mmio<Regs>` (typed register block from `lib/regs`) | segment of type `Phys`, attachable at `#H/N/mmio/K` |
| Interrupt | handler slot: `filter(fn(&D, usize, &IrqCx))` and/or kproc | `#H/N/irq/K`: read blocks until it fires |
| DMA | `DmaCoherent<T: DeviceShared>`, `DmaStream<B, S>`, `DmaPool` | `Dma` segments in the device's IOMMU domain |
| Power | held on by the framework while bound | held on while bound; `ctl` may request idle states |

Grant rules that matter for both tiers:

- **Vetted ranges.** A node can be granted only resources from its own
  resource list. Interrupt controller, IOMMU, ECAM and console registers
  are never granted to a leaf (Asterinas, S§9).
- **Power before touch.** The framework turns on a node's power resources
  (`_PR0`, SCMI power domain) and sets D0 before `attach`, and keeps them
  on for the binding. No code runs against an unpowered device (S§13.2
  class 7, the Q8B resets).
- **DMA-visible memory holds payload and plain data only** (Thunderclap,
  S§12): no kernel pointers, no list links, nothing a device could
  corrupt into a kernel bug. `DeviceShared` types are `#[repr(C)]`
  integers and arrays with explicit padding, checked by a `const`
  assertion.
- **Device-written data is untrusted.** Indices, lengths and status words
  read from shared memory or registers are bounds-checked; a bad value is
  a `DeviceError`, never a panic.

### 4.8 Classes are Plan 9 devices

| Class device | Leaf trait (kernel tier) | Files |
|---|---|---|
| `#S` sd | `SdQueues`: batch submit, batched completion | `sdC0/{ctl,raw,data,…}` as in Plan 9 |
| `#l` ether | `EtherQueues`: tx/rx queue pairs, offloads, link state | `ether0/{clone,addr,ifstats,stats,N/…}` as in Plan 9 |
| `#t` uart | `UartHw` | `eia0`, `eia0ctl`, `eia0status` |
| `#c` cons | `ConsoleOut`, `ConsoleIn` | `cons`, `kprint`, … |
| `#u` usb | host controller trait (xHCI) | Plan 9's `#u/usb/epN.M` endpoint files for user-tier class drivers |
| `#r` rtc, watchdog, entropy | small traits | `rtc`, `wdog`, `random` |
| `#H` hardware | — (§7) | grants as files |

Class traits are **batch-first and queue-shaped**, requests are owned
values **completed exactly once** (dropping one is a verifier report and
an `Error::Io`), and cancellation belongs to the class (A§9.1).
`SDifc` and `Ether` are the Plan 9 ancestors of `SdQueues` and
`EtherQueues`; the change is ownership and batching, not shape.

## 5. Lifecycle

### 5.1 Detach, in both tiers

One sequence, whatever the reason (`Requested`, `Forced`, `Parent`,
`Gone`, `Shutdown`, `Reset`, `DriverChange`, and for the user tier
`ServerExit`):

1. **Ask**, for `Requested` only: `may_detach`, and whether any class
   `Chan` is open (`InUse` without `force`).
2. **Detach consumers** in the dependency graph, then children.
3. **Revoke handles.** Close every gate; wait for calls already inside.
   Open `Chan`s now fail with `Hungup`.
4. **Stop the hardware.** `stop(why)` in the kernel tier. In the user tier
   the kernel does it itself (§7.4): it cannot trust a dead server to.
5. **Tear down async sources** in reverse order: interrupt slots (cleared,
   then an epoch wait so no CPU is still inside), timers, kprocs. The
   binding's registry is complete because drivers have no other way to
   create them.
6. **Drop the driver.** Grants are fields and release on drop. DMA
   mappings are unmapped and the IOTLB invalidated **before** pages return
   to the allocator.
7. **Check the ledger.** Every grant came back, or the leak is reported
   with the driver's name (a CI failure in verifier mode).
8. **Release** power and move the node to `Unbound`, or delete it.

All transitions run on manager workers, never inside a driver callback. A
node is never detached half-attached.

### 5.2 Reset and rebind

A driver (or `#$/N/ctl reset`) requests a device reset. The framework runs
the detach protocol with `Reset`, asks the bus for the reset (PCIe FLR,
hub port reset, power-domain cycle for platform devices), and attaches
again. A class keeps its published objects when the new binding
republishes the same identity, so open `Chan`s survive a reset where the
class can retry. This is Windows TDR (S§7.1), and it is how a hung GPU
recovers in the user tier too (§8.6).

### 5.3 Verifier mode

As A§13.1, on in debug builds and with `*verifier=1` on the kernel
command line: binding ledger, request accounting, poisoned bounce pages,
seeded fault injection for allocations, grants and timeouts, detach
timeouts with backtraces. P4 adds a detach stress test: repeated
`#$` detach and attach of virtio-blk and NVMe in QEMU under I/O.

## 6. `#$`: the device tree as files

Plan 9's `#$` (`port/devpnp.c`) served ISA PnP and PCI listings. Plan R
keeps the letter and makes it the whole device tree. There is no
`devinfo`, `devctl` or `devd`: `ls`, `cat`, `echo` and the plumber do
those jobs.

### 6.1 Files

```
#$/
    ctl                 "freeze", "thaw", "rescan"
    events              tree changes, one line each (§6.3)
    waiting             nodes still Waiting, with their supplier chains
    graph               the dependency graph, one edge per line
    0/                  one directory per node, by DeviceId index
        name            "ether0", or empty while unbound
        status          state, driver, power, unit; one line
        ident           bus identity: "pci 0000:03:00.0 10ec:8126 sub 1d05:1234 class 020000 rev 01"
                        or "acpi \_SB.PCI0.GP17.XHC0 hid PNP0D10 uid 2"
        res             granted and requested resources, one per line
        deps            "required 14 attached", "optional 22 unbound", …
        ctl             "detach [force]", "attach", "reset", "driver <name>", "driver",
                        "user" (bind to #H), "kernel" (undo), "disable", "enable"
        parent          the parent's index
        children/       one entry per child index
```

Everything is text, one record per line, fields separated by spaces, so
`grep`, `awk` and `rc` handle it. The formats live in `lib/devtree` and
are host-tested; `cat '#$/3/ident'` is the stable way to identify a
device in scripts. Writes to `ctl` need the hostowner.

### 6.2 Moving a device between tiers

`echo user > '#$/14/ctl'` detaches whatever kernel driver holds node 14
and binds it to `#H`. `echo kernel > '#$/14/ctl'` undoes it. Builds mark
the classes that default to `#H` (GPU, NPU). This is VFIO's job on Linux
and `ppt` on FreeBSD, as one `ctl` message.

### 6.3 Events

`#$/events` delivers lines such as:

```
attach 14 ether0 pci 0000:03:00.0 10ec:8126
detach 14 ether0 requested
nomatch 15 pci 0000:04:00.0 1e0f:0001 class 010802
gone 22 usbxhci3
wait 9 required 31 unresolved \_SB.I2C0.UXC0
```

Unlike FreeBSD's single-reader `/dev/devctl` (S§2.3.7), **each open gets
its own stream**, starting at open, with a bounded per-fid queue; on
overflow the reader gets one `overflow N` line. A small user program
forwards events to the plumber, so hotplug policy is plumbing rules, not
a daemon with its own configuration language.

## 7. `#H`: grants as files

`#H` is the user tier's half of the broker. It is a kernel driver that
holds a node's grants and serves them as files to one server process.
[HARDWARE.md §6](HARDWARE.md) sketches it; this section fixes the rules.

### 7.1 Files

```
#H/
    0/                  one directory per node bound to #H (the same index as #$)
        ctl             exclusive open; the server holds it for as long as it drives the device
        info            ident, resource list, IOMMU domain, coherency (_CCA / IORT)
        mmio/0 …        Phys segments, one per MMIO window: segattach("mmio", "#H/0/mmio/0", …)
        irq/0 …         one file per interrupt line or MSI vector
        dma/            "new" → allocate; N/{ctl,seg}
        fw              write-only: firmware image for the bus to load, where the bus does it
```

`ctl` messages: `reset`, `power on|idle|off`, `clock <name> <hz>` (through
the platform providers, not raw SCMI), `irq K mask|unmask`,
`dma export N <name>`, `dma import <name>`.

### 7.2 Interrupts as files

A read of `#H/N/irq/K` blocks until the line fires, then returns the
count of firings since the last read. The kernel's filter for a
`#H` interrupt does the minimum it can do without knowing the device:

- **Level-triggered** lines (a Mali GPU's job, MMU and GPU lines are
  level SPIs): the filter **masks the line** at the interrupt controller
  and wakes the reader. The server clears the cause in the device's own
  registers, then writes `irq K unmask`. The device can't storm the CPU,
  even with a buggy server.
- **MSI/MSI-X**: edge semantics; the filter counts and wakes, no masking.

Reads fit `alt` in `libr::thread`, so a server waits on its interrupts and
its clients' requests in one loop, as a Plan 9 file server does.

### 7.3 DMA and the IOMMU

- **No IOMMU, no DMA.** `#H` refuses `dma/new`, and refuses to bind any
  node whose device can master the bus, unless the node is in an IOMMU
  domain. A user-space process that can program DMA to arbitrary physical
  addresses owns the kernel, whatever its privileges. On the Orange Pi,
  firmware leaves the SMMU in bypass ([HARDWARE.md §4](HARDWARE.md)); the
  user tier therefore waits for the P9 IOMMU work. A `#$` ctl override
  (`user unsafe`) exists for bring-up and is logged as tainting the
  kernel.
- One IOMMU domain per `#H` node (per IOMMU group, where devices can't be
  separated). `dma/new` allocates pages, maps them in that domain at an
  IOVA the kernel picks, and returns both as a `Dma` segment the server
  can attach and pass to clients.
- **Page lifetime is the union of its users.** A page is freed only when
  no process has it attached **and** no IOMMU domain maps it **and** the
  IOTLB invalidation has completed. This is the rule whose absence caused
  the Mali kbase use-after-free on user-mapped CSF I/O pages
  (Appendix A, the "VMA splitting" row): the fix is a refcount on the
  page that both the segment and the IOMMU mapping hold, not a language
  feature.
- **Sharing between devices** (a GPU renders, a display engine scans out):
  `dma export N <name>` publishes a buffer under a kernel-scoped name with
  the exporting server as owner; the other device's server writes
  `dma import <name>` and gets the same pages mapped in **its** IOMMU
  domain. This is dma-buf, as two `ctl` messages. Import needs the
  exporter's permission (the name's mode).

### 7.4 When the server goes away

The binding's state machine is the kernel's, not the server's. When the
`ctl` channel is closed (exit, crash, note-kill), `#H` runs detach with
`ServerExit` and does step 4 itself:

1. Switch the device's IOMMU stream to **abort** (DMA faults instead of
   landing anywhere).
2. Mask every interrupt line and disable MSI at the bus.
3. Reset: PCIe FLR or secondary-bus reset, or for platform devices a power
   domain off-on cycle through the platform providers.
4. Unmap the domain and invalidate. Pages stay alive while clients still
   have segments attached (§7.3), but the device can no longer reach them.

Then the node goes back to `Unbound` under `#H`, and a restarted server
can open it again. Clients of the dead server see their 9P mount hang up
(`Hungup`), which is the normal Plan 9 failure of a file server.

### 7.5 Cost

The user tier pays a crossing where a kernel driver would make a call:

| Operation | Path | Rate in a GPU workload |
|---|---|---|
| Submission | client → 9P write → server → shared-memory ring + doorbell (MMIO store in the server's mapped page) | per batch of command streams, around frame rate |
| Completion | IRQ → filter masks and wakes → server reads `irq` → writes fence → client read returns | per batch |
| Buffer allocation | client → server → `#H` `dma/new` | at load time, then pooled |
| Vertex, texture and command data | none: shared `Dma` segments, written in place | — |

At 144 frames a second that is a few thousand crossings a second, well
inside what a 9P server on one core does. The survey's 50–150% figures
are for a crossing per *I/O* at hundreds of thousands per second, which is
why storage and network stay in the kernel.

## 8. Worked example: the Mali CSF GPU on the Orange Pi

The Immortalis-G720 on the CIX P1 is a Command Stream Frontend GPU, the
generation Linux's Panthor drives. This section shows the model applied to
it. Hardware facts come from Collabora's Panthor and Tyr write-ups and
from Arm's prose; where they are not confirmed against a register
specification, they are marked.

### 8.1 What the hardware asks of the host

- **An MCU to boot.** A Cortex-M class microcontroller runs Arm's firmware
  (`mali_csffw.bin`). The host parses the firmware's interface entries,
  allocates and maps the sections it asks for, powers the MCU on, and
  talks to it through a **global interface** in shared memory: control,
  input and output areas, plus doorbells.
- **Its own MMU.** The GPU walks its own page tables, one set per
  **address space (AS) slot**. The number of AS slots is small and fixed
  by the hardware *(exact count for G720 unverified)*. The host writes
  those page tables.
- **Groups and streams.** Work is organized as **command stream groups**
  (CSGs) holding **command streams** (CSs). Each CS has a ring buffer of
  CS instructions in GPU-visible memory and input/output pages for its
  pointers. The firmware schedules among the CSGs **resident in its slots**.
- **A host scheduler is still needed.** CSG slots are few *(count
  unverified for G720)*. When there are more groups than slots, the host
  rotates groups onto slots, suspending and resuming them through the
  firmware. Panthor has such a tick-based scheduler. The report's claim
  that the kernel only "appends jump addresses and rings a doorbell"
  leaves this out.
- **Interrupts**: job (firmware events), MMU (faults), GPU (power and
  reset), level-triggered *(names from the Panthor DT binding; the CIX
  ACPI description must be read from the DSDT)*.
- **Power and clocks** come from SCMI over PCC on the CIX P1, through the
  platform providers.

### 8.2 Two MMUs, two owners

This is the point the security argument turns on.

```
 GPU shader / MCU access ─► GPU MMU (per-AS page tables) ─► IOVA ─► SoC SMMU ─► physical memory
                             written by gpufs (user)               programmed by the kernel
```

`gpufs` writes the GPU's page tables, so a compromised or buggy `gpufs`
can point any client's GPU addresses anywhere in **its own IOVA space**.
It cannot reach anything else, because the SoC SMMU, which only the
kernel programs, maps that IOVA space onto the pages `#H` allocated for
this device and nothing more. Without the SMMU, a GPU page-table entry is
a physical address and `gpufs` is effectively in the kernel. Hence the
"no IOMMU, no DMA" rule (§7.3), and hence the GPU waits for P9.

The report conflates these two layers ("dynamically adjusting the MCU MPU
and GPU SMMU page tables"); Appendix A says more.

### 8.3 Placement

| Piece | Where | Why |
|---|---|---|
| Power domain, clocks, IOMMU domain, IRQ masking, reset | kernel (platform providers, `#H`) | suppliers (§3.1 rule 3); safety on server exit (§7.4) |
| Firmware load and MCU boot, global interface, group scheduler, GPU page tables, fault handling | `gpufs` | complex, firmware-facing, not per-I/O |
| SPIR-V → Valhall ISA compilation, command stream building, descriptor and memory management | client library (the Vulkan driver), in the application | as on every OS (PanVK's job on Linux) |
| Display (the Linlon engine, a separate device on the CIX P1) | `displayfs`, its own `#H` node | separate hardware; shares buffers by `dma export`/`import` |

### 8.4 Files served by `gpufs`

This refines the report's `/dev/gpu` and HARDWARE.md's `/dev/accel` into
one tree:

```
/dev/accel/0/
    info            "class gpu" "vendor arm" "model g720" "cores 10" "as-slots N" "csg-slots M" …
    ctl             "reset", "power auto|on" (hostowner)
    perf            counters
    clone           open → a new context; read gives its number N
    N/
        ctl         "vm <va-base> <va-size>"
                    "map <buf> <gpu-va> <offset> <len> r|rw|rx"   "unmap <gpu-va> <len>"
                    "queue new <priority>"                        "close"
        mem/
            new     open → allocate; read gives the buffer number B
            B/ctl   "size", "free"
            B/seg   segattach-able Dma segment (local clients)
            B/data  read/write at an offset (remote clients, §8.5)
        queue/
            K/submit   binary records: { cs-va u64, cs-len u32, waits[], signals[] }
            K/fence    read: the latest completed sequence number, blocking until it changes
            K/status   "running", "suspended", "faulted <reason>"
        faults      one line per GPU fault in this context
```

- **One context = one GPU VM + one CSG.** `clone` creates the VM and the
  group, but does **not** occupy hardware slots: the scheduler binds the
  VM to an AS slot and the group to a CSG slot when it runs (§8.1).
- `ctl` is text, as Plan 9 control files are. `submit` is binary, because
  it is a data path and the records reference GPU virtual addresses; its
  layout is a `DeviceShared`-style fixed format in `lib/abi`, versioned
  by a line in `info`.
- `fence` reads fit `alt`. GPU-side waits (one queue waiting on another's
  sync object) stay on the GPU, encoded in the command stream by the
  client library; the host only sees host-visible fences.
- **Isolation between clients** is by 9P: each context directory belongs
  to the user who opened `clone`, with mode 0600. Inside the GPU, contexts
  are separated by AS slots (separate GPU page tables). The report's
  "cryptographically isolated" is not a property anything here has.

### 8.5 Local and remote use

- **Local**: buffers are `Dma` segments attached by both the client and
  `gpufs`; the client writes vertices, textures and command streams in
  place; `submit` carries only addresses. Zero copies.
- **Remote** (`import` of another machine's `/dev/accel`): segments do not
  cross 9P, so the client writes buffer contents through `mem/B/data` and
  reads results back the same way. It works, and it is not zero-copy. The
  report claims both network transparency and zero-copy for the same path;
  Plan R promises the first, locally adds the second, and says so.

The P9 exit criterion ([ROADMAP](ROADMAP.md)) is exactly this pair: a
compute job submitted locally on the Orange Pi, and one submitted through
an imported namespace.

### 8.6 Faults, hangs and recovery

- A GPU page fault is reported on the MMU line. `gpufs` reads the fault
  registers, marks the faulting context's queues `faulted`, appends to
  `N/faults`, and evicts its group. Other contexts continue. Faults are
  not demand paging: the report's suggestion of swapping GPU memory out
  under a running group is not how CSF hardware recovers *(Panthor grows
  tiler heaps on firmware request, a narrow, cooperative case)*.
- A hung group (no progress past a timeout) is suspended through the
  firmware; if the firmware itself stops answering, `gpufs` asks for a
  device reset (`#H/0/ctl reset`), reboots the MCU, and fails the
  contexts that were resident. Open `fence` reads return an error, not a
  hang.
- If `gpufs` itself dies, §7.4 runs: the kernel aborts DMA, masks, resets,
  and the next `gpufs` starts clean. A GPU driver crash is a lost frame,
  not a lost machine.

### 8.7 The same shape on the Ryzen

The Radeon iGPU (RDNA) fits the same tree: firmware-scheduled queues with
doorbells, its own GPU page tables behind AMD-Vi. `info` says
`vendor amd`, the `submit` record format differs, the client library
differs, and everything else (contexts, `mem`, `fence`, `faults`, the
`#H` rules) is the same. That is the test of whether `/dev/accel` is a
class or a Mali driver with a generic name.

## 9. Phasing

| Phase | Driver-model work |
|---|---|
| **P1** (now) | Nothing new. The existing early devices (`uart16550`, `pl011`, `hpet`, interrupt controllers, timers) stay as they are; they become `Early<D>` values adopted into the tree later (A§5.4) |
| **P2** | The `Dev` trait and `Chan`, which class devices and `#$` are built on. Context tokens for the new locks (`Sleepable`) |
| **P4** | `lib/devtree` (matching, graph, naming, `#$` formats; host-tested on the ACPI corpus). Device manager, bindings, gates, ledger, verifier. IRQ domains (LAPIC/IOAPIC, GIC/ITS, PCI MSI-X, ACPI GED). ACPI and PCI buses. DMA direct and bounce backends. virtio-pci, virtio-blk, NVMe, AHCI on `#S`; xHCI on `#u`; virtio-net on `#l`. `#$`. Early-device adoption. Shutdown ordering. Detach stress test |
| **P5** | `#l` grows the native NICs' queue-pair features as the IP stack needs them |
| **P7** | Real hardware: ACPI-attached xHCI, RTL8125/8126, SBSA watchdog, dependency edges from power resources and PCC on the Orange Pi |
| **P9** | IOMMU (AMD-Vi, SMMUv3) and interrupt remapping. `#H`. `dma export`/`import`. `gpufs` for Mali CSF, then RDNA. `displayfs`. NPU if documentation allows (Q9). Runtime PM |

**Prototype `#H` before P9.** QEMU's `edu` device is a PCI device with
MMIO, an interrupt and DMA, made for exercising drivers. With
`intel-iommu` (amd64) or `smmuv3` (arm64) in the `*-real` configuration,
a toy `edufs` can test grants, interrupt files, DMA segments and
server-exit teardown in CI, long before a GPU is involved.

## 10. Open questions

1. **User-mode submission.** CSF queues could in principle be driven
   directly by clients, with the ring, input/output pages and doorbell
   mapped into the client: no crossing at all on submit. That bypasses
   `gpufs`'s view of submissions, which the scheduler and fault
   attribution want. Start server-mediated (§8.4), measure, and revisit.
   Whether Linux has moved this way for Panthor is *unverified*.
2. **Where the GPU page-table writer lives.** `gpufs` writing GPU page
   tables is safe only because of the SMMU (§8.2). An alternative is a
   small kernel `#H` service that validates and writes GPU page-table
   entries (the CHERI GPU-compartment work cited by the report moves page
   tables out of the driver for this reason). It would let the user tier
   run without an IOMMU, at the cost of GPU-specific code in the kernel.
3. **Firmware licensing.** `mali_csffw.bin` is distributed by Arm in
   linux-firmware under its own license *(terms to check)*; the server
   loads it from `/lib/firmware`. Ties to Q9.
4. **`sd` letters for NVMe.** Follow 9front's convention, or Plan R's
   own? (§4.3).
5. **`Serialized` and kprocs.** Should a serialized driver's kproc take
   the binding's lock (simpler drivers) or run beside class calls (lower
   latency)? AbyssBSD takes the lock (A§17.1); same here, until the P4
   interrupt-latency benchmark says otherwise.
6. **Typed register blocks.** `lib/regs` needs a `macro_rules!` register
   block design, prototyped against NVMe's and the GIC's maps before P4.
   AbyssBSD has the same open question (A§17.4); share the answer.

## Appendix A: Review of the Panthor report

*Panthor GPU Architecture Analysis* (in the user's Downloads, not in the
repository) argues that a Mali CSF GPU maps naturally onto a Plan 9 file
server. The thesis holds, and §8 adopts it. Several supporting claims are
wrong or overstated, and a design built on them would go wrong in specific
ways. Its own sources are mostly Collabora blog posts, an icecream95
firmware write-up, press summaries and a few security write-ups.

### A.1 What holds

- CSF moved scheduling from the host to firmware on an MCU, so the host
  path is shared-memory rings plus doorbells. This is why a user-tier
  driver is affordable (§7.5).
- A `clone`/`ctl`/memory/queue/fence file tree is a good fit, and lines up
  with HARDWARE.md's `/dev/accel` shape. §8.4 merges the two.
- The user-mode driver (PanVK's role) carries most of the work: shader
  compilation, command stream building, explicit synchronization in the
  stream.
- Text `ctl` messages are routable over 9P where ioctl structs are not.
- Deprivileging GPU drivers is affordable: the CHERI compartment study it
  cites (Cambridge, CCS 2025) reports single-digit overheads.

### A.2 What to correct

| Claim | Problem | What this design does |
|---|---|---|
| The GPU should be served by the kernel (`/dev/gpu`; "the Rust kernel's interrupt handler … awakens the specific process") | Contradicts Plan R's own principle 2 and Q13, and puts the riskiest driver class in ring 0 | User-tier `gpufs` over `#H` (§3, §8.3) |
| Opening `clone` "provisions an isolated CSG slot on the MCU" | CSG and AS slots are few and fixed. Groups are rotated onto them by a host scheduler | `clone` creates a VM and a group; the scheduler binds slots at run time (§8.1, §8.4) |
| "The host kernel's responsibility is reduced to appending jump addresses … and alerting the MCU via a doorbell" | Omits slot scheduling, suspend/resume, fault handling, heap growth, reset | `gpufs` owns all of these (§8.3, §8.6) |
| Allocation "adjusts the MCU MPU and GPU SMMU page tables" | Conflates three things: the MCU's MPU (firmware's own), the GPU's MMU (host-written, per AS) and the SoC SMMU (the system IOMMU) | Two MMUs, two owners (§8.2) |
| Ring entries are "GPFIFO" 8-byte pairs of pushbuffer address and length | GPFIFO and pushbuffers are NVIDIA terms; the citation is an article about NVIDIA doorbells. CSF rings hold CS instructions | `submit` records are our own format; the ring encoding is `gpufs`'s business |
| Contexts are "cryptographically isolated" | Nothing cryptographic is involved. Isolation is GPU page tables per AS, plus file permissions | Mode-0600 context directories, separate AS (§8.4) |
| If memory runs out, the kernel can "seamlessly swap pages out … and instruct the MCU to pause" the group | GPU faults on CSF are not a paging mechanism; Panthor treats most as fatal for the group | Faults kill the group, not the machine (§8.6) |
| Remote `/dev/gpu` with zero-copy `segattach` | Segments don't cross 9P | Local: zero-copy. Remote: `mem/B/data` (§8.5) |
| `cat /net/tcp/0/data > /dev/gpu/1/queue` | Queue records reference GPU virtual addresses that must already hold the streams and data; and it executes code received from the network on the GPU | Not supported. Remote use goes through an imported `/dev/accel` with authenticated 9P |
| Rust's `Drop` "ensures pages cannot be freed while a mapping reference remains" (the kbase VMA-splitting UAF) | `Drop` frees what the owner drops. The bug was a page whose lifetime ignored one of its mappings, which needs a lifetime rule | Page lifetime is the union of CPU and IOMMU mappings (§7.3) |
| Kernel pointer leak (CVE-2023-26083) is fixed by `#[repr(C)]` and "safe casting" | The fix is never putting kernel pointers in device- or user-visible memory | `DeviceShared` rule (§4.7); in the user tier the server has no kernel pointers to leak |
| Protected mode "guarantees secure workloads cannot be intercepted by the host" | Needs secure-world firmware (TF-A, a TEE) that Plan R does not plan to provide | Out of scope |
| The Tyr project shows the full MCU boot "can be safely implemented" in Rust | Tyr is real and progressing, but as of the cited posts it is early (probe, power-on, MCU boot work in progress) | We read Panthor's and Tyr's prose for hardware facts only (GPL code is not ported) |

### A.3 Things to check before writing `gpufs`

- The CIX P1's ACPI description of the GPU: `_HID`, `_CRS` (MMIO, the
  three interrupts), power resources, and IORT stream IDs. Capture it into
  `oracle/acpi-corpus/opi6p/`.
- G720 AS and CSG slot counts, from the GPU's feature registers on the
  board (read-only probe from Linux first).
- Whether the GPU sits behind the SMMU on this SoC (IORT), and whether
  coherent (`_CCA`).
- Which `mali_csffw.bin` version the board's Linux uses, and its license.
