# Hardware

Plan R uses the same reference machines as the sibling project AbyssBSD
(`../AbyssBSD-Rust/docs/HARDWARE.md`), so bring-up knowledge, ACPI dumps
and driver lessons carry over between the two. See [ADR 0009](adr/0009-reference-machines.md).

## 1. Platform requirements

| Requirement | AMD64 | ARM64 |
|---|---|---|
| Firmware | UEFI 2.7+ (64-bit), CSM disabled | UEFI 2.7+, Arm SystemReady (SR/ES/IR-with-ACPI) or SBSA/BSA-conformant |
| ACPI | 6.0+: RSDP 2.0/XSDT, MADT, FADT, MCFG, DSDT | 6.3+: MADT (GICC/GICD/GICR/ITS), GTDT, MCFG, IORT, PPTT; SPCR **or DBG2** |
| CPU | **x86-64-v2** minimum ([ADR 0007](adr/0007-amd64-baseline-x86-64-v2.md)); xAPIC **and** x2APIC | Armv8.1-A minimum (LSE atomics, PAN, VHE); Armv9 features (PAC, BTI, MTE, SVE) used when present |
| Interrupts | xAPIC/x2APIC + IOAPIC, MSI/MSI-X | GICv3 + ITS |
| Boot media | GPT disk with an ESP | GPT disk with an ESP |

**UEFI + ACPI only** ([ADR 0003](adr/0003-uefi-acpi-only.md)). Boards that
only provide a device tree are out of scope.

**Discovery order:** UEFI memory map and GOP → ACPI static tables →
PCIe (MCFG) → AML namespace (`_HID`/`_CID`/`_CRS`) for platform devices.

## 2. Reference platforms

| Tier | AMD64 | ARM64 |
|---|---|---|
| **Tier 1, virtual** (CI gate) | QEMU q35 + OVMF | QEMU virt (`gic-version=3,acpi=on`) + edk2/AAVMF |
| **Tier 1, physical** | **Ryzen 9 7900X on Gigabyte X870E AORUS ELITE WIFI7** (§5) | **Orange Pi 6 Plus** (CIX P1 / Sky1) (§4) |
| **Tier 2, best effort** | Other UEFI desktops/laptops; the Alder Lake build server (hybrid P/E cores) as an occasional USB-boot target | Radxa Orion O6 (same SoC), other SystemReady boards |
| **Not targeted** | — | Radxa Dragon Q8B (ACPI written for Windows PEP; a vendor hypervisor polices EL2/SMMU) |

**Bring-up order:** both QEMU targets together from P0, so the portable
layer can't quietly become x86-only. Then the physical machines in P7.
Read-only smoke checks of 9boot on the physical machines can happen earlier,
from USB.

## 3. QEMU configurations

What `cargo xtask` runs today, and what comes next:

| Name | Machine | Status |
|---|---|---|
| amd64 base | `-machine q35 -cpu max -smp 4 -m 2G`, OVMF pflash, virtio-blk disk | **P0, in CI** |
| arm64 base | `-machine virt,acpi=on,gic-version=3 -cpu max -smp 4 -m 2G`, AAVMF pflash, virtio-blk disk, `ramfb` | **P0, in CI** |
| `*-virtio` | + virtio-net, virtio-rng, virtio-gpu, virtio-input | P4–P6 |
| `*-real` | + `nvme`, `qemu-xhci`, `igb`, `intel-iommu` / `smmuv3` | P4, real-driver practice |
| `*-net2` | Two instances on a socket/bridge network | P5 distribution tests |

`ramfb` rather than `virtio-gpu-pci` on arm64: AAVMF's virtio-gpu driver
only offers a BltOnly GOP, with no linear framebuffer, and the P1
framebuffer console needs one.

KVM is used automatically for the host's own architecture when `/dev/kvm`
is accessible; otherwise TCG.

## 4. Board profile: Orange Pi 6 Plus (CIX P1 / Sky1)

Ground truth comes from AbyssBSD's board notes
(`../AbyssBSD/docs/boards/orangepi-6-plus/README.md`), where FreeBSD
16-CURRENT was brought up on this board.

