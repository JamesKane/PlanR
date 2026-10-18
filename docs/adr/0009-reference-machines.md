# ADR 0009: Reference machines shared with AbyssBSD

- Status: Accepted
- Date: 2026-10-17
- Resolves: OPEN-QUESTIONS Q8; informs Q11

## Decision

Plan R uses the same test targets as the sibling AbyssBSD project
(`../AbyssBSD-Rust/docs/HARDWARE.md`):

- Virtual (CI): QEMU q35 + OVMF; QEMU virt (GICv3, ACPI) + edk2.
- AMD64 physical: Ryzen 9 7900X on Gigabyte X870E AORUS ELITE WIFI7.
- ARM64 physical: Orange Pi 6 Plus (CIX P1 / Sky1).

Board facts and constraints are recorded in HARDWARE.md §4–§5.

## Consequences

- ACPI corpora, register dumps and driver lessons are shared between the
  projects. The RTL8125/8126 driver serves both boards.
- Known constraints are now requirements: DBG2 console fallback (no SPCR
  on either board), EL2 entry on arm64, xAPIC as well as x2APIC, ACPI-attached
  xHCI, PCC operation regions in AML, and no serial console on the Ryzen
  (framebuffer plus persistent log ring).
- The machines hold other systems and user data, so media-writing tools
  must refuse anything except a designated Plan R medium.
