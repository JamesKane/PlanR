# Hardware

## Platform policy

- **64-bit only:** AMD64 (x86-64-v2 baseline, Q10) and ARM64 (Armv8.2-A
  baseline, using Armv9 features when present).
- **UEFI + ACPI only** ([ADR 0003](adr/0003-uefi-acpi-only.md)). The
  firmware describes the machine; we don't hard-code board files. ARM64
  boards must provide SBSA/SBBR-style ACPI (Arm SystemReady). Device-tree
  support may come later, as a separate decision, for boards that can't do
  ACPI.
- **Discovery order:** UEFI memory map and GOP → ACPI static tables →
  PCIe (MCFG) → AML namespace (`_HID`/`_CID`/`_CRS`) for platform devices.

## Bring-up order

| Stage | Platform | Why |
|---|---|---|
| 1 | QEMU `q35` + OVMF (amd64) | Most mature, fastest debug loop, gdbstub |
| 1 | QEMU `virt,acpi=on` + edk2 (arm64) | Same; keeps the arm64 port honest from day one |
| 2 | QEMU with emulated NVMe, xHCI, e1000e/igb, IOMMU (`intel-iommu`/`smmuv3`) | Real-driver practice before real silicon |
| 3 | AMD Ryzen board | Mature UEFI/ACPI; closest to QEMU q35 |
| 4 | Orange Pi 6 Plus | ARM64 SystemReady-class board with GPU/NPU |

We bring up both architectures in QEMU together, so the portable layer
doesn't quietly turn x86-only.

## QEMU configurations

| Name | Command sketch | Purpose |
|---|---|---|
| `amd64-min` | `-M q35 -cpu max -smp 4 -m 2G -bios OVMF.fd -serial stdio -device isa-debug-exit` | CI boot tests |
| `arm64-min` | `-M virt,acpi=on,gic-version=3 -cpu max -smp 4 -m 2G -bios QEMU_EFI.fd -serial stdio` | CI boot tests |
| `*-virtio` | `+ virtio-blk-pci, virtio-net-pci, virtio-rng-pci, virtio-gpu-pci, virtio-keyboard/mouse-pci` | P4–P6 development |
| `*-real` | `+ nvme, qemu-xhci, igb, intel-iommu / smmuv3` | Real-driver practice |
| `*-net2` | Two instances on a `-netdev socket`/bridge | P5 distribution tests |

## AMD Ryzen board

Exact board and CPU to be chosen (Q8). Expected work:

- Firmware: AMI/Insyde UEFI. Expect ACPI quirks, so collect `acpidump`
  output early and add it to the AML test corpus.
- CPU: x2APIC, invariant TSC, CPPC (`_CPC`) for P-states, mixed Zen 5/5c
  cores on some parts (topology from CPUID + PPTT).
- IOMMU: AMD-Vi (IVRS).
- Storage: NVMe. USB: xHCI. Network: typically Realtek RTL8125/8126 or
  Intel i225/i226. Choose a board whose NIC is documented.
- Display: GOP framebuffer first; RDNA iGPU native support in P9.
- NPU: AMD XDNA on "Ryzen AI" parts. P9, depending on documentation.
- Debug: serial header or PCIe/USB serial; also netconsole over UDP.

## Orange Pi 6 Plus

SoC: **CIX P1** (12-core Armv9.2: Cortex-A720 big/mid + Cortex-A520
little; Arm Mali-G720-class GPU; integrated NPU, ~30 TOPS NPU, ~45 TOPS
total by vendor claims; LPDDR5; PCIe Gen4; dual 5GbE). **Check every
detail once the board is in hand.** Vendor specs and early firmware
change.

Expected work:

- Firmware: CIX ships EDK2-based UEFI with ACPI (the same SoC is used in
  other boards marketed as SystemReady). **Biggest early risk:** check how
  complete the ACPI tables are (MADT/GICv3, GTDT, SPCR, MCFG, IORT, PPTT,
  DSDT for on-SoC devices). If they are inadequate, fall back to Q11.
- Interrupts: GICv3 (+ ITS for MSI). Timers: Arm generic timer.
- Boot CPU topology: three core classes, so the scheduler must read PPTT
  and CPPC (`_CPC` highest/nominal performance) to tell them apart.
- Console: UART as described by SPCR.
- IOMMU: SMMUv3 (IORT).
- Storage: NVMe on M.2 PCIe. Network: RTL8126 5GbE over PCIe.
- USB: xHCI (USB 3/USB-C). Display: GOP framebuffer first; Mali display
  and GPU in P9.
- Security features to use: PAN, PXN, BTI, PAC, and MTE if the CIX P1
  enables it.
- Debug: UART header with a USB-serial adapter; JTAG if exposed.

## Accelerator model

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

A uniform shape keeps tools generic. A sketch, to be refined in P9:

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
`libr::thread`. Over the network, 9P carries bulk data instead. Slower,
but transparent.

### Classes

| Class | First target | Notes |
|---|---|---|
| GPU | AMD RDNA iGPU, Mali (CIX P1) | Display first, compute second, 3D last. Open Linux drivers (amdgpu, Panthor) are reference docs. |
| NPU | CIX NPU, AMD XDNA | Needs vendor documentation and firmware (Q9). We may only manage to run vendor-compiled models. |
| Video | CIX VPU, AMD VCN | Later; decode for `page`/media players. |
| Crypto engines | ARMv8 Crypto Extensions, AES-NI, SHA-NI | CPU instructions in `lib/sec` first; off-core engines only if they win. |
| DPU / SmartNIC | none at first | NIC offloads (csum/TSO/RSS) in P5; a DPU as a 9P endpoint (9P over RDMA) later. |

### Heterogeneous CPUs and power

- The scheduler learns core classes from PPTT/CPPC and keeps
  per-class capacity. Placement is energy-aware: interactive and
  latency-sensitive procs go on big cores, background work on little ones.
- `/dev/power` and `/dev/thermal` expose the ACPI thermal zones, CPPC
  requests and device power states.