| Item | Fact | What it means for Plan R |
|---|---|---|
| SoC | CIX P1 (CD8180): 12 Armv9.2 cores, 4× A720 @ 2.6 GHz + 4× A720 @ 2.5 GHz + 4× A520; 32 GB | Three core classes, so the scheduler needs capacity-aware placement (PPTT + `_CPC`). PAC, BTI, MTE, SVE available |
| Firmware | UEFI, ACPI only: MADT, GTDT, IORT, MCFG, PCCT, PPTT, SDEI, DBG2, CSRT, DSDT + 2 SSDTs. **No SPCR** | Early console falls back to **DBG2** (COM2, `ARMH0011` at `0x40d0000`) |
| Exception level | UEFI enters at **EL2**, VHE capable, no vendor hypervisor | arm64 entry must handle EL2: minimal EL2 stub, drop to EL1, keep EL2 free for later |
| FP/SIMD | **SVE** | Proc FPU state must save SVE; vector length restored after core power-down |
| Interrupts | GICv3 + ITS (GICv4.1), 512 SPIs. Arm erratum 2941627 | Errata framework in `arch/arm64` from P1/P2 |
| Timers | Generic timer at 1 GHz; no memory-mapped timer; core timers **stop in `_LPI` power-down** | Idle uses WFI only until a broadcast timer (CIX GPT `CIXH1007`) exists |
| Clocks / power | Power resources (`_PR0`/`_PR3`); clocks are **AML methods calling SCMI over PCC** | The AML interpreter must support **PCC operation regions** and power resources (P4) |
| AML quirks | DSDT references nonexistent `I2C0.UXC0`–`UXC3` | Unresolved references are logged and non-fatal, never a panic |
| PCIe | ECAM (MCFG), 3 `PNP0A08` root ports | Generic ECAM, no known quirks |
| Storage | NVMe behind SMMUv3 (firmware leaves bypass) | NVMe works before the IOMMU subsystem (P9) |
| Network | 2× **RTL8126 5 GbE** | The only wired NIC, so an RTL8125/8126 driver is required for P7 |
| USB | 10 xHCI hosts as ACPI `PNP0D10` (**not PCI**) | xHCI attaches via ACPI as well as PCI |
| Watchdog | SBSA generic watchdog; writes to the refresh frame **don't refresh**; use `WOR` | Bring it up early: hangs become resets |
| Console | 10-pin 3.3 V debug header: UART2 pin 1 TX, pin 3 RX, pin 5 GND. GOP 1920×1080 | USB-TTL adapter is the bring-up console |
| Accelerators | Immortalis-G720 GPU (Panthor/CSF), Linlon display, Zhouyi NPU, Mali-V codec, HDA | P9 targets (§6) |

**Access.** Ubuntu on DHCP, so the address moves (seen at `192.168.0.27`
and `192.168.0.19`); user `jkane`, key login; root steps go through a
script the user runs. The board is shared with
AbyssBSD. Plan R boots from USB until a dedicated medium is agreed. Read
the Linux/FreeBSD ground truth before experimenting, since hangs cost power
cycles. ACPI dumps go into `oracle/acpi-corpus/opi6p/`.

## 5. Board profile: Ryzen 9 7900X / X870E AORUS ELITE WIFI7

| Item | Fact | What it means for Plan R |
|---|---|---|
| CPU | Zen 4, 12C/24T, 2 CCDs. AVX-512, FSGSBASE, SMEP, SMAP, UMIP, PKU, RDSEED, SHA, VAES, CPPC. **No LA57, no TSC-deadline** | 4-level paging; LAPIC timer one-shot, calibrated against HPET/TSC; two L3 domains matter for placement |
| Memory | 96 GB DDR5 | — |
| Firmware | AMI UEFI, Secure Boot disabled (setup mode) | Unsigned 9boot boots. Add our own boot entry, never edit Fedora's |
| ACPI | MADT, FACP, DSDT, **15 SSDTs**, HPET, MCFG, IVRS, WDRT, … **No SPCR** | Large AMD/AMI AML corpus; WDRT gives the watchdog; IVRS for AMD-Vi |
| Interrupts | Firmware leaves the APIC in **xAPIC** mode | APIC driver supports xAPIC MMIO as well as x2APIC MSRs |
| Timers | Invariant TSC, HPET, LAPIC | TSC is the clock; HPET calibrates |
| Display | Boot GOP is the **NVIDIA RTX 4070**; Radeon iGPU also present | GOP framebuffer is the primary console |
| Storage | NVMe 0 = **Fedora system disk**; second NVMe for OS work; AHCI ×2 | Plan R may only write its designated medium (see safety rule below) |
| Data disks | **2× WD 8 TB (RAID 0) + HGST 6 TB: user data** | **Never written** |
| Network | **Realtek RTL8125 2.5 GbE**; MT7925 Wi-Fi (out of scope) | Shares the RTL8125/8126 driver with the Orange Pi |
| USB | Chipset and CPU xHCI on PCI; USB4 router (out of scope) | — |
| Serial | **No serial console** | See the console strategy below |

**Console strategy without serial:** (1) GOP framebuffer console from P1;
(2) a persistent log ring in loader-reserved RAM that survives warm reset,
which 9boot saves to `\planr\last.log` on the next boot (it only
overwrites that file where it already exists, so it never writes a volume
that is not Plan R media); (3) the FCH watchdog (WDRT),
so hangs become warm resets; (4) optional UDP log over the RTL8125, or a PCIe
16550 card; (5) USB keyboard with the xHCI work.

