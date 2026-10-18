# ADR 0003: UEFI and ACPI only

- Status: Accepted
- Date: 2026-10-17

## Context

The targets are modern AMD64 and ARM64 machines. Plan 9's PC port depended
on BIOS, real mode, MP tables and ISA probing. ARM ports in the tree used
per-board code.

## Decision

Boot only through UEFI (our `9boot` loader) and discover hardware only
through ACPI (plus PCIe enumeration). No BIOS, MP tables, or device trees
in the initial design.

## Consequences

- One discovery path for both architectures and all target boards.
- An AML interpreter is required (written by us).
- ARM64 boards need SystemReady-class firmware. Orange Pi 6 Plus ACPI
  quality is a key risk (Q11).
