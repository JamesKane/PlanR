# ADR 0010: 9boot → kernel boot protocol

- Status: Accepted
- Date: 2026-10-18

## Context

The kernel must start on two architectures, from UEFI firmware that may
enter at EL1 or EL2 (with or without VHE) on arm64, with enough of the
machine described to bring up a console and find ACPI. Doing the paging and
exception-level setup in a Rust loader, while UEFI's identity map is still
there, is much simpler than doing it in position-independent kernel assembly.

## Decision

**Files.** 9boot loads `\planr\kernel` (static, non-PIE ELF64) and
`\planr\plan9.ini` (optional) from the volume it was started from.

**Kernel image.** Linked at `KERNEL_BASE = 0xffff_ffff_8000_0000`, with
three `PT_LOAD` segments (text RX, rodata R, data+bss RW). 9boot refuses
writable-and-executable segments and segments that share a page.

**Page tables built by 9boot** (4 KiB granule, 4 levels, 48-bit VA;
2 MiB blocks where possible):

| Range | Maps | Permissions |
|---|---|---|
| identity, lower half | all RAM | RWX (temporary; the kernel removes it) |
| `HHDM_BASE = 0xffff_8000_0000_0000 + phys` | all RAM | RW, NX |
| `KERNEL_BASE` … | kernel segments | per segment, W^X |

"RAM" is every memory-map region except MMIO, reserved and bad memory.
On arm64 the lower half is TTBR0 and the upper half TTBR1. `MAIR_EL1` uses
the indices in `bootinfo::arm64` (0 normal WB, 1 device nGnRnE, 2 normal NC).

**Memory types.** 9boot tags its allocations with OS-loader UEFI types
(`0x8000_0001` kernel image, `0x8000_0002` page tables/stack/`BootInfo`),
so the final memory map tells the kernel what is in use. UEFI's map is
converted to sorted, merged `bootinfo::Region`s between the final
`GetMemoryMap` and `ExitBootServices`, without allocating.

**Machine state at kernel entry.**

| | amd64 | arm64 |
|---|---|---|
| Mode | long mode, ring 0 | EL1h (dropped from EL2 if entered there) |
| Paging | `CR3` = boot tables; `EFER.NXE`, `CR0.WP` set | MMU, D and I caches on; `TCR_EL1` 48-bit both halves |
| Interrupts | `cli` | DAIF masked |
| FP/SIMD | not used by the kernel (soft-float target) | `CPACR_EL1.FPEN` enabled |
| Argument | `rdi` = `&BootInfo` (direct-map VA) | `x0` = `&BootInfo` |
| Stack | 64 KiB, direct-map VA | same, `SP_EL1` |
| Descriptor tables | firmware GDT/IDT still loaded; the kernel replaces them first | `VBAR_EL1` unset; the kernel sets it first |

**EL2.** When entered at EL2, 9boot sets `HCR_EL2.RW` (and API/APK so
pointer authentication does not trap). It keeps `HCR_EL2.E2H` as the
firmware left it, programs the EL1 registers (through the `*_EL12` aliases
when E2H=1), enables EL1 timer/counter access and FP in the
E2H-appropriate layouts, and `eret`s straight to the kernel with
translation on. EL2 stays available for a future hypervisor. Installing an
EL2 stub vector table for that is future work.

**`BootInfo`** (`lib/bootinfo`): magic `PLANR9BI`, a version, entry level,
memory regions, RSDP, UEFI system table, framebuffer, kernel physical
location, and config text. All pointers are direct-map addresses.

## Consequences

- The kernel starts in Rust with a stack, the direct map and its image
  mapped with final permissions. No position-independent early assembly.
- 9boot holds architecture-specific paging and EL2 code. The kernel has its
  own MMU code from P1 (MMIO mapping) and replaces the boot tables in P2.
- The three arm64 entry paths (EL2+VHE, EL2 without VHE, EL1) are all
  exercised by `cargo xtask ci`.
- No KASLR yet: the kernel is static. A relocatable (static-PIE) kernel
  is a P10 item.