**Access.** Fedora 44 at `192.168.0.30` (user `jkane`, key login; root
steps via a user-run script). Boot with `BootNext` (`efibootmgr -n`) so a
failed boot falls back to Fedora on reset.

### Booting Plan R on a board

1. `cargo xtask image --arch <arch> --config scripts/hwcheck.ini` builds
   `build/<arch>/planr.img`. That config runs the in-kernel tests and
   halts with the report on screen (`exit=halt`).
2. On brimstone: `sudo scripts/write-medium.sh build/<arch>/planr.img
   <serial>`. It writes only a USB disk whose serial is listed in
   `scripts/planr-media.conf`, and reads the image back to verify.
3. Plug the stick into the board, copy `scripts/boot-once.sh` to the
   board's `/tmp`, and run `sudo bash /tmp/boot-once.sh` there. It finds
   the stick by Plan R's GPT disk GUID, adds a "Plan R" entry with
   `--create-only` (BootOrder untouched), and sets `BootNext`.
4. Reboot. Plan R runs once; the next reset boots the usual OS.

For log-ring survival use `scripts/hwreset.ini` instead. Its first boot
resets warm, and 9boot, seeing `test=reset` on a fresh ring, has set
`BootNext` to the entry it was started from, so Plan R boots a second
time. That boot saves the first boot's log to `\planr\last.log` on the
stick and halts showing `log: … boot 2`. Read the file back on brimstone
with the stick mounted read-only.

### Storage safety rule

Any `xtask` command that writes physical media (P7 `deploy`) must refuse
every target except an explicitly designated Plan R medium, identified by
GPT disk GUID or serial number, never by device path. Storage drivers attach
foreign disks **read-only** by default.

## 6. Accelerator model

Modern SoCs carry many engines: GPU, NPU, video encode/decode, ISP, crypto,
DMA, DPU/SmartNIC. Plan R models them **as file servers**, so they can be
used locally, imported over the network with `import`, and sandboxed with
namespaces like anything else.

### Layering

```
 apps ──► /dev/accel/N/… (9P)          ◄── same interface locally or via import
           ▲
 accelerator file server (user space)   ← GPU/NPU-specific logic, firmware,
           ▲                               command-stream building
 #H hardware device (kernel)            ← MMIO segments, IRQs-as-files,
           ▲                               IOMMU-bound DMA segments
 IOMMU (AMD-Vi / SMMUv3)                ← isolation; mandatory for #H
```

The kernel provides **mechanism only**:

- `#H/N/mmio`: segment-attachable MMIO window(s) for one device.
- `#H/N/irq`: a read blocks until the next interrupt (MSI/MSI-X).
- `#H/N/dma`: allocate DMA buffers as `Dma` segments, mapped in the
  device's IOMMU domain and shareable with clients through `segattach`.
- `#H/N/ctl`: reset, power state (ACPI `_PS0/_PS3` or SCMI on SoCs),
  firmware load.

### Common file shape

```
/dev/accel/0/
    ctl        "info", "reset", "power on|off", "fw load <path>"
    info       class (gpu|npu|video|crypto|dpu), vendor, model, limits
    mem/       new → allocate buffer; N/{ctl,data,seg} (segattach-able)
    queue/     new → create submission queue; N/{submit,fence,status}
    perf       counters
/dev/display/0/   modes, planes, framebuffers (KMS-like, as files)
```

Data moves in shared `Dma` segments (zero-copy). Control goes through
9P writes. Completions come as reads on `fence` files, which fit `alt` in
`libr::thread`. Over the network, 9P carries bulk data instead.

### Classes

| Class | Reference-machine targets | Notes |
|---|---|---|
| GPU | Immortalis-G720 (CIX P1); Radeon iGPU (Ryzen); RTX 4070 framebuffer only | Display first, compute second, 3D last. Linux Panthor and amdgpu are reference docs, not code to port. NVIDIA beyond GOP is not planned |
| NPU | Zhouyi NPU (CIX P1) | Needs vendor documentation/firmware (Q9) |
| Video | Mali-V codec (CIX P1), VCN (Ryzen iGPU) | Later |
| Crypto | AES-NI, SHA, VAES (Zen 4); Armv8 Crypto Extensions | CPU instructions in `lib/sec` first |
| DPU / SmartNIC | none on the reference machines | NIC offloads in P5; DPU as a 9P endpoint later |

### Heterogeneous CPUs and power

- The scheduler learns core classes from PPTT/CPPC (Orange Pi: three
  classes; Ryzen: two CCDs/L3 domains; Alder Lake: P/E cores) and keeps
  per-class capacity. Placement is energy-aware.
- `/dev/power` and `/dev/thermal` expose ACPI thermal zones, CPPC requests
  and device power states. On the Orange Pi these are AML → PCC → SCMI.
